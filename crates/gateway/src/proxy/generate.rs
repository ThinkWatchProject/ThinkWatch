//! The four generation surfaces — `/v1/chat/completions`, `/v1/messages`,
//! `/v1/responses` and Gemini's `/v1beta/models/{model}:generateContent`
//! (`:streamGenerateContent`) — as one pipeline.
//!
//! # Forward what can be forwarded, convert what must be
//!
//! A request that reaches an upstream speaking its own format goes out
//! **as the caller sent it**: only the model name changes, the model's
//! output cap is applied, and in enforce mode redacted values are swapped
//! for placeholders. That is not a shortcut. The intermediate
//! representation the conversion layer uses has no place for Anthropic's
//! `cache_control` breakpoints, server-side tools, or `metadata`, and a
//! same-format request rebuilt through it loses all three — the prompt
//! cache turns back into full-price input on every turn.
//!
//! Only a request crossing formats is decoded and re-encoded, and there
//! the conversion layer reports what it had no way to carry.
//!
//! The previous design rebuilt every request as a chat-shaped DTO. It
//! read an Anthropic `system` with `as_str()`, which is `None` for the
//! array form Claude Code sends, so its whole system prompt was dropped —
//! along with every tool and every cache breakpoint.
//!
//! # The guards, on the bytes the caller sent
//!
//! The content filter reads the caller's text where the request's own
//! format puts it, and a rule that strips text strips it there: the
//! request goes on as the stripped one, decoded again. Outbound redaction
//! then searches the whole request, numbers what it finds once, and every
//! hop — forwarded or converted — goes out through it (see
//! `crate::redaction`). Both are thinkwatch-core's, shared with the
//! desktop gateway.

use std::convert::Infallible;

use crate::call_ctx::CallCtx;
use crate::error::GatewayError;
use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::IntoResponse;
use rust_decimal::Decimal;
use serde_json::Value;
use tw_dialect::convert::Session;
use tw_dialect::ir::{Dialect, Target};

use super::body_capture::prepare_body_capture;
use super::early_cancel::EarlyCancelSlot;
use super::headers::{request_id_header, resolve_session_id, resolve_trace_id};
use super::log_ctx::{LogCtx, emit_gateway_error_log, emit_gateway_log};
use super::pipeline::{launch_stream_pump, run_buffered_post_invoke, run_preflight_stages};
use super::routing::{
    build_selection_ctx, finalize_health, select_route_for_stream, select_route_with_failover,
    set_affinity,
};
use super::shaper::{StreamShaper, rewrite_model};
use super::{GatewayErrorResponse, GatewayRequestIdentity, GatewayState};

use crate::cache::ResponseCache;
use crate::guards::Caller;
use crate::lifecycle::Completed;
use crate::metadata::RequestMetadata;
use crate::protocol::UpstreamProtocol;
use crate::redaction::Redaction;
use crate::router::RouteEntry;
use tw_guard::redact::replace::Ledger;

use think_watch_common::audit::BodyCaptureStatus;
use think_watch_common::limits::weight::TokenCounts;

/// What a client-facing endpoint speaks.
#[derive(Clone, Copy)]
pub(crate) struct ClientSurface {
    pub dialect: Dialect,
    /// Only chat completions caches, as before. The other formats never
    /// did, and turning it on for them is its own decision.
    pub caches: bool,
}

const CHAT: ClientSurface = ClientSurface {
    dialect: Dialect::Chat,
    caches: true,
};
const MESSAGES: ClientSurface = ClientSurface {
    dialect: Dialect::Anthropic,
    caches: false,
};
pub(crate) const RESPONSES: ClientSurface = ClientSurface {
    dialect: Dialect::Responses,
    caches: false,
};
const GEMINI: ClientSurface = ClientSurface {
    dialect: Dialect::Gemini,
    caches: false,
};

/// The query a Gemini request is read with inside the gateway.
///
/// **Inside, a Gemini stream is always SSE**, whatever the caller asked
/// for: upstreams are asked for `alt=sse`, and the shaper, the tool-call
/// inspection, the usage sniffer and the error frames all read and write
/// SSE. A caller that asked for Gemini's other stream form — one JSON
/// array, an element per chunk — gets the SSE reframed as that on the
/// way out (`shaper::JsonArrayFramer`).
const GEMINI_SSE: &str = "alt=sse";

/// POST /v1/chat/completions
pub async fn proxy_chat_completion(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    cancel: Option<axum::Extension<EarlyCancelSlot>>,
    body: Bytes,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    generate(
        state,
        headers,
        identity,
        cancel.map(|c| c.0),
        body,
        CHAT,
        "/v1/chat/completions",
        None,
    )
    .await
}

/// POST /v1/messages
pub async fn proxy_anthropic_messages(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    cancel: Option<axum::Extension<EarlyCancelSlot>>,
    body: Bytes,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    generate(
        state,
        headers,
        identity,
        cancel.map(|c| c.0),
        body,
        MESSAGES,
        "/v1/messages",
        None,
    )
    .await
}

/// POST /v1/responses
pub async fn proxy_responses(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    cancel: Option<axum::Extension<EarlyCancelSlot>>,
    body: Bytes,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    generate(
        state,
        headers,
        identity,
        cancel.map(|c| c.0),
        body,
        RESPONSES,
        "/v1/responses",
        None,
    )
    .await
}

/// POST /v1beta/models/{model}:generateContent, and `:streamGenerateContent`
/// for a stream. `/v1/models/…` too: some Gemini clients use that version.
///
/// The model and whether to stream are in the path, not the body.
pub async fn proxy_gemini(
    State(state): State<GatewayState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    cancel: Option<axum::Extension<EarlyCancelSlot>>,
    body: Bytes,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    generate(
        state,
        headers,
        identity,
        cancel.map(|c| c.0),
        body,
        GEMINI,
        uri.path(),
        uri.query(),
    )
    .await
}

/// `/v1beta/models/gemini-2.5-pro:streamGenerateContent` → the model, and
/// whether it is a stream. Only the two generation actions; anything else
/// (`:countTokens`, `:embedContent`) has no counterpart in another format.
fn gemini_target(path: &str) -> Option<(String, bool)> {
    let (_, rest) = path.split_once("/models/")?;
    let (model, action) = rest.rsplit_once(':')?;
    let stream = match action {
        "streamGenerateContent" => true,
        "generateContent" => false,
        _ => return None,
    };
    (!model.is_empty()).then(|| (model.to_string(), stream))
}

// ───────────────────────────────────────────── addressing one upstream

/// The caller's request after inspection, ready to be addressed to any
/// route.
pub(crate) struct Outbound {
    pub surface: ClientSurface,
    /// The path the caller called. A Gemini request's model and action
    /// are in it.
    pub path: String,
    /// What the caller sent — after the content filter and, in enforce
    /// mode, with redacted values swapped for placeholders — otherwise
    /// exactly as sent.
    pub body: Value,
    pub stream: bool,
    /// The caller's headers that belong to its format — `anthropic-beta`
    /// and `anthropic-version`. They travel with a request forwarded as
    /// sent: its body can use a beta feature, and without the header that
    /// turns it on the upstream refuses a request that used to work. A
    /// converted request leaves them behind; they mean nothing in
    /// another format.
    pub dialect_headers: Vec<(String, String)>,
    /// The request's input in tokens, estimated — billed only when the
    /// upstream does not report its own (see `crate::usage_estimate`).
    pub input_estimate: u64,
    /// Outbound redaction for this request: every hop goes out through it.
    pub redaction: Redaction,
    /// The placeholders the caller's request was numbered with.
    pub ledger: Ledger,
    /// The model's output cap (`models.max_output_tokens`), applied to each
    /// hop as it is addressed (see [`Outbound::cap_output`]).
    pub max_output_tokens: Option<u32>,
}

/// The request as it goes out to one upstream, and what it takes to read
/// the answer back.
pub(crate) struct Wire {
    pub body: Vec<u8>,
    pub path: String,
    pub query: Option<String>,
    pub dialect: Dialect,
    /// Headers that go with this particular request (see
    /// [`Outbound::dialect_headers`]).
    pub headers: Vec<(String, String)>,
    /// Converts the upstream's answer to the caller's format. `None` when
    /// the request went out in the caller's own format.
    pub convert: Option<Session>,
    /// Assembles a streamed answer into a whole one, for the cache and
    /// the audit row. Always present: a same-format stream still needs
    /// assembling.
    pub collect: Session,
    /// Restores this hop's answer: the request's ledger, and any value
    /// only this hop carried.
    pub ledger: Ledger,
    /// The body's prompt-cache breakpoints are ones the conversion added:
    /// the caller marked none. An upstream that refuses them gets the
    /// request again without them (see `proxy::cache_marks`).
    pub auto_cache: bool,
}

impl Wire {
    /// Take out the cache breakpoints the conversion added. False when
    /// there were none to take out.
    fn drop_added_marks(&mut self) -> bool {
        if !std::mem::take(&mut self.auto_cache) {
            return false;
        }
        match tw_dialect::cache::strip_marks(self.dialect, &self.body) {
            Some(body) => {
                self.body = body;
                true
            }
            None => false,
        }
    }

    async fn send_to(
        &self,
        upstream: &super::transport::Upstream,
        call_ctx: &CallCtx,
    ) -> Result<reqwest::Response, GatewayError> {
        upstream
            .send(
                self.body.clone(),
                &self.path,
                self.query.as_deref(),
                self.dialect,
                &self.headers,
                call_ctx,
            )
            .await
    }

    /// This hop as `upstream` takes it: without the added breakpoints
    /// when it refused them for `model` before.
    fn for_upstream(mut self, upstream: &super::transport::Upstream, model: &str) -> Wire {
        if self.auto_cache && upstream.cache_marks.refused(model) {
            self.drop_added_marks();
        }
        self
    }
}

impl Outbound {
    /// A Chat stream whose caller did not ask for the usage chunk. The
    /// request goes out asking for it anyway — without it the upstream
    /// reports no usage and the request is billed as zero — and the
    /// shaper takes it back out of what the caller receives.
    pub(crate) fn hides_usage(&self) -> bool {
        self.surface.dialect == Dialect::Chat
            && self.stream
            && self.body.pointer("/stream_options/include_usage") != Some(&Value::Bool(true))
    }

    /// The model's output cap, on what one hop sends: `body`, in `dialect`,
    /// to `model`.
    ///
    /// A limit the request carries (the caller's, or the one a conversion
    /// to Anthropic writes) is lowered to the cap. One it does not carry is
    /// filled in only when the cap is no more than what the gateway knows
    /// the model's family to take (`fallback_max_output_tokens`: 32,000 for
    /// Claude, 8,192 otherwise). Above that, a filled-in limit could be one
    /// the model refuses outright, and the model's own limit applies
    /// instead.
    ///
    /// `official` is whether the hop goes to the vendor's own endpoint, the
    /// same flag the conversion gets: a Chat request with no limit is given
    /// `max_completion_tokens` there (OpenAI's reasoning models refuse
    /// `max_tokens`) and `max_tokens` elsewhere, where compatible servers
    /// mostly read only that.
    fn cap_output(&self, dialect: Dialect, body: &mut Value, model: &str, official: bool) {
        let Some(cap) = self.max_output_tokens.map(u64::from) else {
            return;
        };
        if tw_dialect::params::max_output_tokens(dialect, body).is_none()
            && cap > tw_dialect::official::fallback_max_output_tokens(model)
        {
            return;
        }
        tw_dialect::params::cap_max_output_tokens(dialect, body, cap, official);
    }

    /// What assembles an answer forwarded in the caller's own format, for
    /// the cache and the audit row.
    ///
    /// It is made from the request alone. Decoding also notes how the
    /// caller wants a converted answer written — for a Codex compaction,
    /// as one item carrying the summary the upstream wrote — and a
    /// forwarded answer is the upstream's own: assembled that way, OpenAI's
    /// compaction would be recorded as a failed one. A request the
    /// conversion layer cannot read at all can still be forwarded, and its
    /// answer is assembled knowing only the model and whether it streams.
    ///
    /// A Chat upstream can write a tool call into its answer text instead
    /// of `tool_calls`. A converted answer turns such a call, when the
    /// request defined the tool, into a structured one (see
    /// `tw_dialect::chat::text_calls`); a forwarded answer reaches the
    /// caller as the upstream wrote it, text included, and is assembled as
    /// that (`Session::keep_text_calls`).
    fn forwarded_session(&self, body: &Value, model: &str, target: &Target) -> Session {
        let client = self.surface.dialect;
        let request =
            match tw_dialect::convert::decode(client, body, &self.path, internal_query(client)) {
                Ok(decoded) => decoded.request,
                Err(_) => tw_dialect::ir::Request {
                    model: model.to_string(),
                    stream: self.stream,
                    ..Default::default()
                },
            };
        tw_dialect::convert::encode(&request, target)
            .session
            .keep_text_calls()
    }

    /// Address the request to `protocol`, naming `model` upstream.
    pub(crate) fn address(
        &self,
        protocol: UpstreamProtocol,
        model: &str,
        official: bool,
    ) -> Result<Wire, GatewayError> {
        let client = self.surface.dialect;
        // Output length when the caller set none and the upstream insists
        // on one (Anthropic). There is no per-model output limit on file
        // here, so it goes by the upstream model's name.
        let default_max_tokens = tw_dialect::official::fallback_max_output_tokens(model);
        let target = |dialect| Target {
            dialect,
            official,
            default_max_tokens,
        };
        let decode = |v: &Value| {
            tw_dialect::convert::decode(client, v, &self.path, internal_query(client))
                .map_err(|r| GatewayError::TransformError(r.0))
        };

        if protocol.dialect() == client {
            // Forwarded as sent. Only the model changes, and a Chat
            // stream always asks for its usage (see `hides_usage`).
            let mut body = self.body.clone();
            let (path, query) = if client == Dialect::Gemini {
                // Gemini names the model in the path, and is always asked
                // for SSE (see `GEMINI_SSE`).
                let action = if self.stream {
                    "streamGenerateContent"
                } else {
                    "generateContent"
                };
                let model = model.strip_prefix("models/").unwrap_or(model);
                (
                    format!("/v1beta/models/{model}:{action}"),
                    self.stream.then(|| GEMINI_SSE.to_string()),
                )
            } else {
                (self.path.clone(), None)
            };
            if client != Dialect::Gemini
                && let Some(obj) = body.as_object_mut()
            {
                obj.insert("model".into(), Value::String(model.to_string()));
                if self.hides_usage() {
                    let opts = obj
                        .entry("stream_options")
                        .or_insert_with(|| Value::Object(Default::default()));
                    if !opts.is_object() {
                        *opts = Value::Object(Default::default());
                    }
                    opts["include_usage"] = Value::Bool(true);
                }
            }
            self.cap_output(client, &mut body, model, official);
            let collect = self.forwarded_session(&body, model, &target(client));
            let mut bytes = serde_json::to_vec(&body).unwrap_or_default();
            // Reasoning signatures a conversion wrote earlier in this
            // conversation (`tw1.`-prefixed) were not issued by this
            // upstream, and Anthropic refuses the whole request over
            // them. That reasoning did not come from here anyway.
            if let Some(stripped) = tw_dialect::convert::strip_carried(client, &bytes) {
                bytes = stripped;
            }
            let (bytes, ledger) = self.redaction.replace(bytes, &self.ledger);
            return Ok(Wire {
                body: bytes,
                path,
                query,
                dialect: client,
                headers: self.dialect_headers.clone(),
                convert: None,
                collect,
                ledger,
                auto_cache: false,
            });
        }

        let mut decoded = decode(&self.body)?;
        decoded.request.model = model.to_string();
        let prepared = decoded.encode(&target(protocol.dialect()));
        if !prepared.dropped.is_empty() {
            tracing::info!(
                from = client.slug(),
                to = protocol.dialect().slug(),
                dropped = ?prepared.dropped,
                "Fields the upstream's format cannot carry were left out"
            );
        }
        let mut body = prepared.body;
        if self.max_output_tokens.is_some()
            && let Ok(mut v) = serde_json::from_slice::<Value>(&body)
        {
            self.cap_output(protocol.dialect(), &mut v, model, official);
            body = serde_json::to_vec(&v).unwrap_or(body);
        }
        // The conversion moved the placeholders along with the text; one it
        // assembled from two pieces is numbered here.
        let (body, ledger) = self.redaction.replace(body, &self.ledger);
        // With none of the caller's own, any breakpoints are the ones the
        // conversion added for Claude.
        let auto_cache = decoded.request.cache.is_empty()
            && tw_dialect::cache::may_have_marks(protocol.dialect(), &body);
        Ok(Wire {
            body,
            path: prepared.path,
            query: prepared.query,
            dialect: protocol.dialect(),
            headers: Vec::new(),
            convert: Some(prepared.session.clone()),
            collect: prepared.session,
            ledger,
            auto_cache,
        })
    }
}

/// Send to `entry`, and if the upstream rejects the dialect this route
/// is configured for, try its alternates and remember whichever answers.
///
/// An upstream that refuses the prompt-cache breakpoints the conversion
/// added gets the request once more without them, and is remembered as
/// refusing them for this model (see `proxy::cache_marks`).
///
/// A rejected dialect or breakpoint is known before a single byte of body
/// arrives — the status check happens inside `send` — so a stream needs
/// no special handling: nothing has reached the client yet when the retry
/// happens.
pub(crate) async fn send(
    entry: &RouteEntry,
    outbound: &Outbound,
    call_ctx: &CallCtx,
    db: &sqlx::PgPool,
    model: &str,
) -> Result<(reqwest::Response, Wire), GatewayError> {
    let upstream = &entry.upstream;
    let official = upstream.is_official();
    let first = outbound
        .address(entry.protocol, model, official)?
        .for_upstream(upstream, model);
    let mut last = match first.send_to(upstream, call_ctx).await {
        Ok(resp) => return Ok((resp, first)),
        Err(e) => e,
    };

    if first.auto_cache && super::cache_marks::is_refusal(&last, &upstream.label) {
        let mut wire = first;
        if wire.drop_added_marks() {
            upstream.cache_marks.note(model);
            tracing::info!(
                provider = %entry.provider_name,
                model,
                "Upstream refused the cache breakpoints added on conversion — sending again without them"
            );
            match wire.send_to(upstream, call_ctx).await {
                Ok(resp) => return Ok((resp, wire)),
                Err(e) => last = e,
            }
        }
    }

    if super::protocol_relearn::is_protocol_mismatch(&last) {
        for protocol in &entry.alternates {
            tracing::info!(
                provider = %entry.provider_name,
                model,
                from = %entry.protocol,
                to = %protocol,
                "Upstream rejected the configured protocol — retrying with an alternate"
            );
            let wire = outbound
                .address(*protocol, model, official)?
                .for_upstream(upstream, model);
            match wire.send_to(upstream, call_ctx).await {
                Ok(resp) => {
                    super::protocol_relearn::persist(db, entry.route_id, *protocol).await;
                    return Ok((resp, wire));
                }
                Err(e) => {
                    let keep_going = super::protocol_relearn::is_protocol_mismatch(&e);
                    last = e;
                    if !keep_going {
                        break;
                    }
                }
            }
        }
    }
    Err(last)
}

/// Read a whole answer and put it in the caller's format.
///
/// When the upstream reports no usage, the count is estimated from the
/// request (`input_estimate`) and the answer.
pub(crate) async fn read_whole(
    resp: reqwest::Response,
    wire: &Wire,
    caller_model: &str,
    input_estimate: u64,
) -> Result<Completed, GatewayError> {
    let upstream = resp
        .bytes()
        .await
        .map_err(super::transport::transport_error)?;

    let mut sniffer = tw_dialect::usage::Sniffer::new();
    sniffer.feed(&upstream);
    let (usage, usage_estimated) =
        crate::usage_estimate::complete(sniffer.finish(), true, input_estimate, Some(&upstream));
    if usage_estimated {
        metrics::counter!("gateway_usage_estimated_total").increment(1);
    }

    let body = match &wire.convert {
        Some(session) => session.response(&upstream).ok_or_else(|| {
            GatewayError::ProviderInvalidResponse(
                "The upstream answered with something that is not JSON.".into(),
            )
        })?,
        None => upstream.to_vec(),
    };
    Ok(Completed {
        body: rewrite_model(&body, caller_model),
        usage,
        usage_estimated,
    })
}

// ───────────────────────────────────────────── the pipeline

/// Every error on the way out is in the caller's own format.
///
/// `path` and `query` are the caller's: Gemini puts the model, whether to
/// stream and which stream form in them.
///
/// `cancel` is the middleware's record of a request whose client leaves
/// before the response exists (see `early_cancel`); `None` on a
/// WebSocket turn, whose connection records its own end.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn generate(
    state: GatewayState,
    headers: HeaderMap,
    identity: GatewayRequestIdentity,
    cancel: Option<EarlyCancelSlot>,
    body: Bytes,
    surface: ClientSurface,
    path: &str,
    query: Option<&str>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    run(state, headers, identity, cancel, body, surface, path, query)
        .await
        .map_err(|e| e.in_dialect(surface.dialect))
}

#[allow(clippy::too_many_arguments)]
async fn run(
    state: GatewayState,
    headers: HeaderMap,
    identity: GatewayRequestIdentity,
    cancel: Option<EarlyCancelSlot>,
    body: Bytes,
    surface: ClientSurface,
    path: &str,
    query: Option<&str>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    let trace_id = resolve_trace_id(&headers);
    let session_id = resolve_session_id(&headers);
    let request_started_at = std::time::Instant::now();
    if let Some(c) = &cancel {
        c.request(&trace_id, session_id.as_deref());
    }

    // A row even for a body we cannot read: an operator chasing a 400
    // should find it.
    let early_ctx = LogCtx::new(
        &state.audit,
        &identity,
        &trace_id,
        session_id.as_deref(),
        "(unknown)",
        request_started_at,
    );
    let raw: Value = serde_json::from_slice(&body).map_err(|_| {
        early_ctx.emit(GatewayError::TransformError(
            "The request body is not valid JSON.".into(),
        ))
    })?;
    let (model, is_stream) = if surface.dialect == Dialect::Gemini {
        gemini_target(path).ok_or_else(|| {
            early_ctx.emit(GatewayError::TransformError(format!(
                "The path {path} does not name a Gemini model and one of \
                 :generateContent or :streamGenerateContent."
            )))
        })?
    } else {
        let model = raw
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                early_ctx.emit(GatewayError::TransformError("Missing 'model' field".into()))
            })?
            .to_string();
        (
            model,
            raw.get("stream").and_then(Value::as_bool).unwrap_or(false),
        )
    };
    // Whether the caller reads a stream as SSE. Gemini's does only with
    // `alt=sse`; without it, the stream is one JSON array.
    let client_sse = surface.dialect != Dialect::Gemini
        || query.is_some_and(|q| q.split('&').any(|kv| kv == GEMINI_SSE));

    // 1. Model aliases
    let mapped_model = state.model_mapper.map(&model);
    if let Some(c) = &cancel {
        c.model(&mapped_model);
    }
    let ctx = LogCtx::new(
        &state.audit,
        &identity,
        &trace_id,
        session_id.as_deref(),
        &mapped_model,
        request_started_at,
    );

    // 2. Budget, model access, rate limits — each stage writes its own
    //    audit row on short-circuit.
    let preflight = run_preflight_stages(&state, &identity, &trace_id, &mapped_model).await?;

    let mut metadata = RequestMetadata::extract(&headers, &raw);
    // One id for the request: the one its error rows and the guards'
    // events carry too. Two ids drawn apart for a caller that sent no
    // `x-trace-id` would leave a refusal's audit events pointing at no
    // log row.
    if trace_id.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
        metadata.request_id = trace_id.clone();
    }

    // The guards this request runs under, whatever an admin changes while
    // it is in flight.
    let guards = state.guards.load_full();
    let caller = Caller::of(&identity, &metadata.request_id, &mapped_model);

    // 3. Content filter, on the caller's text where its own format puts
    //    it. Every hit is an audit event (without the text); a refusal is
    //    the caller's 403, quoting their words (masked).
    let screening = guards.content.screen(surface.dialect, &body);
    crate::content_filter::record(&state.audit, &caller, &screening);
    if let Some(hit) = screening.refusal() {
        return Err(ctx
            .emit(crate::content_filter::refusal(hit, &guards.redaction))
            .into());
    }
    // Text was stripped: from here on — decoding, redaction, every hop,
    // the audit row — the request is the stripped one.
    let (body, raw) = match screening.body {
        Some(stripped) => {
            let raw = serde_json::from_slice(&stripped).map_err(|_| {
                ctx.emit(GatewayError::TransformError(
                    "The request could not be read after the content filter removed text from it."
                        .into(),
                ))
            })?;
            (stripped, raw)
        }
        None => (body, raw),
    };

    // 4. Decode once, for the input estimate and to know whether the
    //    request can be converted at all. One that cannot — it continues a
    //    conversation kept on OpenAI's servers (`previous_response_id`), or
    //    carries a compaction only OpenAI can read — can still be forwarded
    //    to an upstream of its own format, so that is where it goes (step 8).
    let decoded =
        tw_dialect::convert::decode(surface.dialect, &raw, path, internal_query(surface.dialect));

    // 5. Outbound redaction: the whole request is searched and what is
    //    found numbered once, in the order the caller wrote it.
    //    Placeholders are stable per value, so two callers sending the same
    //    structure redact to the same bytes and share a cache slot; each
    //    restores their own values on the way out.
    //
    //    The audit row keeps what the caller wrote (their at-rest
    //    redaction setting applies on top).
    let (findings, ledger) = guards.redaction.look(&body);
    crate::redaction::record(&state.audit, &caller, guards.redaction.mode, &findings);
    let request_for_audit = body.to_vec();
    let outbound_body = if ledger.is_empty() {
        raw
    } else {
        let (replaced, _) = guards.redaction.replace(body.to_vec(), &ledger);
        serde_json::from_slice(&replaced).map_err(|_| {
            ctx.emit(GatewayError::TransformError(
                "The request could not be read after redaction.".into(),
            ))
        })?
    };

    // 6. The model's output cap. Applied to each hop as it is addressed
    //    (`Outbound::cap_output`): whether to fill one in depends on the
    //    upstream model, and the field on the upstream's format. The
    //    upstream stops there by itself; the answer is not measured.
    let max_output_tokens = state
        .router
        .load()
        .config_for(&mapped_model)
        .max_output_tokens;

    let call_ctx = CallCtx::new(
        Some(trace_id.clone()),
        identity.user_id.clone(),
        identity.user_email.clone(),
    );

    // 7. Cache. A hit counts its tokens toward the token limits and
    //    budgets like a real call would — the caller received them, and
    //    otherwise repeating a cached prompt would get round every token
    //    limit.
    let cache_fingerprint = if surface.caches {
        ResponseCache::fingerprint(&outbound_body).map(|mut fp| {
            // The cap is applied after this, per hop: an answer made under
            // one cap must not be served under another.
            if let Some(cap) = max_output_tokens {
                fp.extend_from_slice(format!("\nmax_output_tokens={cap}").as_bytes());
            }
            fp
        })
    } else {
        None
    };
    if let Some(fp) = &cache_fingerprint
        && let Some(cached) = state.cache.get(fp).await
    {
        metrics::counter!("gateway_cache_total", "result" => "hit").increment(1);
        // With this caller's values in it: what they would run.
        let restored = crate::redaction::restore_body(&ledger, &cached.body);
        // A stored answer passed the inspection in force when it was
        // stored, not necessarily the one in force now.
        if let Some(e) = crate::tool_inspection::check_whole(
            &guards.tools,
            &guards.redaction,
            &state.audit,
            &caller,
            "cache",
            &restored,
        ) {
            return Err(ctx.emit(e).into());
        }
        // The tokens the stored answer records, weighted for this model,
        // to the same rules and caps the pre-flight checked. No route
        // answered, so no route cap counts them.
        super::post_flight_account(
            state.db.clone(),
            state.redis.clone(),
            state.dynamic_config.clone(),
            state.weight_cache.clone(),
            mapped_model.clone(),
            cached_tokens(&cached),
            &preflight.limits,
            None,
            identity.user_id.clone(),
            identity.user_email.clone(),
            identity.api_key_id.clone(),
            identity.ip_address.clone(),
            state.audit.clone(),
        )
        .await;

        // Same capture pipeline as a fresh request — redaction toggle,
        // byte cap and offload all apply — with the status marked.
        let mut capture = prepare_body_capture(
            &state.dynamic_config,
            &guards.redaction,
            &state.blob_store,
            &metadata.request_id,
            &request_for_audit,
            Some(&cached.body),
        )
        .await;
        capture.status = Some(BodyCaptureStatus::FromCache.as_str());
        emit_gateway_log(
            &state.audit,
            &metadata.request_id,
            session_id.as_deref(),
            identity.user_id.as_deref(),
            identity.user_email.as_deref(),
            identity.api_key_id.as_deref(),
            identity.api_key_lineage_id.as_deref(),
            identity.ip_address.as_deref(),
            &mapped_model,
            None,
            None,
            cached.prompt_tokens,
            cached.completion_tokens,
            Decimal::ZERO,
            request_started_at.elapsed().as_millis() as i64,
            200,
            capture,
        );

        let mut response = if is_stream {
            // The stored answer is whole; replay it as one event so the
            // client gets the framing it asked for.
            let body = String::from_utf8_lossy(&restored).into_owned();
            let events = async_stream::stream! {
                yield Ok::<_, Infallible>(axum::response::sse::Event::default().data(body));
                yield Ok::<_, Infallible>(axum::response::sse::Event::default().data("[DONE]"));
            };
            axum::response::sse::Sse::new(events).into_response()
        } else {
            json_response(restored)
        };
        response
            .headers_mut()
            .insert("X-Cache", HeaderValue::from_static("HIT"));
        response.headers_mut().insert(
            "X-Metadata-Request-Id",
            request_id_header(&metadata.request_id),
        );
        return Ok(response);
    }
    if surface.caches {
        metrics::counter!("gateway_cache_total", "result" => "miss").increment(1);
    }

    // 8. Route.
    let router = state.router.load();
    let routes = router.route(&mapped_model).ok_or_else(|| {
        ctx.emit(GatewayError::ProviderError(format!(
            "No provider found for model: {mapped_model}"
        )))
    })?;
    // A request the conversion layer cannot read goes only to routes that
    // forward it as sent. With none, the reason it cannot be converted is
    // the answer, as it would be from any of the routes.
    let forwarding: Vec<RouteEntry>;
    let (routes, input_estimate) = match &decoded {
        Ok(decoded) => (
            routes.as_slice(),
            crate::usage_estimate::request_tokens(&decoded.request),
        ),
        Err(rejection) => {
            forwarding = routes
                .iter()
                .filter(|r| r.protocol.dialect() == surface.dialect)
                .cloned()
                .collect();
            if forwarding.is_empty() {
                return Err(ctx
                    .emit(GatewayError::TransformError(rejection.0.clone()))
                    .into());
            }
            (
                forwarding.as_slice(),
                crate::usage_estimate::raw_request_tokens(&outbound_body),
            )
        }
    };
    let outbound = Outbound {
        surface,
        path: path.to_string(),
        body: outbound_body,
        stream: is_stream,
        dialect_headers: dialect_headers(&headers),
        input_estimate,
        redaction: guards.redaction.clone(),
        ledger: ledger.clone(),
        max_output_tokens,
    };
    let snapshot = |route: &RouteEntry, sel_record| crate::lifecycle::ChatPostInvokeDeps {
        state: state.clone(),
        guards: guards.clone(),
        request: crate::lifecycle::ChatRequestSnapshot {
            identity: identity.clone(),
            trace_id: metadata.request_id.clone(),
            session_id: session_id.clone(),
            mapped_model: mapped_model.clone(),
            request_for_audit: request_for_audit.clone(),
            cache_fingerprint: cache_fingerprint.clone(),
            request_started_at,
            input_estimate,
        },
        preflight: crate::lifecycle::ChatPreflightLists {
            limits: preflight.limits.clone(),
        },
        route: crate::lifecycle::ChatPickedRoute {
            provider_name: route.provider_name.clone(),
            upstream_model: route.upstream_model.clone(),
            sel_record,
            caps: crate::route_caps::RouteCaps::of(route),
        },
        cache_enabled: surface.caches,
    };

    if outbound.stream {
        // One pick, no retry once bytes have gone to the client. A dialect
        // rejection still gets its retry: it arrives before any body.
        let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
        let (entry, sel_record) = select_route_for_stream(routes, &sel_ctx)
            .await
            .map_err(|e| GatewayErrorResponse::from(ctx.emit(e)))?;

        set_affinity(
            &state.redis,
            identity.user_id.as_deref(),
            &mapped_model,
            sel_ctx.affinity_mode,
            entry,
            sel_ctx.affinity_ttl_secs,
        )
        .await;

        let hide_usage = outbound.hides_usage();
        // Started on the stream's first poll — see `build_chat_pump` for
        // why it must not be awaited here.
        let open: crate::lifecycle::OpenUpstream = {
            let entry = entry.clone();
            let call_ctx = call_ctx.clone();
            let db = state.db.clone();
            let model = entry
                .upstream_model
                .clone()
                .unwrap_or_else(|| mapped_model.clone());
            Box::pin(async move { send(&entry, &outbound, &call_ctx, &db, &model).await })
        };

        let deps = snapshot(entry, sel_record);
        let shaper = StreamShaper::new(mapped_model.clone(), &ledger, surface.dialect)
            .hiding_usage(hide_usage);
        return Ok(launch_stream_pump(
            deps,
            open,
            shaper,
            surface.dialect,
            client_sse,
        ));
    }

    // Buffered: full failover across healthy candidates.
    let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
    // Built before failover so the synchronous error closure can move it
    // in. Error paths capture the request only — nothing succeeded.
    let error_capture = prepare_body_capture(
        &state.dynamic_config,
        &guards.redaction,
        &state.blob_store,
        &metadata.request_id,
        &request_for_audit,
        None,
    )
    .await;
    let (entry, completed, answer_ledger, sel_record) =
        select_route_with_failover(routes, &outbound, &call_ctx, &sel_ctx, &mapped_model)
            .await
            .map_err(|e| {
                // Every candidate failed — no winning provider to name.
                emit_gateway_error_log(
                    &state.audit,
                    &metadata.request_id,
                    session_id.as_deref(),
                    identity.user_id.as_deref(),
                    identity.user_email.as_deref(),
                    identity.api_key_id.as_deref(),
                    identity.api_key_lineage_id.as_deref(),
                    identity.ip_address.as_deref(),
                    &mapped_model,
                    None,
                    request_started_at.elapsed().as_millis() as i64,
                    &e,
                    error_capture,
                );
                GatewayErrorResponse::from(e)
            })?;

    // The answer as the caller will receive it, their values restored.
    let restored = crate::redaction::restore_body(&answer_ledger, &completed.body);

    // Tool calls, on the whole answer before any of it has gone out — in
    // the form the caller would run them. A refusal is the gateway's
    // policy, not the upstream failing, so the route's health counts it as
    // a success; the answer is neither cached nor billed.
    if let Some(e) = crate::tool_inspection::check_whole(
        &guards.tools,
        &guards.redaction,
        &state.audit,
        &caller,
        &entry.provider_name,
        &restored,
    ) {
        finalize_health(&state, &sel_record, true).await;
        return Err(ctx.emit(e).into());
    }

    // Cache fill, audit, breaker and budget debit — the same hooks the
    // stream runs in its tail. The cache keeps the placeholder form.
    let deps = snapshot(entry, sel_record);
    run_buffered_post_invoke(&deps, completed).await;

    tracing::info!(
        request_id = %metadata.request_id,
        metadata = %metadata.to_json(),
        "Audit log: request completed"
    );

    let mut response = json_response(restored);
    response
        .headers_mut()
        .insert("X-Cache", HeaderValue::from_static("MISS"));
    response.headers_mut().insert(
        "X-Metadata-Request-Id",
        request_id_header(&metadata.request_id),
    );
    Ok(response)
}

/// `(prompt, completion)` for billing and limits.
///
/// The prompt count is the whole input — plain, cache read and cache
/// written — which is what OpenAI reports and what most upstreams here
/// reported before. Anthropic's own `input_tokens` excludes the cached
/// part; counting it the same way everywhere keeps one route from
/// looking cheaper than another for the same work.
pub(crate) fn tokens(u: &tw_dialect::usage::Usage) -> (u32, u32) {
    (
        u32::try_from(u.prompt_total()).unwrap_or(u32::MAX),
        u32::try_from(u.output).unwrap_or(u32::MAX),
    )
}

/// What a cache hit counts toward token limits and budgets: the tokens
/// its stored answer records. The store keeps only the input and output
/// totals, so all of the input counts as plain input.
fn cached_tokens(cached: &crate::cache::Cached) -> TokenCounts {
    TokenCounts {
        input: i64::from(cached.prompt_tokens),
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: false,
        output: i64::from(cached.completion_tokens),
    }
}

/// The same usage split the way it is priced: cache reads and writes
/// apart from plain input (see `cost_tracker`).
pub(crate) fn priced(u: &tw_dialect::usage::Usage) -> TokenCounts {
    let n = |x: u64| i64::try_from(x).unwrap_or(i64::MAX);
    TokenCounts {
        input: n(u.input),
        cache_read: n(u.cache_read),
        cache_write: n(u.cache_write),
        cache_write_1h: u.cache_1h,
        output: n(u.output),
    }
}

/// The query a request in `client`'s format is decoded with (see
/// [`GEMINI_SSE`]).
fn internal_query(client: Dialect) -> Option<&'static str> {
    (client == Dialect::Gemini).then_some(GEMINI_SSE)
}

/// The caller's `anthropic-*` headers, to go with a request forwarded in
/// its own format.
fn dialect_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(k, _)| k.as_str().starts_with("anthropic-"))
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect()
}

fn json_response(body: Vec<u8>) -> axum::response::Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gemini_path_names_the_model_and_whether_to_stream() {
        assert_eq!(
            gemini_target("/v1beta/models/gemini-2.5-pro:streamGenerateContent"),
            Some(("gemini-2.5-pro".into(), true))
        );
        assert_eq!(
            gemini_target("/v1/models/gemini-2.5-flash:generateContent"),
            Some(("gemini-2.5-flash".into(), false))
        );
        // Not a generation: nothing to convert it to.
        assert_eq!(gemini_target("/v1beta/models/g:countTokens"), None);
        assert_eq!(gemini_target("/v1beta/models/:generateContent"), None);
    }

    fn outbound(surface: ClientSurface, path: &str, body: Value, cap: Option<u32>) -> Outbound {
        let redaction = Redaction::new(&tw_guard::policy::RedactPolicy {
            mode: tw_guard::policy::Mode::Off,
            ..Default::default()
        });
        let (_, ledger) = redaction.look(b"{}");
        Outbound {
            surface,
            path: path.into(),
            stream: body["stream"] == true,
            body,
            dialect_headers: Vec::new(),
            input_estimate: 0,
            redaction,
            ledger,
            max_output_tokens: cap,
        }
    }

    /// The body a Chat caller's request goes out with to a Chat upstream,
    /// under a model cap of `cap`.
    fn sent_to_chat(ask: Value, cap: u32, official: bool) -> Value {
        let outbound = outbound(CHAT, "/v1/chat/completions", ask, Some(cap));
        let wire = outbound
            .address(UpstreamProtocol::OpenAiChat, "gpt-5", official)
            .unwrap_or_else(|e| panic!("{e:?}"));
        serde_json::from_slice(&wire.body).unwrap()
    }

    #[test]
    fn a_chat_request_without_a_limit_gets_the_field_its_endpoint_reads() {
        let ask = serde_json::json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "ping"}]
        });
        // OpenAI's own endpoint: its reasoning models refuse `max_tokens`.
        let sent = sent_to_chat(ask.clone(), 4096, true);
        assert_eq!(sent["max_completion_tokens"], 4096, "{sent}");
        assert!(sent.get("max_tokens").is_none(), "{sent}");
        // Anywhere else: compatible servers mostly read only `max_tokens`.
        let sent = sent_to_chat(ask, 4096, false);
        assert_eq!(sent["max_tokens"], 4096, "{sent}");
        assert!(sent.get("max_completion_tokens").is_none(), "{sent}");
    }

    /// A Responses request that continues a conversation kept on OpenAI's
    /// servers, or carries a compaction only OpenAI can read, cannot be
    /// converted — and is forwarded as sent to an upstream that speaks
    /// Responses.
    #[test]
    fn a_request_the_conversion_cannot_read_still_goes_out_in_its_own_format() {
        let ask = serde_json::json!({
            "model": "gpt-5.5",
            "stream": true,
            "previous_response_id": "resp_1",
            "input": [
                {"type": "compaction", "encrypted_content": "gAAAAABo"},
                {"type": "message", "role": "user", "content": "next"}
            ]
        });
        let out = outbound(RESPONSES, "/v1/responses", ask.clone(), None);
        let wire = out
            .address(UpstreamProtocol::OpenAiResponses, "gpt-5.5", true)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(wire.convert.is_none());
        let sent: Value = serde_json::from_slice(&wire.body).unwrap();
        assert_eq!(sent, ask);
        assert!(wire.collect.stream && wire.collect.model == "gpt-5.5");

        match out.address(UpstreamProtocol::OpenAiChat, "gpt-5.5", true) {
            Err(GatewayError::TransformError(_)) => {}
            Err(e) => panic!("{e:?}"),
            Ok(_) => panic!("converted a request that points at OpenAI's servers"),
        }
    }

    /// Codex's compaction forwarded to OpenAI comes back as OpenAI's own
    /// compaction item. Assembled for the audit row as if converted, it
    /// would read as a compaction that wrote no summary and failed.
    #[test]
    fn a_forwarded_compaction_is_assembled_as_the_upstreams_answer() {
        let ask = serde_json::json!({
            "model": "gpt-5.5",
            "stream": true,
            "input": [
                {"type": "message", "role": "user", "content": "fix the test"},
                {"type": "compaction_trigger"}
            ]
        });
        let out = outbound(RESPONSES, "/v1/responses", ask.clone(), None);
        let forwarded = out
            .address(UpstreamProtocol::OpenAiResponses, "gpt-5.5", true)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(!forwarded.collect.is_compaction());
        let sent: Value = serde_json::from_slice(&forwarded.body).unwrap();
        assert_eq!(sent, ask);

        // Converted, the upstream's summary is what Codex gets back.
        let converted = out
            .address(UpstreamProtocol::OpenAiChat, "gpt-5.5", true)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(converted.convert.as_ref().unwrap().is_compaction());
    }

    /// A Chat upstream that writes a tool call into its text: a converted
    /// answer carries it as a structured call, a forwarded one is assembled
    /// as the caller received it — text.
    #[test]
    fn a_tool_call_written_into_text_is_assembled_as_the_caller_received_it() {
        const STREAM: &str = concat!(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"role\":\"assistant\",\"content\":\"<tool_call>{\\\"name\\\": \\\"ls\\\", ",
            "\\\"arguments\\\": {\\\"path\\\": \\\"/\\\"}}</tool_call>\"}}]}\n\n",
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,",
            "\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let tools = serde_json::json!([{
            "type": "function",
            "function": {"name": "ls", "parameters": {"type": "object"}}
        }]);
        let assemble = |session: &Session| -> Value {
            let mut c = session.collector();
            c.process(STREAM.as_bytes());
            serde_json::from_slice(&c.finish().unwrap()).unwrap()
        };

        let ask = serde_json::json!({
            "model": "m", "stream": true, "tools": tools,
            "messages": [{"role": "user", "content": "list /"}]
        });
        let forwarded = outbound(CHAT, "/v1/chat/completions", ask, None)
            .address(UpstreamProtocol::OpenAiChat, "m", false)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(forwarded.convert.is_none());
        let message = &assemble(&forwarded.collect)["choices"][0]["message"];
        assert!(
            message["content"].as_str().unwrap().contains("<tool_call>"),
            "{message}"
        );
        assert!(message.get("tool_calls").is_none(), "{message}");

        let ask = serde_json::json!({
            "model": "m", "stream": true, "max_tokens": 100,
            "tools": [{"name": "ls", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "list /"}]
        });
        let converted = outbound(MESSAGES, "/v1/messages", ask, None)
            .address(UpstreamProtocol::OpenAiChat, "m", false)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let content = &assemble(converted.convert.as_ref().unwrap())["content"];
        assert!(
            content
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "tool_use" && b["name"] == "ls"),
            "{content}"
        );
    }

    /// Only breakpoints the conversion added may be taken out when an
    /// upstream refuses them; the caller's own are its decision.
    #[test]
    fn only_breakpoints_the_conversion_added_count_as_added() {
        let chat = outbound(
            CHAT,
            "/v1/chat/completions",
            serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
            None,
        );
        let mut wire = chat
            .address(
                UpstreamProtocol::AnthropicMessages,
                "claude-sonnet-4-5",
                true,
            )
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(wire.auto_cache);
        assert!(wire.drop_added_marks());
        let body = String::from_utf8(wire.body.clone()).unwrap();
        assert!(
            !body.contains("cache_control") && body.contains("hi"),
            "{body}"
        );

        let claude_code = outbound(
            MESSAGES,
            "/v1/messages",
            serde_json::json!({
                "model": "m", "max_tokens": 16,
                "system": [{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}],
                "messages": [{"role": "user", "content": "hi"}]
            }),
            None,
        );
        let mut wire = claude_code
            .address(
                UpstreamProtocol::BedrockNative,
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
                true,
            )
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert!(String::from_utf8_lossy(&wire.body).contains("cachePoint"));
        assert!(!wire.auto_cache);
        assert!(!wire.drop_added_marks());
    }

    #[test]
    fn both_chat_limits_are_held_to_the_cap() {
        // An upstream that reads only `max_tokens` would otherwise go
        // uncapped; the smaller one the caller wrote is kept.
        let ask = serde_json::json!({
            "model": "gpt-5",
            "max_tokens": 99_999,
            "max_completion_tokens": 100,
            "messages": [{"role": "user", "content": "ping"}]
        });
        for official in [true, false] {
            let sent = sent_to_chat(ask.clone(), 4096, official);
            assert_eq!(sent["max_tokens"], 4096, "{sent}");
            assert_eq!(sent["max_completion_tokens"], 100, "{sent}");
        }
    }
}
