//! Shared pre-flight + post-invoke skeleton for the three AI surfaces.
//!
//! Each handler still owns its own body parsing, content filter
//! placement, cache lookup (chat only), and response rendering — those
//! are surface-specific. What's factored out here is the boilerplate
//! that was duplicated verbatim across all three:
//!
//! * [`run_preflight_stages`] — budget + access + rate-limit tower
//! * [`launch_stream_pump`]   — build_chat_pump + detached post-invoke task
//! * [`run_buffered_post_invoke`] — Invoked construction + post-invoke + unwrap
//!
//! Plus [`LogCtx::new`] (in `log_ctx.rs`) builds the audit context in
//! one call instead of 12-field literals at every site.

use super::identity::limits_for_ai_gateway;
use super::{GatewayErrorResponse, GatewayRequestIdentity, GatewayState};

use super::routing::Resend;
use super::shaper::StreamShaper;
use crate::lifecycle::{
    ChatCompletionOutcome, ChatCompletionSurface, ChatPostInvokeDeps, Completed, OpenUpstream,
    build_chat_pump,
};
use think_watch_common::lifecycle::stages::{
    check_access, check_budget, check_limits, run_post_invoke,
};
use think_watch_common::lifecycle::state::{CapturedView, Invoked, LimitCheckRecord, Raw};
use think_watch_common::limits::{RequestLimits, usage};
use tw_dialect::ir::Dialect;

/// Pre-flight result threaded through to `ChatPostInvokeDeps` later
/// in the handler. Computed once by [`run_preflight_stages`] so the
/// caller doesn't have to re-derive the rule/cap lists from identity.
pub(super) struct Preflight {
    pub(super) limits: RequestLimits,
}

/// Run the three shared pre-flight stages — `check_budget` →
/// `check_access` → `check_limits` — that each AI surface gates on.
/// `check_limits` charges the request counters, so it goes last: a
/// request a spent budget or the key's model list refuses must not
/// have used up a request limit. The other two charge nothing.
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
    let limits = limits_for_ai_gateway(identity);
    let fail_closed = state.dynamic_config.rate_limit_fail_closed().await;

    let raw = Raw::<ChatCompletionSurface>::new(
        identity.clone(),
        trace_id.to_string(),
        identity.ip_address.clone(),
    );
    let raw = check_budget::<ChatCompletionSurface>(
        raw,
        &limits.caps,
        &state.redis,
        fail_closed,
        &state.audit,
    )
    .await
    .map_err(short_circuit_to_response)?;
    let access_checked = check_access::<ChatCompletionSurface>(raw, model, &state.audit)
        .await
        .map_err(short_circuit_to_response)?;
    let _authorized = check_limits::<ChatCompletionSurface>(
        access_checked,
        &limits.rules,
        limits.owner,
        &state.redis,
        fail_closed,
        &state.audit,
    )
    .await
    .map_err(short_circuit_to_response)?;
    // Past the rate limits, the request counts on its key and its owner,
    // as it has on every `requests` rule. Fail-open: the counts are for
    // reading.
    if let Some(lineage) = limits.key_lineage
        && let Err(e) =
            usage::record_request(&state.redis, limits.owner, lineage, chrono::Utc::now()).await
    {
        metrics::counter!("gateway_usage_count_fail_open_total").increment(1);
        tracing::warn!("usage request count failed: {e}");
    }

    Ok(Preflight { limits })
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
    mut deps: ChatPostInvokeDeps,
    open: OpenUpstream,
    resend: Resend,
    shaper: StreamShaper,
    client: Dialect,
    client_sse: bool,
) -> axum::response::Response {
    let (response, tail) = build_chat_pump(
        open,
        resend,
        shaper,
        client,
        client_sse,
        deps.state.clone(),
        &deps.guards,
        &deps.request,
        &deps.route.provider_name,
        deps.route.min_speed,
    );
    tokio::spawn(async move {
        let (invoked, sent) = tail.await;
        // Cut for generating too slowly: the session's next request is not
        // sent back to the same upstream by its affinity, and the row says
        // how fast it was going — only when that cut is how the stream
        // ended, so the row, the route's health and the affinity agree.
        if let Some(rate) = sent.too_slow.filter(|_| ended_too_slow(&invoked)) {
            super::forget_affinity(
                &deps.state.redis,
                deps.request.identity.user_id.as_deref(),
                &deps.request.mapped_model,
                deps.route.provider_id,
                deps.route.sel_record.picked_route_id,
            )
            .await;
            deps.route.sel_record.too_slow = Some(rate);
        }
        // A stream that went out again: the route's time is the attempt
        // that ended it, and the row says how many retries there were.
        if sent.retries > 0 {
            deps.route.sel_record.started_at = sent.last_sent;
            deps.route.sel_record.retries = sent.retries;
        }
        deps.route.sel_record.effort = sent.effort;
        // The client left while the stream waited to go out again: the
        // attempt that failed was counted against the route when the retry
        // was decided, and nothing went out since.
        deps.route.sel_record.counted = sent.waiting;
        run_post_invoke::<ChatCompletionSurface>(invoked, &deps).await;
    });
    response
}

/// Whether the stream ended because it generated too slowly (see
/// `super::min_speed`): its outcome, not just the pump's note of the cut.
fn ended_too_slow(invoked: &Invoked<ChatCompletionSurface>) -> bool {
    matches!(
        &invoked.view,
        CapturedView::Streaming {
            outcome: crate::lifecycle::StreamOutcome::UpstreamError { error_type, .. },
            ..
        } if error_type == "UpstreamTooSlow"
    )
}

/// Drive a whole answer through the post-invoke pipeline — cache fill,
/// audit, breaker, budget debit — and hand it back.
///
/// Restoring redacted values is the caller's job: the hooks see, and the
/// cache keeps, the placeholder form.
pub(super) async fn run_buffered_post_invoke(
    deps: &ChatPostInvokeDeps,
    completed: Completed,
) -> Completed {
    let invoked = Invoked {
        identity: deps.request.identity.clone(),
        trace_id: deps.request.trace_id.clone(),
        started_at: deps.request.request_started_at,
        client_ip: deps.request.identity.ip_address.clone(),
        limit_check: LimitCheckRecord {
            currents: Vec::new(),
        },
        access_candidate: deps.request.mapped_model.clone(),
        view: CapturedView::Buffered(ChatCompletionOutcome::Success(completed)),
    };
    let emitted = run_post_invoke::<ChatCompletionSurface>(invoked, deps).await;
    match emitted.response {
        Some(ChatCompletionOutcome::Success(c)) => c,
        _ => unreachable!(
            "Invocation::Buffered(Success(_)) must yield Emitted::Success — \
             the hook chain doesn't construct other variants"
        ),
    }
}
