//! MCP-side wiring for the `common::lifecycle` pipeline. Defines
//! [`McpSurface`] (the [`Surface`] trait impl plugging in MCP's
//! wire-format types) and helpers that build pipeline inputs from
//! the existing [`crate::proxy::RequestContext`] shape.
//!
//! See `crates/common/src/lifecycle/DESIGN.md` for the cross-
//! cutting architecture.

use uuid::Uuid;

use think_watch_common::audit::{AuditActor, AuditEntry, McpActor};
use think_watch_common::lifecycle::Surface;
use think_watch_common::limits::{
    RateLimitRule, RateLimitSubject, Surface as LimitSurface, SurfaceConstraints,
};

use crate::proxy::{INVALID_REQUEST, JsonRpcRequest, JsonRpcResponse, err_response};

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
