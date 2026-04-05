use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use think_watch_auth::jwt::JwtManager;

use crate::proxy::McpProxy;
use crate::proxy::{INVALID_REQUEST, JsonRpcRequest, err_response};
use crate::session::SessionManager;

/// Shared application state for the MCP gateway Axum handlers.
#[derive(Clone)]
pub struct McpGatewayState {
    pub proxy: McpProxy,
    pub sessions: SessionManager,
    pub jwt_manager: Arc<JwtManager>,
}

/// Header name used to carry the MCP session identifier.
const MCP_SESSION_HEADER: &str = "mcp-session-id";

/// Extract the bearer token from the `Authorization` header.
fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// `POST /mcp` — main Streamable HTTP endpoint.
///
/// 1. Optionally extract or create an `Mcp-Session-Id`.
/// 2. Authenticate via Bearer token.
/// 3. Parse the body as a JSON-RPC request.
/// 4. Delegate to `McpProxy::handle_request`.
/// 5. Return the JSON-RPC response together with the session header.
pub async fn handle_post(
    State(state): State<Arc<McpGatewayState>>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    // --- Authentication ----------------------------------------------------
    let token = match extract_bearer_token(&headers) {
        Some(t) => t,
        None => {
            let resp = err_response(request.id.clone(), INVALID_REQUEST, "Missing Bearer token");
            return (StatusCode::UNAUTHORIZED, Json(resp)).into_response();
        }
    };

    let claims = match state.jwt_manager.verify_token(token) {
        Ok(c) => c,
        Err(_) => {
            let resp = err_response(
                request.id.clone(),
                INVALID_REQUEST,
                "Invalid or expired token",
            );
            return (StatusCode::UNAUTHORIZED, Json(resp)).into_response();
        }
    };

    if claims.token_type != "access" {
        let resp = err_response(request.id.clone(), INVALID_REQUEST, "Invalid token type");
        return (StatusCode::UNAUTHORIZED, Json(resp)).into_response();
    }

    let user_id = claims.sub;

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
        state.sessions.create_session(user_id).await
    };

    // --- Dispatch ----------------------------------------------------------
    let response = state.proxy.handle_request(user_id, request).await;

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
pub async fn handle_delete(
    State(state): State<Arc<McpGatewayState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // --- Authentication ----------------------------------------------------
    let token = match extract_bearer_token(&headers) {
        Some(t) => t,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let claims = match state.jwt_manager.verify_token(token) {
        Ok(c) => c,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if claims.token_type != "access" {
        return StatusCode::UNAUTHORIZED.into_response();
    }

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
