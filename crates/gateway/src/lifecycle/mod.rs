//! AI-gateway-side wiring for the `think_watch_common::lifecycle`
//! pipeline: [`ChatCompletionSurface`] (one [`Surface`] impl shared by
//! the three generation endpoints) and the [`ChatPostInvokeDeps`] bundle
//! the post-invoke hooks read.
//!
//! **What the hooks capture is the caller's bytes.** Earlier every
//! provider normalised its stream into OpenAI chat chunks, so the three
//! endpoints shared one typed shape. Requests now go out in the caller's
//! own format when the route allows it, and come back in it, so the
//! shape all three share is simpler: the response bytes as the caller
//! receives them (before redacted values are painted back), and the usage
//! read off the upstream's own bytes.
//!
//! Hook responsibilities:
//! - `record_outcome` → `finalize_health` (breaker).
//! - `write_cache` → `cache.set` (gated by `cache_enabled` AND success).
//! - `record_usage` → `post_flight_account` (limits + budget debit).
//! - `emit_audit` → `prepare_body_capture` + `emit_gateway_log_with_extra`.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::GatewayError;
use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, header};
use futures::StreamExt;
use rust_decimal::Decimal;
use think_watch_common::audit::{AuditActor, AuditEntry, GatewayActor};
use think_watch_common::lifecycle::Surface;
use think_watch_common::lifecycle::state::{CapturedView, Invoked, LimitCheckRecord};
use think_watch_common::limits::RequestLimits;
use tw_dialect::ir::Dialect;

use crate::guards::Guards;
use crate::proxy::generate::{Wire, priced, tokens};
use crate::proxy::retry::{KEEP_ALIVE, KEEP_ALIVE_FRAME};
use crate::proxy::shaper::{StreamShaper, rewrite_model};
use crate::proxy::{
    GatewayRequestIdentity, GatewayState, Resend, SelectionRecord, emit_gateway_log_with_extra,
    finalize_health, post_flight_account, prepare_body_capture,
};

pub use think_watch_common::lifecycle::streaming::StreamOutcome;

/// Surface marker for the generation endpoints. Crate-private so
/// `ChatPostInvokeDeps` and `SelectionRecord` stay crate-private without
/// leaking through the `Surface` trait's associated-type visibility check.
pub(crate) struct ChatCompletionSurface;

/// A whole answer, in the caller's format.
pub struct Completed {
    /// As the upstream answered, in the caller's format, with redaction
    /// placeholders still in place and the answer's text not yet through
    /// the content rules — the form the cache stores, so a later caller
    /// can paint in their own values and have the rules in force then run
    /// on them.
    pub body: Vec<u8>,
    /// What the request's log row stores: the answer as the client
    /// received it (values restored, text through the content rules), with
    /// the values masked again (see [`for_the_log`]). `None`: `body`.
    pub logged: Option<Vec<u8>>,
    /// Read off the upstream's bytes, whatever format they were in, or
    /// estimated when they carried none.
    pub usage: tw_dialect::usage::Usage,
    /// `usage` is at least partly an estimate (see `crate::usage_estimate`).
    pub usage_estimated: bool,
}

/// Either the upstream's answer or a short-circuit from a pipeline stage.
pub enum ChatCompletionOutcome {
    Success(Completed),
    ShortCircuit(GatewayError),
}

/// What a finished stream leaves behind for the hooks. Computed once in
/// the pump's tail so the hooks read it rather than each recomputing.
pub struct ChatStreamCaptured {
    /// What the upstream reported, completed by an estimate where it
    /// reported nothing or was cut short. Zero when no answer came.
    pub usage: tw_dialect::usage::Usage,
    /// `usage` is at least partly an estimate.
    pub usage_estimated: bool,
    pub cost_usd: Decimal,
    /// The stream assembled into a whole answer, in the form the cache
    /// keeps (placeholders in place, text not through the content rules).
    /// `None` when it produced nothing or could not be assembled.
    pub assembled: Option<Vec<u8>>,
    /// The same answer as the client received it, masked again for the
    /// log row (see [`for_the_log`]).
    pub logged: Option<Vec<u8>>,
}

/// The in-flight request, captured once at the handler and read by every
/// attempt and by the streaming tail task. Owned rather than borrowed so
/// the detached tail needs no `'static` borrow.
pub(crate) struct ChatRequestSnapshot {
    pub identity: GatewayRequestIdentity,
    /// The one id every audit row and gateway log carries for this request.
    pub trace_id: String,
    /// Multi-turn conversation id from `x-session-id`.
    pub session_id: Option<String>,
    /// The model the caller named, after aliasing — what lands in
    /// `gateway_logs.model`. Never the upstream's own name.
    pub mapped_model: String,
    /// The request body as the caller sent it — after the content filter
    /// stripped anything, before outbound redaction: the audit row is the
    /// record of what the user wrote. Body capture applies its own
    /// redaction toggle on top.
    pub request_for_audit: Vec<u8>,
    /// Where the cache keeps this request's answer. `None` when the
    /// request must not be cached.
    pub cache_fingerprint: Option<Vec<u8>>,
    pub request_started_at: std::time::Instant,
    /// The request's input in tokens, estimated — billed only when the
    /// upstream reports no usage.
    pub input_estimate: u64,
    /// The conversation a Responses request with a session id belongs to.
    /// The provider that answers it is remembered for it (see
    /// `proxy::sealed`).
    pub conversation: Option<crate::proxy::sealed::Conversation>,
}

/// Pre-flight rule + cap lists, reused by the post-flight debit.
pub(crate) struct ChatPreflightLists {
    pub limits: RequestLimits,
}

/// The route that actually served the request.
pub(crate) struct ChatPickedRoute {
    pub provider_id: uuid::Uuid,
    pub provider_name: String,
    /// Picked because it holds the conversation's encrypted history, not by
    /// the strategy or the affinity (see `proxy::sealed`). Said on the log
    /// row as `"pinned_by": "encrypted_history"`.
    pub pinned: bool,
    /// Upstream-side model id when the route remapped it.
    pub upstream_model: Option<String>,
    /// Used by `finalize_health` inside `record_outcome`.
    pub sel_record: SelectionRecord,
    /// The route's capacity caps: its token cap counts the answer's
    /// tokens in `record_usage`.
    pub caps: crate::route_caps::RouteCaps,
}

/// Everything the post-invoke hooks read. Built once before the upstream
/// call; consumed in the foreground for a whole answer, inside the
/// detached tail task for a stream.
pub(crate) struct ChatPostInvokeDeps {
    pub state: GatewayState,
    /// The guards the request ran under, so the tool-call inspection and
    /// body capture use the same rules the request was screened and
    /// redacted with, even across a hot swap.
    pub guards: Arc<Guards>,
    pub request: ChatRequestSnapshot,
    pub preflight: ChatPreflightLists,
    pub route: ChatPickedRoute,
    /// Only chat completions caches.
    pub cache_enabled: bool,
}

/// The upstream call a stream makes, not yet started: each call starts
/// one attempt.
pub(crate) type OpenUpstream = Box<
    dyn FnMut() -> Pin<
            Box<
                dyn std::future::Future<Output = Result<(reqwest::Response, Wire), GatewayError>>
                    + Send,
            >,
        > + Send,
>;

/// What a stream's tail resolves to once the stream ends: what the hooks
/// read, and how the stream reached its upstream.
pub(crate) type StreamTail =
    Pin<Box<dyn std::future::Future<Output = (Invoked<ChatCompletionSurface>, Sent)> + Send>>;

/// How a stream reached its upstream, for the tail: how many times it went
/// out again after a transient failure, and when the attempt that ended it
/// went out.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Sent {
    pub retries: u32,
    pub last_sent: std::time::Instant,
    /// Waiting to go out again: the attempt that failed was counted
    /// against the route already, and nothing is under way.
    pub waiting: bool,
}

/// Build the streaming pump: forward the upstream's bytes to the caller —
/// converted if the route speaks another format, shaped either way — and
/// return a tail future that resolves once the stream ends.
///
/// **The upstream is called on the stream's first poll, not before the
/// response is returned.** Awaiting it up front would hold the caller's
/// response headers until the upstream's arrived, and a caller who gave
/// up in that window would leave no trace: hyper drops the handler, and
/// nothing after the await point runs. Inside the stream, that same
/// disconnect drops the body and the tail records it as cancelled. A
/// rejected dialect is still retried inside `open`, before any byte
/// reaches the caller.
///
/// Nothing of the answer is buffered. Usage is sniffed and the whole
/// answer assembled alongside the bytes, not by holding them back.
///
/// **A transient failure before any of the answer went out goes out
/// again** to the same route (see `proxy::retry`; `resend` decides): the
/// upstream could not be reached, answered 5xx, broke off, or reported an
/// overload or a server error before any content. While a failure could
/// still be sent again, what the upstream sends ahead of its content is
/// held back (see [`Opening`]), so that a retry's opening is not the
/// client's second one. While it is held, and while the gateway waits to
/// send again, the client gets an SSE keep-alive comment after every
/// [`KEEP_ALIVE`] of silence. The status is 200 from the start, as it
/// always was: a failure that is the answer is said in the stream.
///
/// **A dropped stream is a cancelled request.** When the client goes,
/// hyper drops the body, and with it the sender the tail is waiting on;
/// the tail then records `ClientCancelled` — unless the answer's last
/// frame had already gone out (see [`LastFrame`]). A client that leaves
/// after that leaves having read the whole answer: Codex closes the
/// connection as soon as it has `response.completed`, and an upstream can
/// end its stream a while after that frame. The request finished, and is
/// recorded as `Natural`.
///
/// For the same reason a stream that ends here tells its outcome before
/// its last bytes go out, not after: a consumer that stops reading at
/// those bytes, as the WebSocket relay does at a turn's last event, never
/// polls the stream again.
///
/// **An error the upstream reports in its stream fails the request** (see
/// [`ErrorWatch`]): an Anthropic `error` event, Responses
/// `response.failed`, an `error` in a Chat or Gemini chunk. The upstream
/// opened the stream with 200 and the stream may end normally, but the
/// client got half an answer and an error; recorded as a success, the
/// route would look like it never fails. It is recorded as the error the
/// same refusal would have been as an answer, with the upstream's words
/// (see [`failed_partway`]), whether the stream ran to its end or the
/// client left after the error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_chat_pump(
    open: OpenUpstream,
    resend: Resend,
    mut shaper: StreamShaper,
    client: Dialect,
    client_sse: bool,
    deps_state: GatewayState,
    guards: &Guards,
    request: &ChatRequestSnapshot,
    provider: &str,
) -> (axum::response::Response, StreamTail) {
    let readers = Arc::new(Mutex::new(Readers::default()));
    let readers_for_tail = Arc::clone(&readers);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();
    // Set once the answer's last frame is handed to the client.
    let delivered = Arc::new(AtomicBool::new(false));
    let delivered_for_tail = Arc::clone(&delivered);

    // Tool calls are inspected on what the client is about to receive —
    // converted, if it was, and with redacted values restored — since that
    // is what it would execute. A call that sends a restored credential
    // somewhere is only visible in that form.
    let mut inspector = crate::tool_inspection::StreamInspector::new(
        guards.tools.clone(),
        guards.redaction.clone(),
        deps_state.audit.clone(),
        crate::guards::Caller::of(&request.identity, &request.trace_id, &request.mapped_model),
        provider.to_string(),
    );

    // What the content rules find in the answer is recorded by the tail,
    // whether the stream ran to its end or the client left.
    let answer_hits = shaper.answer_hits();
    let content = guards.content.clone();
    let redaction = guards.redaction.clone();
    let caller =
        crate::guards::Caller::of(&request.identity, &request.trace_id, &request.mapped_model);

    let mut last_frame = LastFrame::new(client, client_sse);
    let provider_for_tail = provider.to_string();
    let provider = provider.to_string();
    let sent = Arc::new(Mutex::new(Sent {
        retries: 0,
        last_sent: std::time::Instant::now(),
        waiting: false,
    }));
    // Gemini clients get no keep-alive comments: the google-genai SDK reads
    // every SSE line as JSON.
    let keep_alive = keeps_alive(client);
    let sent_for_tail = Arc::clone(&sent);
    let body = async_stream::stream! {
        let mut done_tx = Some(done_tx);
        let mut open = open;
        let mut resend = resend;
        // When the client last received something: a keep-alive is due
        // `KEEP_ALIVE` after it while the gateway holds the stream back.
        let mut quiet_since = tokio::time::Instant::now();
        // The wait before the next attempt, after a transient failure.
        let mut backoff: Option<std::time::Duration> = None;

        'attempt: loop {
            if let Some(wait) = backoff.take() {
                let until = tokio::time::Instant::now() + wait;
                if keep_alive {
                    let mut ticks = std::pin::pin!(keep_alives(quiet_since, until));
                    while ticks.next().await.is_some() {
                        quiet_since = tokio::time::Instant::now();
                        yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(KEEP_ALIVE_FRAME));
                    }
                } else {
                    tokio::time::sleep_until(until).await;
                }
            }
            let sent_at = std::time::Instant::now();
            if let Ok(mut s) = sent.lock() {
                *s = Sent { retries: resend.budget.retries(), last_sent: sent_at, waiting: false };
            }
            // What an attempt that failed had read is not this one's: its
            // usage is not billed, its error not the request's.
            if let Ok(mut r) = readers.lock() {
                *r = Readers::default();
            }
            // A retry's upstream says nothing until it answers: the client
            // hears keep-alives meanwhile.
            let resent = resend.budget.retries() > 0;
            let mut opening = open();
            let opened = loop {
                let tick = tokio::time::sleep_until(quiet_since + KEEP_ALIVE);
                let next = tokio::select! {
                    opened = &mut opening => Some(opened),
                    _ = tick, if resent && keep_alive => None,
                };
                match next {
                    Some(opened) => break opened,
                    None => {
                        quiet_since = tokio::time::Instant::now();
                        yield Ok(Bytes::from_static(KEEP_ALIVE_FRAME));
                    }
                }
            };
            let (upstream, wire) = match opened {
                Ok(opened) => opened,
                Err(e) => {
                    // Nothing of the answer went out: one that failed for
                    // the moment goes out again after a wait.
                    if let Some(wait) = resend.budget.wait_after(&e)
                        && resend.admit(elapsed_ms(sent_at)).await
                    {
                        resending(&provider, resend.budget.retries(), wait, e.error_tag(), &readers, &sent);
                        backoff = Some(wait);
                        continue 'attempt;
                    }
                    // Headers already went out as 200, so the refusal is said
                    // in the stream — and logged with the upstream's own status,
                    // so a throttled upstream stays 429 on the audit row.
                    let mut out = shaper.process(&error_frame(client, e.status_code(), &e.to_string()));
                    out.extend(shaper.finish());
                    tell(&mut done_tx, StreamOutcome::UpstreamError {
                        error_type: e.error_tag().to_string(),
                        message: e.to_string(),
                        status_code: e.status_code(),
                    });
                    yield Ok(Bytes::from(out));
                    return;
                }
            };
            if let Ok(mut r) = readers.lock() {
                *r = Readers {
                    sniffer: Some(tw_dialect::usage::Sniffer::new()),
                    collector: Some(wire.collect.collector()),
                    errors: Some(ErrorWatch::new(wire.dialect)),
                    ledger: Some(wire.ledger.clone()),
                    said: None,
                };
            }
            // The hop that answered can have numbered a value of its own.
            shaper.restore_with(&wire.ledger);
            let mut convert = wire.convert.as_ref().map(|s| s.stream());
            // Bedrock streams AWS eventstream frames, not SSE. Unframe them at
            // the door, so the sniffer, the collector and the converter all
            // read the same SSE they read from every other upstream.
            let mut unframe = (wire.dialect == Dialect::Bedrock)
                .then(tw_bedrock::eventstream::Transcoder::new);
            // While a failure could still be sent again, the upstream's
            // opening is held back until its first content (see `Opening`).
            let mut hold = resend.budget.open().then(|| Opening::new(wire.dialect));
            let mut source = upstream.bytes_stream();
            loop {
                let next = tokio::select! {
                    item = source.next() => Some(item),
                    _ = tokio::time::sleep_until(quiet_since + KEEP_ALIVE), if hold.is_some() && keep_alive => None,
                };
                let Some(item) = next else {
                    quiet_since = tokio::time::Instant::now();
                    yield Ok(Bytes::from_static(KEEP_ALIVE_FRAME));
                    continue;
                };
                let Some(item) = item else {
                    // The upstream's stream ended. One whose opening is still
                    // held ended without content: an error it reported that
                    // is transient goes out again; anything else goes to the
                    // client as it would have.
                    if let Some(h) = hold.take() {
                        let said = readers.lock().ok().and_then(|mut r| r.reported(true));
                        if let Some(said) = said
                            && let Some(wait) = retry_after_said(&resend, &provider, &said)
                            && resend.admit(elapsed_ms(sent_at)).await
                        {
                            resending(&provider, resend.budget.retries(), wait, "in-stream error", &readers, &sent);
                            backoff = Some(wait);
                            continue 'attempt;
                        }
                        match deliver(&h.release(), &mut convert, &mut shaper, inspector.as_mut(), client) {
                            Ok(out) => {
                                if !out.is_empty() {
                                    if last_frame.is_in(&out) {
                                        delivered.store(true, Ordering::Release);
                                    }
                                    yield Ok(Bytes::from(out));
                                }
                            }
                            Err((out, outcome)) => {
                                tell(&mut done_tx, outcome);
                                yield Ok(Bytes::from(out));
                                return;
                            }
                        }
                    }
                    break;
                };
                let item = match item {
                    Ok(raw) => match unframe.as_mut() {
                        None => Ok(raw),
                        Some(t) => t
                            .feed(&raw)
                            .map(Bytes::from)
                            .map_err(|e| bedrock_ended(&provider, e)),
                    },
                    Err(e) => Err(broke_off(format!("The upstream stream broke off: {e}"))),
                };
                match item {
                    Ok(chunk) => {
                        let (sealed, said) = match readers.lock() {
                            Ok(mut r) => {
                                r.feed(&chunk);
                                (r.sealed_refusal(), r.reported(false))
                            }
                            Err(_) => (None, None),
                        };
                        // The upstream cannot read the conversation's encrypted
                        // history: the client is told why in plain words, in
                        // place of the upstream's own error.
                        if let Some(said) = sealed {
                            let err = sealed_refused(&provider, &said);
                            tracing::warn!(
                                provider = %provider,
                                message = %said.message,
                                "upstream refused the conversation's encrypted history"
                            );
                            let out = cut(&shaper, convert.as_mut(), client, &[], &err);
                            tell(&mut done_tx, StreamOutcome::UpstreamError {
                                error_type: err.error_tag().to_string(),
                                message: err.to_string(),
                                status_code: err.status_code(),
                            });
                            yield Ok(Bytes::from(out));
                            return;
                        }
                        let chunk = match hold.as_mut() {
                            None => chunk.to_vec(),
                            Some(h) => {
                                // An error before any content: one that is
                                // transient goes out again. Anything else is
                                // the answer, and goes out with the opening.
                                if !h.feed(&chunk) {
                                    let Some(said) = &said else { continue };
                                    if let Some(wait) = retry_after_said(&resend, &provider, said)
                                        && resend.admit(elapsed_ms(sent_at)).await
                                    {
                                        resending(&provider, resend.budget.retries(), wait, "in-stream error", &readers, &sent);
                                        backoff = Some(wait);
                                        continue 'attempt;
                                    }
                                }
                                hold.take().map(Opening::release).unwrap_or_default()
                            }
                        };
                        match deliver(&chunk, &mut convert, &mut shaper, inspector.as_mut(), client) {
                            Ok(out) => {
                                if !out.is_empty() {
                                    if last_frame.is_in(&out) {
                                        delivered.store(true, Ordering::Release);
                                    }
                                    yield Ok(Bytes::from(out));
                                }
                            }
                            // A tool call the inspection stops: what came
                            // before still goes out, then the refusal.
                            Err((out, outcome)) => {
                                tell(&mut done_tx, outcome);
                                yield Ok(Bytes::from(out));
                                return;
                            }
                        }
                    }
                    Err((message, outcome)) => {
                        // Broken off before any content: the upstream's
                        // failure, which goes out again.
                        if let Some(h) = hold.take() {
                            if let StreamOutcome::UpstreamError { error_type, status_code, .. } = &outcome
                                && let Some(wait) = resend.budget.wait_after_outcome(error_type, *status_code)
                                && resend.admit(elapsed_ms(sent_at)).await
                            {
                                resending(&provider, resend.budget.retries(), wait, error_type, &readers, &sent);
                                backoff = Some(wait);
                                continue 'attempt;
                            }
                            // The opening it sent goes out before the error.
                            match deliver(&h.release(), &mut convert, &mut shaper, inspector.as_mut(), client) {
                                Ok(out) => {
                                    if !out.is_empty() {
                                        yield Ok(Bytes::from(out));
                                    }
                                }
                                Err((out, outcome)) => {
                                    tell(&mut done_tx, outcome);
                                    yield Ok(Bytes::from(out));
                                    return;
                                }
                            }
                        }
                        // Headers are gone; the only way left to say it is in
                        // the stream, in the caller's own format.
                        let tail = match convert.as_mut() {
                            Some(c) => c.fail(&message),
                            None => error_frame(client, 502, &message),
                        };
                        let mut out = shaper.process(&tail);
                        out.extend(shaper.finish());
                        tell(&mut done_tx, outcome);
                        yield Ok(Bytes::from(out));
                        return;
                    }
                }
            }
            let tail = convert.as_mut().map(|c| c.finish()).unwrap_or_default();
            let mut out = shaper.process(&tail);
            out.extend(shaper.finish());
            // The converter's last bytes can complete a tool call (the block's
            // stop), so they are inspected too.
            // A Chat call with no `finish_reason` or `[DONE]` completes only
            // as the stream closes; one that stops it keeps the tail back.
            let stop = inspector.as_mut().and_then(|i| {
                i.check(&out)
                    .or_else(|| i.finish().map(|err| (err, 0)))
            });
            if let Some((err, safe)) = stop {
                let out = cut(&shaper, None, client, &out[..safe], &err);
                tell(&mut done_tx, StreamOutcome::UpstreamError {
                    error_type: err.error_tag().to_string(),
                    message: err.to_string(),
                    status_code: err.status_code(),
                });
                yield Ok(Bytes::from(out));
                return;
            }
            let outcome = match readers.lock().ok().and_then(|mut r| r.upstream_error()) {
                Some(said) => failed_partway(&provider, &said),
                None => StreamOutcome::Natural,
            };
            tell(&mut done_tx, outcome);
            if !out.is_empty() {
                yield Ok(Bytes::from(out));
            }
            return;
        }
    };

    // A Gemini caller that did not ask for SSE reads one JSON array.
    let (body, content_type) = if client_sse {
        (Body::from_stream(body), "text/event-stream")
    } else {
        (Body::from_stream(as_json_array(body)), "application/json")
    };
    let mut response = axum::response::Response::new(body);
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));

    let identity = request.identity.clone();
    let trace_id = request.trace_id.clone();
    let started_at = request.request_started_at;
    let mapped_model = request.mapped_model.clone();
    let input_estimate = request.input_estimate;

    let tail = Box::pin(async move {
        let outcome = match done_rx.await {
            Ok(outcome) => outcome,
            // Dropped before the stream ended: the client left.
            Err(_) => match readers_for_tail
                .lock()
                .ok()
                .and_then(|mut r| r.upstream_error())
            {
                // After the upstream had failed, which the client leaving
                // does not change.
                Some(said) => failed_partway(&provider_for_tail, &said),
                // After the answer's last frame went out: the client left
                // having read all of it.
                None if delivered_for_tail.load(Ordering::Acquire) => StreamOutcome::Natural,
                None => StreamOutcome::ClientCancelled,
            },
        };
        metrics::counter!(
            "gateway_stream_completion_total",
            "outcome" => outcome.metric_label()
        )
        .increment(1);

        if let Ok(hits) = answer_hits.lock() {
            crate::content_filter::record_answer(
                &deps_state.audit,
                &caller,
                &provider_for_tail,
                content.mode,
                &hits,
            );
        }

        // The sniffer exists once the upstream answered. Without an
        // answer there is nothing to bill.
        let (answered, reported, assembled, ledger) = match readers_for_tail.lock() {
            Ok(mut r) => {
                let sniffer = r.sniffer.take();
                (
                    sniffer.is_some(),
                    sniffer.and_then(|s| s.finish()),
                    r.collector.take().and_then(|c| c.finish().ok()),
                    r.ledger.take(),
                )
            }
            Err(_) => (false, None, None, None),
        };
        // A stream that did not run to its end lost the upstream's final
        // count with it: the caller left, or the upstream broke off.
        let (usage, usage_estimated) = if answered {
            crate::usage_estimate::complete(
                reported,
                outcome.is_natural(),
                input_estimate,
                assembled.as_deref(),
            )
        } else {
            (tw_dialect::usage::Usage::default(), false)
        };
        if usage_estimated {
            metrics::counter!("gateway_usage_estimated_total").increment(1);
        }
        let cost_usd = deps_state
            .cost_tracker
            .calculate_cost(&mapped_model, &priced(&usage))
            .await;
        // A cache hit hands this back to a caller, so it carries the
        // caller's model name like everything else they receive. It stays
        // in placeholder form, unfiltered: the content rules run on the
        // restored text of each hit.
        let assembled = assembled.map(|b| rewrite_model(&b, &mapped_model));
        // The log row gets what the client was shown. (The hits were
        // recorded above, from the stream itself.)
        let logged = match (&assembled, &ledger) {
            (Some(b), Some(l)) => Some(for_the_log(&content, &redaction, l, client, b)),
            _ => None,
        };
        let captured = ChatStreamCaptured {
            usage,
            usage_estimated,
            cost_usd,
            assembled,
            logged,
        };
        let sent = sent_for_tail.lock().map(|s| *s).unwrap_or(Sent {
            retries: 0,
            last_sent: started_at,
            waiting: false,
        });
        let invoked = Invoked {
            client_ip: identity.ip_address.clone(),
            identity,
            trace_id,
            started_at,
            limit_check: LimitCheckRecord {
                currents: Vec::new(),
            },
            access_candidate: mapped_model,
            view: CapturedView::Streaming { outcome, captured },
        };
        (invoked, sent)
    });
    (response, tail)
}

/// The answer as the client received it, for the request's log row:
/// `body` (placeholder form) with the caller's values restored and the
/// text through the content rules — **the rules always run on restored
/// text**, so a rule never cuts into a placeholder — and then the values
/// masked again with the same placeholders, by the rules and by the ledger
/// ([`crate::redaction::Redaction::mask_for_storage`]), so the row holds no
/// more of the caller's values than it did before answers were filtered. Nothing
/// is recorded here: the hits were recorded on the way to the client.
pub(crate) fn for_the_log(
    content: &crate::content_filter::ContentFilter,
    redaction: &crate::redaction::Redaction,
    ledger: &tw_guard::redact::replace::Ledger,
    client: Dialect,
    body: &[u8],
) -> Vec<u8> {
    if content.answer.is_none() {
        return body.to_vec();
    }
    let restored = crate::redaction::restore_body(ledger, body);
    let (filtered, _) = content.whole(client, restored);
    redaction.mask_for_storage(filtered, ledger)
}

/// What the pump reads off the upstream's bytes as they pass, for the
/// tail. Each reader exists once the upstream answered.
#[derive(Default)]
struct Readers {
    sniffer: Option<tw_dialect::usage::Sniffer>,
    collector: Option<tw_dialect::convert::Collector>,
    /// `None` once it found an error.
    errors: Option<ErrorWatch>,
    /// The ledger the answering hop restores with, for the log row.
    ledger: Option<tw_guard::redact::replace::Ledger>,
    /// The first error the upstream reported in its stream.
    said: Option<Said>,
}

impl Readers {
    fn feed(&mut self, chunk: &[u8]) {
        if let Some(s) = self.sniffer.as_mut() {
            s.feed(chunk);
        }
        if let Some(c) = self.collector.as_mut() {
            c.process(chunk);
        }
        if let Some(said) = self.errors.as_mut().and_then(|w| w.feed(chunk)) {
            self.said = Some(said);
            self.errors = None;
        }
    }

    /// The upstream's refusal of the conversation's encrypted history, once
    /// its stream has said it. Left in place for the tail.
    fn sealed_refusal(&self) -> Option<Said> {
        self.said.as_ref().filter(|s| s.sealed).cloned()
    }

    /// The error the upstream reported in its stream so far, left in place
    /// for the tail. `ended`: the stream ended, so a last frame without a
    /// blank line after it is read too.
    fn reported(&mut self, ended: bool) -> Option<Said> {
        if ended
            && self.said.is_none()
            && let Some(said) = self.errors.as_mut().and_then(ErrorWatch::flush)
        {
            self.said = Some(said);
            self.errors = None;
        }
        self.said.clone()
    }

    /// The error the upstream reported in its stream, if it did — read to
    /// the end, for an upstream whose last frame has no blank line after
    /// it.
    fn upstream_error(&mut self) -> Option<Said> {
        if let Some(mut w) = self.errors.take() {
            self.said = self.said.take().or_else(|| w.flush());
        }
        self.said.take()
    }
}

/// Watches the upstream's frames for an error it reports in its stream
/// (see [`said_in`]: an Anthropic `error` event, Responses
/// `response.failed`, an `error` in a Chat or Gemini chunk). Read on the
/// upstream's bytes, so a converted stream is watched the same way as one
/// forwarded as sent.
struct ErrorWatch {
    upstream: Dialect,
    frames: tw_dialect::frame::Decoder,
}

impl ErrorWatch {
    fn new(upstream: Dialect) -> Self {
        Self {
            upstream,
            frames: Default::default(),
        }
    }

    /// What the upstream said, if this chunk completes an error frame.
    fn feed(&mut self, chunk: &[u8]) -> Option<Said> {
        let frames = self.frames.feed(chunk);
        self.first(&frames)
    }

    /// The stream ended: a last frame without a blank line after it.
    fn flush(&mut self) -> Option<Said> {
        let frames = self.frames.flush();
        self.first(&frames)
    }

    fn first(&self, frames: &[tw_dialect::frame::Frame]) -> Option<Said> {
        frames.iter().find_map(|f| said_in(self.upstream, f))
    }
}

/// An error the upstream reported in its stream.
#[derive(Debug, Clone, PartialEq)]
struct Said {
    /// In its words, or [`NO_MESSAGE`] when the error carried none.
    message: String,
    /// The status the same error has as an answer, when the error names
    /// one (see [`status_of`]).
    status: Option<u16>,
    /// The error refuses the conversation's encrypted history: another
    /// provider sealed it (see `proxy::sealed`).
    sealed: bool,
}

/// The message of an error the upstream reported without one.
const NO_MESSAGE: &str = "The error carried no message.";

/// What the upstream said in `f`, if `f` is an error frame.
///
/// `tw_dialect::convert::stream_error` picks the frame: the same reading
/// the converter gives it, and most frames are passed over without being
/// parsed. The error itself is read here, where each format keeps it:
/// Anthropic's `error`; Responses' `response.error` in `response.failed`,
/// or the `error` event's own fields (or its `error`); the top-level
/// `error` of a Chat or Gemini chunk. Gemini sends an error as a
/// one-element array outside SSE; upstreams are asked for SSE, but a
/// frame like that is read the same.
///
/// A Chat or Gemini chunk whose `error` is `null` is not an error: an
/// OpenAI-compatible relay can send `"error": null` in every chunk. And
/// the message is never the frame's own text, which can be the model's
/// answer: an error without a message gets [`NO_MESSAGE`].
fn said_in(upstream: Dialect, f: &tw_dialect::frame::Frame) -> Option<Said> {
    use serde_json::Value;

    tw_dialect::convert::stream_error(upstream, f)?;
    let v = match serde_json::from_str::<Value>(f.data.trim()) {
        Ok(Value::Array(mut items)) if items.len() == 1 => items.remove(0),
        Ok(v) => v,
        Err(_) => Value::Null,
    };
    fn present(e: Option<&Value>) -> Option<&Value> {
        e.filter(|e| !e.is_null())
    }
    let error = match upstream {
        Dialect::Chat | Dialect::Gemini => Some(present(v.get("error"))?),
        Dialect::Responses => {
            let kind = v.get("type").and_then(Value::as_str).or(f.event.as_deref());
            if kind == Some("response.failed") {
                present(v.pointer("/response/error"))
            } else {
                Some(present(v.get("error")).unwrap_or(&v))
            }
        }
        Dialect::Anthropic => Some(present(v.get("error")).unwrap_or(&v)),
        Dialect::Bedrock => Some(&v),
    };
    let message = match error {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(e) => e.get("message").and_then(Value::as_str),
        None => None,
    }
    .map(str::trim)
    .filter(|m| !m.is_empty())
    .unwrap_or(NO_MESSAGE);
    Some(Said {
        message: message.to_string(),
        status: error.and_then(status_of),
        sealed: crate::proxy::sealed::refused(upstream, &f.data),
    })
}

/// The status an error the upstream reports in its stream has as an
/// answer: a numeric `code` from 400 to 599 (Gemini's, and some relays'),
/// or what its `code`, `type` or `status` names (see [`status_named`]).
/// `None` when it says nothing recognisable.
fn status_of(e: &serde_json::Value) -> Option<u16> {
    use serde_json::Value;

    let numeric = match e.get("code") {
        Some(Value::Number(n)) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
        Some(Value::String(s)) => s.trim().parse::<u16>().ok(),
        _ => None,
    };
    numeric.filter(|c| (400..600).contains(c)).or_else(|| {
        ["code", "type", "status"]
            .iter()
            .find_map(|key| e.get(*key)?.as_str().and_then(status_named))
    })
}

/// The status each format documents for an error it names: Anthropic's
/// error types, OpenAI's error types and codes, Gemini's statuses — the
/// statuses the same errors come with as an answer. An overload is 529 as
/// Anthropic sends it, or 503.
fn status_named(name: &str) -> Option<u16> {
    Some(match name {
        "invalid_request_error"
        | "context_length_exceeded"
        | "invalid_prompt"
        | "INVALID_ARGUMENT"
        | "FAILED_PRECONDITION" => 400,
        "authentication_error" | "invalid_api_key" | "UNAUTHENTICATED" => 401,
        "billing_error" => 402,
        "permission_error" | "PERMISSION_DENIED" => 403,
        "not_found_error" | "model_not_found" | "NOT_FOUND" => 404,
        "request_too_large" => 413,
        "rate_limit_error"
        | "rate_limit_exceeded"
        | "insufficient_quota"
        | "usage_limit_reached"
        | "RESOURCE_EXHAUSTED" => 429,
        "api_error" | "server_error" => 500,
        "server_is_overloaded" | "slow_down" | "UNAVAILABLE" => 503,
        "timeout_error" | "DEADLINE_EXCEEDED" => 504,
        "overloaded_error" => 529,
        _ => return None,
    })
}

/// The outcome of a stream the upstream reported an error in: the error
/// the same refusal would have been as an answer (see
/// `transport::status_error`), so the route's health and circuit breaker
/// treat it as they treat that answer — a 5xx, 408 or 429 counts against
/// the route, a request the upstream refuses (another 4xx) does not.
/// An error that names no status is the upstream's failure, 502.
///
/// The message carries what the upstream said, 500 characters at most as
/// a refusal's body is cut — except for a refused credential, whose
/// words stay out of the row as an answer's do: they can name the account
/// behind it. The log has them either way.
fn failed_partway(provider: &str, said: &Said) -> StreamOutcome {
    tracing::warn!(
        provider,
        status = ?said.status,
        message = %said.message,
        "upstream reported an error partway through a stream"
    );
    outcome_of(provider, said)
}

/// [`failed_partway`], without the log line.
fn outcome_of(provider: &str, said: &Said) -> StreamOutcome {
    let words: String = said.message.chars().take(500).collect();
    if said.sealed {
        let e = sealed_refused(provider, said);
        return StreamOutcome::UpstreamError {
            error_type: e.error_tag().to_string(),
            message: e.to_string(),
            status_code: e.status_code(),
        };
    }
    let what = format!("{provider} reported an error partway through the answer: {words}");
    let e = match said.status {
        Some(status) => crate::proxy::transport::status_error(status, None, what.clone()),
        None => GatewayError::ProviderError(what.clone()),
    };
    let message = match &e {
        GatewayError::UpstreamRateLimited { .. } => format!("{e}: {what}"),
        GatewayError::UpstreamAuthError { .. } => {
            format!("{e}: {provider} reported it partway through the answer")
        }
        _ => e.to_string(),
    };
    StreamOutcome::UpstreamError {
        error_type: e.error_tag().to_string(),
        message,
        status_code: e.status_code(),
    }
}

/// The upstream refused the conversation's encrypted history in its
/// stream: the error said plainly, with its words (500 characters at most).
/// A refusal that names no status is a bad request's.
fn sealed_refused(provider: &str, said: &Said) -> GatewayError {
    GatewayError::SealedHistoryRefused {
        status: said.status.unwrap_or(400),
        provider: provider.to_string(),
        said: said.message.chars().take(500).collect(),
    }
}

/// A stream that broke off in transit: what the client is told, and the
/// outcome. No usable answer arrived — the upstream's failure, 502.
fn broke_off(message: String) -> (String, StreamOutcome) {
    tracing::warn!("{message}");
    let outcome = StreamOutcome::UpstreamError {
        error_type: "transport".into(),
        message: message.clone(),
        status_code: 502,
    };
    (message, outcome)
}

/// Bedrock's event stream ended in an error: what the client is told —
/// the same whichever it was — and the outcome.
///
/// A damaged frame broke the stream off. An exception Bedrock reported
/// in the stream (a `throttlingException` partway through, say) is an
/// error the upstream reported, recorded like the others (see
/// [`failed_partway`]) with the status [`bedrock_status`] gives it.
fn bedrock_ended(
    provider: &str,
    e: tw_bedrock::eventstream::StreamError,
) -> (String, StreamOutcome) {
    use tw_bedrock::eventstream::StreamError;

    let told = format!("Bedrock ended the stream: {e}");
    match e {
        StreamError::Malformed(_) => broke_off(told),
        StreamError::Upstream { kind, message } => {
            let said = Said {
                message: format!("{kind}: {message}"),
                status: Some(bedrock_status(&kind)),
                sealed: false,
            };
            (told, failed_partway(provider, &said))
        }
    }
}

/// The status of an exception Bedrock reports in its event stream, as the
/// desktop gateway reads one that arrives in a stream: throttling 429,
/// an invalid request 400, access denied 403, a model timeout 504, the
/// service unavailable 503, and any other exception (an internal error,
/// a model stream error) 500. Bedrock names them in camel case
/// (`throttlingException`); any case is read.
fn bedrock_status(kind: &str) -> u16 {
    const STATUSES: [(&str, u16); 5] = [
        ("throttlingException", 429),
        ("serviceUnavailableException", 503),
        ("validationException", 400),
        ("accessDeniedException", 403),
        ("modelTimeoutException", 504),
    ];
    STATUSES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(kind))
        .map_or(500, |(_, status)| *status)
}

/// The wait before sending again after an error the upstream reported
/// in its stream before any content, when it is transient (see
/// `proxy::retry`) and the wait fits.
fn retry_after_said(resend: &Resend, provider: &str, said: &Said) -> Option<std::time::Duration> {
    match outcome_of(provider, said) {
        StreamOutcome::UpstreamError {
            error_type,
            status_code,
            ..
        } => resend.budget.wait_after_outcome(&error_type, status_code),
        _ => None,
    }
}

/// Whether the client may be sent keep-alive comments while the gateway
/// holds its stream back. Not a Gemini client: the google-genai SDK reads
/// every SSE line as JSON, and a JSON-array stream has no place for them.
fn keeps_alive(client: Dialect) -> bool {
    client != Dialect::Gemini
}

/// A stream goes out again after `wait`: what the failed attempt left for
/// the tail is cleared — a client that leaves during the wait left a
/// request whose failed attempt is already counted against the route
/// (see `routing::Resend::admit`), with nothing to bill — and the log
/// line written.
fn resending(
    provider: &str,
    retry: u32,
    wait: std::time::Duration,
    error: &str,
    readers: &Mutex<Readers>,
    sent: &Mutex<Sent>,
) {
    if let Ok(mut r) = readers.lock() {
        *r = Readers::default();
    }
    if let Ok(mut s) = sent.lock() {
        *s = Sent {
            retries: retry,
            last_sent: std::time::Instant::now(),
            waiting: true,
        };
    }
    tracing::info!(
        provider,
        retry,
        wait_ms = wait.as_millis() as u64,
        error,
        "Upstream failed for the moment before any of the answer went out, sending again after a wait"
    );
}

fn elapsed_ms(since: std::time::Instant) -> u32 {
    since.elapsed().as_millis().min(u32::MAX as u128) as u32
}

/// A tick each time the client is due a keep-alive while the gateway waits
/// until `until`: [`KEEP_ALIVE`] after `quiet_since`, then every
/// [`KEEP_ALIVE`]. Ends at `until`.
fn keep_alives(
    quiet_since: tokio::time::Instant,
    until: tokio::time::Instant,
) -> impl futures::Stream<Item = ()> {
    async_stream::stream! {
        let mut due = quiet_since + KEEP_ALIVE;
        while due < until {
            tokio::time::sleep_until(due).await;
            yield ();
            due += KEEP_ALIVE;
        }
        tokio::time::sleep_until(until).await;
    }
}

/// What the client gets for `chunk` of the upstream's bytes: converted if
/// the route speaks another format, shaped, and inspected for tool calls.
/// `Err`: a tool call the inspection stops — what came before it, then
/// the refusal — and the stream's outcome.
fn deliver(
    chunk: &[u8],
    convert: &mut Option<tw_dialect::convert::StreamConverter>,
    shaper: &mut StreamShaper,
    inspector: Option<&mut crate::tool_inspection::StreamInspector>,
    client: Dialect,
) -> Result<Vec<u8>, (Vec<u8>, StreamOutcome)> {
    let client_bytes = match convert.as_mut() {
        Some(c) => c.process(chunk),
        None => chunk.to_vec(),
    };
    let out = shaper.process(&client_bytes);
    match inspector.and_then(|i| i.check(&out)) {
        Some((err, safe)) => {
            let out = cut(shaper, convert.as_mut(), client, &out[..safe], &err);
            Err((
                out,
                StreamOutcome::UpstreamError {
                    error_type: err.error_tag().to_string(),
                    message: err.to_string(),
                    status_code: err.status_code(),
                },
            ))
        }
        None => Ok(out),
    }
}

/// A stream's opening, held back from the client while a failure could
/// still be sent again (see `proxy::retry`): what an upstream sends as
/// soon as it takes the request — Anthropic's `message_start` and `ping`,
/// Responses' `response.created` and `response.in_progress`, a Chat chunk
/// with only the role, Bedrock's `messageStart` — is not yet the answer.
/// Sent ahead of a failure, it could not be taken back, and a retry would
/// send it twice. It goes out with the first content, or with an error
/// that is the answer.
///
/// Read on the upstream's bytes in its own format, as
/// `tw_dialect::convert::Reader` reads them: anything past the start and
/// the usage is content. An opening that grows past [`Opening::MAX`]
/// without any is let through: a broken upstream must not make it grow
/// without end.
struct Opening {
    reader: tw_dialect::convert::Reader,
    held: Vec<u8>,
}

impl Opening {
    const MAX: usize = 1024 * 1024;

    fn new(upstream: Dialect) -> Self {
        Self {
            reader: tw_dialect::convert::Reader::new(upstream),
            held: Vec::new(),
        }
    }

    /// Hold `chunk` back. True once it carries content ahead of any error,
    /// and with it what was held can go out: an error after content is
    /// one after the answer began, whatever piece of the upstream's bytes
    /// it came in.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        use tw_dialect::ir::Event;
        self.held.extend_from_slice(chunk);
        let content = self
            .reader
            .feed(chunk)
            .iter()
            .take_while(|e| !matches!(e, Event::Error { .. }))
            .any(|e| !matches!(e, Event::Start { .. } | Event::Usage(_)));
        content || self.held.len() > Self::MAX
    }

    /// What was held back, to go out.
    fn release(self) -> Vec<u8> {
        self.held
    }
}

/// Tell the tail how the stream ended. Only the first outcome counts.
fn tell(done_tx: &mut Option<tokio::sync::oneshot::Sender<StreamOutcome>>, outcome: StreamOutcome) {
    if let Some(tx) = done_tx.take() {
        let _ = tx.send(outcome);
    }
}

/// Watches what the client receives for the answer's last frame
/// (`tw_dialect::convert::ends_answer`): Responses `response.completed`,
/// `response.incomplete` or `response.failed`, Anthropic `message_stop`,
/// Chat `[DONE]`. A Gemini answer has none, SSE or JSON array: it ends
/// with the stream.
struct LastFrame {
    client: Dialect,
    /// `None` once the frame was seen, and for an answer without one.
    frames: Option<tw_dialect::frame::Decoder>,
}

impl LastFrame {
    fn new(client: Dialect, client_sse: bool) -> Self {
        let has_one = client_sse && !matches!(client, Dialect::Gemini | Dialect::Bedrock);
        Self {
            client,
            frames: has_one.then(Default::default),
        }
    }

    /// Whether `out`, the next bytes the client receives, completes the
    /// last frame. True once; nothing is read after it.
    fn is_in(&mut self, out: &[u8]) -> bool {
        let Some(frames) = self.frames.as_mut() else {
            return false;
        };
        let found = frames
            .feed(out)
            .iter()
            .any(|f| tw_dialect::convert::ends_answer(self.client, f));
        if found {
            self.frames = None;
        }
        found
    }
}

/// Reframe a client-format SSE stream as Gemini's JSON-array stream (see
/// [`crate::proxy::shaper::JsonArrayFramer`]).
fn as_json_array(
    sse: impl futures::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static,
) -> impl futures::Stream<Item = Result<Bytes, std::convert::Infallible>> + Send + 'static {
    async_stream::stream! {
        let mut framer = crate::proxy::shaper::JsonArrayFramer::default();
        let mut sse = Box::pin(sse);
        while let Some(Ok(chunk)) = sse.next().await {
            let out = framer.process(&chunk);
            if !out.is_empty() {
                yield Ok(Bytes::from(out));
            }
        }
        yield Ok(Bytes::from(framer.finish()));
    }
}

/// End a stream at a tool call the inspection stops: what came before it
/// (`safe`, already shaped) still goes out, then the refusal, in the
/// caller's format. A Gemini caller reading a JSON array gets the refusal
/// as the array's last element, then `]`.
///
/// An incomplete tool call cannot be executed, so the client is left with
/// nothing it can run. Nothing from the frame that matched on goes out —
/// not even text the restorer was still holding back.
fn cut(
    shaper: &StreamShaper,
    convert: Option<&mut tw_dialect::convert::StreamConverter>,
    client: Dialect,
    safe: &[u8],
    err: &crate::error::GatewayError,
) -> Vec<u8> {
    let message = err.to_string();
    let refusal = match convert {
        Some(c) => c.fail(&message),
        None => error_frame(client, err.status_code(), &message),
    };
    let mut out = safe.to_vec();
    out.extend(shaper.rename(&refusal));
    out
}

/// A standalone error frame in the caller's format, for a stream that has
/// no converter to write one (forwarded as sent, or never opened). `status`
/// is what the error would have been as a response, and picks its class.
fn error_frame(client: Dialect, status: i64, message: &str) -> Vec<u8> {
    let status = u16::try_from(status).unwrap_or(502);
    tw_dialect::convert::error_frame(client, status, message).into_bytes()
}

impl Surface for ChatCompletionSurface {
    type Identity = GatewayRequestIdentity;
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

    fn rate_limited_response(label: &str, retry_after_secs: u64) -> Self::Response {
        // `LocalRateLimited` so `status_code() == 429` on the wire.
        // The label (`"<subject>:<metric>/<window>"`) lets clients
        // diff exhausted windows; `Retry-After` says when this one
        // has room again.
        ChatCompletionOutcome::ShortCircuit(GatewayError::rate_limited(label, retry_after_secs))
    }

    fn rate_limiter_unavailable_response() -> Self::Response {
        // Same `LocalRateLimited` variant with the sentinel label
        // dashboards already filter on.
        ChatCompletionOutcome::ShortCircuit(GatewayError::limiter_unavailable(
            "rate_limiter_unavailable",
        ))
    }

    /// See [`GatewayRequestIdentity::may_call`]. The MCP surface reads
    /// an empty list the same way: nothing is allowed.
    fn is_access_allowed(identity: &Self::Identity, candidate: &str) -> bool {
        identity.may_call(candidate)
    }

    fn access_denied_response(candidate: &str) -> Self::Response {
        ChatCompletionOutcome::ShortCircuit(GatewayError::TransformError(format!(
            "Model '{candidate}' is not allowed for this API key"
        )))
    }

    fn budget_exceeded_response(label: &str, retry_after_secs: u64) -> Self::Response {
        // Wire status 429; the label carries which cap fired (e.g.
        // `"user:budget/monthly"`) so dashboards can split budget
        // exhaustion from rate-limit hits. `Retry-After` is the end of
        // the cap's period, and SDKs are told not to retry by themselves.
        ChatCompletionOutcome::ShortCircuit(GatewayError::budget_exhausted(label, retry_after_secs))
    }

    fn budget_unavailable_response() -> Self::Response {
        ChatCompletionOutcome::ShortCircuit(GatewayError::limiter_unavailable("budget_unavailable"))
    }

    async fn record_outcome(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        // A client that leaves did nothing wrong to the upstream, and
        // neither did one that refused the request (see
        // `routing::is_upstream_failure`).
        let success = match &invoked.view {
            CapturedView::Streaming { outcome, .. } => match outcome {
                StreamOutcome::Natural | StreamOutcome::ClientCancelled => true,
                StreamOutcome::UpstreamError {
                    error_type,
                    status_code,
                    ..
                } => !crate::proxy::upstream_failed(error_type, *status_code),
            },
            CapturedView::Buffered(ChatCompletionOutcome::Success(_)) => true,
            CapturedView::Buffered(ChatCompletionOutcome::ShortCircuit(_)) => false,
        };
        finalize_health(&deps.state, &deps.route.sel_record, success).await;

        // The provider that answered holds whatever it sealed in the answer
        // (see `proxy::sealed`). Only a whole answer: one cut short never
        // reached the client to be sent back.
        let answered = matches!(
            &invoked.view,
            CapturedView::Streaming {
                outcome: StreamOutcome::Natural,
                ..
            } | CapturedView::Buffered(ChatCompletionOutcome::Success(_))
        );
        if answered && let Some(c) = &deps.request.conversation {
            c.answered_by(
                &deps.state.redis,
                deps.route.provider_id,
                &deps.route.provider_name,
            )
            .await;
        }
    }

    async fn write_cache(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        if !deps.cache_enabled {
            return;
        }
        let Some(fp) = &deps.request.cache_fingerprint else {
            return;
        };
        // A stream reaches here only on a natural end — the stage gate
        // already filtered the rest — and its assembled form is exactly
        // what a whole answer would have stored.
        let (prompt_tokens, completion_tokens) = tokens(&extract_usage(&invoked.view));
        let body = match &invoked.view {
            CapturedView::Buffered(ChatCompletionOutcome::Success(c)) => Some(&c.body),
            CapturedView::Streaming { captured, .. } => captured.assembled.as_ref(),
            CapturedView::Buffered(ChatCompletionOutcome::ShortCircuit(_)) => None,
        };
        if let Some(body) = body {
            let cached = crate::cache::Cached {
                body: body.clone(),
                prompt_tokens,
                completion_tokens,
            };
            deps.state.cache.set(fp, &cached, None).await;
        }
    }

    async fn record_usage(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        post_flight_account(
            deps.state.db.clone(),
            deps.state.redis.clone(),
            deps.state.dynamic_config.clone(),
            deps.state.weight_cache.clone(),
            deps.request.mapped_model.clone(),
            priced(&extract_usage(&invoked.view)),
            &deps.preflight.limits,
            Some(&deps.route.caps),
            deps.request.identity.user_id.clone(),
            deps.request.identity.user_email.clone(),
            deps.request.identity.api_key_id.clone(),
            deps.request.identity.ip_address.clone(),
            deps.state.audit.clone(),
        )
        .await;
    }

    async fn emit_audit(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        let usage = extract_usage(&invoked.view);
        let (prompt_tokens, completion_tokens) = tokens(&usage);
        let (response_body, cost, logged_status, error_detail) = match &invoked.view {
            CapturedView::Streaming { outcome, captured } => {
                let (status, mut detail) = outcome.logged_status_and_detail();
                // The response went out as 200 before the stream failed;
                // `status` is what the failure would have been as a
                // response. Both are kept.
                if let (StreamOutcome::UpstreamError { .. }, Some(serde_json::Value::Object(d))) =
                    (outcome, detail.as_mut())
                {
                    d.insert("client_status".into(), 200.into());
                }
                (
                    captured.logged.as_deref().or(captured.assembled.as_deref()),
                    captured.cost_usd,
                    status,
                    detail,
                )
            }
            CapturedView::Buffered(ChatCompletionOutcome::Success(c)) => {
                let cost = deps
                    .state
                    .cost_tracker
                    .calculate_cost(&deps.request.mapped_model, &priced(&usage))
                    .await;
                (
                    Some(c.logged.as_deref().unwrap_or(c.body.as_slice())),
                    cost,
                    200_i64,
                    None,
                )
            }
            CapturedView::Buffered(ChatCompletionOutcome::ShortCircuit(e)) => (
                None,
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
            &deps.guards.redaction,
            &deps.state.blob_store,
            &deps.request.trace_id,
            &deps.request.request_for_audit,
            response_body,
        )
        .await;
        emit_gateway_log_with_extra(
            &deps.state.audit,
            &deps.request.trace_id,
            deps.request.session_id.as_deref(),
            deps.request.identity.user_id.as_deref(),
            deps.request.identity.user_email.as_deref(),
            deps.request.identity.api_key_id.as_deref(),
            deps.request.identity.api_key_lineage_id.as_deref(),
            deps.request.identity.ip_address.as_deref(),
            &deps.request.mapped_model,
            Some(deps.route.provider_name.as_str()),
            deps.route.upstream_model.as_deref(),
            prompt_tokens,
            completion_tokens,
            cost,
            deps.request.request_started_at.elapsed().as_millis() as i64,
            logged_status,
            with_usage_detail(
                retried_detail(
                    pinned_detail(error_detail, deps.route.pinned),
                    deps.route.sel_record.retries,
                ),
                &usage,
                usage_estimated(&invoked.view),
            ),
            body_capture,
        );
    }
}

/// The audit detail, with how the input splits over the prompt cache and
/// whether the count is an estimate — `input_tokens` on the row is the
/// whole input, and the cost depends on the split.
fn with_usage_detail(
    detail: Option<serde_json::Value>,
    usage: &tw_dialect::usage::Usage,
    estimated: bool,
) -> Option<serde_json::Value> {
    let mut extra = serde_json::Map::new();
    if usage.cache_read > 0 {
        extra.insert("cache_read_tokens".into(), usage.cache_read.into());
    }
    if usage.cache_write > 0 {
        extra.insert("cache_write_tokens".into(), usage.cache_write.into());
        if usage.cache_1h {
            extra.insert("cache_write_1h".into(), true.into());
        }
    }
    if estimated {
        extra.insert("usage_estimated".into(), true.into());
    }
    if extra.is_empty() {
        return detail;
    }
    if let Some(serde_json::Value::Object(d)) = detail {
        extra.extend(d);
    }
    Some(serde_json::Value::Object(extra))
}

/// The audit detail, with `"pinned_by": "encrypted_history"` when the
/// route was picked because it holds the conversation's encrypted history.
fn pinned_detail(detail: Option<serde_json::Value>, pinned: bool) -> Option<serde_json::Value> {
    if !pinned {
        return detail;
    }
    let mut d = match detail {
        Some(serde_json::Value::Object(d)) => d,
        _ => serde_json::Map::new(),
    };
    d.insert("pinned_by".into(), "encrypted_history".into());
    Some(serde_json::Value::Object(d))
}

/// The audit detail, with `"retries": n` when the request went out again
/// `n` times after a transient failure (see `proxy::retry`).
fn retried_detail(detail: Option<serde_json::Value>, retries: u32) -> Option<serde_json::Value> {
    if retries == 0 {
        return detail;
    }
    let mut d = match detail {
        Some(serde_json::Value::Object(d)) => d,
        _ => serde_json::Map::new(),
    };
    d.insert("retries".into(), retries.into());
    Some(serde_json::Value::Object(d))
}

/// The usage a captured view bills — shared by `record_usage` and
/// `emit_audit` so the budget and the audit row can never disagree.
fn extract_usage(view: &CapturedView<ChatCompletionSurface>) -> tw_dialect::usage::Usage {
    match view {
        CapturedView::Streaming { captured, .. } => captured.usage,
        CapturedView::Buffered(ChatCompletionOutcome::Success(c)) => c.usage,
        CapturedView::Buffered(ChatCompletionOutcome::ShortCircuit(_)) => {
            tw_dialect::usage::Usage::default()
        }
    }
}

fn usage_estimated(view: &CapturedView<ChatCompletionSurface>) -> bool {
    match view {
        CapturedView::Streaming { captured, .. } => captured.usage_estimated,
        CapturedView::Buffered(ChatCompletionOutcome::Success(c)) => c.usage_estimated,
        CapturedView::Buffered(ChatCompletionOutcome::ShortCircuit(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: &str, v: serde_json::Value) -> String {
        format!("event: {kind}\ndata: {v}\n\n")
    }

    /// What an upstream sends as soon as it takes the request is held;
    /// the first content lets it all through.
    #[test]
    fn a_streams_opening_is_held_until_its_first_content() {
        use serde_json::json;

        let start = frame(
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "m",
                   "usage": {"input_tokens": 3, "output_tokens": 1}}}),
        );
        let ping = frame("ping", json!({"type": "ping"}));
        let block = frame(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        );
        let mut o = Opening::new(Dialect::Anthropic);
        assert!(!o.feed(start.as_bytes()));
        assert!(!o.feed(ping.as_bytes()));
        assert!(o.feed(block.as_bytes()));
        assert_eq!(o.release(), [start, ping, block].concat().into_bytes());

        let mut o = Opening::new(Dialect::Responses);
        let created = json!({"response": {"id": "resp_1", "model": "m"}});
        assert!(!o.feed(frame("response.created", created.clone()).as_bytes()));
        assert!(!o.feed(frame("response.in_progress", created).as_bytes()));
        assert!(
            o.feed(
                frame(
                    "response.output_text.delta",
                    json!({"type": "response.output_text.delta", "output_index": 0,
                       "content_index": 0, "delta": "Hi"}),
                )
                .as_bytes()
            )
        );

        let chunk = |delta: serde_json::Value| {
            format!(
                "data: {}\n\n",
                json!({"id": "c", "model": "m", "choices": [{"index": 0, "delta": delta}]})
            )
        };
        let mut o = Opening::new(Dialect::Chat);
        assert!(!o.feed(chunk(json!({"role": "assistant", "content": ""})).as_bytes()));
        assert!(o.feed(chunk(json!({"content": "Hi"})).as_bytes()));
    }

    /// An error is not content: whether it goes out again is decided
    /// with the opening still held. Content ahead of it in the same piece
    /// of bytes began the answer.
    #[test]
    fn an_error_in_the_opening_is_not_content() {
        use serde_json::json;

        let start = frame(
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "m"}}),
        );
        let overload = frame(
            "error",
            json!({"type": "error",
                   "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        );
        let text = frame(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        );
        let mut o = Opening::new(Dialect::Anthropic);
        assert!(!o.feed([start.as_str(), overload.as_str()].concat().as_bytes()));

        let mut o = Opening::new(Dialect::Anthropic);
        assert!(o.feed([start, text, overload].concat().as_bytes()));
    }

    /// An opening that grows without any content is let through.
    #[test]
    fn an_opening_does_not_grow_without_end() {
        let mut o = Opening::new(Dialect::Chat);
        let comment = format!(": {}\n\n", "x".repeat(64 * 1024));
        let fed = (0..32)
            .map(|_| o.feed(comment.as_bytes()))
            .collect::<Vec<_>>();
        assert!(!fed[0]);
        assert!(fed.last().copied().unwrap_or(false));
    }

    /// Which errors an upstream reports in its stream are worth sending
    /// again for: an overload or a server error, not a refusal.
    #[test]
    fn an_overload_or_a_server_error_in_a_stream_is_transient() {
        let cases = [
            (
                Dialect::Anthropic,
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
                true,
            ),
            (
                Dialect::Anthropic,
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"Internal\"}}\n\n",
                true,
            ),
            (
                Dialect::Chat,
                "data: {\"error\":{\"message\":\"boom\",\"type\":\"server_error\"}}\n\n",
                true,
            ),
            (
                Dialect::Responses,
                "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"busy\"}}}\n\n",
                true,
            ),
            (
                Dialect::Gemini,
                "data: {\"error\":{\"code\":503,\"message\":\"busy\",\"status\":\"UNAVAILABLE\"}}\n\n",
                true,
            ),
            (
                Dialect::Anthropic,
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"too long\"}}\n\n",
                false,
            ),
            (
                Dialect::Anthropic,
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}\n\n",
                false,
            ),
            // Nothing recognisable: not sent again on a guess.
            (
                Dialect::Chat,
                "data: {\"error\":{\"message\":\"odd\",\"code\":\"upstream_wobble\"}}\n\n",
                false,
            ),
        ];
        for (dialect, frames, transient) in cases {
            let mut r = Readers {
                errors: Some(ErrorWatch::new(dialect)),
                ..Default::default()
            };
            r.feed(frames.as_bytes());
            let said = r.reported(false).unwrap_or_else(|| panic!("{frames}"));
            let StreamOutcome::UpstreamError {
                error_type,
                status_code,
                ..
            } = outcome_of("up", &said)
            else {
                panic!("not a failure");
            };
            assert_eq!(
                crate::proxy::retry::transient(&error_type, status_code),
                transient,
                "{frames}"
            );
            // Left in place for the tail.
            assert_eq!(r.upstream_error(), Some(said));
        }
    }

    /// A last error frame without a blank line after it is read once the
    /// stream ends.
    #[test]
    fn an_error_in_the_last_frame_is_reported_at_the_end() {
        let mut r = Readers {
            errors: Some(ErrorWatch::new(Dialect::Chat)),
            ..Default::default()
        };
        r.feed(b"data: {\"error\":{\"message\":\"boom\",\"type\":\"server_error\"}}");
        assert_eq!(r.reported(false), None);
        let said = r.reported(true).unwrap();
        assert_eq!(said.status, Some(500));
    }

    /// A Gemini client, SSE or JSON array, never gets a keep-alive comment.
    #[test]
    fn only_non_gemini_clients_get_keep_alives() {
        assert!(!keeps_alive(Dialect::Gemini));
        for client in [Dialect::Chat, Dialect::Anthropic, Dialect::Responses] {
            assert!(keeps_alive(client));
        }
    }

    /// A retry clears what the failed attempt left for the tail, and says
    /// the stream is waiting.
    #[test]
    fn deciding_a_retry_clears_the_failed_attempt() {
        let readers = Mutex::new(Readers {
            errors: Some(ErrorWatch::new(Dialect::Anthropic)),
            sniffer: Some(tw_dialect::usage::Sniffer::new()),
            ..Default::default()
        });
        readers.lock().unwrap().feed(
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"x\"}}\n\n",
        );
        let sent = Mutex::new(Sent {
            retries: 0,
            last_sent: std::time::Instant::now(),
            waiting: false,
        });
        resending(
            "up",
            1,
            std::time::Duration::from_secs(2),
            "x",
            &readers,
            &sent,
        );
        let mut r = readers.lock().unwrap();
        assert!(r.sniffer.is_none() && r.upstream_error().is_none());
        let s = *sent.lock().unwrap();
        assert_eq!((s.retries, s.waiting), (1, true));
    }

    /// While the gateway waits, the client hears a keep-alive every 15
    /// seconds of silence, and the wait ends on time.
    #[tokio::test(start_paused = true)]
    async fn a_wait_sends_a_keep_alive_every_fifteen_seconds() {
        let start = tokio::time::Instant::now();
        let quiet_since = start - std::time::Duration::from_secs(5);
        let until = start + std::time::Duration::from_secs(40);
        let ticks = keep_alives(quiet_since, until);
        let mut ticks = std::pin::pin!(ticks);
        let mut at = Vec::new();
        while ticks.next().await.is_some() {
            at.push((tokio::time::Instant::now() - start).as_secs());
        }
        assert_eq!(at, [10, 25]);
        assert_eq!((tokio::time::Instant::now() - start).as_secs(), 40);
    }

    #[test]
    fn the_last_frame_is_seen_in_the_chunk_that_completes_it() {
        let mut w = LastFrame::new(Dialect::Responses, true);
        // The model's text naming the event is not the event.
        assert!(!w.is_in(
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"response.completed\"}\n\n"
        ));
        assert!(!w.is_in(b"event: response.completed\ndata: {\"type\":\"response.com"));
        assert!(w.is_in(b"pleted\",\"response\":{\"status\":\"completed\"}}\n\n"));
        // Seen once.
        assert!(!w.is_in(b"event: response.completed\ndata: {}\n\n"));

        let mut w = LastFrame::new(Dialect::Anthropic, true);
        assert!(!w.is_in(b"event: message_delta\ndata: {\"type\":\"message_delta\"}\n\n"));
        assert!(w.is_in(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));

        let mut w = LastFrame::new(Dialect::Chat, true);
        assert!(!w.is_in(b"data: {\"choices\":[]}\n\n"));
        assert!(w.is_in(b"data: [DONE]\n\n"));
    }

    #[test]
    fn an_error_the_upstream_reports_in_its_stream_is_found_in_its_words() {
        let mut r = Readers {
            errors: Some(ErrorWatch::new(Dialect::Responses)),
            ..Default::default()
        };
        // The model's text naming an error is not one.
        r.feed(b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"an \\\"error\\\" here\"}\n\n");
        r.feed(b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",");
        assert!(r.said.is_none());
        r.feed(b"\"error\":{\"code\":\"server_error\",\"message\":\"boom\"}}}\n\n");
        assert_eq!(
            r.upstream_error().map(|s| s.message).as_deref(),
            Some("boom")
        );

        // An upstream whose last frame has no blank line after it.
        let mut r = Readers {
            errors: Some(ErrorWatch::new(Dialect::Anthropic)),
            ..Default::default()
        };
        r.feed(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}");
        assert_eq!(
            r.upstream_error().map(|s| s.message).as_deref(),
            Some("Overloaded")
        );

        // A stream without one.
        let mut r = Readers {
            errors: Some(ErrorWatch::new(Dialect::Chat)),
            ..Default::default()
        };
        r.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n");
        assert_eq!(r.upstream_error(), None);
    }

    /// What a stream whose upstream sent `frames` is recorded as, with
    /// whether it counts against the route: `None` when it carried no
    /// error.
    fn recorded(upstream: Dialect, frames: &str) -> Option<(String, i64, String, bool)> {
        let mut r = Readers {
            errors: Some(ErrorWatch::new(upstream)),
            ..Default::default()
        };
        r.feed(frames.as_bytes());
        let said = r.upstream_error()?;
        let StreamOutcome::UpstreamError {
            error_type,
            message,
            status_code,
        } = failed_partway("up", &said)
        else {
            panic!("not a failure");
        };
        let counted = crate::proxy::upstream_failed(&error_type, status_code);
        Some((error_type, status_code, message, counted))
    }

    fn responses_failed(error: serde_json::Value) -> String {
        let v = serde_json::json!({
            "type": "response.failed",
            "response": {"id": "resp_1", "status": "failed", "error": error},
        });
        format!("event: response.failed\ndata: {v}\n\n")
    }

    /// A refusal of the conversation's encrypted history in the stream —
    /// as `response.failed` or as the Codex backend's bare `error` event —
    /// is said plainly, keeps the upstream's words, and is a refusal, not
    /// the route failing.
    #[test]
    fn a_refusal_of_encrypted_history_in_a_stream_is_said_plainly() {
        let failed = responses_failed(serde_json::json!({
            "code": "invalid_encrypted_content",
            "message": "The encrypted content for item rs_68 could not be verified.",
        }));
        let bare = "event: error\ndata: {\"type\":\"error\",\"message\":\"Encrypted function output content could not be decrypted or decoded.\"}\n\n";
        for frames in [failed.as_str(), bare] {
            let mut r = Readers {
                errors: Some(ErrorWatch::new(Dialect::Responses)),
                ..Default::default()
            };
            r.feed(frames.as_bytes());
            assert!(r.sealed_refusal().is_some(), "{frames}");
            let (error_type, status, message, counted) =
                recorded(Dialect::Responses, frames).unwrap();
            assert_eq!(
                (error_type.as_str(), status),
                ("SealedHistoryRefused", 400),
                "{frames}"
            );
            assert!(!counted);
            assert!(
                message
                    .starts_with("Provider `up` cannot read this conversation's encrypted history"),
                "{message}"
            );
            assert!(message.contains("could not be"), "{message}");
        }

        // Another error is not one.
        let other = responses_failed(serde_json::json!({
            "code": "invalid_prompt", "message": "Invalid prompt",
        }));
        let mut r = Readers {
            errors: Some(ErrorWatch::new(Dialect::Responses)),
            ..Default::default()
        };
        r.feed(other.as_bytes());
        assert!(r.sealed_refusal().is_none());
    }

    #[test]
    fn a_pinned_route_is_said_on_the_log_row() {
        assert_eq!(pinned_detail(None, false), None);
        assert_eq!(
            pinned_detail(None, true),
            Some(serde_json::json!({"pinned_by": "encrypted_history"}))
        );
        assert_eq!(
            pinned_detail(Some(serde_json::json!({"error_type": "x"})), true),
            Some(serde_json::json!({"error_type": "x", "pinned_by": "encrypted_history"}))
        );
    }

    /// Throttling stays a 429, and counts against the route like a 429
    /// answer does: that upstream's quota, which another route does not
    /// share.
    #[test]
    fn a_rate_limit_in_a_stream_is_a_429() {
        let (error_type, status, message, counted) = recorded(
            Dialect::Responses,
            &responses_failed(serde_json::json!({
                "code": "rate_limit_exceeded",
                "message": "Rate limit reached for requests",
            })),
        )
        .unwrap();
        assert_eq!((error_type.as_str(), status), ("UpstreamRateLimited", 429));
        assert!(counted);
        assert!(
            message.contains("Rate limit reached for requests"),
            "{message}"
        );

        // Gemini names it by number and by status.
        let gemini = "data: {\"error\":{\"code\":429,\"message\":\"Resource has been exhausted\",\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n";
        assert_eq!(recorded(Dialect::Gemini, gemini).unwrap().1, 429);
        // An Anthropic one by its type.
        let anthropic = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}\n\n";
        assert_eq!(recorded(Dialect::Anthropic, anthropic).unwrap().1, 429);
    }

    /// A request the upstream refuses is the caller's, as when the
    /// upstream refuses it with an answer: it does not count against the
    /// route.
    #[test]
    fn a_refused_request_in_a_stream_does_not_count_against_the_route() {
        let (error_type, status, message, counted) = recorded(
            Dialect::Responses,
            &responses_failed(serde_json::json!({
                "code": "context_length_exceeded",
                "message": "Your input exceeds the context window of this model.",
            })),
        )
        .unwrap();
        assert_eq!((error_type.as_str(), status), ("ProviderHttpError", 400));
        assert!(!counted);
        assert!(message.contains("exceeds the context window"), "{message}");

        // A Chat chunk: the code says nothing known, the type does.
        let chat = "data: {\"error\":{\"message\":\"bad\",\"type\":\"invalid_request_error\",\"code\":\"weird_param\"}}\n\n";
        let (_, status, _, counted) = recorded(Dialect::Chat, chat).unwrap();
        assert_eq!((status, counted), (400, false));
        // A Gemini one by its number.
        let gemini = "data: {\"error\":{\"code\":404,\"message\":\"no such model\",\"status\":\"NOT_FOUND\"}}\n\n";
        let (_, status, _, counted) = recorded(Dialect::Gemini, gemini).unwrap();
        assert_eq!((status, counted), (404, false));
    }

    #[test]
    fn an_overload_in_a_stream_counts_against_the_route() {
        let frames = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let (error_type, status, message, counted) = recorded(Dialect::Anthropic, frames).unwrap();
        assert_eq!((error_type.as_str(), status), ("ProviderHttpError", 529));
        assert!(counted);
        assert!(
            message.contains("up reported an error partway through the answer: Overloaded"),
            "{message}"
        );
    }

    /// An error that names no status is the upstream's failure. Without a
    /// message of its own, the row says so in a fixed sentence — never
    /// the chunk's text, which can be the model's answer.
    #[test]
    fn an_error_that_names_no_status_is_the_upstreams_failure() {
        let chat = "data: {\"choices\":[{\"delta\":{\"content\":\"the secret plan\"}}],\"error\":{\"code\":\"upstream_wobble\"}}\n\n";
        let (error_type, status, message, counted) = recorded(Dialect::Chat, chat).unwrap();
        assert_eq!((error_type.as_str(), status), ("ProviderError", 502));
        assert!(counted);
        assert!(message.ends_with(NO_MESSAGE), "{message}");
        assert!(!message.contains("secret plan"), "{message}");

        // Nor the text of an Anthropic error event without a message.
        let anthropic = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\"},\"note\":\"the secret plan\"}\n\n";
        let (_, status, message, counted) = recorded(Dialect::Anthropic, anthropic).unwrap();
        assert_eq!((status, counted), (500, true));
        assert!(message.ends_with(NO_MESSAGE), "{message}");
        assert!(!message.contains("secret plan"), "{message}");

        // A `response.failed` without an error object is still a failure.
        let (_, status, message, _) = recorded(
            Dialect::Responses,
            &responses_failed(serde_json::Value::Null),
        )
        .unwrap();
        assert_eq!(status, 502);
        assert!(message.ends_with(NO_MESSAGE), "{message}");
    }

    /// An OpenAI-compatible relay can send `"error": null` in every chunk.
    #[test]
    fn a_null_error_is_no_error() {
        let chat = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"error\":null}\n\ndata: [DONE]\n\n";
        assert_eq!(recorded(Dialect::Chat, chat), None);
        let gemini = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}],\"error\":null}\n\n";
        assert_eq!(recorded(Dialect::Gemini, gemini), None);

        // And an error after such chunks is still seen: OpenAI's
        // `server_error` is its 500.
        let then = format!(
            "{chat}data: {{\"error\":{{\"message\":\"boom\",\"type\":\"server_error\"}}}}\n\n"
        );
        assert_eq!(recorded(Dialect::Chat, &then).unwrap().1, 500);
    }

    /// Cut to 500 characters, as a refusal's body is.
    #[test]
    fn what_the_upstream_said_is_cut() {
        let said = Said {
            message: "x".repeat(2_000),
            status: None,
            sealed: false,
        };
        let StreamOutcome::UpstreamError { message, .. } = failed_partway("anthropic-main", &said)
        else {
            panic!("not a failure");
        };
        assert!(
            message.contains("anthropic-main reported an error partway through the answer: xxx")
        );
        assert!(message.len() < 600, "{}", message.len());
    }

    /// A refused credential is logged without the upstream's words, as
    /// an answer that refuses it is.
    #[test]
    fn a_refused_credential_in_a_stream_keeps_its_words_out_of_the_row() {
        let said = Said {
            message: "arn:aws:iam::123456789012:user/gateway may not".into(),
            status: Some(403),
            sealed: false,
        };
        let StreamOutcome::UpstreamError {
            error_type,
            message,
            status_code,
        } = failed_partway("up", &said)
        else {
            panic!("not a failure");
        };
        assert_eq!(
            (error_type.as_str(), status_code),
            ("UpstreamAuthError", 401)
        );
        assert!(crate::proxy::upstream_failed(&error_type, status_code));
        assert!(!message.contains("arn:aws"), "{message}");
    }

    /// What a Bedrock stream that ended in `e` is recorded as, with
    /// whether it counts against the route, and what the client is told.
    fn bedrock_recorded(e: tw_bedrock::eventstream::StreamError) -> (String, i64, bool, String) {
        let (told, outcome) = bedrock_ended("bedrock-main", e);
        let StreamOutcome::UpstreamError {
            error_type,
            status_code,
            ..
        } = outcome
        else {
            panic!("not a failure");
        };
        let counted = crate::proxy::upstream_failed(&error_type, status_code);
        (error_type, status_code, counted, told)
    }

    fn exception(kind: &str, message: &str) -> tw_bedrock::eventstream::StreamError {
        tw_bedrock::eventstream::StreamError::Upstream {
            kind: kind.into(),
            message: message.into(),
        }
    }

    /// An exception Bedrock reports in its stream is recorded as the same
    /// exception as an answer: throttling stays a 429 and counts like one,
    /// a request Bedrock refuses does not count against the route.
    #[test]
    fn a_bedrock_exception_in_the_stream_is_classified_by_its_name() {
        let (error_type, status, counted, told) =
            bedrock_recorded(exception("throttlingException", "Too many requests"));
        assert_eq!(
            (error_type.as_str(), status, counted),
            ("UpstreamRateLimited", 429, true)
        );
        // The client is told what it was told before.
        assert_eq!(
            told,
            "Bedrock ended the stream: throttlingException: Too many requests"
        );

        let (error_type, status, counted, _) =
            bedrock_recorded(exception("validationException", "Malformed input request"));
        assert_eq!(
            (error_type.as_str(), status, counted),
            ("ProviderHttpError", 400, false)
        );

        for (kind, expected) in [
            ("internalServerException", 500),
            ("modelStreamErrorException", 500),
            ("serviceUnavailableException", 503),
            ("modelTimeoutException", 504),
            ("ThrottlingException", 429),
        ] {
            let (_, status, counted, _) = bedrock_recorded(exception(kind, "x"));
            assert_eq!((status, counted), (expected, true), "{kind}");
        }

        // Access denied keeps the IAM principal out of the row, as a 403
        // answer does.
        let (_, outcome) = bedrock_ended(
            "bedrock-main",
            exception(
                "accessDeniedException",
                "User: arn:aws:iam::123456789012:user/gateway is not authorized",
            ),
        );
        let StreamOutcome::UpstreamError {
            error_type,
            message,
            status_code,
        } = outcome
        else {
            panic!("not a failure");
        };
        assert_eq!(
            (error_type.as_str(), status_code),
            ("UpstreamAuthError", 401)
        );
        assert!(!message.contains("arn:aws"), "{message}");
    }

    /// A damaged frame broke the stream off: no usable answer arrived.
    #[test]
    fn a_damaged_bedrock_frame_broke_the_stream_off() {
        let (error_type, status, counted, told) = bedrock_recorded(
            tw_bedrock::eventstream::StreamError::Malformed("bad CRC".into()),
        );
        assert_eq!(
            (error_type.as_str(), status, counted),
            ("transport", 502, true)
        );
        assert!(told.starts_with("Bedrock ended the stream: "), "{told}");
    }

    /// A Gemini answer ends with its stream, whichever form the caller
    /// reads it in.
    #[test]
    fn a_gemini_answer_has_no_last_frame() {
        let last = b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n";
        for sse in [true, false] {
            assert!(!LastFrame::new(Dialect::Gemini, sse).is_in(last));
        }
    }

    /// An answer rule whose text is also in a placeholder's label: run on
    /// the placeholder form it would cut `<<TW_EMAIL_1>>` apart.
    #[test]
    fn the_log_gets_the_restored_answer_filtered_and_masked_again() {
        use tw_guard::policy::{
            ContentAction, ContentMatch, ContentPolicy, ContentScope, CustomContentRule, Mode,
            RedactPolicy,
        };
        let redaction = crate::redaction::Redaction::new(&RedactPolicy {
            mode: Mode::Enforce,
            enable: vec!["email".into()],
            ..Default::default()
        });
        let request = serde_json::json!({"messages": [
            {"role": "user", "content": "my email is alice@corp.example.org"}
        ]})
        .to_string();
        let (_, ledger) = redaction.look(
            request.as_bytes(),
            crate::redaction::Shape::Generate(Dialect::Chat),
        );
        let content = crate::content_filter::ContentFilter::new(&ContentPolicy {
            mode: Mode::Enforce,
            custom: vec![CustomContentRule {
                name: "Word".into(),
                pattern: "email".into(),
                scope: vec![ContentScope::Response],
                matching: ContentMatch::Contains,
                case_sensitive: false,
                action: ContentAction::Strip,
                replacement: "***".into(),
                disabled: false,
            }],
            ..Default::default()
        });
        let upstream = serde_json::json!({"choices": [{"index": 0, "message": {
            "role": "assistant", "content": "my email is <<TW_EMAIL_1>>"}}]})
        .to_string();
        let logged = for_the_log(
            &content,
            &redaction,
            &ledger,
            Dialect::Chat,
            upstream.as_bytes(),
        );
        let v: serde_json::Value = serde_json::from_slice(&logged).unwrap();
        assert_eq!(
            v["choices"][0]["message"]["content"], "my  is <<TW_EMAIL_1>>",
            "the rule ran on the address, the log keeps its placeholder"
        );
    }
}
