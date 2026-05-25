//! Durable webhook redelivery.
//!
//! When an inline forwarder dispatch fails ([`forward_to_all`] in
//! `logger.rs`), the audit entry is parked in `webhook_outbox`. This
//! module's [`webhook_outbox_drain_loop`] picks up due rows on a 10s
//! tick, redispatches them through the appropriate transport, and
//! either deletes (on success) or reschedules with exponential backoff
//! (on failure). After [`MAX_OUTBOX_ATTEMPTS`] tries the row is
//! dropped with a counter bump so the operator sees something.
//!
//! Also emits a `alert.outbox_depth_high` audit entry when the backlog
//! has been over [`OUTBOX_ALERT_THRESHOLD`] for more than
//! [`OUTBOX_ALERT_AFTER`] — routes via the existing webhook
//! forwarders so on-call gets paged without a separate alerting stack.

use sqlx::PgPool;
use uuid::Uuid;

use super::forwarders::{
    ForwarderRegistry, send_kafka, send_tcp_syslog, send_udp_syslog, send_webhook,
};
use super::logger::forward_to_all;
use super::types::AuditEntry;

/// Threshold and dwell-time for the "outbox is backed up" audit alert.
const OUTBOX_ALERT_THRESHOLD: i64 = 100;
const OUTBOX_ALERT_AFTER: std::time::Duration = std::time::Duration::from_secs(300);
const OUTBOX_ALERT_REPEAT: std::time::Duration = std::time::Duration::from_secs(900);

const MAX_OUTBOX_ATTEMPTS: i32 = 24;

/// Background drain for `webhook_outbox`. Polls every 10s, picks up
/// to 100 due rows, attempts redelivery, deletes on success, bumps
/// attempt count + reschedules on failure. Caps backoff at one hour
/// and gives up after 24 attempts (~1 day worth of redelivery).
pub(super) async fn webhook_outbox_drain_loop(db: PgPool, registry: ForwarderRegistry) {
    // SSRF defense: don't auto-follow redirects on webhook delivery.
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Tracks when the backlog first crossed `OUTBOX_ALERT_THRESHOLD`.
    // Once it has stayed above the threshold for `OUTBOX_ALERT_AFTER`,
    // we emit an audit entry that the webhook forwarders pick up — and
    // arm a re-fire window so a chronic backlog isn't silent for hours.
    let mut over_threshold_since: Option<std::time::Instant> = None;
    let mut last_alert_at: Option<std::time::Instant> = None;
    loop {
        interval.tick().await;
        if let Err(e) = drain_once(
            &db,
            &registry,
            &http,
            &mut over_threshold_since,
            &mut last_alert_at,
        )
        .await
        {
            tracing::warn!("webhook_outbox drain failed: {e}");
        }
    }
}

/// Compute the next-attempt backoff (in seconds) for a webhook outbox
/// row that just failed `attempt_number` times (1-indexed: first
/// retry is attempt 1). Doubles every attempt, capped at 1 hour so
/// a long-broken receiver doesn't rot in the table for days between
/// attempts. Extracted so the schedule is unit-testable without
/// standing up a Postgres fixture.
fn outbox_backoff_secs(attempt_number: i32) -> u64 {
    let n = attempt_number.max(1) as u32;
    // Saturating shift caps the doubling at attempt 8 → 30 × 128 = 3840s,
    // then clamped to 3600. Anything past attempt 8 stays at 1h.
    let exp = (n - 1).min(7);
    (30u64.saturating_mul(1u64 << exp)).min(3600)
}

pub(super) async fn drain_once(
    db: &PgPool,
    registry: &ForwarderRegistry,
    http: &reqwest::Client,
    over_threshold_since: &mut Option<std::time::Instant>,
    last_alert_at: &mut Option<std::time::Instant>,
) -> Result<(), sqlx::Error> {
    // Surface the backlog depth every tick so operators can alert on
    // "outbox > N rows for M minutes". Published before the drain so
    // the gauge reflects the pre-drain snapshot the tick operated on.
    let depth: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM webhook_outbox")
        .fetch_one(db)
        .await
        .unwrap_or(0);
    metrics::gauge!("webhook_outbox_depth").set(depth as f64);

    // Sustained-backlog alert. The gauge alone tells dashboards what's
    // happening; the audit entry below routes the same signal through
    // the existing webhook forwarders so on-call gets paged without
    // wiring up Prometheus → Alertmanager separately. Forwarders pick
    // it up by `action = "alert.outbox_depth_high"`.
    let now = std::time::Instant::now();
    if depth >= OUTBOX_ALERT_THRESHOLD {
        let crossed_at = *over_threshold_since.get_or_insert(now);
        let dwell = now.duration_since(crossed_at);
        let due_for_first_alert = last_alert_at.is_none() && dwell >= OUTBOX_ALERT_AFTER;
        let due_for_repeat = last_alert_at
            .map(|t| now.duration_since(t) >= OUTBOX_ALERT_REPEAT)
            .unwrap_or(false);
        if due_for_first_alert || due_for_repeat {
            // Forwarder dispatch is fire-and-forget: build the alert
            // payload as if it were a regular audit row and feed it
            // straight to whichever forwarders are subscribed to
            // `audit` log_type. We don't go through AuditLogger::log
            // because we only have access to the registry here, not
            // the channel. Bare entry is intentional — this fires from
            // the outbox monitor with no actor in scope (the SystemActor
            // pattern would work, but this code path doesn't import the
            // trait and adding the import to the monitor module is more
            // noise than it's worth for a single self-emitted alert).
            #[allow(deprecated)]
            let entry = AuditEntry::new("alert.outbox_depth_high")
                .resource("webhook_outbox")
                .detail(serde_json::json!({
                    "depth": depth,
                    "threshold": OUTBOX_ALERT_THRESHOLD,
                    "sustained_secs": dwell.as_secs(),
                }));
            forward_to_all(http, registry, &None, &entry).await;
            *last_alert_at = Some(now);
        }
    } else {
        *over_threshold_since = None;
        *last_alert_at = None;
    }

    #[derive(sqlx::FromRow)]
    struct OutboxRow {
        id: Uuid,
        forwarder_id: Uuid,
        payload: serde_json::Value,
        attempts: i32,
    }

    // Atomic claim + 5-minute lease in one statement. Without this,
    // two server replicas' drain loops would both pick up the same
    // due rows and POST duplicates to the receiver. The inner
    // `FOR UPDATE SKIP LOCKED` skips rows another worker is in the
    // middle of claiming; the outer UPDATE bumps `next_attempt_at`
    // to "5 min from now" as a lease, so even if THIS worker crashes
    // between claim and successful dispatch, the row becomes
    // re-available in 5 min for any worker to retry. On successful
    // dispatch we DELETE the row; on transient failure we UPDATE
    // `next_attempt_at` to the backoff time — either path overrides
    // the lease. RETURNING gives us the columns we'd have selected.
    let due: Vec<OutboxRow> = sqlx::query_as(
        "UPDATE webhook_outbox \
         SET next_attempt_at = now() + interval '5 minutes' \
         WHERE id IN ( \
             SELECT id FROM webhook_outbox \
             WHERE next_attempt_at <= now() \
             ORDER BY next_attempt_at ASC \
             LIMIT 100 \
             FOR UPDATE SKIP LOCKED \
         ) \
         RETURNING id, forwarder_id, payload, attempts",
    )
    .fetch_all(db)
    .await?;

    if due.is_empty() {
        return Ok(());
    }

    let registry_guard = registry.read().await;
    for row in due {
        // Forwarder may have been deleted (cascade should have removed
        // the row, but races happen) or disabled — skip and let the
        // next tick reconsider. Disabled forwarders also stop draining.
        let runtime = match registry_guard.get(&row.forwarder_id) {
            Some(rt) if rt.config.enabled => rt,
            _ => {
                continue;
            }
        };

        // Re-deserialise the entry. A schema drift between insert and
        // drain (extremely unlikely) would surface here as a parse
        // error; in that case we drop the row to avoid a poison pill.
        let entry: AuditEntry = match serde_json::from_value(row.payload.clone()) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    outbox_id = %row.id,
                    error = %e,
                    "outbox payload no longer parses; dropping"
                );
                let _ = sqlx::query("DELETE FROM webhook_outbox WHERE id = $1")
                    .bind(row.id)
                    .execute(db)
                    .await;
                continue;
            }
        };

        // Dispatch by forwarder type, matching the inline path
        // (`forward_to_all`). The previous shape hard-coded
        // `send_webhook` regardless of the forwarder's actual
        // transport, so any `udp_syslog` / `tcp_syslog` / `kafka`
        // delivery that landed in the outbox got retried as a webhook
        // POST against whatever URL `send_webhook` could scrape from
        // the syslog/kafka config (`url` key absent → instant "Missing
        // 'url'" error, infinite-retry until 24-attempt cap). The doc
        // comment above already described this fix as the intent.
        let dispatch_result = match runtime.config.forwarder_type.as_str() {
            "udp_syslog" => send_udp_syslog(runtime, &entry),
            "tcp_syslog" => send_tcp_syslog(runtime, &entry).await,
            "kafka" => send_kafka(http, &runtime.config, &entry).await,
            "webhook" => send_webhook(http, &runtime.config, &entry).await,
            other => Err(format!("Unknown forwarder type for outbox replay: {other}")),
        };
        match dispatch_result {
            Ok(()) => {
                // Dispatch succeeded; tombstone the row so the next drain
                // tick doesn't re-claim and double-send. Silent swallow
                // here used to leave delivered rows in the outbox: the
                // 10s tick re-leased them, the upstream got the payload
                // again, attempts climbed to the 24 cap, and the row was
                // *dropped* as if delivery had failed — masking the
                // double-delivery with a "max retries exceeded" metric.
                if let Err(e) = sqlx::query("DELETE FROM webhook_outbox WHERE id = $1")
                    .bind(row.id)
                    .execute(db)
                    .await
                {
                    metrics::counter!("webhook_outbox_delete_failed_total").increment(1);
                    tracing::error!(
                        outbox_id = %row.id,
                        forwarder_id = %row.forwarder_id,
                        error = %e,
                        "outbox DELETE failed after successful dispatch — row will be re-attempted, \
                         the receiver may see a duplicate"
                    );
                }
                if let Err(e) = sqlx::query(
                    "UPDATE log_forwarders SET sent_count = sent_count + 1, \
                                                last_sent_at = now(), \
                                                updated_at = now() \
                     WHERE id = $1",
                )
                .bind(row.forwarder_id)
                .execute(db)
                .await
                {
                    // Stats drift isn't critical — log at warn and move on.
                    tracing::warn!(
                        forwarder_id = %row.forwarder_id,
                        error = %e,
                        "outbox sent_count UPDATE failed; forwarder health stats may drift"
                    );
                }
            }
            Err(err_msg) => {
                let next_attempts = row.attempts + 1;
                if next_attempts >= MAX_OUTBOX_ATTEMPTS {
                    // Give up — drop the row and surface a metric so
                    // the operator can investigate without an
                    // ever-growing table. Also bump `error_count` on
                    // the forwarder row: this is THE permanent-failure
                    // path, so the counter semantics from
                    // `forward_to_all` are completed here (transient
                    // inline failures no longer bump error_count;
                    // only this exhaustion does).
                    metrics::counter!("audit_log_dropped_total", "kind" => "webhook_outbox_exhausted")
                        .increment(1);
                    let _ = sqlx::query("DELETE FROM webhook_outbox WHERE id = $1")
                        .bind(row.id)
                        .execute(db)
                        .await;
                    let _ = sqlx::query(
                        "UPDATE log_forwarders SET error_count = error_count + 1, \
                                                    last_error = $2, \
                                                    updated_at = now() \
                         WHERE id = $1",
                    )
                    .bind(row.forwarder_id)
                    .bind(&err_msg)
                    .execute(db)
                    .await;
                    tracing::error!(
                        forwarder_id = %row.forwarder_id,
                        attempts = next_attempts,
                        error = %err_msg,
                        "webhook outbox row exhausted; dropping"
                    );
                } else {
                    // Exponential backoff capped at 1h — see `outbox_backoff_secs`.
                    let delay_secs = outbox_backoff_secs(next_attempts);
                    let _ = sqlx::query(
                        "UPDATE webhook_outbox \
                            SET attempts = $2, \
                                last_error = $3, \
                                next_attempt_at = now() + ($4 || ' seconds')::interval \
                          WHERE id = $1",
                    )
                    .bind(row.id)
                    .bind(next_attempts)
                    .bind(&err_msg)
                    .bind(delay_secs.to_string())
                    .execute(db)
                    .await;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbox_backoff_doubles_then_caps_at_one_hour() {
        // Sanity: monotonic non-decreasing and exactly the documented
        // schedule for the first 8 attempts.
        assert_eq!(outbox_backoff_secs(1), 30);
        assert_eq!(outbox_backoff_secs(2), 60);
        assert_eq!(outbox_backoff_secs(3), 120);
        assert_eq!(outbox_backoff_secs(4), 240);
        assert_eq!(outbox_backoff_secs(5), 480);
        assert_eq!(outbox_backoff_secs(6), 960);
        assert_eq!(outbox_backoff_secs(7), 1920);
        // attempt 8 onwards: clamped at 1h.
        assert_eq!(outbox_backoff_secs(8), 3600);
        assert_eq!(outbox_backoff_secs(9), 3600);
        assert_eq!(outbox_backoff_secs(MAX_OUTBOX_ATTEMPTS - 1), 3600);
    }

    #[test]
    fn outbox_backoff_floors_attempt_number_at_one() {
        // Defensive: caller passing 0 or negative shouldn't underflow
        // the shift. Treat as attempt 1.
        assert_eq!(outbox_backoff_secs(0), 30);
        assert_eq!(outbox_backoff_secs(-5), 30);
    }

    #[test]
    fn outbox_backoff_total_max_lifetime_is_under_one_day() {
        // 24 attempts at the cap == ~24h — keeps the doc claim
        // ("dropped after ~1 day") honest. Exact upper bound:
        // 30 + 60 + 120 + 240 + 480 + 960 + 1920 + 16 × 3600.
        let total: u64 = (1..=MAX_OUTBOX_ATTEMPTS).map(outbox_backoff_secs).sum();
        // < 25 hours (86_400 s × 25 / 24 ≈ 90_000); roughly a day.
        assert!(total < 90_000, "total backoff window {total}s exceeds ~25h");
        assert!(
            total > 60_000,
            "total backoff window {total}s shorter than expected"
        );
    }
}
