//! Latency SLO snapshot endpoint.
//!
//! Returns p50 / p95 / p99 latency over a configurable window plus
//! the per-provider error rate, so the admin SLO board can render a
//! one-glance "are we within target" view. Sources from CH gateway_logs
//! using `quantilesExact` — exact-percentile sampling is fine at our
//! per-window row volume and gives operators answers that match what
//! provider-side traces show.
//!
//! Targets themselves (e.g. "p99 < 2000ms") live in
//! `system_settings.slo.*` and are read by the frontend; the backend
//! just exposes the measured numbers.

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use think_watch_common::errors::AppError;

use super::clickhouse_util::{ch_available, ch_client};
use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Deserialize)]
pub struct SloQuery {
    /// Time range in hours (1, 24, 168). Default 24.
    #[serde(default)]
    pub hours: Option<u32>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SloSnapshot {
    pub window_hours: u32,
    pub total_requests: u64,
    pub error_requests: u64,
    pub error_rate: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

#[utoipa::path(
    get,
    path = "/api/admin/slo",
    tag = "SLO",
    responses(
        (status = 200, description = "Latency + error-rate snapshot", body = SloSnapshot),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_slo_snapshot(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<SloQuery>,
) -> Result<Json<SloSnapshot>, AppError> {
    auth_user.require_permission("analytics:read_all")?;
    auth_user
        .assert_scope_global(&state.db, "analytics:read_all")
        .await?;

    // Bound the window so a typo ("hours=99999") doesn't trigger a
    // multi-day CH scan. 1 / 24 / 168 (1h / 1d / 1w) cover every UI
    // toggle the dashboard offers.
    let hours = match q.hours.unwrap_or(24) {
        1 | 24 | 168 => q.hours.unwrap_or(24),
        _ => 24,
    };

    if !ch_available(&state) {
        return Ok(Json(SloSnapshot {
            window_hours: hours,
            total_requests: 0,
            error_requests: 0,
            error_rate: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
        }));
    }

    let ch = ch_client(&state)?;
    #[derive(Debug, clickhouse::Row, Deserialize)]
    struct Row {
        total: u64,
        errors: u64,
        p50: f64,
        p95: f64,
        p99: f64,
    }

    // quantileExact rather than the tuple-returning quantilesExact —
    // single-column results decode cleanly into the row struct without
    // chasing the clickhouse-rs Tuple binding.
    let row: Row = ch
        .query(&format!(
            "SELECT \
                count()                                AS total, \
                countIf(status_code >= 400)            AS errors, \
                quantileExact(0.5)(latency_ms)         AS p50, \
                quantileExact(0.95)(latency_ms)        AS p95, \
                quantileExact(0.99)(latency_ms)        AS p99 \
              FROM gateway_logs \
              PREWHERE created_at >= now() - INTERVAL {hours} HOUR \
                AND latency_ms IS NOT NULL"
        ))
        .fetch_one()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let error_rate = if row.total > 0 {
        row.errors as f64 / row.total as f64
    } else {
        0.0
    };

    Ok(Json(SloSnapshot {
        window_hours: hours,
        total_requests: row.total,
        error_requests: row.errors,
        error_rate,
        p50_ms: row.p50,
        p95_ms: row.p95,
        p99_ms: row.p99,
    }))
}
