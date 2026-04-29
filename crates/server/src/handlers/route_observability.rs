//! Routing observability — live health snapshot per route. Read-only
//! and Redis-backed (see `crates/gateway/src/health.rs`).

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;
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
    /// Health snapshot (`closed`/`open`/`half_open` + counts + EWMA).
    /// Defaults to a "closed, no data" record when the route has had
    /// no recent traffic.
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
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;

    #[derive(sqlx::FromRow)]
    struct Row {
        route_id: Uuid,
        provider_id: Uuid,
        provider_name: String,
        upstream_model: String,
        weight: i32,
        enabled: bool,
    }
    let rows: Vec<Row> = sqlx::query_as(
        r#"SELECT mr.id AS route_id, mr.provider_id,
                  p.name AS provider_name,
                  mr.upstream_model, mr.weight, mr.enabled
             FROM model_routes mr
             JOIN providers p ON p.id = mr.provider_id
            WHERE mr.model_id = $1 AND p.deleted_at IS NULL
            ORDER BY mr.weight DESC"#,
    )
    .bind(&model_id)
    .fetch_all(&state.db)
    .await?;

    // Reuse the gateway's HealthTracker — same Redis instance, same
    // window — so the UI sees the exact view the breaker uses to
    // make selection decisions.
    let tracker = think_watch_gateway::health::HealthTracker::new(state.redis.clone());
    let window_secs = state.dynamic_config.cb_window_secs().await;

    let route_ids: Vec<Uuid> = rows.iter().map(|r| r.route_id).collect();
    let healths = tracker.snapshot_many(&route_ids, window_secs).await;

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
