use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Serialize, FromRow, utoipa::ToSchema)]
pub struct McpToolRow {
    #[schema(value_type = String, format = Uuid)]
    pub id: uuid::Uuid,
    #[schema(value_type = String, format = Uuid)]
    pub server_id: uuid::Uuid,
    pub server_name: String,
    pub name: String,
    pub namespaced_name: String,
    pub description: Option<String>,
    #[schema(value_type = Object)]
    pub input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct McpToolListQuery {
    pub q: Option<String>,
    /// UUID of the MCP server to filter by. Missing / empty = all servers.
    pub server_id: Option<uuid::Uuid>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct McpToolListResponse {
    pub items: Vec<McpToolRow>,
    pub total: i64,
}

#[utoipa::path(
    get,
    path = "/api/mcp/tools",
    tag = "MCP Tools",
    params(
        ("q" = Option<String>, Query, description = "Search by tool name / namespaced name / description"),
        ("server_id" = Option<String>, Query, description = "Filter by MCP server UUID"),
        ("page" = Option<i64>, Query, description = "Page number (1-based)"),
        ("page_size" = Option<i64>, Query, description = "Items per page (default 50, max 200)"),
    ),
    responses(
        (status = 200, description = "Paginated list of active MCP tools", body = McpToolListResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_tools(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<McpToolListQuery>,
) -> Result<Json<McpToolListResponse>, AppError> {
    let page_size = query.page_size.unwrap_or(50).clamp(1, 200);
    let page = query.page.unwrap_or(1).max(1);
    let offset = (page - 1) * page_size;
    let search = query.q.as_deref().unwrap_or("").trim();
    let search_pattern = format!("%{search}%");

    // $1='' OR ... lets a single prepared statement handle "no search"
    // without branching on SQL text. `namespaced_name` is computed via
    // `s.namespace_prefix || '__' || t.tool_name`, so we search that
    // composed form directly rather than piecing it back together in SQL
    // for each row.
    let total: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*)
           FROM mcp_tools t
           JOIN mcp_servers s ON s.id = t.server_id
           WHERE t.is_active = true
             AND ($3::uuid IS NULL OR t.server_id = $3)
             AND ($1 = ''
                  OR t.tool_name ILIKE $2
                  OR (s.namespace_prefix || '__' || t.tool_name) ILIKE $2
                  OR COALESCE(t.description, '') ILIKE $2)"#,
    )
    .bind(search)
    .bind(&search_pattern)
    .bind(query.server_id)
    .fetch_one(&state.db)
    .await?;

    let items = sqlx::query_as::<_, McpToolRow>(
        r#"SELECT
             t.id,
             t.server_id,
             s.name AS server_name,
             t.tool_name AS name,
             s.namespace_prefix || '__' || t.tool_name AS namespaced_name,
             t.description,
             t.input_schema
           FROM mcp_tools t
           JOIN mcp_servers s ON s.id = t.server_id
           WHERE t.is_active = true
             AND ($3::uuid IS NULL OR t.server_id = $3)
             AND ($1 = ''
                  OR t.tool_name ILIKE $2
                  OR (s.namespace_prefix || '__' || t.tool_name) ILIKE $2
                  OR COALESCE(t.description, '') ILIKE $2)
           ORDER BY s.name, t.tool_name
           LIMIT $4 OFFSET $5"#,
    )
    .bind(search)
    .bind(&search_pattern)
    .bind(query.server_id)
    .bind(page_size)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(McpToolListResponse { items, total }))
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

    let http = state.http_client.load();
    let count = crate::mcp_runtime::discover_and_persist_tools(
        &state.db,
        &http,
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
