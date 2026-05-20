//! The [`Surface`] trait — each gateway crate implements this to
//! plug its wire-format types into the pipeline.

use crate::audit::AuditEntry;

/// Per-surface plumbing. Each impl is a zero-sized marker type
/// (`struct McpSurface;`, `struct OpenAiChatSurface;` …) — stages
/// reference `S::Identity` / `S::Response` / etc. via associated
/// types and never construct an `S`.
///
/// All associated types are `Send + Sync + 'static` so they can be
/// moved across `tokio::spawn` boundaries (the streaming on-done
/// task in phase 2 will rely on this).
pub trait Surface: Send + Sync + 'static {
    /// Identity established by upstream auth middleware. Carries
    /// at minimum a stable subject id + the materialised
    /// [`crate::limits::SurfaceConstraints`] the limit/budget
    /// stages consume.
    type Identity: Send + Sync + 'static;

    /// Wire request body the surface received. Stage 1 (`check_limits`)
    /// doesn't read this; later stages (e.g. `check_access` which
    /// inspects the requested model/tool name) do.
    type RequestBody: Send + Sync + 'static;

    /// Wire response shape. Short-circuit stages build one of these
    /// via the `*_response` factory methods below; the buffered-
    /// success path materialises one in `emit_audit`'s terminal
    /// state.
    type Response: Send + Sync + 'static;

    /// Surface-specific detail blob the [`stages::emit_audit`]
    /// terminal stage attaches to the audit row's `detail` JSON.
    /// Each surface defines what shape this is — for MCP it's
    /// `{server_id, server_name, tool_name, …}`; for the AI
    /// gateway it's `{model, route_id, provider_id, tokens, …}`.
    type AuditDetail: Send + Sync + 'static;

    /// Build the audit-row scaffolding (actor + action + base
    /// detail) the pipeline's emit stage will fill in. Surfaces
    /// implement this by combining their `*Actor` (McpActor /
    /// GatewayActor) with the requested action label.
    fn audit_entry(identity: &Self::Identity, action: &str) -> AuditEntry;

    /// Render the surface's "you got rate limited" response. The
    /// `label` is a human string like `"user:requests/1m"` that
    /// the limit engine produces; the surface decides whether to
    /// surface it verbatim or wrap it.
    fn rate_limited_response(label: &str) -> Self::Response;

    /// Render the surface's "rate-limiter unavailable, fail-closed"
    /// response. Distinct from `rate_limited_response` because the
    /// audit row should distinguish "user hit a cap" from "Redis
    /// is down so we refused everyone".
    fn rate_limiter_unavailable_response() -> Self::Response;

    /// Decide whether `candidate` is permitted by the identity's
    /// access policy. Pure boolean — the stage layer wraps this
    /// in audit + short-circuit handling, so impls focus on the
    /// surface-specific pattern grammar:
    ///
    /// - MCP: namespaced tool name (`stream__test_tool`) against
    ///   the identity's `allowed_mcp_tools` patterns
    ///   (`mysql__*`, `github__list_issues`, …).
    /// - AI gateway: flat model id against the identity's
    ///   `allowed_models` list.
    fn is_access_allowed(identity: &Self::Identity, candidate: &str) -> bool;

    /// Render the surface's "access denied" response. `candidate`
    /// is the rejected subject (tool / model) so the wire body
    /// can carry it.
    fn access_denied_response(candidate: &str) -> Self::Response;
}
