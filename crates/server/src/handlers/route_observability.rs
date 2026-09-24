//! Routing observability — live health snapshot per route. Read-only
//! and Redis-backed (see `crates/gateway/src/health.rs`).

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;
use crate::services::observability_repository as repo;
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;
use think_watch_common::errors::AppError;
use think_watch_gateway::health::RouteHealth;
use uuid::Uuid;

/// One entry in the per-model route-health response.
#[derive(Debug, Serialize)]
pub struct RouteHealthEntry {
    pub route_id: Uuid,
    pub provider_id: Uuid,
    pub provider_name: String,
    pub upstream_model: String,
    pub weight: i32,
    pub enabled: bool,
    /// Health snapshot — rolling-window state (`closed`/`open`/
    /// `half_open` + counts + EWMA) plus the cumulative
    /// `lifetime_requests` counter. Defaults to a "closed, no data"
    /// record when the route has had no traffic at all.
    pub health: RouteHealth,
}

/// `GET /api/admin/models/{model_id}/route-health`
///
/// Returns one entry per route attached to the model with the latest
/// rolling-window health snapshot. The UI polls this every few seconds
/// to render the live status badges and EWMA latency column on the
/// model-detail drawer.
pub async fn list_route_health(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<Json<Vec<RouteHealthEntry>>, AppError> {
    auth_user
        .require_global_permission(&state.db, "models:read")
        .await?;

    let rows = repo::model_routes(&state.db, &model_id).await?;

    // Reuse the gateway's HealthTracker — same Redis instance, same
    // window — so the UI sees the exact view the breaker uses to
    // make selection decisions.
    let tracker = think_watch_gateway::health::HealthTracker::new(state.redis.clone());
    let cfg = think_watch_gateway::health::CircuitBreakerConfig::load(&state.dynamic_config).await;

    let route_ids: Vec<Uuid> = rows.iter().map(|r| r.route_id).collect();
    let healths = tracker.snapshot_many(&route_ids, cfg).await;

    let mut by_id: std::collections::HashMap<Uuid, RouteHealth> = healths.into_iter().collect();
    let entries: Vec<RouteHealthEntry> = rows
        .into_iter()
        .map(|r| RouteHealthEntry {
            health: by_id.remove(&r.route_id).unwrap_or_default(),
            route_id: r.route_id,
            provider_id: r.provider_id,
            provider_name: r.provider_name,
            upstream_model: r.upstream_model,
            weight: r.weight,
            enabled: r.enabled,
        })
        .collect();

    Ok(Json(entries))
}
