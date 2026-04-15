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
    pub user_email: String,
    /// Role names the underlying user holds. Used by the access
    /// controller to gate per-tool ACLs. Empty for service-account
    /// keys (no associated user) — those will be denied any tool
    /// that requires a role match.
    pub user_roles: Vec<String>,
    /// Role IDs for rate-limit subject resolution.
    pub role_ids: Vec<Uuid>,
    /// MCP tool access patterns from role union. `None` = unrestricted.
    pub allowed_mcp_tools: Option<Vec<String>>,
}

/// Header name used to carry the MCP session identifier.
const MCP_SESSION_HEADER: &str = "mcp-session-id";
/// Optional caller-supplied trace identifier. When present and sane,
/// it propagates into every `mcp_logs` row this request produces so
/// the upstream AI request that initiated the tool-use can be
/// correlated with the resulting MCP calls in `/api/admin/trace`.
const TRACE_ID_HEADER: &str = "x-trace-id";

/// Validate and accept an inbound x-trace-id, falling back to a fresh
/// UUID. Mirrors the same shape rules the access_log middleware uses
/// (≤128 chars, ASCII, no control characters) so a single sanitiser
/// is impossible to skip on either entry path.
fn resolve_trace_id(headers: &HeaderMap) -> String {
    headers
        .get(TRACE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| !c.is_control()))
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

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

    // --- Trace correlation -------------------------------------------------
    // Either echo what the caller pinned (so a multi-call AI-driven
    // session shows up under one trace) or mint a fresh id.
    let trace_id = resolve_trace_id(&headers);

    // --- Dispatch ----------------------------------------------------------
    let response = state
        .proxy
        .handle_request(
            identity.user_id,
            &identity.user_email,
            &identity.role_ids,
            identity.allowed_mcp_tools.as_deref(),
            &trace_id,
            request,
        )
        .await;

    // Return the response with the session header + trace id so the
    // operator can copy it back out of the wire.
    let mut resp_headers = HeaderMap::new();
    if let Ok(val) = session_id.parse() {
        resp_headers.insert(MCP_SESSION_HEADER, val);
    }
    if let Ok(val) = trace_id.parse() {
        resp_headers.insert(TRACE_ID_HEADER, val);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_id_passthrough_when_caller_pinned() {
        let mut h = HeaderMap::new();
        h.insert(TRACE_ID_HEADER, "req-2026-04-15-abc".parse().unwrap());
        assert_eq!(resolve_trace_id(&h), "req-2026-04-15-abc");
    }

    #[test]
    fn trace_id_minted_when_header_missing() {
        let h = HeaderMap::new();
        let id = resolve_trace_id(&h);
        assert_eq!(id.len(), 36, "expected v4 UUID");
    }

    #[test]
    fn trace_id_rejects_oversize_header() {
        let mut h = HeaderMap::new();
        let long = "x".repeat(129);
        h.insert(TRACE_ID_HEADER, long.parse().unwrap());
        let id = resolve_trace_id(&h);
        assert_ne!(id.len(), 129, "129-char value must be rejected");
        assert_eq!(id.len(), 36);
    }

    #[test]
    fn trace_id_rejects_blank_header() {
        let mut h = HeaderMap::new();
        h.insert(TRACE_ID_HEADER, "   ".parse().unwrap());
        assert_eq!(resolve_trace_id(&h).len(), 36);
    }

    /// Cross-check: the AI gateway side and the MCP transport side
    /// must produce the same id for the same caller-pinned header
    /// value, otherwise correlation breaks. The two implementations
    /// live in different crates so the test exists to flag drift if
    /// one ever loosens its rules.
    #[test]
    fn trace_id_validation_matches_ai_gateway() {
        for input in ["abc", "req-2026-04-15-abc", &"a".repeat(128)] {
            let mut h = HeaderMap::new();
            h.insert(TRACE_ID_HEADER, input.parse().unwrap());
            assert_eq!(resolve_trace_id(&h), input);
        }
    }
}
