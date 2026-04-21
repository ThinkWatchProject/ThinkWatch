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
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use sqlx::Row;
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CostForecast {
    pub month_to_date_usd: f64,
    pub days_elapsed: u32,
    pub days_in_month: u32,
    /// Linear month-end projection assuming today's daily run rate
    /// holds for the rest of the month.
    pub projected_month_end_usd: f64,
    /// Same-window spend last month for comparison (`null` if last
    /// month didn't yet have this many days of data — first-month
    /// installs).
    pub prior_month_same_window_usd: Option<f64>,
    /// Percent change of MTD vs prior_month_same_window (null when
    /// prior is null or zero).
    pub trend_pct: Option<f64>,
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
    auth_user.require_permission("analytics:read_all")?;
    auth_user
        .assert_scope_global(&state.db, "analytics:read_all")
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
    // Both spans use SUM(cost_usd) on usage_records.
    let prior_year = if month == 1 { year - 1 } else { year };
    let prior_month = if month == 1 { 12 } else { month - 1 };
    let prior_start = chrono::NaiveDate::from_ymd_opt(prior_year, prior_month, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|n| n.and_utc())
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("date math failed")))?;
    let prior_window_end = prior_start + Duration::days(day as i64);

    let row = sqlx::query(
        "SELECT \
           COALESCE(SUM(cost_usd) FILTER (WHERE created_at >= $1), 0)                                AS mtd, \
           COALESCE(SUM(cost_usd) FILTER (WHERE created_at >= $2 AND created_at < $3), 0)            AS prior \
         FROM usage_records \
         WHERE created_at >= LEAST($1, $2)",
    )
    .bind(month_start)
    .bind(prior_start)
    .bind(prior_window_end)
    .fetch_one(&state.db)
    .await?;

    let mtd: Decimal = row.try_get("mtd").unwrap_or(Decimal::ZERO);
    let prior: Decimal = row.try_get("prior").unwrap_or(Decimal::ZERO);
    let mtd_f = mtd.to_f64().unwrap_or(0.0);
    let prior_f = prior.to_f64().unwrap_or(0.0);

    let projected = if day == 0 {
        0.0
    } else {
        mtd_f * (days_in as f64) / (day as f64)
    };

    let prior_opt = if prior_f > 0.0 { Some(prior_f) } else { None };
    let trend_pct = prior_opt.map(|p| (mtd_f - p) / p * 100.0);

    Ok(Json(CostForecast {
        month_to_date_usd: mtd_f,
        days_elapsed: day,
        days_in_month: days_in,
        projected_month_end_usd: projected,
        prior_month_same_window_usd: prior_opt,
        trend_pct,
    }))
}
