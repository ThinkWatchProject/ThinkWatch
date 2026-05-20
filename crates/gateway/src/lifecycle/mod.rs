//! AI-gateway-side wiring for the
//! `think_watch_common::lifecycle` pipeline. Defines
//! [`ChatCompletionSurface`] (one [`Surface`] impl shared across
//! all three AI handlers — chat completions, Anthropic Messages,
//! OpenAI Responses — because their providers normalise upstream
//! streams to OpenAI `ChatCompletionChunk` so the captured shape
//! is identical), plus the [`ChatPostInvokeDeps`] bundle the
//! post-invoke hooks read.
//!
//! The single per-request variation point between the three
//! handlers — whether to fill the response cache (chat does;
//! Anthropic / Responses don't, matching the pre-migration
//! buffered behaviour) — is a `cache_enabled: bool` flag on
//! `ChatPostInvokeDeps` rather than three near-identical Surface
//! impls. When/if Anthropic or Responses ever needs a
//! buffered-Response type distinct from `ChatCompletionResponse`
//! (e.g. native Anthropic cache shape), splitting into separate
//! Surface impls is the natural next step.
//!
//! Hook responsibilities (each Surface trait method):
//! - `record_outcome` → `finalize_health` (breaker).
//! - `write_cache` → `cache.set` (gated by `cache_enabled` AND
//!   `CapturedView::is_success`).
//! - `record_usage` → `post_flight_account` (limits + budget
//!   debit).
//! - `emit_audit` → `prepare_body_capture` +
//!   `emit_gateway_log_with_extra`.

use std::sync::Arc;

use rust_decimal::Decimal;
use think_watch_common::audit::{AuditActor, AuditEntry, GatewayActor};
use think_watch_common::lifecycle::Surface;
use think_watch_common::lifecycle::state::{CapturedView, Invoked};
use think_watch_common::limits::{BudgetCap, RateLimitRule};

use crate::pii_redactor::PiiRedactor;
use crate::providers::traits::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, GatewayError,
    Usage,
};
use crate::proxy::{
    GatewayRequestIdentity, GatewayState, SelectionRecord, emit_gateway_log_with_extra,
    finalize_health, post_flight_account, prepare_body_capture, stream_usage_or_estimate,
};
use crate::streaming::{StreamOutcome, StreamResult, assemble_response};

/// Surface marker for the OpenAI chat completions API
/// (`POST /v1/chat/completions`). Zero-size. Crate-private — the
/// handlers in `crate::proxy` are the only callers, and keeping
/// the surface marker `pub(crate)` lets `ChatPostInvokeDeps` and
/// `SelectionRecord` stay crate-private without leaking through
/// the `Surface` trait's associated-type visibility check.
pub(crate) struct ChatCompletionSurface;

/// Either the buffered completion response that came back from the
/// upstream, or a [`GatewayError`] short-circuit produced by a
/// pipeline stage. Distinct from `S::StreamResponse` because the
/// cache stores the structured completion shape, not the wire SSE
/// envelope; distinct from a single `Response = GatewayError` choice
/// because the buffered success path needs typed access to the
/// completion fields for cache writes + body capture.
pub enum ChatCompletionOutcome {
    /// Upstream produced a complete response.
    Success(ChatCompletionResponse),
    /// A pipeline stage short-circuited. The handler turns this
    /// into a wire response via `GatewayErrorResponse::from`. As of
    /// phase 2, `proxy_chat_completion` doesn't yet drive its early
    /// errors through the common stages — when it does, this is
    /// the variant short-circuit factories return.
    ShortCircuit(GatewayError),
}

/// Streaming capture for the OpenAI chat surface. Pre-computed
/// inside the pump's tail future so the three post-invoke hooks
/// don't each pay for token resolution / response assembly.
pub struct ChatStreamCaptured {
    /// Raw chunks (chunk-bounded by `MAX_CACHED_CHUNKS` upstream).
    /// Currently unused by the hooks — kept so a future audit-debug
    /// view can replay the upstream timeline.
    pub chunks: Vec<ChatCompletionChunk>,
    /// Last usage seen on any chunk; `None` when the upstream never
    /// surfaced one (common without `stream_options.include_usage`).
    pub raw_usage: Option<Usage>,
    /// Resolved tokens from [`stream_usage_or_estimate`] — preserves
    /// the cancelled-stream contract that audit / budget agree on
    /// the token count even when no usage chunk arrived.
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Cost in USD using current platform pricing.
    pub cost_usd: Decimal,
    /// Assembled response for cache fill / audit body. `None` when
    /// the stream produced no chunks before terminating.
    pub assembled: Option<ChatCompletionResponse>,
}

/// Per-request post-invoke hook deps for the chat surface. Built
/// once before `invoke_upstream`; consumed by [`run_post_invoke`]
/// (in the foreground for buffered, inside the detached tail task
/// for streaming).
///
/// Crate-private so the `pub(crate) SelectionRecord` field doesn't
/// leak through. The handlers in `crate::proxy` are the only
/// callers anyway.
///
/// [`run_post_invoke`]: think_watch_common::lifecycle::stages::run_post_invoke
pub(crate) struct ChatPostInvokeDeps {
    pub state: GatewayState,
    /// Snapshot of the PII redactor — taken once per request so the
    /// audit-time body capture sees the same patterns the redaction
    /// pass used (a mid-flight hot-swap doesn't change what's
    /// already in flight).
    pub pii_redactor: Arc<PiiRedactor>,
    /// Pre-redaction messages so audit body capture reflects what
    /// the user authored (request.messages holds the redacted form
    /// after the upfront redaction pass).
    pub messages_for_audit: Vec<ChatMessage>,
    /// Request post-redaction, kept for cache key derivation and
    /// (for streaming) cache fill on natural completion.
    pub request_for_cache: ChatCompletionRequest,
    /// Caller-visible model name (before model_mapper + route
    /// upstream_model resolution).
    pub original_model: String,
    /// Provider id that served the request (`"openai"`,
    /// `"anthropic"`, …).
    pub provider_name: String,
    /// Upstream-side model id when the route remapped it.
    pub upstream_model: Option<String>,
    /// Selection record for the picked route — used by
    /// `finalize_health` inside `record_outcome`.
    pub sel_record: SelectionRecord,
    /// Materialised rate-limit rules for `post_flight_account`'s
    /// tokens / total_tokens debit.
    pub request_rules: Vec<RateLimitRule>,
    /// Materialised budget caps for `post_flight_account`.
    pub budget_caps: Vec<BudgetCap>,
    /// Resolved client identity — replicated here so the streaming
    /// detached task doesn't need to thread a `&Identity` through
    /// `'static` bounds.
    pub identity: GatewayRequestIdentity,
    pub trace_id: String,
    pub session_id: Option<String>,
    pub request_started_at: std::time::Instant,
    /// Whether `write_cache` should fill the response cache for this
    /// surface. The OpenAI chat completion handler caches; Anthropic
    /// Messages and the OpenAI Responses handler do not (their
    /// buffered counterparts don't cache either, so the streaming
    /// fill would be inconsistent). One flag per request keeps the
    /// three handler call sites composable with a single surface
    /// impl instead of three near-identical clones.
    pub cache_enabled: bool,
}

/// Materialise the [`ChatStreamCaptured`] view from a finished
/// [`StreamResult`]. Resolves token counts, assembles the canonical
/// response, and computes the cost — all once, so the post-invoke
/// hooks can read pre-computed fields instead of recomputing per
/// hook.
pub async fn capture_chat_stream(
    state: &GatewayState,
    original_model: &str,
    request_messages: &[ChatMessage],
    result: StreamResult,
) -> (StreamOutcome, ChatStreamCaptured) {
    let (prompt_tokens, completion_tokens) = stream_usage_or_estimate(&result, request_messages);
    let cost_usd = state
        .cost_tracker
        .calculate_cost(original_model, prompt_tokens, completion_tokens)
        .await;
    let assembled = assemble_response(&result.chunks, result.usage.clone());
    let captured = ChatStreamCaptured {
        chunks: result.chunks,
        raw_usage: result.usage,
        prompt_tokens,
        completion_tokens,
        cost_usd,
        assembled,
    };
    (result.outcome, captured)
}

impl Surface for ChatCompletionSurface {
    type Identity = GatewayRequestIdentity;
    /// `Raw.body` is plumbing the common stages carry through
    /// without ever reading. The AI gateway's three handlers build
    /// their typed `ChatCompletionRequest` at different points in
    /// the flow (chat-completions has one upfront from the Json
    /// extractor; Anthropic Messages / Responses synthesise one
    /// AFTER the preflight stages), so the lifecycle body is the
    /// unit type and the handler keeps the typed request as a
    /// local variable.
    type RequestBody = ();
    type Response = ChatCompletionOutcome;
    type StreamResponse = axum::response::Response;
    type AuditDetail = serde_json::Value;
    type StreamCaptured = ChatStreamCaptured;
    type PostInvokeDeps = ChatPostInvokeDeps;

    fn audit_entry(identity: &Self::Identity, action: &str) -> AuditEntry {
        GatewayActor {
            user_id: identity.user_id.as_deref(),
            user_email: identity.user_email.as_deref(),
            api_key_id: identity.api_key_id.as_deref(),
            api_key_lineage_id: identity.api_key_lineage_id.as_deref(),
            ip: identity.ip_address.as_deref(),
            session_id: None,
        }
        .audit(action)
    }

    fn rate_limited_response(label: &str) -> Self::Response {
        // `LocalRateLimited` so `status_code() == 429` on the wire.
        // The label (`"<subject>:<metric>/<window>"`) is what the
        // pre-migration `preflight_request_limits` already produced;
        // keeping it intact lets clients diff exhausted windows.
        ChatCompletionOutcome::ShortCircuit(GatewayError::LocalRateLimited(label.to_owned()))
    }

    fn rate_limiter_unavailable_response() -> Self::Response {
        // Matches the pre-migration `preflight_request_limits`
        // fail-closed path — same `LocalRateLimited` variant with
        // the sentinel label dashboards already filter on.
        ChatCompletionOutcome::ShortCircuit(GatewayError::LocalRateLimited(
            "rate_limiter_unavailable".to_owned(),
        ))
    }

    fn is_access_allowed(identity: &Self::Identity, candidate: &str) -> bool {
        identity
            .allowed_models
            .as_ref()
            .map(|allowed| {
                allowed.is_empty()
                    || allowed
                        .iter()
                        .any(|m| candidate == *m || candidate.starts_with(m))
            })
            .unwrap_or(true)
    }

    fn access_denied_response(candidate: &str) -> Self::Response {
        ChatCompletionOutcome::ShortCircuit(GatewayError::TransformError(format!(
            "Model '{candidate}' is not allowed for this API key"
        )))
    }

    fn budget_exceeded_response(label: &str) -> Self::Response {
        // `LocalRateLimited` per its docstring's explicit budget
        // coverage — wire status 429, label carries which cap fired
        // (e.g. `"user:budget/monthly"`) so dashboards can split
        // budget exhaustion from rate-limit hits.
        ChatCompletionOutcome::ShortCircuit(GatewayError::LocalRateLimited(label.to_owned()))
    }

    fn budget_unavailable_response() -> Self::Response {
        ChatCompletionOutcome::ShortCircuit(GatewayError::LocalRateLimited(
            "budget_unavailable".to_owned(),
        ))
    }

    async fn record_outcome(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        // Stream: Natural + ClientCancelled count as success against
        // the upstream (the latter is the client's choice). Upstream
        // errors fail the breaker.
        // Buffered: the buffered success path doesn't currently go
        // through this hook (proxy_chat_completion's buffered branch
        // still emits inline) — the ShortCircuit variant is not
        // expected from invoke_upstream. Match-all-other defaults to
        // success so the type system is exhaustive.
        let success = match &invoked.view {
            CapturedView::Streaming { outcome, .. } => matches!(
                outcome,
                StreamOutcome::Natural | StreamOutcome::ClientCancelled
            ),
            CapturedView::Buffered(ChatCompletionOutcome::Success(_)) => true,
            CapturedView::Buffered(ChatCompletionOutcome::ShortCircuit(_)) => false,
        };
        finalize_health(&deps.state, &deps.sel_record, success).await;
    }

    async fn write_cache(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        // Per-surface cache opt-in — Anthropic / Responses streaming
        // paths don't cache (their buffered cousins don't either, so
        // a streaming fill would be the only place caching happens).
        // Single flag on deps keeps the three handler call sites on
        // one surface impl without duplicating hook bodies.
        if !deps.cache_enabled {
            return;
        }
        // Buffered success: cache the response.
        // Streaming Natural: cache the assembled completion (the
        // run_post_invoke stage gate already filtered non-Natural
        // outcomes — assembled is the canonical completion shape,
        // identical to what a buffered request would have stored).
        let response = match &invoked.view {
            CapturedView::Buffered(ChatCompletionOutcome::Success(r)) => Some(r),
            CapturedView::Streaming { captured, .. } => captured.assembled.as_ref(),
            CapturedView::Buffered(ChatCompletionOutcome::ShortCircuit(_)) => None,
        };
        if let Some(response) = response {
            deps.state
                .cache
                .set(&deps.request_for_cache, response, None)
                .await;
        }
    }

    async fn record_usage(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        // Debit the limits engine + budget caps using the same token
        // resolution `emit_audit` will surface. Streaming pre-computed
        // the counts (see `capture_chat_stream`) so the budget reflects
        // what the upstream actually generated even on a client-cancel
        // before the final usage chunk arrived. ShortCircuit outcomes
        // contribute zero tokens — the debit is a no-op there but the
        // call still happens for trace-shape symmetry.
        let (prompt_tokens, completion_tokens) = extract_usage_tokens(&invoked.view);
        post_flight_account(
            deps.state.db.clone(),
            deps.state.redis.clone(),
            deps.state.dynamic_config.clone(),
            deps.state.weight_cache.clone(),
            deps.original_model.clone(),
            prompt_tokens,
            completion_tokens,
            deps.request_rules.clone(),
            deps.budget_caps.clone(),
            deps.identity.user_id.clone(),
            deps.identity.user_email.clone(),
            deps.identity.api_key_id.clone(),
            deps.identity.ip_address.clone(),
            deps.state.audit.clone(),
        )
        .await;
    }

    async fn emit_audit(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        // Streaming: pull pre-computed token counts + cost +
        // assembled response from the captured view.
        // Buffered: read from the response.
        let (assembled_ref, prompt_tokens, completion_tokens, cost, logged_status, error_detail) =
            match &invoked.view {
                CapturedView::Streaming { outcome, captured } => {
                    let (status, detail) = outcome.logged_status_and_detail();
                    (
                        captured.assembled.as_ref(),
                        captured.prompt_tokens,
                        captured.completion_tokens,
                        captured.cost_usd,
                        status,
                        detail,
                    )
                }
                CapturedView::Buffered(ChatCompletionOutcome::Success(r)) => {
                    let (pt, ct) = r
                        .usage
                        .as_ref()
                        .map(|u| (u.prompt_tokens, u.completion_tokens))
                        .unwrap_or((0, 0));
                    let cost = deps
                        .state
                        .cost_tracker
                        .calculate_cost(&deps.original_model, pt, ct)
                        .await;
                    (Some(r), pt, ct, cost, 200_i64, None)
                }
                CapturedView::Buffered(ChatCompletionOutcome::ShortCircuit(e)) => (
                    None,
                    0u32,
                    0u32,
                    Decimal::ZERO,
                    e.status_code(),
                    Some(serde_json::json!({
                        "error_type": e.error_tag(),
                        "error_message": e.to_string(),
                    })),
                ),
            };
        let body_capture = prepare_body_capture(
            &deps.state.dynamic_config,
            &deps.pii_redactor,
            &deps.state.blob_store,
            &deps.trace_id,
            &deps.messages_for_audit,
            assembled_ref,
        )
        .await;
        emit_gateway_log_with_extra(
            &deps.state.audit,
            &deps.trace_id,
            deps.session_id.as_deref(),
            deps.identity.user_id.as_deref(),
            deps.identity.user_email.as_deref(),
            deps.identity.api_key_id.as_deref(),
            deps.identity.api_key_lineage_id.as_deref(),
            deps.identity.ip_address.as_deref(),
            &deps.original_model,
            Some(deps.provider_name.as_str()),
            deps.upstream_model.as_deref(),
            prompt_tokens,
            completion_tokens,
            cost,
            deps.request_started_at.elapsed().as_millis() as i64,
            logged_status,
            error_detail,
            body_capture,
        );
    }
}

/// Pull `(prompt_tokens, completion_tokens)` out of a captured view.
/// Streaming uses the values `capture_chat_stream` resolved (handles
/// the no-usage-chunk-arrived case for client-cancelled streams);
/// buffered reads from `response.usage`. Shared between
/// [`ChatCompletionSurface::record_usage`] and
/// [`ChatCompletionSurface::emit_audit`] so the two hooks always
/// agree on the token count.
fn extract_usage_tokens(view: &CapturedView<ChatCompletionSurface>) -> (u32, u32) {
    match view {
        CapturedView::Streaming { captured, .. } => {
            (captured.prompt_tokens, captured.completion_tokens)
        }
        CapturedView::Buffered(ChatCompletionOutcome::Success(r)) => r
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0)),
        CapturedView::Buffered(ChatCompletionOutcome::ShortCircuit(_)) => (0, 0),
    }
}
