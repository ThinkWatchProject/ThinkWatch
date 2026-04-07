use axum::Json;
use axum::extract::State;

use think_watch_common::errors::AppError;
use think_watch_common::models::McpTool;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

pub async fn list_tools(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<McpTool>>, AppError> {
    let tools = sqlx::query_as::<_, McpTool>(
        "SELECT * FROM mcp_tools WHERE is_active = true ORDER BY server_id, tool_name",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(tools))
}

/// Admin-triggered tool discovery for a specific MCP server. Delegates to
/// the shared `discover_and_persist_tools` so the auth/HTTP/upsert logic
/// stays in one place — the same function is also called from the startup
/// loader and from the create/update CRUD paths.
pub async fn discover_tools(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Path(server_id): axum::extract::Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let server = sqlx::query_as::<_, think_watch_common::models::McpServer>(
        "SELECT * FROM mcp_servers WHERE id = $1",
    )
    .bind(server_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    let count = crate::app::discover_and_persist_tools(
        &state.db,
        &state.http_client,
        &server,
        &state.config.encryption_key,
    )
    .await
    .map_err(|e| AppError::BadRequest(format!("Tool discovery failed: {e}")))?;

    // Reflect the freshly-discovered tools in the in-memory registry so
    // `tools/list` returns them without waiting for the health loop.
    if let Ok(updated) =
        crate::app::build_registered_server(&state.db, &server, &state.config.encryption_key).await
    {
        state.mcp_registry.register(updated).await;
    }

    Ok(Json(serde_json::json!({
        "status": "discovery_complete",
        "server_id": server_id,
        "tools_discovered": count,
    })))
}
