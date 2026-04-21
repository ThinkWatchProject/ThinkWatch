//! Usage-license dashboard — reports month-to-date Billable Tokens and
//! MCP Tool Calls so operators can see which license tier they're
//! trending toward, *without* phoning home or gating anything. This
//! endpoint is read-only and strictly local.
//!
//! Definitions follow [LICENSING.md](../../../../LICENSING.md):
//!   - Billable Tokens = SUM(input_tokens + output_tokens) on gateway_logs
//!   - MCP Tool Calls  = COUNT(*) on mcp_logs WHERE tool_name != 'tools/list'
//!     (the spec excludes tool discovery)

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::ch_client;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LicenseTier {
    /// One of: Starter / Growth / Scale / Enterprise / Custom.
    pub name: String,
    /// Upper bound for Billable Tokens in this tier, or null for Custom.
    pub tokens_ceiling: Option<i64>,
    /// Upper bound for MCP Tool Calls in this tier, or null for Custom.
    pub calls_ceiling: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UsageLicenseResponse {
    /// UTC calendar-month start for the reported values.
    pub month_start: DateTime<Utc>,
    pub billable_tokens_mtd: i64,
    pub mcp_calls_mtd: i64,
    pub current_tier: LicenseTier,
    /// The next tier up, or null when already at Custom.
    pub next_tier: Option<LicenseTier>,
    /// Per-day Billable Tokens for the current month (1st → today).
    pub tokens_daily: Vec<DailyBucket>,
    /// Per-day MCP Tool Calls for the current month (1st → today).
    pub calls_daily: Vec<DailyBucket>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DailyBucket {
    /// YYYY-MM-DD (UTC).
    pub date: String,
    pub value: i64,
}

/// Fixed Starter/Growth/... table from LICENSING.md. Hard-coded rather
/// than read from the DB because changing the public licensing tiers
/// should be a code change with a PR, not a runtime config tweak.
fn tiers() -> [LicenseTier; 5] {
    [
        LicenseTier {
            name: "Starter".into(),
            tokens_ceiling: Some(10_000_000),
            calls_ceiling: Some(10_000),
        },
        LicenseTier {
            name: "Growth".into(),
            tokens_ceiling: Some(100_000_000),
            calls_ceiling: Some(100_000),
        },
        LicenseTier {
            name: "Scale".into(),
            tokens_ceiling: Some(1_000_000_000),
            calls_ceiling: Some(1_000_000),
        },
        LicenseTier {
            name: "Enterprise".into(),
            tokens_ceiling: Some(10_000_000_000),
            calls_ceiling: Some(10_000_000),
        },
        LicenseTier {
            name: "Custom".into(),
            tokens_ceiling: None,
            calls_ceiling: None,
        },
    ]
}

/// Per LICENSING.md §Tier Determination, the applicable tier is the
/// higher tier reached by either metric. Returns `(current_index, next_index)`.
fn resolve_tier_indices(tokens: i64, calls: i64) -> (usize, Option<usize>) {
    let tiers = tiers();
    let idx_for = |value: i64, f: fn(&LicenseTier) -> Option<i64>| -> usize {
        for (i, t) in tiers.iter().enumerate() {
            match f(t) {
                None => return i, // Custom — unbounded
                Some(ceiling) if value <= ceiling => return i,
                _ => continue,
            }
        }
        tiers.len() - 1
    };
    let tokens_idx = idx_for(tokens, |t| t.tokens_ceiling);
    let calls_idx = idx_for(calls, |t| t.calls_ceiling);
    let cur = tokens_idx.max(calls_idx);
    let next = if cur + 1 < tiers.len() {
        Some(cur + 1)
    } else {
        None
    };
    (cur, next)
}

#[utoipa::path(
    get,
    path = "/api/admin/usage-license",
    tag = "Admin",
    responses(
        (status = 200, description = "Month-to-date usage against licensing tiers", body = UsageLicenseResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden — requires analytics:read_all"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_usage_license(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<UsageLicenseResponse>, AppError> {
    // Only operators who already see global analytics should see the
    // license-volume figures — they are a legal-relevance number.
    auth_user.require_permission("analytics:read_all")?;
    auth_user
        .assert_scope_global(&state.db, "analytics:read_all")
        .await?;

    let now = chrono::Utc::now();
    let month_start = now
        .date_naive()
        .with_day(1)
        .unwrap_or(now.date_naive())
        .and_hms_opt(0, 0, 0)
        .expect("valid hms")
        .and_utc();

    // All license metrics now come from ClickHouse — gateway_logs for
    // billable tokens, mcp_logs for tool calls. CH is effectively
    // required for this endpoint.
    let ch = ch_client(&state)?;
    let month_start_str = month_start.format("%Y-%m-%d %H:%M:%S").to_string();

    #[derive(clickhouse::Row, serde::Deserialize)]
    struct TokenCount {
        cnt: u64,
    }
    #[derive(clickhouse::Row, serde::Deserialize)]
    struct DailyTokens {
        d: String,
        v: u64,
    }

    // Billable tokens MTD — input + output summed over the window.
    // Wrap in ifNull(…, 0) because CH's `Nullable(Int64)` sums to
    // Nullable unless we flatten the NULLs up front.
    let tokens: i64 = ch
        .query(
            "SELECT toUInt64(sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))) AS cnt \
               FROM gateway_logs \
              WHERE created_at >= parseDateTimeBestEffort(?)",
        )
        .bind(&month_start_str)
        .fetch_one::<TokenCount>()
        .await
        .map(|r| r.cnt as i64)
        .unwrap_or(0);

    let token_rows: Vec<DailyTokens> = ch
        .query(
            "SELECT toString(toDate(created_at)) AS d, \
                    toUInt64(sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))) AS v \
               FROM gateway_logs \
              WHERE created_at >= parseDateTimeBestEffort(?) \
              GROUP BY d ORDER BY d ASC",
        )
        .bind(&month_start_str)
        .fetch_all::<DailyTokens>()
        .await
        .unwrap_or_default();
    let parsed_tokens: Vec<(chrono::NaiveDate, i64)> = token_rows
        .into_iter()
        .filter_map(|r| {
            chrono::NaiveDate::parse_from_str(&r.d, "%Y-%m-%d")
                .ok()
                .map(|d| (d, r.v as i64))
        })
        .collect();
    let tokens_daily = densify_daily(&parsed_tokens, month_start, now);

    // MCP tool calls — excludes tools/list per spec.
    #[derive(clickhouse::Row, serde::Deserialize)]
    struct CallCount {
        cnt: u64,
    }
    #[derive(clickhouse::Row, serde::Deserialize)]
    struct CallDaily {
        d: String,
        v: u64,
    }

    let calls = ch
        .query(
            "SELECT count() AS cnt FROM mcp_logs \
             WHERE created_at >= parseDateTimeBestEffort(?) \
               AND (tool_name IS NULL OR tool_name != 'tools/list')",
        )
        .bind(&month_start_str)
        .fetch_one::<CallCount>()
        .await
        .map(|r| r.cnt as i64)
        .unwrap_or(0);

    let call_rows: Vec<CallDaily> = ch
        .query(
            "SELECT toString(toDate(created_at)) AS d, count() AS v \
               FROM mcp_logs \
              WHERE created_at >= parseDateTimeBestEffort(?) \
                AND (tool_name IS NULL OR tool_name != 'tools/list') \
              GROUP BY d ORDER BY d ASC",
        )
        .bind(&month_start_str)
        .fetch_all::<CallDaily>()
        .await
        .unwrap_or_default();
    let parsed_calls: Vec<(chrono::NaiveDate, i64)> = call_rows
        .into_iter()
        .filter_map(|r| {
            chrono::NaiveDate::parse_from_str(&r.d, "%Y-%m-%d")
                .ok()
                .map(|d| (d, r.v as i64))
        })
        .collect();
    let calls_daily = densify_daily(&parsed_calls, month_start, now);

    let (cur_idx, next_idx) = resolve_tier_indices(tokens, calls);
    let tier_list = tiers();
    Ok(Json(UsageLicenseResponse {
        month_start,
        billable_tokens_mtd: tokens,
        mcp_calls_mtd: calls,
        current_tier: tier_list[cur_idx].clone(),
        next_tier: next_idx.map(|i| tier_list[i].clone()),
        tokens_daily,
        calls_daily,
    }))
}

// Clone derive for LicenseTier (used to materialise current/next tier).
impl Clone for LicenseTier {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            tokens_ceiling: self.tokens_ceiling,
            calls_ceiling: self.calls_ceiling,
        }
    }
}

/// Fill gaps between 1st-of-month and today with zero-valued buckets so
/// the frontend can render a continuous x-axis.
fn densify_daily(
    rows: &[(chrono::NaiveDate, i64)],
    month_start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<DailyBucket> {
    use std::collections::HashMap;
    let lookup: HashMap<chrono::NaiveDate, i64> = rows.iter().copied().collect();
    let first = month_start.date_naive();
    let today = now.date_naive();
    let mut out = Vec::new();
    let mut d = first;
    while d <= today {
        out.push(DailyBucket {
            date: d.format("%Y-%m-%d").to_string(),
            value: *lookup.get(&d).unwrap_or(&0),
        });
        d = d.succ_opt().unwrap_or(d);
        if d == first {
            break; // paranoia — succ_opt returning same date
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_resolution_picks_higher_of_both_metrics() {
        // 8M tokens + 25k calls → Growth (calls pushes past Starter)
        let (cur, next) = resolve_tier_indices(8_000_000, 25_000);
        assert_eq!(tiers()[cur].name, "Growth");
        assert_eq!(tiers()[next.unwrap()].name, "Scale");

        // 220M tokens + 80k calls → Scale
        let (cur, _) = resolve_tier_indices(220_000_000, 80_000);
        assert_eq!(tiers()[cur].name, "Scale");

        // Within Starter on both axes
        let (cur, _) = resolve_tier_indices(5_000_000, 5_000);
        assert_eq!(tiers()[cur].name, "Starter");

        // Above Enterprise on tokens → Custom; no next tier.
        let (cur, next) = resolve_tier_indices(50_000_000_000, 100);
        assert_eq!(tiers()[cur].name, "Custom");
        assert!(next.is_none());
    }

    #[test]
    fn tier_boundary_values_stay_in_lower_tier() {
        // 10M tokens exactly is still Starter (range is 0..=10M inclusive)
        let (cur, _) = resolve_tier_indices(10_000_000, 10_000);
        assert_eq!(tiers()[cur].name, "Starter");

        // One over on tokens → Growth
        let (cur, _) = resolve_tier_indices(10_000_001, 10_000);
        assert_eq!(tiers()[cur].name, "Growth");
    }

    #[test]
    fn densify_daily_fills_zero_buckets_for_missing_days() {
        let month_start = chrono::NaiveDate::from_ymd_opt(2026, 4, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let now = chrono::NaiveDate::from_ymd_opt(2026, 4, 5)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let rows = vec![
            (chrono::NaiveDate::from_ymd_opt(2026, 4, 2).unwrap(), 100),
            (chrono::NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(), 300),
        ];
        let out = densify_daily(&rows, month_start, now);
        let values: Vec<i64> = out.iter().map(|b| b.value).collect();
        assert_eq!(values, vec![0, 100, 0, 300, 0]);
        assert_eq!(out.first().unwrap().date, "2026-04-01");
        assert_eq!(out.last().unwrap().date, "2026-04-05");
    }
}
