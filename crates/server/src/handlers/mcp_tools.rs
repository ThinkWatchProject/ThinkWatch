use axum::Json;
use axum::extract::State;
use serde::Serialize;
use sqlx::FromRow;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Serialize, FromRow)]
pub struct McpToolRow {
    pub id: uuid::Uuid,
    pub server_id: uuid::Uuid,
    pub server_name: String,
    /// Server's `namespace_prefix` — the authoritative key for ACL
    /// matching. Surfaced separately so the frontend doesn't have to
    /// reverse-engineer it from `namespaced_name` (prefixes may
    /// themselves contain `__`).
    pub server_prefix: String,
    pub name: String,
    pub namespaced_name: String,
    pub description: Option<String>,
    pub input_schema: Option<serde_json::Value>,
}

#[utoipa::path(
    get,
    path = "/api/mcp/tools",
    tag = "MCP Tools",
    responses(
        (status = 200, description = "All active MCP tools across all servers"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_tools(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<McpToolRow>>, AppError> {
    let tools = sqlx::query_as::<_, McpToolRow>(
        r#"SELECT
             t.id,
             t.server_id,
             s.name AS server_name,
             s.namespace_prefix AS server_prefix,
             t.tool_name AS name,
             s.namespace_prefix || '__' || t.tool_name AS namespaced_name,
             t.description,
             t.input_schema
           FROM mcp_tools t
           JOIN mcp_servers s ON s.id = t.server_id
           WHERE t.is_active = true
           ORDER BY s.name, t.tool_name"#,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(tools))
}

/// Admin-triggered tool discovery for a specific MCP server. Delegates to
/// the shared `discover_and_persist_tools` so the auth/HTTP/upsert logic
/// stays in one place — the same function is also called from the startup
/// loader and from the create/update CRUD paths.
#[utoipa::path(
    post,
    path = "/api/mcp/servers/{id}/discover",
    tag = "MCP Tools",
    params(
        ("id" = uuid::Uuid, Path, description = "MCP server ID to run tool discovery against"),
    ),
    responses(
        (status = 200, description = "Discovery result with count of discovered tools"),
        (status = 400, description = "Discovery failed"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Server not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn discover_tools(
    auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Path(server_id): axum::extract::Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("mcp_servers:update")?;
    let server = sqlx::query_as::<_, think_watch_common::models::McpServer>(
        "SELECT * FROM mcp_servers WHERE id = $1",
    )
    .bind(server_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    let count = crate::mcp_runtime::discover_and_persist_tools(
        &state.db,
        &state.http_client,
        &server,
        &state.config.encryption_key,
    )
    .await
    .map_err(|e| AppError::BadRequest(format!("Tool discovery failed: {e}")))?;

    // Reflect the freshly-discovered tools in the in-memory registry so
    // `tools/list` returns them without waiting for the health loop.
    if let Ok(updated) = crate::mcp_runtime::build_registered_server(
        &state.db,
        &server,
        &state.config.encryption_key,
    )
    .await
    {
        state.mcp_registry.register(updated).await;
    }

    Ok(Json(serde_json::json!({
        "status": "discovery_complete",
        "server_id": server_id,
        "tools_discovered": count,
    })))
}
