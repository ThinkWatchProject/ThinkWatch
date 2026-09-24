use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;
use crate::services::mcp_tool_repository::{self as repo, CatalogQuery, McpToolRow};

#[derive(Debug, Deserialize)]
pub struct McpToolListQuery {
    pub q: Option<String>,
    /// UUID of the MCP server to filter by. Missing / empty = all servers.
    pub server_id: Option<uuid::Uuid>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    /// When `true`, also include rows from `mcp_user_tools` for the
    /// **calling user** (auth_user.sub) — the admin-facing API-key
    /// picker uses this so admins can grant their own per-user tools
    /// (e.g. their personal GitHub catalog) to an API key. Default
    /// `false` keeps the system-level catalog clean for surfaces that
    /// must not surface user-specific data (admin /mcp/tools page,
    /// store browsing).
    pub include_user_tools: Option<bool>,
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
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<McpToolListQuery>,
) -> Result<Json<McpToolListResponse>, AppError> {
    let page_size = query.page_size.unwrap_or(50).clamp(1, 200);
    let page = query.page.unwrap_or(1).max(1);
    let offset = (page - 1) * page_size;
    let search = query.q.as_deref().unwrap_or("").trim();
    let search_pattern = format!("%{search}%");
    let include_user = query.include_user_tools.unwrap_or(false);
    // When `include_user_tools` is true, $6 is the caller's UUID so the
    // CTE filters their personal `mcp_user_tools`. When false, $6 is
    // a NULL sentinel and the user-tools branch returns zero rows —
    // the resulting union is identical to the legacy system-only view.
    let user_filter: Option<uuid::Uuid> = if include_user {
        Some(auth_user.claims.sub)
    } else {
        None
    };

    let catalog = CatalogQuery {
        search,
        search_pattern: &search_pattern,
        server_id: query.server_id,
        page_size,
        offset,
        user_id: user_filter,
    };
    let total = repo::count_catalog(&state.db, &catalog).await?;
    let items = repo::list_catalog(&state.db, &catalog).await?;

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
        (status = 200, description = "Discovery result: discriminated union with `status` in {discovery_complete, auth_required, discovery_failed}. Non-2xx statuses are reserved for auth/permission/lookup errors."),
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
    let server = crate::services::mcp_server_repository::find(&state.db, server_id)
        .await?
        .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    use crate::mcp_runtime::SystemDiscoveryOutcome;
    let http = state.http_client.load();
    let outcome = crate::mcp_runtime::discover_and_persist_tools(&state.db, &http, &server).await;

    match outcome {
        SystemDiscoveryOutcome::Tools(count) => {
            // Reflect the freshly-discovered tools in the in-memory
            // registry so `tools/list` returns them without waiting for
            // the health loop.
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
        SystemDiscoveryOutcome::AuthRequired => {
            // Not an error — auth-required servers don't expose their
            // tool catalog to anonymous probes by design. Return 200
            // with a status the frontend renders as a neutral info
            // toast, not a red error.
            Ok(Json(serde_json::json!({
                "status": "auth_required",
                "server_id": server_id,
                "tools_discovered": 0,
                "hint": "This server requires per-user authorization. \
                         Tools populate as users connect their accounts.",
            })))
        }
        SystemDiscoveryOutcome::Failed(e) => {
            // Return the underlying error so admins can tell whether the
            // failure was a network timeout, a 5xx, a malformed response,
            // etc. Match the `auth_required` shape (200 OK with a
            // discriminated `status`) so the frontend can surface the
            // detail in a single try-branch. The error string is bounded
            // to keep log floods and oversize toasts in check.
            let mut detail = e.to_string().trim().to_string();
            const MAX_ERROR_LEN: usize = 500;
            if detail.chars().count() > MAX_ERROR_LEN {
                detail = detail.chars().take(MAX_ERROR_LEN).collect::<String>() + "…";
            }
            Ok(Json(serde_json::json!({
                "status": "discovery_failed",
                "server_id": server_id,
                "tools_discovered": 0,
                "error": detail,
            })))
        }
    }
}
