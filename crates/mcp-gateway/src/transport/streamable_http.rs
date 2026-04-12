use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use uuid::Uuid;

use crate::proxy::McpProxy;
use crate::proxy::{INVALID_REQUEST, JsonRpcRequest, err_response};
use crate::session::SessionManager;

/// Shared application state for the MCP gateway Axum handlers.
///
/// Authentication is handled by a router-level middleware in the
/// parent server crate (the `require_api_key("mcp_gateway")` layer);
/// the handlers below just read the `McpRequestIdentity` extension
/// the middleware inserts.
#[derive(Clone)]
pub struct McpGatewayState {
    pub proxy: McpProxy,
    pub sessions: SessionManager,
}

/// Identity inserted by the parent crate's API-key middleware after
/// it validates a `tw-` token whose `surfaces` array contains
/// `mcp_gateway`. Lives in this crate so the transport handlers can
/// `axum::Extension`-extract it without depending on the server
/// crate.
#[derive(Debug, Clone)]
pub struct McpRequestIdentity {
    pub user_id: Uuid,
    /// Role names the underlying user holds. Used by the access
    /// controller to gate per-tool ACLs. Empty for service-account
    /// keys (no associated user) — those will be denied any tool
    /// that requires a role match.
    pub user_roles: Vec<String>,
    /// Role IDs for rate-limit subject resolution.
    pub role_ids: Vec<Uuid>,
}

/// Header name used to carry the MCP session identifier.
const MCP_SESSION_HEADER: &str = "mcp-session-id";

/// `POST /mcp` — main Streamable HTTP endpoint.
///
/// Auth runs as a router-level layer before this handler fires;
/// here we just read the resolved identity from the request
/// extensions. Returns a JSON-RPC error if the layer didn't
/// install the extension (which means it's misconfigured).
pub async fn handle_post(
    State(state): State<Arc<McpGatewayState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<McpRequestIdentity>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    // --- Session management ------------------------------------------------
    let session_id = if let Some(sid) = headers
        .get(MCP_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        // Validate the existing session.
        if state.sessions.get_session(sid).await.is_none() {
            let resp = err_response(request.id.clone(), INVALID_REQUEST, "Unknown session");
            return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
        }
        state.sessions.update_activity(sid).await;
        sid.to_owned()
    } else {
        // First request — create a new session.
        state.sessions.create_session(identity.user_id).await
    };

    // --- Dispatch ----------------------------------------------------------
    let response = state
        .proxy
        .handle_request(
            identity.user_id,
            &identity.user_roles,
            &identity.role_ids,
            request,
        )
        .await;

    // Return the response with the session header.
    let mut resp_headers = HeaderMap::new();
    if let Ok(val) = session_id.parse() {
        resp_headers.insert(MCP_SESSION_HEADER, val);
    }

    (StatusCode::OK, resp_headers, Json(response)).into_response()
}

/// `DELETE /mcp` — close an MCP session.
///
/// Expects the `Mcp-Session-Id` header.  Returns 204 on success.
/// Auth is handled by the router-level layer; identity is read but
/// the only field we touch here is the session id from the header.
pub async fn handle_delete(
    State(state): State<Arc<McpGatewayState>>,
    headers: HeaderMap,
    axum::Extension(_identity): axum::Extension<McpRequestIdentity>,
) -> impl IntoResponse {
    // --- Session teardown --------------------------------------------------
    let session_id = match headers
        .get(MCP_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(sid) => sid,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    state.sessions.remove_session(session_id).await;

    StatusCode::NO_CONTENT.into_response()
}
