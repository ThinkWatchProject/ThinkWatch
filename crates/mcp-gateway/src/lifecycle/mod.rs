//! MCP-side wiring for the `think_watch_common::lifecycle`
//! pipeline. Defines [`McpSurface`] (the [`Surface`] trait impl
//! plugging in MCP's wire-format types) and helpers that build
//! pipeline inputs from the existing [`crate::proxy::RequestContext`]
//! shape.
//!
//! Stages that are surface-specific to MCP (cache lookup, circuit
//! breaker check) live in the [`stages`] submodule here; truly
//! cross-cutting stages (check_limits, check_budget, check_access)
//! live in `think_watch_common::lifecycle::stages`.

pub mod stages;

use uuid::Uuid;

use think_watch_common::audit::{AuditActor, AuditEntry, McpActor};
use think_watch_common::lifecycle::Surface;
use think_watch_common::lifecycle::state::{CapturedView, Invoked};
use think_watch_common::lifecycle::streaming::StreamOutcome;
use think_watch_common::limits::{
    RateLimitRule, RateLimitSubject, Surface as LimitSurface, SurfaceConstraints,
};

use crate::access_control::is_tool_allowed;
use crate::cache::CallerScope;
use crate::proxy::{
    INTERNAL_ERROR, INVALID_REQUEST, JsonRpcRequest, JsonRpcResponse, McpProxy, StreamingPayload,
    err_response, pick_response_envelope,
};
use crate::registry::ServerCacheScope;

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

    fn budget_exceeded_response(label: &str) -> Self::Response {
        // MCP doesn't currently wire budget caps into
        // `handle_tools_call` (the AI gateway is the only consumer
        // of `check_budget` today). The factory exists so the
        // `Surface` trait is satisfied uniformly — surfaces a
        // JSON-RPC error with the same label-carrying shape rate-
        // limit denies use, in case a future MCP feature lights it
        // up.
        metrics::counter!("mcp_budget_exceeded_total").increment(1);
        err_response(None, INVALID_REQUEST, format!("Budget exceeded: {label}"))
    }

    fn budget_unavailable_response() -> Self::Response {
        err_response(
            None,
            INVALID_REQUEST,
            "Budget cap backend unavailable".to_string(),
        )
    }

    /// MCP streaming capture is the JSON-RPC event timeline the
    /// pump accumulates as upstream events flow through (every
    /// `notifications/progress` plus the final response envelope).
    /// `record_outcome` / `write_cache` / `emit_audit` derive the
    /// canonical response by handing this list to
    /// [`pick_response_envelope`]; the audit hook also serialises
    /// it into the row's response_body so trace replay UI shows
    /// the full timeline, not just the final envelope.
    type StreamCaptured = Vec<serde_json::Value>;

    /// Streaming wire response — the SSE body the transport layer
    /// hands to axum before the post-call tail resolves. Wraps the
    /// chunk-by-chunk pass-through plus the optional new
    /// upstream-session id surfaced from headers.
    type StreamResponse = StreamingPayload;

    type PostInvokeDeps = McpPostInvokeDeps;

    async fn record_outcome(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        // Streaming transport errors are unambiguous upstream
        // faults — straight `record_failure` so a flapping upstream
        // opens the breaker. Every other outcome (Natural, Client-
        // Cancelled, any buffered response) goes through the
        // shared response-code classifier so a single user's bad
        // INVALID_PARAMS doesn't trip the breaker for everyone.
        match &invoked.view {
            CapturedView::Streaming {
                outcome: StreamOutcome::UpstreamError { .. },
                ..
            } => {
                deps.proxy
                    .circuit_breakers
                    .record_failure(deps.server_id, &deps.server_name)
                    .await;
            }
            _ => {
                let response = response_for_hooks(invoked, deps);
                deps.proxy
                    .record_breaker_for_response(deps.server_id, &deps.server_name, &response)
                    .await;
            }
        }
    }

    async fn write_cache(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        if deps.effective_cache_ttl == 0 {
            return;
        }
        let response = response_for_hooks(invoked, deps);
        // The run_post_invoke stage gate filtered non-Natural
        // streams; we still need the per-response error gate so
        // buffered JSON-RPC errors don't poison the cache.
        if response.error.is_some() {
            return;
        }
        let scope = match deps.cache_scope_kind {
            ServerCacheScope::Global => None,
            ServerCacheScope::PerCaller => Some(CallerScope {
                user_id: &invoked.identity.user_id,
                account_label: deps.cache_account_label.as_deref(),
            }),
        };
        deps.proxy
            .cache
            .set(
                &deps.server_id,
                scope,
                &deps.upstream_request,
                &response,
                deps.effective_cache_ttl,
            )
            .await;
    }

    /// MCP `tools/call` has no token / cost concept, so there's
    /// nothing to debit against the limits or budget engines.
    /// Explicit opt-out per `Surface::record_usage`'s docstring
    /// guidance — relying on the trait default would silently skip
    /// accounting and be indistinguishable from a forgotten
    /// override on a future surface that DOES need it.
    async fn record_usage(_deps: &Self::PostInvokeDeps, _invoked: &Invoked<Self>) {}

    async fn emit_audit(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        let response = response_for_hooks(invoked, deps);
        // The streaming branch's full event timeline lands in the
        // audit row's response_body so trace replay UI shows
        // progress notifications + the final envelope. The buffered
        // branch reuses `deps.stream_audit_body` for the case where
        // the upstream itself replied with SSE inside a buffered
        // call (captured by `send_request`).
        let stream_audit_body = match &invoked.view {
            CapturedView::Streaming { captured, .. } => serde_json::to_string(captured).ok(),
            CapturedView::Buffered(_) => deps.stream_audit_body.clone(),
        };
        deps.proxy
            .emit_tools_call_audit(
                invoked.identity.user_id,
                &invoked.identity.user_email,
                invoked.identity.ip_address.as_deref(),
                deps.server_id,
                &deps.server_name,
                &deps.tool_name,
                &invoked.trace_id,
                deps.logged_arguments.as_ref(),
                invoked.started_at,
                &response,
                stream_audit_body.as_deref(),
            )
            .await;
    }
}

/// Per-request handles + identifiers the post-invoke hooks need.
/// `proxy` is cheap to clone (every field is `Arc`-backed or `Copy`-
/// shaped); the rest are surface-specific identifiers the pipeline
/// is generic over, so they cannot live on `Invoked<S>` itself.
pub struct McpPostInvokeDeps {
    pub proxy: McpProxy,
    pub server_id: Uuid,
    pub server_name: String,
    pub tool_name: String,
    /// The transformed (un-namespaced) upstream request body. Used
    /// as the cache key in `write_cache` and forwarded into the
    /// audit row's tool arguments via `logged_arguments`.
    pub upstream_request: JsonRpcRequest,
    /// Caller's tool arguments before transformation. Audited so
    /// trace replay shows what the user invoked, not what we
    /// forwarded.
    pub logged_arguments: Option<serde_json::Value>,
    pub cache_scope_kind: ServerCacheScope,
    pub cache_account_label: Option<String>,
    /// 0 ⇒ caching explicitly disabled for this server.
    pub effective_cache_ttl: u64,
    /// Event timeline captured by `send_request` when the upstream
    /// replied with SSE inside a *buffered* call (the streaming
    /// path captures its timeline into `CapturedView::Streaming`
    /// directly, so this stays `None` there).
    pub stream_audit_body: Option<String>,
    /// Wire-level JSON-RPC id of the inbound request. Used by
    /// [`response_for_hooks`] to bind a synthetic err_response when
    /// no upstream envelope arrived.
    pub original_request_id: Option<serde_json::Value>,
}

/// Materialise the canonical [`JsonRpcResponse`] the post-invoke
/// hooks operate on. Buffered captures already carry it; streaming
/// captures hold the raw event timeline so we pick the response
/// envelope per the shared id-matching rule. Synthesises an
/// INTERNAL_ERROR envelope (with the request id bound) when no
/// envelope arrived — preserves the pre-migration error wire shape
/// the existing `build_chunk_passthrough` produced.
fn response_for_hooks(invoked: &Invoked<McpSurface>, deps: &McpPostInvokeDeps) -> JsonRpcResponse {
    match &invoked.view {
        CapturedView::Buffered(r) => r.clone(),
        CapturedView::Streaming { outcome, captured } => {
            pick_response_envelope(captured, deps.original_request_id.as_ref()).unwrap_or_else(
                || {
                    let msg = match outcome {
                        StreamOutcome::Natural => {
                            "Upstream stream ended without a response envelope".to_string()
                        }
                        StreamOutcome::UpstreamError { message, .. } => {
                            format!("Upstream stream error: {message}")
                        }
                        StreamOutcome::ClientCancelled => {
                            "Client cancelled before upstream replied".to_string()
                        }
                    };
                    err_response(deps.original_request_id.clone(), INTERNAL_ERROR, msg)
                },
            )
        }
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
