//! Cost-trend forecast endpoint.
//!
//! Linear extrapolation: take month-to-date USD spend, divide by the
//! days elapsed in the current calendar month, project the daily run
//! rate to month-end. Operators can act on "you'll hit ~$X by month
//! end" without waiting for the bill. We also report the percent
//! change vs the same number of days last month so dashboards can
//! show ↑/↓ trend chips next to the projection.
//!
//! The forecast is a planning aid, not a promise — the comment on
//! `extrapolate` notes the assumption (constant daily rate). A
//! richer ARIMA / weekday-seasonal model can swap in here later
//! without changing the route shape.

use axum::Json;
use axum::extract::State;
use chrono::{Datelike, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use think_watch_common::cost_decimal::decode_i128;
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::handlers::clickhouse_util::ch_client;
use crate::middleware::auth_guard::AuthUser;

/// Cost numbers serialized as decimal strings (`"12.3456"`) so the
/// frontend's decimal.js never sees an f64. Matches the contract every
/// other cost-bearing endpoint already follows.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CostForecast {
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub month_to_date_usd: Decimal,
    pub days_elapsed: u32,
    pub days_in_month: u32,
    /// Linear month-end projection assuming today's daily run rate
    /// holds for the rest of the month.
    #[serde(with = "rust_decimal::serde::str")]
    #[schema(value_type = String)]
    pub projected_month_end_usd: Decimal,
    /// Same-window spend last month for comparison (`null` if last
    /// month didn't yet have this many days of data — first-month
    /// installs).
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub prior_month_same_window_usd: Option<Decimal>,
    /// Percent change of MTD vs prior_month_same_window (null when
    /// prior is null or zero). Decimal so the frontend gets exact
    /// arithmetic, not the IEEE-754 approximation that f64 carried.
    #[serde(with = "rust_decimal::serde::str_option")]
    #[schema(value_type = Option<String>)]
    pub trend_pct: Option<Decimal>,
}

/// Days in the year/month tuple. Returns 28..=31.
fn days_in_month(year: i32, month: u32) -> u32 {
    let next = if month == 12 {
        chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1);
    match (first, next) {
        (Some(a), Some(b)) => (b - a).num_days() as u32,
        _ => 30,
    }
}

#[utoipa::path(
    get,
    path = "/api/analytics/cost-forecast",
    tag = "Analytics",
    responses(
        (status = 200, description = "MTD + month-end cost projection", body = CostForecast),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_cost_forecast(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<CostForecast>, AppError> {
    auth_user
        .require_global_permission(&state.db, "analytics:read_all")
        .await?;

    let now = Utc::now();
    let year = now.year();
    let month = now.month();
    let day = now.day();
    let days_in = days_in_month(year, month);

    let month_start = chrono::NaiveDate::from_ymd_opt(year, month, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|n| n.and_utc())
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("date math failed")))?;

    // Prior-month same window: from prior month's first day, for
    // exactly the same number of days as elapsed so far this month.
    // Both spans read SUM(cost_usd) from ClickHouse `gateway_logs`;
    // that's now the authoritative store for per-request cost.
    let prior_year = if month == 1 { year - 1 } else { year };
    let prior_month = if month == 1 { 12 } else { month - 1 };
    let prior_start = chrono::NaiveDate::from_ymd_opt(prior_year, prior_month, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|n| n.and_utc())
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("date math failed")))?;
    let prior_window_end = prior_start + Duration::days(day as i64);

    // sumIf(Decimal(18,10)) widens to Decimal(38,10) which CH ships
    // as i128 on the wire. Decoding via decode_i128 keeps the cost
    // pipeline Decimal end-to-end; the previous f64 path silently
    // dropped sub-cent precision and disagreed with every other
    // analytics endpoint.
    #[derive(clickhouse::Row, Deserialize)]
    struct ForecastRow {
        mtd: i128,
        prior: i128,
    }

    let ch = ch_client(&state)?;
    // CH doesn't know PG's `FILTER (WHERE …)` clause but `sumIf` does
    // the same job — conditional sums in a single scan over the union
    // of both windows. No GROUP BY needed because we want a single
    // aggregate row.
    let month_start_str = month_start.format("%Y-%m-%d %H:%M:%S").to_string();
    let prior_start_str = prior_start.format("%Y-%m-%d %H:%M:%S").to_string();
    let prior_end_str = prior_window_end.format("%Y-%m-%d %H:%M:%S").to_string();
    let row = ch
        .query(
            "SELECT \
                sumIf(ifNull(cost_usd, 0), created_at >= parseDateTimeBestEffort(?))                           AS mtd, \
                sumIf(ifNull(cost_usd, 0), created_at >= parseDateTimeBestEffort(?) \
                                          AND created_at <  parseDateTimeBestEffort(?)) AS prior \
             FROM gateway_logs \
             WHERE created_at >= least(parseDateTimeBestEffort(?), parseDateTimeBestEffort(?))",
        )
        .bind(&month_start_str)
        .bind(&prior_start_str)
        .bind(&prior_end_str)
        .bind(&month_start_str)
        .bind(&prior_start_str)
        .fetch_one::<ForecastRow>()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cost_forecast ClickHouse query: {e}")))?;

    let mtd = decode_i128(row.mtd);
    let prior = decode_i128(row.prior);

    let projected = if day == 0 {
        Decimal::ZERO
    } else {
        mtd * Decimal::from(days_in) / Decimal::from(day)
    };

    let prior_opt = if prior > Decimal::ZERO {
        Some(prior)
    } else {
        None
    };
    let hundred = Decimal::from(100);
    let trend_pct = prior_opt.map(|p| (mtd - p) / p * hundred);

    Ok(Json(CostForecast {
        month_to_date_usd: mtd,
        days_elapsed: day,
        days_in_month: days_in,
        projected_month_end_usd: projected,
        prior_month_same_window_usd: prior_opt,
        trend_pct,
    }))
}

#[cfg(test)]
mod tests {
    use super::days_in_month;

    #[test]
    fn standard_31_day_months() {
        for m in [1, 3, 5, 7, 8, 10, 12] {
            assert_eq!(days_in_month(2025, m), 31, "month {m} should be 31 days");
        }
    }

    #[test]
    fn standard_30_day_months() {
        for m in [4, 6, 9, 11] {
            assert_eq!(days_in_month(2025, m), 30, "month {m} should be 30 days");
        }
    }

    #[test]
    fn february_non_leap_year() {
        assert_eq!(days_in_month(2025, 2), 28);
        assert_eq!(days_in_month(2023, 2), 28);
    }

    #[test]
    fn february_leap_year() {
        // 2024 = divisible by 4, not by 100 → leap
        assert_eq!(days_in_month(2024, 2), 29);
        // 2000 = divisible by 400 → leap
        assert_eq!(days_in_month(2000, 2), 29);
    }

    #[test]
    fn february_century_non_leap() {
        // 1900, 2100 = divisible by 100 but not 400 → NOT leap.
        // Lock this in — common bug-magnet for hand-rolled implementations.
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2100, 2), 28);
    }

    #[test]
    fn december_to_january_wraps_year_correctly() {
        // The implementation forms "next month" by adding 1 to month
        // unless month == 12, in which case it advances to (year+1, 1).
        // Verify both branches return correctly without an off-by-one
        // or year overflow.
        assert_eq!(days_in_month(2025, 12), 31);
        assert_eq!(days_in_month(2025, 11), 30);
    }

    #[test]
    fn invalid_month_falls_back_to_30() {
        // Defensive: a month value outside 1..=12 makes
        // `from_ymd_opt` return None, and the function falls back to
        // 30 rather than panicking. Catches the case where a caller
        // forgets that chrono::Datelike returns u32.
        assert_eq!(days_in_month(2025, 13), 30);
        assert_eq!(days_in_month(2025, 0), 30);
    }
}
