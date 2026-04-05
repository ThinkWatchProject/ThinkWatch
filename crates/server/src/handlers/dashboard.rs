use axum::Json;
use axum::extract::State;
use serde::Serialize;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Serialize)]
pub struct DashboardStats {
    pub total_requests_today: i64,
    pub active_providers: i64,
    pub active_api_keys: i64,
    pub connected_mcp_servers: i64,
}

pub async fn get_dashboard_stats(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<DashboardStats>, AppError> {
    let today = chrono::Utc::now().date_naive();

    let total_requests: Option<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_records WHERE created_at::date = $1")
            .bind(today)
            .fetch_one(&state.db)
            .await?;

    let active_providers: Option<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM providers WHERE is_active = true")
            .fetch_one(&state.db)
            .await?;

    let active_api_keys: Option<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE is_active = true")
            .fetch_one(&state.db)
            .await?;

    let connected_mcp_servers: Option<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM mcp_servers WHERE status = 'connected'")
            .fetch_one(&state.db)
            .await?;

    Ok(Json(DashboardStats {
        total_requests_today: total_requests.unwrap_or(0),
        active_providers: active_providers.unwrap_or(0),
        active_api_keys: active_api_keys.unwrap_or(0),
        connected_mcp_servers: connected_mcp_servers.unwrap_or(0),
    }))
}
