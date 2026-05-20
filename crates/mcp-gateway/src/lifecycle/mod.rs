//! MCP-side wiring for the `common::lifecycle` pipeline. Defines
//! [`McpSurface`] (the [`Surface`] trait impl plugging in MCP's
//! wire-format types) and helpers that build pipeline inputs from
//! the existing [`crate::proxy::RequestContext`] shape.
//!
//! See `crates/common/src/lifecycle/DESIGN.md` for the cross-
//! cutting architecture. Stages that are surface-specific (cache
//! lookup, circuit breaker check) live in the [`stages`]
//! submodule here; truly cross-cutting stages (check_limits,
//! check_access) live in `common::lifecycle::stages`.

pub mod stages;

use uuid::Uuid;

use think_watch_common::audit::{AuditActor, AuditEntry, McpActor};
use think_watch_common::lifecycle::Surface;
use think_watch_common::lifecycle::state::Invoked;
use think_watch_common::limits::{
    RateLimitRule, RateLimitSubject, Surface as LimitSurface, SurfaceConstraints,
};

use crate::access_control::is_tool_allowed;
use crate::proxy::{
    INVALID_REQUEST, JsonRpcRequest, JsonRpcResponse, StreamingPayload, err_response,
};

/// The MCP surface marker. Zero-size — stage code references the
/// surface's wire-format types via `McpSurface::Identity` /
/// `McpSurface::Response` / etc.
pub struct McpSurface;

/// The bag of fields stages need to attribute audit rows, key
/// rate-limit subjects, and look up server-specific policy. Owned
/// by the pipeline state, not borrowed — so it can move through
/// the stage chain (and into a detached on-done streaming task
/// when phase 2 ships streaming).
#[derive(Debug, Clone)]
pub struct McpIdentity {
    pub user_id: Uuid,
    pub user_email: String,
    pub ip_address: Option<String>,
    /// The materialised most-restrictive limit/budget envelope
    /// the parent crate computed across every role/team policy.
    /// Used by `check_limits` to extract the MCP block's rules.
    pub surface_constraints: SurfaceConstraints,
    /// MCP tool-name patterns the calling API key is restricted
    /// to. `None` = unrestricted; `Some([])` = deny all; supports
    /// `<server>__*` and exact `<server>__<tool>` patterns.
    /// Consumed by [`check_access`].
    pub allowed_mcp_tools: Option<Vec<String>>,
}

impl Surface for McpSurface {
    type Identity = McpIdentity;
    type RequestBody = JsonRpcRequest;
    type Response = JsonRpcResponse;
    /// MCP audit rows write their detail via `proxy::audit::
    /// emit_tools_call_audit` (still in-tree as of phase 1). Once
    /// the buffered-success terminal stage lands in
    /// `common::lifecycle::stages::emit_audit`, this `AuditDetail`
    /// will carry the per-tool `{server_id, tool_name, arguments,
    /// duration_ms, status, error_message}` object. For phase 1
    /// (short-circuit emits only) we use a free-form `Value`.
    type AuditDetail = serde_json::Value;

    fn audit_entry(identity: &Self::Identity, action: &str) -> AuditEntry {
        McpActor {
            user_id: identity.user_id,
            user_email: &identity.user_email,
            ip: identity.ip_address.as_deref(),
        }
        .audit(action)
    }

    fn rate_limited_response(label: &str) -> Self::Response {
        // Bumps the existing operator-facing metric so dashboards
        // built around `mcp_rate_limited_total` keep working after
        // the migration.
        metrics::counter!("mcp_rate_limited_total").increment(1);
        err_response(None, INVALID_REQUEST, format!("Rate limited: {label}"))
    }

    fn rate_limiter_unavailable_response() -> Self::Response {
        err_response(
            None,
            INVALID_REQUEST,
            "Rate limited: rate_limiter_unavailable".to_string(),
        )
    }

    fn is_access_allowed(identity: &Self::Identity, candidate: &str) -> bool {
        is_tool_allowed(identity.allowed_mcp_tools.as_deref(), candidate)
    }

    fn access_denied_response(_candidate: &str) -> Self::Response {
        // Preserve the pre-migration wire shape — the inline check
        // returned `"Access denied for this tool"` without echoing
        // the tool name (which the caller already knows from their
        // own request body).
        err_response(None, INVALID_REQUEST, "Access denied for this tool")
    }

    /// MCP streaming capture is the JSON-RPC event timeline the
    /// pump accumulates as upstream events flow through. Phase 2
    /// step 2 (STREAMING.md migration) wires this into
    /// `build_mcp_pump`; for now the type exists so the Surface
    /// trait is satisfied and downstream stages can be written
    /// against `CapturedView::Streaming { captured, .. }`.
    type StreamCaptured = Vec<serde_json::Value>;

    /// Streaming wire response — the SSE body the transport layer
    /// hands to axum before the post-call tail resolves. Defined
    /// here so the `Surface` trait is satisfied; phase 2 step 2
    /// wires this into `build_mcp_pump`.
    type StreamResponse = StreamingPayload;

    /// Post-invoke hook deps for MCP. Empty marker for now —
    /// STREAMING.md step 2 fills this with a clone of the
    /// `McpProxy` handle (circuit breakers, cache, audit, server-
    /// id, tool-name, …) so the three hooks below can dispatch
    /// into the existing post-call code.
    type PostInvokeDeps = ();

    /// No-op until STREAMING.md step 2. The MCP pipeline does not
    /// yet route post-invoke work through
    /// [`think_watch_common::lifecycle::stages::run_post_invoke`];
    /// the existing `crate::proxy::streaming::build_chunk_passthrough`
    /// owns breaker accounting until step 2 moves it here.
    async fn record_outcome(_deps: &Self::PostInvokeDeps, _invoked: &Invoked<Self>) {}

    /// No-op until STREAMING.md step 2 (see [`record_outcome`]).
    async fn write_cache(_deps: &Self::PostInvokeDeps, _invoked: &Invoked<Self>) {}

    /// No-op until STREAMING.md step 2 (see [`record_outcome`]).
    async fn emit_audit(_deps: &Self::PostInvokeDeps, _invoked: &Invoked<Self>) {}
}

/// Build the MCP-surface `requests` rate-limit rules from a
/// materialised [`SurfaceConstraints`]. Mirrors the inline
/// extraction in the previous `handle_tools_call` — same shape,
/// just lifted out so the surface stage gets a clean
/// `&[RateLimitRule]` slice without re-implementing the
/// extraction at every call site.
pub fn rate_limit_rules(constraints: &SurfaceConstraints, user_id: Uuid) -> Vec<RateLimitRule> {
    constraints
        .block(LimitSurface::McpGateway)
        .map(|block| {
            block
                .rules
                .iter()
                .filter(|r| r.enabled)
                .map(|r| RateLimitRule {
                    id: Uuid::nil(),
                    subject_kind: RateLimitSubject::User,
                    subject_id: user_id,
                    surface: LimitSurface::McpGateway,
                    metric: r.metric,
                    window_secs: r.window_secs,
                    max_count: r.max_count,
                    enabled: true,
                    expires_at: None,
                    reason: None,
                    created_by: None,
                })
                .collect()
        })
        .unwrap_or_default()
}
