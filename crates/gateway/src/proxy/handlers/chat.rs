//! `POST /v1/chat/completions` — OpenAI-compatible chat completions
//! with cache + quota.

use std::convert::Infallible;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use rust_decimal::Decimal;

use super::super::body_capture::prepare_body_capture;
use super::super::headers::{request_id_header, resolve_session_id, resolve_trace_id};
use super::super::log_ctx::{LogCtx, emit_gateway_error_log, emit_gateway_log};
use super::super::pipeline::{launch_stream_pump, run_buffered_post_invoke, run_preflight_stages};
use super::super::routing::{
    build_selection_ctx, finalize_health, select_route_for_stream, select_route_with_failover,
    set_affinity,
};
use super::super::{GatewayErrorResponse, GatewayRequestIdentity, GatewayState};

use crate::content_filter::Action;
use crate::metadata::RequestMetadata;
use crate::pii_redactor::PiiStreamRestorer;
use crate::providers::traits::{ChatCompletionRequest, GatewayError};

use think_watch_common::audit::BodyCaptureStatus;

/// POST /v1/chat/completions
///
/// Proxies chat completion requests to the appropriate AI provider based
/// on the model name in the request body. Supports both streaming (SSE)
/// and non-streaming (JSON) modes.
///
/// Request pipeline:
/// 1. Model mapping (aliases)
/// 2. Enforce allowed_models from API key (if set)
/// 3. Content filter (prompt injection detection)
/// 4. Token quota check
/// 5. Cache lookup (non-streaming only)
/// 6. Route to provider
/// 7. On success: consume quota, store cache, return response
pub async fn proxy_chat_completion(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    Json(mut request): Json<ChatCompletionRequest>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    // 1. Apply model mapping
    request.model = state.model_mapper.map(&request.model);

    // Resolve trace_id and start clock up front so every early-return
    // path (allowed_models reject / preflight rate limit / content
    // filter block / route lookup miss) can emit a gateway_logs row
    // before bubbling. RequestMetadata::extract honors the same
    // x-trace-id header further down, so metadata.request_id ends up
    // matching trace_id.
    let trace_id = resolve_trace_id(&headers);
    let session_id = resolve_session_id(&headers);
    let request_started_at = std::time::Instant::now();
    let ctx = LogCtx::new(
        &state.audit,
        &identity,
        &trace_id,
        session_id.as_deref(),
        &request.model,
        request_started_at,
    );

    // 2. Pre-flight: rate-limit + budget peek + access control.
    //    Each stage emits its own audit row on short-circuit; the
    //    deny path stays uniform across all three AI surfaces.
    let preflight = run_preflight_stages(&state, &identity, &trace_id, &request.model).await?;

    // 4. Extract per-request metadata from headers and body
    let metadata = RequestMetadata::extract(&headers, &request);
    tracing::info!(
        request_id = %metadata.request_id,
        model = %metadata.model,
        tags = ?metadata.tags,
        "Request metadata extracted"
    );

    // 4. Content filter — check for prompt injection
    let content_filter = state.content_filter.load();
    if let Some(m) = content_filter.check(&request.messages) {
        // Log lines use `log_summary()` (no matched snippet) so user
        // prompt content doesn't tunnel into the centralized log
        // pipeline. The client-facing error still uses the full
        // Display form so the caller can see what triggered the rule
        // and adjust their prompt — that surface is the user's own
        // request body, so showing it back is not a leak.
        match m.action {
            Action::Block => {
                tracing::warn!("Content filter blocked request: {}", m.log_summary());
                return Err(ctx
                    .emit(GatewayError::TransformError(format!(
                        "Request blocked by content filter: {m}"
                    )))
                    .into());
            }
            Action::Warn => {
                tracing::warn!(
                    "Content filter warning (request allowed): {}",
                    m.log_summary()
                );
            }
            Action::Log => {
                tracing::info!("Content filter log: {}", m.log_summary());
            }
        }
    }

    // 5. Caller identity travels alongside the request, not inside it —
    //    see `CallCtx`. Built once here and cloned into each provider call.
    let call_ctx = crate::providers::traits::CallCtx::new(
        Some(trace_id.clone()),
        identity.user_id.clone(),
        identity.user_email.clone(),
    );

    // 6. PII redaction — redact user messages before sending upstream.
    //    Placeholders are stable (no per-request salt) so two callers
    //    sending structurally-identical prompts produce identical
    //    redacted bodies. The cache keys on the redacted form: same
    //    structure ⇒ same key ⇒ shared cache slot. The cache stores
    //    the *unrestored* response (with placeholders intact); each
    //    retrieving caller restores using their own redaction context
    //    on the way out. Two callers with different PII embedded
    //    inside the same prompt structure each see their own values
    //    on restoration — symmetric and correct because upstream
    //    only ever saw the placeholder.
    let pii_redactor = state.pii_redactor.load();
    // Snapshot the pre-redaction messages so the audit pipeline can
    // capture what the user actually authored. Upstream sees the
    // redacted form, but the audit row is the legal record of
    // intent: "user X asked Y, gateway sent placeholder-substituted
    // form upstream". If we logged the post-redaction shape, the
    // audit trail would be sanitized in a way the auditor can't
    // un-sanitize (placeholders use stable salts shared by every
    // caller with the same prompt). The clone is per-request and
    // bounded by `audit.body_max_bytes`.
    let messages_for_audit = request.messages.clone();
    let (redacted_messages, redaction_ctx) = pii_redactor.redact_messages(&request.messages);
    request.messages = redacted_messages;

    // 指纹在脱敏之后算：缓存存的是带占位符的那一版，取出来时按各自的
    // 上下文还原，所以两个调用方问同样的问题能共用一个槽
    let cache_fingerprint = crate::cache::ResponseCache::fingerprint(&request);

    // 7. Check token quota — use user/api_key as quota key when available.
    //
    // Key is `{id}:{client-requested model}`, NOT the upstream model the
    // router eventually selects. Users see and reason about the model
    // alias they typed (e.g. `gpt-4`); their quota dashboards group by
    // that alias too. If we keyed by `upstream_model`, an alias that
    // routes to two different upstreams would split a user's budget
    // across two counters and surprise them. Audit rows still log
    // `upstream_model` separately so operators can attribute capacity.
    let quota_key = identity
        .user_id
        .as_deref()
        .or(identity.api_key_id.as_deref())
        .map(|id| format!("{id}:{}", request.model))
        .unwrap_or_else(|| request.model.clone());
    if let Err(e) = state.quota.check_quota(&quota_key).await {
        tracing::warn!("Quota exceeded for {quota_key}: {e}");
        return Err(ctx
            .emit(GatewayError::ProviderError(format!("Quota exceeded: {e}")))
            .into());
    }

    let is_stream = request.stream.unwrap_or(false);

    // Cache lookup — semantic cache shared across all users.
    // Both streaming and non-streaming paths check cache; on a hit
    // for a streaming request we re-emit the assembled response as
    // a single-chunk SSE stream so the client gets the format it
    // asked for.
    //
    // Two contracts the lookup enforces:
    //
    // 1. **Key by pre-redaction content** so identical user-visible
    //    prompts collide on the same cache slot regardless of whose
    //    PII the prompt contained. The stored response carries
    //    redaction placeholders (`{{EMAIL_1}}` etc.) and we restore
    //    using THIS caller's redaction context on the way out. Two
    //    callers with identical pre-redaction text MUST share
    //    identical redaction contexts (the PII values come from the
    //    text itself), so cross-caller restoration is symmetric.
    //
    // 2. **Cache hits debit quota** the same way an upstream call
    //    would have. The traditional "cache hits are free" reading
    //    lets a user with a deterministic prompt amortise a single
    //    real call across an unbounded quota window — i.e. quota
    //    enforcement becomes optional. Debit the cached
    //    `usage.total_tokens` so monthly caps still bind.
    let cached = match &cache_fingerprint {
        Some(fp) => state.cache.get(fp).await,
        None => None,
    };
    if let Some(mut cached) = cached {
        metrics::counter!("gateway_cache_total", "result" => "hit").increment(1);
        tracing::debug!(model = %request.model, stream = is_stream, "Cache HIT");

        // (2) Quota — debit before serving the cached body so the user
        // can't trivially exceed their monthly cap through cached
        // round-trips. Quota errors here STILL serve the cached
        // response because we already passed the `check_quota` gate
        // at the top of the handler; treating consume as best-effort
        // matches the post-upstream path below.
        if let Some(ref usage) = cached.usage
            && let Err(e) = state.quota.consume(&quota_key, usage.total_tokens).await
        {
            tracing::warn!(
                quota_key = %quota_key,
                tokens = usage.total_tokens,
                "quota consume on cache hit failed: {e}"
            );
        }

        // (1) Restore PII for this caller using their own redaction
        // context. The cached response carries opaque placeholders;
        // each consumer paints in their own values.
        pii_redactor.restore_response(&mut cached, &redaction_ctx);

        // Cache hits previously bypassed `gateway_logs` entirely, so
        // the bastion's audit story had a hole — "user X called model
        // Y" showed nothing for any deterministic prompt repeat. Emit
        // a gateway row with status `from_cache` so the audit timeline
        // is complete; cost is 0 because no upstream tokens were
        // spent (the original miss already booked them, the cache hit
        // is free). Request body is the user's actual prompt; response
        // is the cached completion.
        let (cached_pt, cached_ct) = cached
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        // Cache-hit body capture goes through the SAME pipeline as
        // fresh requests — PII redaction toggle, byte-cap truncation,
        // and S3 offload all apply. The prior shortcut here called
        // `serde_json::to_string` directly which broke three contracts:
        // (1) `audit.body_redact_pii=true` was silently ignored for
        // cache hits, (2) a multi-MB cached response landed inline in
        // CH without truncation, (3) oversize cached responses never
        // offloaded to S3 even when configured. Override the status
        // back to `from_cache` afterwards so auditors can still tell
        // these rows apart from fresh captures.
        let mut cache_body_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &metadata.request_id,
            &messages_for_audit,
            Some(&cached),
        )
        .await;
        cache_body_capture.status = Some(BodyCaptureStatus::FromCache.as_str());
        emit_gateway_log(
            &state.audit,
            &metadata.request_id,
            session_id.as_deref(),
            identity.user_id.as_deref(),
            identity.user_email.as_deref(),
            identity.api_key_id.as_deref(),
            identity.api_key_lineage_id.as_deref(),
            identity.ip_address.as_deref(),
            &request.model,
            None,
            None,
            cached_pt,
            cached_ct,
            Decimal::ZERO,
            request_started_at.elapsed().as_millis() as i64,
            200,
            cache_body_capture,
        );

        if is_stream {
            // Re-emit as SSE: one data chunk with the full response + [DONE]
            let chunk_json = crate::streaming::serialize_sse_chunk(&cached);
            let body = async_stream::stream! {
                yield Ok::<axum::response::sse::Event, Infallible>(
                    axum::response::sse::Event::default().data(chunk_json),
                );
                yield Ok::<axum::response::sse::Event, Infallible>(
                    axum::response::sse::Event::default().data("[DONE]"),
                );
            };
            let mut response = axum::response::sse::Sse::new(body).into_response();
            response
                .headers_mut()
                .insert("X-Cache", axum::http::HeaderValue::from_static("HIT"));
            response.headers_mut().insert(
                "X-Metadata-Request-Id",
                request_id_header(&metadata.request_id),
            );
            return Ok(response);
        }
        let mut response = Json(&cached).into_response();
        response
            .headers_mut()
            .insert("X-Cache", axum::http::HeaderValue::from_static("HIT"));
        response.headers_mut().insert(
            "X-Metadata-Request-Id",
            request_id_header(&metadata.request_id),
        );
        return Ok(response);
    }
    metrics::counter!("gateway_cache_total", "result" => "miss").increment(1);

    // Route to provider — multi-route failover
    let mapped_model = request.model.clone();
    let router = state.router.load();
    let routes = router.route(&request.model).ok_or_else(|| {
        ctx.emit(GatewayError::ProviderError(format!(
            "No provider found for model: {}",
            request.model
        )))
    })?;

    if is_stream {
        // Select route (with affinity) for streaming — no retry after
        // first chunk, so pick the best candidate up front. Route-
        // lookup failures on the streaming branch deserve a
        // gateway_logs row just like the non-streaming bubble above
        // — operators debugging "my SSE stream never started" would
        // otherwise find zero trace events to correlate against.
        let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
        let (entry, sel_record) = select_route_for_stream(routes, &sel_ctx)
            .await
            .map_err(|e| GatewayErrorResponse::from(ctx.emit(e)))?;

        // Replace model with upstream_model if configured
        if let Some(ref upstream) = entry.upstream_model {
            request.model = upstream.clone();
        }

        set_affinity(
            &state.redis,
            identity.user_id.as_deref(),
            &mapped_model,
            sel_ctx.affinity_mode,
            entry,
            sel_ctx.affinity_ttl_secs,
        )
        .await;

        // Post-invoke pipeline owns audit emit + cache fill + breaker
        // accounting + budget debit for the streaming branch. The
        // detached task inside `launch_stream_pump` picks `deps` up
        // after the stream terminates.
        let deps = crate::lifecycle::ChatPostInvokeDeps {
            state: state.clone(),
            pii_redactor: pii_redactor.clone(),
            request: crate::lifecycle::ChatRequestSnapshot {
                identity: identity.clone(),
                trace_id: metadata.request_id.clone(),
                session_id: session_id.clone(),
                mapped_model: mapped_model.clone(),
                messages_for_audit: messages_for_audit.clone(),
                cache_fingerprint: cache_fingerprint.clone(),
                request_started_at,
            },
            preflight: crate::lifecycle::ChatPreflightLists {
                request_rules: preflight.request_rules.clone(),
                budget_caps: preflight.budget_caps.clone(),
            },
            route: crate::lifecycle::ChatPickedRoute {
                provider_name: entry.provider_name.clone(),
                upstream_model: entry.upstream_model.clone(),
                sel_record,
            },
            // Chat completions cache — both the buffered branch
            // and a successful stream fill the same slot.
            cache_enabled: true,
        };
        let pump_ctx =
            crate::lifecycle::ChatPumpContext::from_deps(&deps, request.messages.clone());
        let stream = super::super::protocol_relearn::open_stream_with_relearn(
            entry,
            request,
            call_ctx.clone(),
            state.db.clone(),
        );
        let stream_restorer = Some(PiiStreamRestorer::new(&redaction_ctx));
        Ok(launch_stream_pump(deps, pump_ctx, stream, stream_restorer))
    } else {
        // Non-streaming: full failover with retry across healthy candidates
        let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
        // Prepare the error-path body capture BEFORE select_route_with_failover
        // so the (synchronous) map_err closure can move it in without
        // needing to await. Error paths capture the request body only —
        // there's no response from any upstream that succeeded.
        let error_path_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &metadata.request_id,
            &messages_for_audit,
            None,
        )
        .await;
        let (chosen_entry, mut response, sel_record) =
            select_route_with_failover(routes, &request, &call_ctx, &sel_ctx)
                .await
                .map_err(|e| {
                    // select_route_with_failover just errored across every
                    // candidate — there's no winning provider to attribute
                    // this failure to, so provider stays None.
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
                        error_path_capture,
                    );
                    GatewayErrorResponse::from(e)
                })?;

        // Restore original model name in response (don't leak upstream_model)
        response.model = mapped_model.clone();

        // 8a-pre. Output guardrails — enforce per-model size / shape
        // caps on the assistant message before it reaches the caller.
        // Runs BEFORE PII restore because the rule operates on raw
        // completion text; running it after would let a redaction
        // placeholder push a legitimate completion past the cap.
        // Streaming guardrails would require buffering the whole
        // stream, which fights latency — non-streaming only for now.
        let model_cfg = router.config_for(&mapped_model);
        if let Err(e) = crate::output_guardrails::apply_output_guardrails(
            &response,
            &model_cfg.output_guardrails,
        ) {
            finalize_health(&state, &sel_record, false).await;
            return Err(ctx.emit(e).into());
        }

        // Run the buffered branch through the lifecycle's post-invoke
        // pipeline. The hooks own cache fill (pre-restore form, so
        // future cache hits can apply per-caller restoration), audit
        // emit, breaker accounting, and limits/budget debit — exactly
        // what the streaming branch above does, just synchronously.
        // Quota.consume + PII restore stay inline after the pipeline
        // because they need the response back in hand.
        let deps = crate::lifecycle::ChatPostInvokeDeps {
            state: state.clone(),
            pii_redactor: pii_redactor.clone(),
            request: crate::lifecycle::ChatRequestSnapshot {
                identity: identity.clone(),
                trace_id: metadata.request_id.clone(),
                session_id: session_id.clone(),
                mapped_model: mapped_model.clone(),
                messages_for_audit: messages_for_audit.clone(),
                cache_fingerprint: cache_fingerprint.clone(),
                request_started_at,
            },
            preflight: crate::lifecycle::ChatPreflightLists {
                request_rules: preflight.request_rules.clone(),
                budget_caps: preflight.budget_caps.clone(),
            },
            route: crate::lifecycle::ChatPickedRoute {
                provider_name: chosen_entry.provider_name.clone(),
                upstream_model: chosen_entry.upstream_model.clone(),
                sel_record,
            },
            cache_enabled: true,
        };
        let mut response = run_buffered_post_invoke(&deps, response).await;

        // Restore PII in the response (this caller's view). The cache
        // already stored the pre-restore form so a later caller can
        // paint their own values onto the placeholders.
        pii_redactor.restore_response(&mut response, &redaction_ctx);

        // Consume quota based on actual token usage. Independent of
        // the limits engine accounting that ran inside emit_audit.
        if let Some(ref usage) = response.usage {
            let total = usage.total_tokens;
            if let Err(e) = state.quota.consume(&quota_key, total).await {
                tracing::warn!("Failed to consume quota: {e}");
            }
        }

        tracing::info!(
            request_id = %metadata.request_id,
            metadata = %metadata.to_json(),
            "Audit log: request completed"
        );

        let mut http_response = Json(&response).into_response();
        http_response
            .headers_mut()
            .insert("X-Cache", axum::http::HeaderValue::from_static("MISS"));
        http_response.headers_mut().insert(
            "X-Metadata-Request-Id",
            request_id_header(&metadata.request_id),
        );
        Ok(http_response)
    }
}
