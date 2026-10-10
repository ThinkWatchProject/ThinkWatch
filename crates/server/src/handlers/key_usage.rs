// ============================================================================
// GET /v1/usage (gateway port)
//
// What the calling API key has used today and this month, every limit
// that binds its AI-gateway requests with what has been used of it, and
// when the key stops working.
//
// The limits are the ones enforcement builds for a request with this key
// (`limits_for_ai_gateway`): the key's own, on its lineage's counters, and
// its owner's effective limits — roles, including those a team grants,
// merged most-restrictive, then the user's overrides — on the owner's
// counters, which count every key the owner has. They are read from the
// same Redis counters, the same way, without charging them. MCP-surface
// limits don't apply to model requests and are left out.
//
// Mounted behind `require_api_key_to_read`: the same key checks as a model
// request, but calling it is not a use of the key.
// ============================================================================

use std::cmp::Ordering;

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Datelike, NaiveTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::limits::{
    BudgetCap, BudgetPeriod, BudgetSubject, RateLimitRule, RateLimitSubject, RequestLimits, budget,
    key_usage, secs_to_window, sliding,
};
use think_watch_gateway::proxy::{GatewayRequestIdentity, limits_for_ai_gateway};

use crate::app::AppState;

#[derive(Debug, Serialize, PartialEq)]
pub struct KeyUsageResponse {
    pub usage: Usage,
    pub limits: Vec<LimitUsage>,
    /// When the key stops authenticating; `null` when it does not.
    pub expires_at: Option<DateTime<Utc>>,
}

/// What this key (its lineage, across rotations) has done, UTC day and
/// month. Tokens are weighted tokens, as limits count them.
#[derive(Debug, Serialize, PartialEq)]
pub struct Usage {
    pub requests_today: i64,
    pub tokens_today: i64,
    pub requests_month: i64,
    pub tokens_month: i64,
    /// The key's cost this month from the request log, as the cost
    /// reports sum it; `null` without ClickHouse or when it can't be read.
    pub cost_usd_month: Option<f64>,
}

/// One limit that binds the key's requests.
#[derive(Debug, Serialize, PartialEq)]
pub struct LimitUsage {
    /// `key`: set on the key, counted for it alone. `user`: its owner's,
    /// counted for everything the owner does.
    pub scope: &'static str,
    /// `requests` or `tokens` (weighted).
    pub kind: &'static str,
    /// A rate limit's sliding window (`1m` … `1w`) or a budget's calendar
    /// period (`daily`, `weekly`, `monthly`).
    pub window: String,
    /// A sliding window's length; `null` for a calendar period.
    pub window_secs: Option<i64>,
    pub limit: i64,
    /// What enforcement compares with `limit`.
    pub used: i64,
    /// A calendar period's end (UTC); `null` for a sliding window.
    pub resets_at: Option<DateTime<Utc>>,
}

/// GET /v1/usage
pub async fn get_key_usage(
    State(state): State<AppState>,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
) -> Response {
    let limits = limits_for_ai_gateway(&identity);
    let now = Utc::now();

    let counts = match limits.key_lineage {
        Some(lineage) => match key_usage::read(&state.redis, lineage, now).await {
            Ok(c) => c,
            Err(e) => return unavailable(&e),
        },
        None => key_usage::KeyUsageCounts::default(),
    };
    let mut rule_used = Vec::with_capacity(limits.rules.len());
    for rule in &limits.rules {
        let resolved = sliding::ResolvedRule::new(rule, limits.owner);
        match sliding::read_count(&state.redis, &resolved, now.timestamp()).await {
            Ok(n) => rule_used.push(n),
            Err(e) => return unavailable(&e),
        }
    }
    let cap_used: Vec<i64> = match budget::read_spend(&state.redis, &limits.caps, now).await {
        Ok(statuses) => statuses.into_iter().map(|s| s.current).collect(),
        Err(e) => return unavailable(&e),
    };
    let cost = match limits.key_lineage {
        Some(lineage) => cost_this_month(&state, lineage, now).await,
        None => None,
    };

    Json(response(
        &limits,
        &rule_used,
        &cap_used,
        counts,
        cost,
        identity.key_expires_at,
        now,
    ))
    .into_response()
}

/// The answer for a request's limits and what each has used
/// (`rule_used` pairs with `limits.rules`, `cap_used` with
/// `limits.caps`), the key's counts and cost. Limits are ordered by the
/// share of them left, least first.
fn response(
    limits: &RequestLimits,
    rule_used: &[i64],
    cap_used: &[i64],
    counts: key_usage::KeyUsageCounts,
    cost_usd_month: Option<f64>,
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> KeyUsageResponse {
    let mut entries: Vec<LimitUsage> = limits
        .rules
        .iter()
        .zip(rule_used)
        .map(|(rule, &used)| rule_entry(rule, used))
        .chain(
            limits
                .caps
                .iter()
                .zip(cap_used)
                .map(|(cap, &used)| cap_entry(cap, used, now)),
        )
        .collect();
    entries.sort_by(by_share_left);
    KeyUsageResponse {
        usage: Usage {
            requests_today: counts.requests_today,
            tokens_today: counts.tokens_today,
            requests_month: counts.requests_month,
            tokens_month: counts.tokens_month,
            cost_usd_month,
        },
        limits: entries,
        expires_at,
    }
}

fn rule_entry(rule: &RateLimitRule, used: i64) -> LimitUsage {
    LimitUsage {
        scope: match rule.subject_kind {
            RateLimitSubject::ApiKeyLineage => "key",
            RateLimitSubject::User => "user",
        },
        kind: rule.metric.as_str(),
        window: secs_to_window(rule.window_secs)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}s", rule.window_secs)),
        window_secs: Some(i64::from(rule.window_secs)),
        limit: rule.max_count,
        used,
        resets_at: None,
    }
}

fn cap_entry(cap: &BudgetCap, used: i64, now: DateTime<Utc>) -> LimitUsage {
    LimitUsage {
        scope: match cap.subject_kind {
            BudgetSubject::ApiKeyLineage => "key",
            BudgetSubject::User => "user",
        },
        kind: "tokens",
        window: cap.period.as_str().to_string(),
        window_secs: None,
        limit: cap.limit_tokens,
        used,
        resets_at: Some(budget::period_end(cap.period, now)),
    }
}

/// Least of its limit left first; on a tie, the key's before its
/// owner's, sliding windows shortest first before calendar periods, and
/// requests before tokens.
fn by_share_left(a: &LimitUsage, b: &LimitUsage) -> Ordering {
    // (limit - used) / limit, compared without dividing; limits are > 0.
    let left = |e: &LimitUsage| i128::from(e.limit) - i128::from(e.used);
    let (la, lb) = (i128::from(a.limit.max(1)), i128::from(b.limit.max(1)));
    (left(a) * lb)
        .cmp(&(left(b) * la))
        .then_with(|| (a.scope != "key").cmp(&(b.scope != "key")))
        .then_with(|| window_rank(a).cmp(&window_rank(b)))
        .then_with(|| (a.kind != "requests").cmp(&(b.kind != "requests")))
}

fn window_rank(e: &LimitUsage) -> i64 {
    e.window_secs.unwrap_or_else(|| {
        let period = BudgetPeriod::parse(&e.window);
        i64::MAX
            - match period {
                Some(BudgetPeriod::Daily) => 3,
                Some(BudgetPeriod::Weekly) => 2,
                Some(BudgetPeriod::Monthly) | None => 1,
            }
    })
}

/// The key's cost since the 1st of this month, UTC, summed from the
/// request log as the cost reports sum it. `None` without ClickHouse;
/// a failed read is logged and also `None` — the cost is informational,
/// the counters above are what limits use.
async fn cost_this_month(state: &AppState, lineage: Uuid, now: DateTime<Utc>) -> Option<f64> {
    #[derive(clickhouse::Row, Deserialize)]
    struct Cost {
        cost: i128,
    }
    let ch = state.clickhouse.as_ref()?;
    let month_start = now
        .date_naive()
        .with_day(1)
        .expect("every month has a 1st")
        .and_time(NaiveTime::MIN)
        .and_utc();
    let row = ch
        .query(
            "SELECT sum(ifNull(cost_usd, 0)) AS cost FROM gateway_logs \
             PREWHERE created_at >= parseDateTimeBestEffort(?) \
                AND api_key_lineage_id = ?",
        )
        .bind(month_start.format("%Y-%m-%d %H:%M:%S").to_string())
        .bind(lineage.to_string())
        .fetch_one::<Cost>()
        .await;
    match row {
        Ok(r) => think_watch_common::cost_decimal::decode_i128(r.cost).to_f64(),
        Err(e) => {
            tracing::warn!(error = %e, "key usage: monthly cost read failed");
            None
        }
    }
}

/// The counters could not be read. Answering zero would say the key has
/// room it may not have.
fn unavailable(e: &fred::error::Error) -> Response {
    tracing::warn!("key usage read failed: {e}");
    let body = tw_dialect::convert::error_body(
        tw_dialect::ir::Dialect::Chat,
        StatusCode::SERVICE_UNAVAILABLE.as_u16(),
        "Usage counters are unavailable.",
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use think_watch_common::limits::{
        RateMetric, Surface, SurfaceBlock, SurfaceBudget, SurfaceConstraints, SurfaceRule,
    };

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn block(rules: &[(RateMetric, i32, i64)], budgets: &[(BudgetPeriod, i64)]) -> SurfaceBlock {
        SurfaceBlock {
            rules: rules
                .iter()
                .map(|&(metric, window_secs, max_count)| SurfaceRule {
                    metric,
                    window_secs,
                    max_count,
                    enabled: true,
                })
                .collect(),
            budgets: budgets
                .iter()
                .map(|&(period, limit_tokens)| SurfaceBudget {
                    period,
                    limit_tokens,
                    enabled: true,
                })
                .collect(),
        }
    }

    fn counts() -> key_usage::KeyUsageCounts {
        key_usage::KeyUsageCounts {
            requests_today: 3,
            tokens_today: 300,
            requests_month: 40,
            tokens_month: 4_000,
        }
    }

    #[test]
    fn a_key_bound_by_nothing_still_reports_its_usage() {
        let limits = RequestLimits::for_request(
            Surface::AiGateway,
            Uuid::new_v4(),
            &SurfaceConstraints::default(),
            Some((Uuid::new_v4(), &SurfaceConstraints::default())),
        );
        let body = response(&limits, &[], &[], counts(), None, None, at(2026, 10, 14, 9));
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({
                "usage": {"requests_today": 3, "tokens_today": 300,
                          "requests_month": 40, "tokens_month": 4000,
                          "cost_usd_month": null},
                "limits": [],
                "expires_at": null,
            })
        );
    }

    #[test]
    fn the_keys_and_its_owners_limits_map_to_the_contract_shape() {
        // Wednesday 2026-10-14, 09:00 UTC. The owner has a requests/min
        // rule and a monthly budget (MCP rules don't bind model requests);
        // the key a 5h token window and a daily budget.
        let user = SurfaceConstraints {
            ai_gateway: Some(block(
                &[(RateMetric::Requests, 60, 10)],
                &[(BudgetPeriod::Monthly, 1_000_000)],
            )),
            mcp_gateway: Some(block(&[(RateMetric::Requests, 60, 1)], &[])),
        };
        let key = SurfaceConstraints {
            ai_gateway: Some(block(
                &[(RateMetric::Tokens, 18_000, 50_000)],
                &[(BudgetPeriod::Daily, 100_000)],
            )),
            mcp_gateway: None,
        };
        let limits = RequestLimits::for_request(
            Surface::AiGateway,
            Uuid::new_v4(),
            &user,
            Some((Uuid::new_v4(), &key)),
        );
        // Input order: user rule, key rule; user cap, key cap.
        let body = response(
            &limits,
            &[5, 45_000],
            &[250_000, 100_500],
            counts(),
            Some(1.25),
            Some(at(2026, 12, 31, 0)),
            at(2026, 10, 14, 9),
        );
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({
                "usage": {"requests_today": 3, "tokens_today": 300,
                          "requests_month": 40, "tokens_month": 4000,
                          "cost_usd_month": 1.25},
                "limits": [
                    // Overshot: -0.5 % left.
                    {"scope": "key", "kind": "tokens", "window": "daily", "window_secs": null,
                     "limit": 100000, "used": 100500, "resets_at": "2026-10-15T00:00:00Z"},
                    // 10 % left.
                    {"scope": "key", "kind": "tokens", "window": "5h", "window_secs": 18000,
                     "limit": 50000, "used": 45000, "resets_at": null},
                    // 50 % left.
                    {"scope": "user", "kind": "requests", "window": "1m", "window_secs": 60,
                     "limit": 10, "used": 5, "resets_at": null},
                    // 75 % left.
                    {"scope": "user", "kind": "tokens", "window": "monthly", "window_secs": null,
                     "limit": 1000000, "used": 250000, "resets_at": "2026-11-01T00:00:00Z"},
                ],
                "expires_at": "2026-12-31T00:00:00Z",
            })
        );
    }

    #[test]
    fn ties_put_the_key_first_then_the_shorter_window_then_requests() {
        let e = |scope, kind, window: &str, window_secs| LimitUsage {
            scope,
            kind,
            window: window.to_string(),
            window_secs,
            limit: 10,
            used: 0,
            resets_at: None,
        };
        let mut v = [
            e("user", "requests", "1m", Some(60)),
            e("key", "tokens", "monthly", None),
            e("key", "tokens", "weekly", None),
            e("key", "tokens", "1w", Some(604_800)),
            e("key", "tokens", "1m", Some(60)),
            e("key", "requests", "1m", Some(60)),
        ];
        v.sort_by(by_share_left);
        let order: Vec<(&str, &str, &str)> = v
            .iter()
            .map(|e| (e.scope, e.kind, e.window.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("key", "requests", "1m"),
                ("key", "tokens", "1m"),
                ("key", "tokens", "1w"),
                ("key", "tokens", "weekly"),
                ("key", "tokens", "monthly"),
                ("user", "requests", "1m"),
            ]
        );
    }

    #[test]
    fn every_allowed_window_has_its_label() {
        let r = |secs| RateLimitRule {
            id: Uuid::nil(),
            subject_kind: RateLimitSubject::User,
            subject_id: Uuid::nil(),
            surface: Surface::AiGateway,
            metric: RateMetric::Requests,
            window_secs: secs,
            max_count: 1,
            enabled: true,
            expires_at: None,
            reason: None,
            created_by: None,
        };
        let labels: Vec<String> = [60, 300, 3_600, 18_000, 86_400, 604_800]
            .into_iter()
            .map(|s| rule_entry(&r(s), 0).window)
            .collect();
        assert_eq!(labels, ["1m", "5m", "1h", "5h", "1d", "1w"]);
    }
}
