// ============================================================================
// Natural-period budget caps
//
// Unlike `sliding`, which is a moving 60-bucket window, this module
// implements **calendar-aligned** period counters: daily / weekly /
// monthly. Counters reset at the period boundary in the system TZ
// (currently fixed to UTC; can be lifted to a setting later).
//
// Storage shape (Redis):
//
//   budget:<subject_kind>:<subject_id>:<period>:<bucket_id>
//
// Where `bucket_id` is the ISO calendar key:
//
//   daily   → "2026-04-08"
//   weekly  → "2026-W15"        (ISO week)
//   monthly → "2026-04"
//
// Each key is a single INCRBY counter holding the running weighted
// token total for that subject in that period. TTL is set to 2 ×
// the period length so old keys self-evict without a background
// sweep.
//
// Why a separate module from `sliding`:
//   - Period semantics are different from window semantics — a 5h
//     "today" budget would be confusing.
//   - The check is post-hoc only (responses, not requests) so the
//     all-or-nothing Lua dance is unnecessary; plain INCRBY +
//     value-read is enough.
//   - The Redis key namespace is intentionally separate to avoid
//     collisions if a future migration changes one shape.
// ============================================================================

use chrono::{DateTime, Datelike, Utc};
use fred::clients::Client;
use fred::interfaces::KeysInterface;

use super::{BudgetCap, BudgetSubject};

// ----------------------------------------------------------------------------
// Threshold alerting
// ----------------------------------------------------------------------------

/// A budget threshold crossing event, emitted when `add_weighted_tokens`
/// pushes spend past a configured percentage boundary.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BudgetCrossing {
    pub cap_id: uuid::Uuid,
    pub subject_kind: String,
    pub subject_id: uuid::Uuid,
    pub period: String,
    pub threshold_pct: u8,
    pub limit_tokens: i64,
    pub current_tokens: i64,
}

/// Percentage thresholds at which a crossing fires a budget alert.
/// Each crossing in this list bumps the metric exactly once per
/// period bucket — once 80% has been crossed, subsequent calls in
/// the same calendar bucket only re-fire 95% / 100%, never 80%
/// again.
///
/// The 100% step is the "soft cap" boundary documented in the
/// README — it's the last alert before the engine starts rejecting
/// new requests in the same period.
pub const ALERT_THRESHOLDS_PCT: &[u8] = &[50, 80, 95, 100];

/// Identify which thresholds in `ALERT_THRESHOLDS_PCT` got crossed
/// going from `prev` to `new` against `limit`. Pure function — no
/// I/O, fully testable.
pub fn crossed_thresholds(prev: i64, new: i64, limit: i64) -> Vec<u8> {
    if limit <= 0 || new <= prev {
        return Vec::new();
    }
    ALERT_THRESHOLDS_PCT
        .iter()
        .copied()
        .filter(|pct| {
            // Threshold value in absolute weighted-token units.
            // Use i128 to avoid overflow on the multiply.
            let threshold = (limit as i128 * *pct as i128 / 100) as i64;
            prev < threshold && new >= threshold
        })
        .collect()
}

// ----------------------------------------------------------------------------
// Period key calculation
// ----------------------------------------------------------------------------

/// Format the calendar bucket id for `now` in the given period.
///
/// Stable across processes — every gateway pod for the same period
/// produces the same string, so they all hit the same Redis key.
pub fn bucket_id(period: &str, now: DateTime<Utc>) -> String {
    match period {
        "daily" => now.format("%Y-%m-%d").to_string(),
        "weekly" => {
            let iso = now.iso_week();
            format!("{:04}-W{:02}", iso.year(), iso.week())
        }
        "monthly" => now.format("%Y-%m").to_string(),
        // Unknown period → fall back to monthly so we don't hand
        // back an empty string. The CHECK constraint catches the
        // bad input at insert time anyway.
        _ => now.format("%Y-%m").to_string(),
    }
}

/// TTL (in seconds) to set on the period counter when we INCRBY it.
/// Picked at 2 × the period so a slow process can still find the
/// key on the day after, but it's well gone before any chance of
/// reuse. The period name is the canonical lower-case string from
/// `BudgetPeriod::as_str`.
fn period_ttl_secs(period: &str) -> i64 {
    match period {
        "daily" => 2 * 86_400,
        "weekly" => 2 * 7 * 86_400,
        "monthly" => 2 * 31 * 86_400,
        _ => 31 * 86_400,
    }
}

/// Build the Redis key for a `(subject, period)` counter at the
/// current bucket. Helper to keep the key scheme out of the request
/// path.
pub fn build_key(
    subject_kind: &str,
    subject_id: uuid::Uuid,
    period: &str,
    now: DateTime<Utc>,
) -> String {
    format!(
        "budget:{subject_kind}:{subject_id}:{period}:{}",
        bucket_id(period, now)
    )
}

// ----------------------------------------------------------------------------
// Check / record API
// ----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CapStatus {
    /// The cap row this status came from.
    pub cap_id: uuid::Uuid,
    pub subject_kind: BudgetSubject,
    pub subject_id: uuid::Uuid,
    /// Current weighted-token spend in the period BEFORE the
    /// caller's add (or AFTER, depending on which helper produced it
    /// — read the function docstring).
    pub current: i64,
    pub limit: i64,
}

/// Read the current spend for every cap in the slice. No mutation —
/// used for "show me the dashboard".
pub async fn current_spend(
    redis: &Client,
    caps: &[BudgetCap],
) -> Result<Vec<CapStatus>, fred::error::Error> {
    let now = Utc::now();
    let mut out = Vec::with_capacity(caps.len());
    for cap in caps {
        let key = build_key(
            cap.subject_kind.as_str(),
            cap.subject_id,
            cap.period.as_str(),
            now,
        );
        let v: Option<i64> = redis.get(&key).await.ok();
        out.push(CapStatus {
            cap_id: cap.id,
            subject_kind: cap.subject_kind,
            subject_id: cap.subject_id,
            current: v.unwrap_or(0),
            limit: cap.limit_tokens,
        });
    }
    Ok(out)
}

/// Add `weighted_tokens` to every cap counter in the slice and
/// return the new running totals so the caller can fire alerts on
/// crossings. Soft cap: this never blocks the request — it only
/// records.
///
/// On Redis error this logs and returns `Ok(empty)` so the gateway
/// keeps running with broken accounting (surfaced via the
/// `gateway_budget_fail_open_total` metric). Same fail-open posture
/// as `sliding::check_and_record`.
pub async fn add_weighted_tokens(
    redis: &Client,
    caps: &[BudgetCap],
    weighted_tokens: i64,
) -> Result<(Vec<CapStatus>, Vec<BudgetCrossing>), fred::error::Error> {
    if caps.is_empty() || weighted_tokens <= 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let now = Utc::now();
    let mut out = Vec::with_capacity(caps.len());
    let mut crossings = Vec::new();
    for cap in caps {
        let key = build_key(
            cap.subject_kind.as_str(),
            cap.subject_id,
            cap.period.as_str(),
            now,
        );
        let new_total: Result<i64, _> = redis.incr_by(&key, weighted_tokens).await;
        let new_total = match new_total {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("budget INCRBY failed for {key}: {e}; failing open");
                metrics::counter!("gateway_budget_fail_open_total").increment(1);
                continue;
            }
        };
        // Detect threshold crossings off the prev/new pair. Fires a
        // metric + structured warn for every threshold crossed in
        // this single INCR. Detection is purely arithmetic — no
        // extra Redis round trips.
        let prev_total = new_total - weighted_tokens;
        for pct in crossed_thresholds(prev_total, new_total, cap.limit_tokens) {
            metrics::counter!(
                "gateway_budget_alert_total",
                "subject_kind" => cap.subject_kind.as_str(),
                "period" => cap.period.as_str(),
                "threshold_pct" => pct.to_string(),
            )
            .increment(1);
            tracing::warn!(
                cap_id = %cap.id,
                subject_kind = cap.subject_kind.as_str(),
                subject_id = %cap.subject_id,
                period = cap.period.as_str(),
                threshold_pct = pct,
                limit = cap.limit_tokens,
                current = new_total,
                "budget threshold crossed"
            );
            crossings.push(BudgetCrossing {
                cap_id: cap.id,
                subject_kind: cap.subject_kind.as_str().to_string(),
                subject_id: cap.subject_id,
                period: cap.period.as_str().to_string(),
                threshold_pct: pct,
                limit_tokens: cap.limit_tokens,
                current_tokens: new_total,
            });
        }
        // Refresh TTL on every write — cheap and keeps the key
        // alive across restarts so a forgotten counter never lingers.
        let _: Result<(), _> = redis
            .expire(&key, period_ttl_secs(cap.period.as_str()), None)
            .await;
        out.push(CapStatus {
            cap_id: cap.id,
            subject_kind: cap.subject_kind,
            subject_id: cap.subject_id,
            current: new_total,
            limit: cap.limit_tokens,
        });
    }
    Ok((out, crossings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn bucket_ids_render_correctly() {
        let t = Utc.with_ymd_and_hms(2026, 4, 8, 12, 0, 0).unwrap();
        assert_eq!(bucket_id("daily", t), "2026-04-08");
        assert_eq!(bucket_id("monthly", t), "2026-04");
        // ISO week — 2026-04-08 is week 15.
        let w = bucket_id("weekly", t);
        assert!(w.starts_with("2026-W"), "got {w}");
    }

    #[test]
    fn unknown_period_falls_back_to_monthly() {
        let t = Utc.with_ymd_and_hms(2026, 4, 8, 0, 0, 0).unwrap();
        assert_eq!(bucket_id("nonsense", t), "2026-04");
    }

    #[test]
    fn crossings_fire_only_on_step() {
        // limit = 1000 → thresholds at 500/800/950/1000
        // 0 → 600 crosses 500 only
        assert_eq!(crossed_thresholds(0, 600, 1000), vec![50]);
        // 600 → 850 crosses 800 only (50 already above prev)
        assert_eq!(crossed_thresholds(600, 850, 1000), vec![80]);
        // 850 → 1100 crosses 95 and 100 (over-shoot the cap in
        // one go — both fire)
        assert_eq!(crossed_thresholds(850, 1100, 1000), vec![95, 100]);
        // Same bucket re-INCR → no re-fire
        assert_eq!(crossed_thresholds(1100, 1200, 1000), Vec::<u8>::new());
        // limit = 0 (defensive — can't divide) → never fires
        assert_eq!(crossed_thresholds(0, 999, 0), Vec::<u8>::new());
    }

    #[test]
    fn key_format_is_stable() {
        let t = Utc.with_ymd_and_hms(2026, 4, 8, 0, 0, 0).unwrap();
        let id = uuid::Uuid::nil();
        let key = build_key("user", id, "daily", t);
        assert_eq!(
            key,
            "budget:user:00000000-0000-0000-0000-000000000000:daily:2026-04-08"
        );
    }
}
