//! The four generation surfaces — `/v1/chat/completions`, `/v1/messages`,
//! `/v1/responses` and Gemini's `/v1beta/models/{model}:generateContent`
//! (`:streamGenerateContent`) — as one pipeline.
//!
//! # Forward what can be forwarded, convert what must be
//!
//! A request that reaches an upstream speaking its own format goes out
//! **as the caller sent it**: only the model name changes, and any PII is
//! swapped for placeholders. That is not a shortcut. The intermediate
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
//! # One pass of inspection, on a structure that is known
//!
//! The request is still decoded once, whatever its route, because the
//! content filter and PII detection need to know where the caller's text
//! is. The decoded form is only read; what is sent is the raw request,
//! with the found PII carried back onto it.

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
use crate::content_filter::Action;
use crate::lifecycle::Completed;
use crate::metadata::RequestMetadata;
use crate::protocol::UpstreamProtocol;
use crate::router::RouteEntry;

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
    body: Bytes,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    generate(
        state,
        headers,
        identity,
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
    body: Bytes,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    generate(
        state,
        headers,
        identity,
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
    body: Bytes,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    generate(
        state,
        headers,
        identity,
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
    body: Bytes,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    generate(
        state,
        headers,
        identity,
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
    /// Redacted, otherwise exactly as sent.
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
            let collect = decode(&body)?.encode(&target(client)).session;
            let mut bytes = serde_json::to_vec(&body).unwrap_or_default();
            // Reasoning signatures a conversion wrote earlier in this
            // conversation (`tw1.`-prefixed) were not issued by this
            // upstream, and Anthropic refuses the whole request over
            // them. That reasoning did not come from here anyway.
            if let Some(stripped) = tw_dialect::convert::strip_carried(client, &bytes) {
                bytes = stripped;
            }
            return Ok(Wire {
                body: bytes,
                path,
                query,
                dialect: client,
                headers: self.dialect_headers.clone(),
                convert: None,
                collect,
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
        Ok(Wire {
            body: prepared.body,
            path: prepared.path,
            query: prepared.query,
            dialect: protocol.dialect(),
            headers: Vec::new(),
            convert: Some(prepared.session.clone()),
            collect: prepared.session,
        })
    }
}

/// Send to `entry`, and if the upstream rejects the dialect this route
/// is configured for, try its alternates and remember whichever answers.
///
/// A rejected dialect is known before a single byte of body arrives —
/// the status check happens inside `send` — so a stream needs no special
/// handling: nothing has reached the client yet when the retry happens.
pub(crate) async fn send(
    entry: &RouteEntry,
    outbound: &Outbound,
    call_ctx: &CallCtx,
    db: &sqlx::PgPool,
    model: &str,
) -> Result<(reqwest::Response, Wire), GatewayError> {
    let official = entry.upstream.is_official();
    let first = outbound.address(entry.protocol, model, official)?;
    let result = entry
        .upstream
        .send(
            first.body.clone(),
            &first.path,
            first.query.as_deref(),
            first.dialect,
            &first.headers,
            call_ctx,
        )
        .await;
    let mut last = match result {
        Ok(resp) => return Ok((resp, first)),
        Err(e) => e,
    };

    if super::protocol_relearn::is_protocol_mismatch(&last) {
        for protocol in &entry.alternates {
            tracing::info!(
                provider = %entry.provider_name,
                model,
                from = %entry.protocol,
                to = %protocol,
                "Upstream rejected the configured protocol — retrying with an alternate"
            );
            let wire = outbound.address(*protocol, model, official)?;
            match entry
                .upstream
                .send(
                    wire.body.clone(),
                    &wire.path,
                    wire.query.as_deref(),
                    wire.dialect,
                    &wire.headers,
                    call_ctx,
                )
                .await
            {
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
pub(crate) async fn generate(
    state: GatewayState,
    headers: HeaderMap,
    identity: GatewayRequestIdentity,
    body: Bytes,
    surface: ClientSurface,
    path: &str,
    query: Option<&str>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    run(state, headers, identity, body, surface, path, query)
        .await
        .map_err(|e| e.in_dialect(surface.dialect))
}

async fn run(
    state: GatewayState,
    headers: HeaderMap,
    identity: GatewayRequestIdentity,
    body: Bytes,
    surface: ClientSurface,
    path: &str,
    query: Option<&str>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    let trace_id = resolve_trace_id(&headers);
    let session_id = resolve_session_id(&headers);
    let request_started_at = std::time::Instant::now();

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
    let ctx = LogCtx::new(
        &state.audit,
        &identity,
        &trace_id,
        session_id.as_deref(),
        &mapped_model,
        request_started_at,
    );

    // 2. Rate limits, budget, model access — each stage writes its own
    //    audit row on short-circuit.
    let preflight = run_preflight_stages(&state, &identity, &trace_id, &mapped_model).await?;

    let metadata = RequestMetadata::extract(&headers, &raw);

    // 3. Decode once, to know where the caller's text is.
    let mut decoded =
        tw_dialect::convert::decode(surface.dialect, &raw, path, internal_query(surface.dialect))
            .map_err(|r| ctx.emit(GatewayError::TransformError(r.0)))?;

    // 4. Content filter. Log lines carry `log_summary()` (no snippet) so
    //    prompt content stays out of the log pipeline; the caller sees
    //    the full match, since it is their own text.
    if let Some(m) = state.content_filter.load().check_request(&decoded.request) {
        match m.action {
            Action::Block => {
                tracing::warn!("Content filter blocked request: {}", m.log_summary());
                return Err(ctx
                    .emit(GatewayError::TransformError(format!(
                        "Request blocked by content filter: {m}"
                    )))
                    .into());
            }
            Action::Warn => tracing::warn!(
                "Content filter warning (request allowed): {}",
                m.log_summary()
            ),
            Action::Log => tracing::info!("Content filter log: {}", m.log_summary()),
        }
    }

    // 4b. Invisible characters that can carry an instruction past a
    //     reader — in what the caller typed, or in a tool result.
    let hidden_action = crate::hidden_text::action(&state.dynamic_config).await;
    if hidden_action != crate::hidden_text::Action::Off {
        let found = crate::hidden_text::scan(&decoded.request);
        if !found.is_empty() {
            use crate::hidden_text::Action as H;
            use think_watch_common::audit::{AuditActor, GatewayActor, LogType};
            metrics::counter!("gateway_hidden_text_total", "action" => format!("{hidden_action:?}"))
                .increment(1);
            tracing::warn!(trace_id = %trace_id, ?found, "request carries hidden characters");
            if matches!(hidden_action, H::Warn | H::Block) {
                let blocked = hidden_action == H::Block;
                state.audit.log(
                    GatewayActor {
                        user_id: identity.user_id.as_deref(),
                        user_email: identity.user_email.as_deref(),
                        api_key_id: identity.api_key_id.as_deref(),
                        api_key_lineage_id: identity.api_key_lineage_id.as_deref(),
                        ip: identity.ip_address.as_deref(),
                        session_id: None,
                    }
                    .audit(if blocked {
                        "gateway.hidden_text_blocked"
                    } else {
                        "gateway.hidden_text_flagged"
                    })
                    .log_type(LogType::Audit)
                    .detail(serde_json::json!({
                        "trace_id": trace_id,
                        "model": mapped_model,
                        "found": found,
                    })),
                );
            }
            if hidden_action == H::Block {
                return Err(ctx.emit(crate::hidden_text::refusal(&found)).into());
            }
        }
    }

    let call_ctx = CallCtx::new(
        Some(trace_id.clone()),
        identity.user_id.clone(),
        identity.user_email.clone(),
    );

    // 5. PII. Found on the decoded form, carried back onto the raw one.
    //    Placeholders are stable per value, so two callers sending the
    //    same structure redact to the same bytes and share a cache slot;
    //    each restores their own values on the way out.
    //
    //    The audit row keeps what the caller actually wrote.
    let pii_redactor = state.pii_redactor.load_full();
    let redaction = pii_redactor.redact_request(&mut decoded.request);
    let mut redacted = raw;
    crate::pii_redactor::apply_to(&redaction, &mut redacted);
    let request_for_audit = body.to_vec();

    // 6. Quota, keyed on the model the caller named — that is what their
    //    dashboards group by.
    let quota_key = identity
        .user_id
        .as_deref()
        .or(identity.api_key_id.as_deref())
        .map(|id| format!("{id}:{mapped_model}"))
        .unwrap_or_else(|| mapped_model.clone());
    if let Err(e) = state.quota.check_quota(&quota_key).await {
        tracing::warn!("Quota exceeded for {quota_key}: {e}");
        return Err(ctx
            .emit(GatewayError::ProviderError(format!("Quota exceeded: {e}")))
            .into());
    }

    // 7. Cache. A hit debits quota like a real call would — otherwise a
    //    deterministic prompt amortises one upstream call across an
    //    unbounded quota window.
    let cache_fingerprint = if surface.caches {
        ResponseCache::fingerprint(&redacted)
    } else {
        None
    };
    if let Some(fp) = &cache_fingerprint
        && let Some(cached) = state.cache.get(fp).await
    {
        metrics::counter!("gateway_cache_total", "result" => "hit").increment(1);
        // A stored answer passed the inspection in force when it was
        // stored, not necessarily the one in force now.
        if let Some(e) = crate::tool_inspection::check_whole(
            &state.tool_inspection.load(),
            &state.audit,
            &crate::tool_inspection::Caller::of(&identity, &metadata.request_id, &mapped_model),
            "cache",
            &cached.body,
        ) {
            return Err(ctx.emit(e).into());
        }
        let total = cached.prompt_tokens + cached.completion_tokens;
        if let Err(e) = state.quota.consume(&quota_key, total).await {
            tracing::warn!(quota_key = %quota_key, tokens = total, "quota consume on cache hit failed: {e}");
        }

        // Same capture pipeline as a fresh request — PII toggle, byte
        // cap and offload all apply — with the status marked.
        let mut capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
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

        let restored = crate::pii_redactor::restore_body(&redaction, &cached.body);
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

    let input_estimate = crate::usage_estimate::request_tokens(&decoded.request);
    let outbound = Outbound {
        surface,
        path: path.to_string(),
        body: redacted,
        stream: is_stream,
        dialect_headers: dialect_headers(&headers),
        input_estimate,
    };
    let snapshot = |route: &RouteEntry, sel_record| crate::lifecycle::ChatPostInvokeDeps {
        state: state.clone(),
        pii_redactor: pii_redactor.clone(),
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
            request_rules: preflight.request_rules.clone(),
            budget_caps: preflight.budget_caps.clone(),
        },
        route: crate::lifecycle::ChatPickedRoute {
            provider_name: route.provider_name.clone(),
            upstream_model: route.upstream_model.clone(),
            sel_record,
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
        let shaper = StreamShaper::new(mapped_model.clone(), &redaction, surface.dialect)
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
        &pii_redactor,
        &state.blob_store,
        &metadata.request_id,
        &request_for_audit,
        None,
    )
    .await;
    let (entry, completed, sel_record) =
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

    // Output guardrails run on the completion before PII is painted
    // back, so a placeholder cannot push a legitimate answer past a cap.
    let model_cfg = router.config_for(&mapped_model);
    if let Err(e) = crate::output_guardrails::apply_output_guardrails(
        &completed.body,
        surface.dialect,
        &model_cfg.output_guardrails,
    ) {
        finalize_health(&state, &sel_record, false).await;
        return Err(ctx.emit(e).into());
    }

    // Tool calls, on the whole answer before any of it has gone out. A
    // refusal is the gateway's policy, not the upstream failing, so the
    // route's health counts it as a success. Like an output-guardrail
    // refusal, the answer is neither cached nor billed.
    if let Some(e) = crate::tool_inspection::check_whole(
        &state.tool_inspection.load(),
        &state.audit,
        &crate::tool_inspection::Caller::of(&identity, &metadata.request_id, &mapped_model),
        &entry.provider_name,
        &completed.body,
    ) {
        finalize_health(&state, &sel_record, true).await;
        return Err(ctx.emit(e).into());
    }

    // Cache fill, audit, breaker and budget debit — the same hooks the
    // stream runs in its tail. The cache keeps the placeholder form.
    let deps = snapshot(entry, sel_record);
    let completed = run_buffered_post_invoke(&deps, completed).await;

    let (prompt, completion) = tokens(&completed.usage);
    let total = prompt + completion;
    if total > 0
        && let Err(e) = state.quota.consume(&quota_key, total).await
    {
        tracing::warn!("Failed to consume quota: {e}");
    }

    tracing::info!(
        request_id = %metadata.request_id,
        metadata = %metadata.to_json(),
        "Audit log: request completed"
    );

    let mut response = json_response(crate::pii_redactor::restore_body(
        &redaction,
        &completed.body,
    ));
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
}
