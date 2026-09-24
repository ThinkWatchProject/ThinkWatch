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
//! receives them (before PII is painted back), and the usage read off
//! the upstream's own bytes.
//!
//! Hook responsibilities:
//! - `record_outcome` → `finalize_health` (breaker).
//! - `write_cache` → `cache.set` (gated by `cache_enabled` AND success).
//! - `record_usage` → `post_flight_account` (limits + budget debit).
//! - `emit_audit` → `prepare_body_capture` + `emit_gateway_log_with_extra`.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::error::GatewayError;
use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, header};
use futures::StreamExt;
use rust_decimal::Decimal;
use think_watch_common::audit::{AuditActor, AuditEntry, GatewayActor};
use think_watch_common::lifecycle::Surface;
use think_watch_common::lifecycle::state::{CapturedView, Invoked, LimitCheckRecord};
use think_watch_common::limits::{BudgetCap, RateLimitRule};
use tw_dialect::ir::Dialect;

use crate::pii_redactor::PiiRedactor;
use crate::proxy::generate::{Wire, priced, tokens};
use crate::proxy::shaper::{StreamShaper, rewrite_model};
use crate::proxy::{
    GatewayRequestIdentity, GatewayState, SelectionRecord, emit_gateway_log_with_extra,
    finalize_health, post_flight_account, prepare_body_capture,
};

pub use think_watch_common::lifecycle::streaming::StreamOutcome;

/// Surface marker for the generation endpoints. Crate-private so
/// `ChatPostInvokeDeps` and `SelectionRecord` stay crate-private without
/// leaking through the `Surface` trait's associated-type visibility check.
pub(crate) struct ChatCompletionSurface;

/// A whole answer, in the caller's format.
pub struct Completed {
    /// As the caller will receive it, except that PII placeholders are
    /// still in place — this is also the form the cache stores, so a
    /// later caller can paint in their own values.
    pub body: Vec<u8>,
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
    /// The stream assembled into a whole answer, for the cache and the
    /// audit row. `None` when it produced nothing or could not be
    /// assembled.
    pub assembled: Option<Vec<u8>>,
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
    /// The request body exactly as the caller sent it, before redaction:
    /// the audit row is the record of what the user wrote. Body capture
    /// applies its own redaction toggle on top.
    pub request_for_audit: Vec<u8>,
    /// Where the cache keeps this request's answer. `None` when the
    /// request must not be cached.
    pub cache_fingerprint: Option<Vec<u8>>,
    pub request_started_at: std::time::Instant,
    /// The request's input in tokens, estimated — billed only when the
    /// upstream reports no usage.
    pub input_estimate: u64,
}

/// Pre-flight rule + cap lists, reused by the post-flight debit.
pub(crate) struct ChatPreflightLists {
    pub request_rules: Vec<RateLimitRule>,
    pub budget_caps: Vec<BudgetCap>,
}

/// The route that actually served the request.
pub(crate) struct ChatPickedRoute {
    pub provider_name: String,
    /// Upstream-side model id when the route remapped it.
    pub upstream_model: Option<String>,
    /// Used by `finalize_health` inside `record_outcome`.
    pub sel_record: SelectionRecord,
}

/// Everything the post-invoke hooks read. Built once before the upstream
/// call; consumed in the foreground for a whole answer, inside the
/// detached tail task for a stream.
pub(crate) struct ChatPostInvokeDeps {
    pub state: GatewayState,
    /// Snapshot of the redactor, so body capture sees the same patterns
    /// the request was redacted with even across a hot swap.
    pub pii_redactor: Arc<PiiRedactor>,
    pub request: ChatRequestSnapshot,
    pub preflight: ChatPreflightLists,
    pub route: ChatPickedRoute,
    /// Only chat completions caches.
    pub cache_enabled: bool,
}

/// The upstream call a stream makes, not yet started.
pub(crate) type OpenUpstream = Pin<
    Box<dyn std::future::Future<Output = Result<(reqwest::Response, Wire), GatewayError>> + Send>,
>;

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
/// Nothing is buffered. Usage is sniffed and the whole answer assembled
/// alongside the bytes, not by holding them back.
///
/// **A dropped stream is a cancelled request.** When the client goes,
/// hyper drops the body, and with it the sender the tail is waiting on;
/// the tail then records `ClientCancelled`.
pub(crate) fn build_chat_pump(
    open: OpenUpstream,
    mut shaper: StreamShaper,
    client: Dialect,
    deps_state: GatewayState,
    request: &ChatRequestSnapshot,
    provider: &str,
) -> (
    axum::response::Response,
    Pin<Box<dyn std::future::Future<Output = Invoked<ChatCompletionSurface>> + Send>>,
) {
    struct Readers {
        sniffer: Option<tw_dialect::usage::Sniffer>,
        collector: Option<tw_dialect::convert::Collector>,
    }
    let readers = Arc::new(Mutex::new(Readers {
        sniffer: None,
        collector: None,
    }));
    let readers_for_tail = Arc::clone(&readers);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<StreamOutcome>();

    // Tool calls are inspected on what the client is about to receive —
    // converted, if it was — since that is what it would execute.
    let mut inspector = crate::tool_inspection::StreamInspector::new(
        deps_state.tool_inspection.load_full(),
        deps_state.audit.clone(),
        crate::tool_inspection::Caller::of(
            &request.identity,
            &request.trace_id,
            &request.mapped_model,
        ),
        provider.to_string(),
    );

    let body = async_stream::stream! {
        let mut done_tx = Some(done_tx);

        let (upstream, wire) = match open.await {
            Ok(opened) => opened,
            Err(e) => {
                // Headers already went out as 200, so the refusal is said
                // in the stream — and logged with the upstream's own status,
                // so a throttled upstream stays 429 on the audit row.
                let mut out = shaper.process(&error_frame(client, e.status_code(), &e.to_string()));
                out.extend(shaper.finish());
                yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(out));
                if let Some(tx) = done_tx.take() {
                    let _ = tx.send(StreamOutcome::UpstreamError {
                        error_type: e.error_tag().to_string(),
                        message: e.to_string(),
                        status_code: e.status_code(),
                    });
                }
                return;
            }
        };
        if let Ok(mut r) = readers.lock() {
            r.sniffer = Some(tw_dialect::usage::Sniffer::new());
            r.collector = Some(wire.collect.collector());
        }
        let mut convert = wire.convert.as_ref().map(|s| s.stream());
        // Bedrock streams AWS eventstream frames, not SSE. Unframe them at
        // the door, so the sniffer, the collector and the converter all
        // read the same SSE they read from every other upstream.
        let mut unframe = (wire.dialect == Dialect::Bedrock)
            .then(crate::bedrock::eventstream::Transcoder::new);
        let mut source = upstream.bytes_stream();
        while let Some(item) = source.next().await {
            let item = match item {
                Ok(raw) => match unframe.as_mut() {
                    None => Ok(raw),
                    Some(t) => t
                        .feed(&raw)
                        .map(Bytes::from)
                        .map_err(|e| format!("Bedrock ended the stream: {e}")),
                },
                Err(e) => Err(format!("The upstream stream broke off: {e}")),
            };
            match item {
                Ok(chunk) => {
                    if let Ok(mut r) = readers.lock() {
                        if let Some(s) = r.sniffer.as_mut() { s.feed(&chunk); }
                        if let Some(c) = r.collector.as_mut() { c.process(&chunk); }
                    }
                    let client_bytes = match convert.as_mut() {
                        Some(c) => c.process(&chunk),
                        None => chunk.to_vec(),
                    };
                    if let Some((err, safe)) = inspector.as_mut().and_then(|i| i.check(&client_bytes)) {
                        yield Ok(Bytes::from(cut(&mut shaper, convert.as_mut(), client, &client_bytes[..safe], &err)));
                        if let Some(tx) = done_tx.take() {
                            let _ = tx.send(StreamOutcome::UpstreamError {
                                error_type: err.error_tag().to_string(),
                                message: err.to_string(),
                                status_code: err.status_code(),
                            });
                        }
                        return;
                    }
                    let out = shaper.process(&client_bytes);
                    if !out.is_empty() {
                        yield Ok(Bytes::from(out));
                    }
                }
                Err(message) => {
                    // Headers are gone; the only way left to say it is in
                    // the stream, in the caller's own format.
                    tracing::warn!("{message}");
                    let tail = match convert.as_mut() {
                        Some(c) => c.fail(&message),
                        None => error_frame(client, 502, &message),
                    };
                    let mut out = shaper.process(&tail);
                    out.extend(shaper.finish());
                    yield Ok(Bytes::from(out));
                    if let Some(tx) = done_tx.take() {
                        let _ = tx.send(StreamOutcome::UpstreamError {
                            error_type: "transport".into(),
                            message,
                            status_code: 502,
                        });
                    }
                    return;
                }
            }
        }
        let tail = convert.as_mut().map(|c| c.finish()).unwrap_or_default();
        // The converter's last bytes can complete a tool call (the block's
        // stop), so they are inspected too.
        if let Some((err, safe)) = inspector.as_mut().and_then(|i| i.check(&tail)) {
            yield Ok(Bytes::from(cut(&mut shaper, None, client, &tail[..safe], &err)));
            if let Some(tx) = done_tx.take() {
                let _ = tx.send(StreamOutcome::UpstreamError {
                    error_type: err.error_tag().to_string(),
                    message: err.to_string(),
                    status_code: err.status_code(),
                });
            }
            return;
        }
        let mut out = shaper.process(&tail);
        out.extend(shaper.finish());
        if !out.is_empty() {
            yield Ok(Bytes::from(out));
        }
        if let Some(tx) = done_tx.take() {
            let _ = tx.send(StreamOutcome::Natural);
        }
    };

    let mut response = axum::response::Response::new(Body::from_stream(body));
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));

    let identity = request.identity.clone();
    let trace_id = request.trace_id.clone();
    let started_at = request.request_started_at;
    let mapped_model = request.mapped_model.clone();
    let input_estimate = request.input_estimate;

    let tail = Box::pin(async move {
        let outcome = done_rx.await.unwrap_or(StreamOutcome::ClientCancelled);
        metrics::counter!(
            "gateway_stream_completion_total",
            "outcome" => outcome.metric_label()
        )
        .increment(1);

        // The sniffer exists once the upstream answered. Without an
        // answer there is nothing to bill.
        let (answered, reported, assembled) = match readers_for_tail.lock() {
            Ok(mut r) => {
                let sniffer = r.sniffer.take();
                (
                    sniffer.is_some(),
                    sniffer.and_then(|s| s.finish()),
                    r.collector.take().and_then(|c| c.finish().ok()),
                )
            }
            Err(_) => (false, None, None),
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
        let captured = ChatStreamCaptured {
            usage,
            usage_estimated,
            cost_usd,
            // A cache hit hands this back to a caller, so it carries the
            // caller's model name like everything else they receive.
            assembled: assembled.map(|b| rewrite_model(&b, &mapped_model)),
        };
        Invoked {
            client_ip: identity.ip_address.clone(),
            identity,
            trace_id,
            started_at,
            limit_check: LimitCheckRecord {
                currents: Vec::new(),
            },
            access_candidate: mapped_model,
            view: CapturedView::Streaming { outcome, captured },
        }
    });
    (response, tail)
}

/// End a stream at a tool call the inspection stops: what came before it
/// still goes out, then the refusal, in the caller's format.
///
/// An incomplete tool call cannot be executed, so the client is left with
/// nothing it can run.
fn cut(
    shaper: &mut StreamShaper,
    convert: Option<&mut tw_dialect::convert::StreamConverter>,
    client: Dialect,
    safe: &[u8],
    err: &crate::error::GatewayError,
) -> Vec<u8> {
    let message = err.to_string();
    let mut out = shaper.process(safe);
    let refusal = match convert {
        Some(c) => c.fail(&message),
        None => error_frame(client, err.status_code(), &message),
    };
    out.extend(shaper.process(&refusal));
    out.extend(shaper.finish());
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
            deps.preflight.request_rules.clone(),
            deps.preflight.budget_caps.clone(),
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
                let (status, detail) = outcome.logged_status_and_detail();
                (
                    captured.assembled.as_deref(),
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
                (Some(c.body.as_slice()), cost, 200_i64, None)
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
            &deps.pii_redactor,
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
            with_usage_detail(error_detail, &usage, usage_estimated(&invoked.view)),
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
