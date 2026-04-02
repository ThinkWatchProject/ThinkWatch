use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use agent_bastion_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Deserialize)]
pub struct McpLogsQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct McpLogEntry {
    pub id: uuid::Uuid,
    pub tool_name: String,
    pub server_name: String,
    pub user_email: Option<String>,
    pub duration_ms: Option<i32>,
    pub status: String,
    pub error_message: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn list_mcp_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<McpLogsQuery>,
) -> Result<Json<Vec<McpLogEntry>>, AppError> {
    let limit = params.limit.unwrap_or(100).min(500);

    let rows = sqlx::query_as::<_, McpLogEntry>(
        r#"SELECT
            l.id, l.tool_name,
            COALESCE(s.name, 'unknown') as server_name,
            u.email as user_email,
            l.duration_ms, l.status, l.error_message, l.created_at
           FROM mcp_call_logs l
           LEFT JOIN mcp_servers s ON s.id = l.server_id
           LEFT JOIN users u ON u.id = l.user_id
           ORDER BY l.created_at DESC
           LIMIT $1"#,
    )
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(rows))
}
