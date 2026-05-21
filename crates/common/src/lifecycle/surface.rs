//! The [`Surface`] trait — each gateway crate implements this to
//! plug its wire-format types into the pipeline.

use std::future::Future;

use crate::audit::AuditEntry;

use super::state::Invoked;

/// Per-surface plumbing. Each impl is a zero-sized marker type
/// (`struct McpSurface;`, `struct OpenAiChatSurface;` …) — stages
/// reference `S::Identity` / `S::Response` / etc. via associated
/// types and never construct an `S`.
///
/// All associated types are `Send + Sync + 'static` so they can be
/// moved across `tokio::spawn` boundaries (the streaming on-done
/// task in phase 2 will rely on this).
pub trait Surface: Sized + Send + Sync + 'static {
    /// Identity established by upstream auth middleware. Carries
    /// at minimum a stable subject id + the materialised
    /// [`crate::limits::SurfaceConstraints`] the limit/budget
    /// stages consume.
    type Identity: Send + Sync + 'static;

    /// Buffered wire response shape. Short-circuit stages build one
    /// of these via the `*_response` factory methods below; the
    /// buffered-success path threads one through
    /// [`super::state::CapturedView::Buffered`] into the terminal
    /// [`super::state::Emitted`]. The cache stores this shape; the
    /// audit pipeline captures this shape.
    type Response: Send + Sync + 'static;

    /// Streaming wire response shape, distinct from
    /// [`Self::Response`] because the SSE body type rarely matches
    /// the inner buffered shape. MCP: `StreamingPayload` (an SSE
    /// body + optional session id); AI gateway: the axum
    /// `Sse<...>::into_response()` envelope. Surface handlers
    /// return [`super::state::Invocation::Streaming.response`]
    /// straight to axum BEFORE the post-call tail resolves.
    type StreamResponse: Send + 'static;

    /// Surface-specific detail blob the [`stages::emit_audit`]
    /// terminal stage attaches to the audit row's `detail` JSON.
    /// Each surface defines what shape this is — for MCP it's
    /// `{server_id, server_name, tool_name, …}`; for the AI
    /// gateway it's `{model, route_id, provider_id, tokens, …}`.
    type AuditDetail: Send + Sync + 'static;

    /// Per-surface accumulator the streaming pump fills as chunks
    /// flow through. For the AI gateway this is the
    /// `Vec<ChatCompletionChunk>` + `Option<Usage>` pair; for MCP
    /// it's the JSON-RPC event timeline (`Vec<serde_json::Value>`).
    /// Post-invoke stages access it via
    /// [`super::state::CapturedView::Streaming`].
    type StreamCaptured: Send + Sync + 'static;

    /// Surface-supplied bundle of handles the post-invoke stages
    /// hand back into the trait methods below. Each surface
    /// composes whatever combination it needs (cache handle,
    /// breaker handle, audit logger, …) — the pipeline carries
    /// the reference through but never dereferences fields itself.
    type PostInvokeDeps: Send + Sync + 'static;

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

    /// Render the surface's "budget cap exhausted" response. The
    /// label is `"<subject>:budget/<period>"` (e.g.
    /// `"user:budget/monthly"`) so clients can tell which cap fired
    /// without parsing prose. Maps to 429 on the wire so existing
    /// rate-limit retry semantics apply — `GatewayError::LocalRateLimited`'s
    /// docstring explicitly covers both rate and budget under that
    /// status family.
    fn budget_exceeded_response(label: &str) -> Self::Response;

    /// Render the surface's "budget read backend unavailable"
    /// response (Redis outage + `fail_closed` enabled). Same wire
    /// shape as `rate_limiter_unavailable_response` — distinct only
    /// so the audit row can tell the two upstream-infrastructure
    /// failures apart.
    fn budget_unavailable_response() -> Self::Response;

    /// Record the outcome against this surface's circuit breaker.
    /// Called by [`super::stages::record_outcome`] exactly once per
    /// request. The view tells the impl whether to derive
    /// success/failure from a buffered response or a stream
    /// outcome — surface decides the per-response classification
    /// rule (JSON-RPC caller-side errors don't trip MCP's breaker;
    /// 5xx upstream replies trip the AI gateway's).
    fn record_outcome(
        deps: &Self::PostInvokeDeps,
        invoked: &Invoked<Self>,
    ) -> impl Future<Output = ()> + Send;

    /// Write the response to the surface's cache. Called by
    /// [`super::stages::write_cache`] only when
    /// [`super::state::CapturedView::is_success`] returns true —
    /// no need for the impl to re-check the outcome enum, only to
    /// inspect the buffered response (if any) for surface-specific
    /// success conditions (HTTP non-2xx, JSON-RPC `error`, …).
    fn write_cache(
        deps: &Self::PostInvokeDeps,
        invoked: &Invoked<Self>,
    ) -> impl Future<Output = ()> + Send;

    /// Debit limits / budget counters for the usage the request
    /// produced. Called by [`super::stages::record_usage`] after
    /// [`Self::write_cache`] and before [`Self::emit_audit`] so the
    /// audit row reflects post-debit counter values. The MCP
    /// surface doesn't currently track usage (no token concept on
    /// JSON-RPC `tools/call`); its impl provides an explicit no-op
    /// override rather than relying on the default below. The AI
    /// gateway overrides this to call `post_flight_account`.
    ///
    /// **Default body is `async {}`** — silently skips accounting.
    /// This makes a surface impl that forgets to opt in (whether
    /// to debit or to explicitly no-op) compile without warning,
    /// which can mask a real accounting bug. When you add a new
    /// surface, decide consciously: either override with the real
    /// debit path, or override with an empty body + a comment
    /// explaining why this surface doesn't account.
    fn record_usage(
        _deps: &Self::PostInvokeDeps,
        _invoked: &Invoked<Self>,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Emit the audit row (gateway_logs / mcp_logs) for this
    /// request. Called by [`super::stages::emit_audit`] exactly
    /// once. The surface owns row shape (body capture, blob
    /// offload, token accounting). This is the single audit-emit
    /// site for the post-invoke path; short-circuit stages
    /// (check_limits / check_budget / check_access) emit their own
    /// rows directly from inside the stage before returning the
    /// short-circuit response.
    fn emit_audit(
        deps: &Self::PostInvokeDeps,
        invoked: &Invoked<Self>,
    ) -> impl Future<Output = ()> + Send;
}
