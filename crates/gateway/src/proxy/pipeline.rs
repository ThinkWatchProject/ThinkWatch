//! Shared pre-flight + post-invoke skeleton for the three AI surfaces.
//!
//! Each handler still owns its own body parsing, content filter
//! placement, cache lookup (chat only), and response rendering — those
//! are surface-specific. What's factored out here is the boilerplate
//! that was duplicated verbatim across all three:
//!
//! * [`run_preflight_stages`] — rate-limit + budget + access tower
//! * [`launch_stream_pump`]   — build_chat_pump + detached post-invoke task
//! * [`run_buffered_post_invoke`] — Invoked construction + post-invoke + unwrap
//!
//! Plus [`LogCtx::new`] (in `log_ctx.rs`) builds the audit context in
//! one call instead of 12-field literals at every site.

use std::pin::Pin;

use futures::Stream;

use super::identity::{budgets_for_ai_gateway, rules_for_ai_gateway};
use super::{GatewayErrorResponse, GatewayRequestIdentity, GatewayState};

use crate::lifecycle::{
    ChatCompletionOutcome, ChatCompletionSurface, ChatPostInvokeDeps, ChatPumpContext,
    build_chat_pump,
};
use crate::pii_redactor::PiiStreamRestorer;
use crate::providers::traits::{ChatCompletionChunk, ChatCompletionResponse, GatewayError};
use think_watch_common::lifecycle::stages::{
    check_access, check_budget, check_limits, run_post_invoke,
};
use think_watch_common::lifecycle::state::{CapturedView, Invoked, LimitCheckRecord, Raw};
use think_watch_common::limits::{BudgetCap, RateLimitRule};

/// Pre-flight result threaded through to `ChatPostInvokeDeps` later
/// in the handler. Computed once by [`run_preflight_stages`] so the
/// caller doesn't have to re-derive the rule/cap lists from identity.
pub(super) struct Preflight {
    pub(super) request_rules: Vec<RateLimitRule>,
    pub(super) budget_caps: Vec<BudgetCap>,
}

/// Run the three shared pre-flight stages — `check_limits` →
/// `check_budget` → `check_access` — that each AI surface gates on.
///
/// Returns the resolved rule + cap lists so the caller can feed them
/// into [`ChatPostInvokeDeps`] without re-walking the identity.
///
/// On any stage short-circuit, the matching `gateway_logs` row is
/// already emitted by the stage itself; this helper just unwraps the
/// outcome enum into the wire-shaped `GatewayErrorResponse`.
pub(super) async fn run_preflight_stages(
    state: &GatewayState,
    identity: &GatewayRequestIdentity,
    trace_id: &str,
    model: &str,
) -> Result<Preflight, GatewayErrorResponse> {
    let request_rules = rules_for_ai_gateway(identity);
    let budget_caps = budgets_for_ai_gateway(identity);
    let fail_closed = state.dynamic_config.rate_limit_fail_closed().await;

    let raw = Raw::<ChatCompletionSurface>::new(
        identity.clone(),
        trace_id.to_string(),
        identity.ip_address.clone(),
    );
    let limits_checked = check_limits::<ChatCompletionSurface>(
        raw,
        &request_rules,
        &state.redis,
        fail_closed,
        &state.audit,
    )
    .await
    .map_err(short_circuit_to_response)?;
    let limits_checked = check_budget::<ChatCompletionSurface>(
        limits_checked,
        &budget_caps,
        &state.redis,
        fail_closed,
        &state.audit,
    )
    .await
    .map_err(short_circuit_to_response)?;
    let _authorized = check_access::<ChatCompletionSurface>(limits_checked, model, &state.audit)
        .await
        .map_err(short_circuit_to_response)?;

    Ok(Preflight {
        request_rules,
        budget_caps,
    })
}

fn short_circuit_to_response(outcome: ChatCompletionOutcome) -> GatewayErrorResponse {
    match outcome {
        ChatCompletionOutcome::ShortCircuit(e) => GatewayErrorResponse::from(e),
        ChatCompletionOutcome::Success(_) => unreachable!(
            "pre-flight stages never return Success — they pass through to the next type-state \
             or short-circuit with ShortCircuit"
        ),
    }
}

/// Wire up the streaming pump and spawn its post-invoke tail.
///
/// Single audit-emit site for the streaming path: the spawned task
/// awaits the tail future, then drives `run_post_invoke` through
/// record_outcome → write_cache → record_usage → emit_audit.
pub(super) fn launch_stream_pump(
    deps: ChatPostInvokeDeps,
    pump_ctx: ChatPumpContext,
    stream: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>,
    stream_restorer: Option<PiiStreamRestorer>,
) -> axum::response::Response {
    let (response, tail) = build_chat_pump(stream, stream_restorer, pump_ctx);
    tokio::spawn(async move {
        let invoked = tail.await;
        run_post_invoke::<ChatCompletionSurface>(invoked, &deps).await;
    });
    response
}

/// Drive a buffered (non-streaming) response through the post-invoke
/// pipeline: construct `Invoked`, run the hook chain (cache fill +
/// audit emit + breaker + budget debit), then unwrap the emitted
/// response variant.
///
/// Reads identity / trace_id / started_at / mapped_model directly off
/// `deps.request` — handlers don't have to re-thread them through the
/// call.
///
/// PII restore is the caller's job because anthropic/responses need
/// to convert the response shape *after* restore, while chat returns
/// the response shape directly.
pub(super) async fn run_buffered_post_invoke(
    deps: &ChatPostInvokeDeps,
    response: ChatCompletionResponse,
) -> ChatCompletionResponse {
    let invoked = Invoked {
        identity: deps.request.identity.clone(),
        trace_id: deps.request.trace_id.clone(),
        started_at: deps.request.request_started_at,
        client_ip: deps.request.identity.ip_address.clone(),
        limit_check: LimitCheckRecord {
            currents: Vec::new(),
        },
        access_candidate: deps.request.mapped_model.clone(),
        view: CapturedView::Buffered(ChatCompletionOutcome::Success(response)),
    };
    let emitted = run_post_invoke::<ChatCompletionSurface>(invoked, deps).await;
    match emitted.response {
        Some(ChatCompletionOutcome::Success(r)) => r,
        _ => unreachable!(
            "Invocation::Buffered(Success(_)) must yield Emitted::Success — \
             the hook chain doesn't construct other variants"
        ),
    }
}
