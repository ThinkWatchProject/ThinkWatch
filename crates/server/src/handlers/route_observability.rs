//! Routing observability — live health snapshot per route + a tail
//! of recent routing decisions. Both are read-only and Redis-backed
//! (see `crates/gateway/src/{health,decision_log}.rs`).

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use think_watch_common::errors::AppError;
use think_watch_gateway::decision_log::DecisionRecord;
use think_watch_gateway::health::RouteHealth;
use uuid::Uuid;

/// One entry in the per-model route-health response.
#[derive(Debug, Serialize)]
pub struct RouteHealthEntry {
    pub route_id: Uuid,
    pub provider_id: Uuid,
    pub provider_name: String,
    pub upstream_model: Option<String>,
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
        upstream_model: Option<String>,
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

#[derive(Debug, Deserialize)]
pub struct DecisionsQuery {
    /// Filter to one model. When absent, returns the union across all
    /// models that currently have buffered decisions, capped at `limit`.
    pub model_id: Option<String>,
    /// 1-200, default 50. Each model bucket holds at most 200 entries
    /// so this is also the per-bucket ceiling when `model_id` is set.
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct DecisionsResponse {
    pub items: Vec<DecisionRecord>,
}

#[derive(Debug, Serialize)]
pub struct DecisionsModelsResponse {
    /// Model_ids with buffered decisions. Used to populate the UI's
    /// filter dropdown.
    pub items: Vec<String>,
}

/// `GET /api/admin/route-decisions?model_id=&limit=`
///
/// Read-only tail of routing decisions for live debugging. Redis-only
/// — capped at 200 per model, 24h TTL.
pub async fn list_decisions(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<DecisionsQuery>,
) -> Result<Json<DecisionsResponse>, AppError> {
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;

    let limit = q.limit.unwrap_or(50).clamp(1, 200);

    let items = if let Some(m) = q.model_id.as_deref() {
        think_watch_gateway::decision_log::recent(&state.redis, m, limit).await
    } else {
        // Union across all models with buffered decisions. Each
        // bucket is small so this is bounded; the merged list is then
        // sorted by timestamp and trimmed.
        let models = think_watch_gateway::decision_log::list_models(&state.redis).await;
        let per_model = (limit / models.len().max(1) as i64).max(10);
        let mut merged: Vec<DecisionRecord> = Vec::new();
        for m in models {
            let mut rs =
                think_watch_gateway::decision_log::recent(&state.redis, &m, per_model).await;
            merged.append(&mut rs);
        }
        merged.sort_by_key(|d| std::cmp::Reverse(d.ts_ms));
        merged.truncate(limit as usize);
        merged
    };

    Ok(Json(DecisionsResponse { items }))
}

/// `GET /api/admin/route-decisions/models` — model filter dropdown.
pub async fn list_decision_models(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<DecisionsModelsResponse>, AppError> {
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;

    let items = think_watch_gateway::decision_log::list_models(&state.redis).await;
    Ok(Json(DecisionsModelsResponse { items }))
}
