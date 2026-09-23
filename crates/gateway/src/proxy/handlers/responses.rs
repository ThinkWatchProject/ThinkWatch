//! `POST /v1/responses` — OpenAI Responses API (new format, 2025+).
//! Supports tool use, multi-turn, and structured outputs natively.
//! ThinkWatch proxies this by converting to internal
//! ChatCompletionRequest format, routing through the same provider
//! pipeline, then converting the response back.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;

use super::super::body_capture::prepare_body_capture;
use super::super::headers::{resolve_session_id, resolve_trace_id};
use super::super::log_ctx::{LogCtx, emit_gateway_error_log};
use super::super::pipeline::{launch_stream_pump, run_buffered_post_invoke, run_preflight_stages};
use super::super::routing::{
    build_selection_ctx, finalize_health, select_route_for_stream, select_route_with_failover,
    set_affinity,
};
use super::super::{GatewayErrorResponse, GatewayRequestIdentity, GatewayState};

use crate::content_filter::Action;
use crate::pii_redactor::PiiStreamRestorer;
use crate::providers::traits::GatewayError;

/// POST /v1/responses
///
/// OpenAI Responses API (new format, 2025+). Supports tool use, multi-turn,
/// and structured outputs natively. ThinkWatch proxies this by converting
/// to internal ChatCompletionRequest format, routing through the same
/// provider pipeline, then converting the response back.
///
/// For providers that support the Responses API natively (OpenAI), this
/// could be a direct passthrough in the future.
pub async fn proxy_responses(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    Json(body): Json<serde_json::Value>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    let trace_id = resolve_trace_id(&headers);
    let session_id = resolve_session_id(&headers);
    let request_started_at = std::time::Instant::now();

    let early_ctx = LogCtx::new(
        &state.audit,
        &identity,
        &trace_id,
        session_id.as_deref(),
        "(unknown)",
        request_started_at,
    );
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            early_ctx.emit(GatewayError::TransformError("Missing 'model' field".into()))
        })?
        .to_string();

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mapped_model = state.model_mapper.map(&model);
    let ctx = LogCtx::new(
        &state.audit,
        &identity,
        &trace_id,
        session_id.as_deref(),
        &mapped_model,
        request_started_at,
    );

    // Pre-flight: rate-limit + budget peek + access control — see
    // `proxy_anthropic_messages` for the cross-surface symmetry
    // rationale.
    let preflight = run_preflight_stages(&state, &identity, &trace_id, &mapped_model).await?;

    // Extract messages from the "input" field (Responses API format)
    // Input can be a string or an array of messages
    let mut messages = Vec::new();

    if let Some(instructions) = body.get("instructions").and_then(|v| v.as_str()) {
        messages.push(crate::providers::traits::ChatMessage {
            role: "system".to_string(),
            content: serde_json::Value::String(instructions.to_string()),
            ..Default::default()
        });
    }

    match body.get("input") {
        Some(serde_json::Value::String(s)) => {
            messages.push(crate::providers::traits::ChatMessage {
                role: "user".to_string(),
                content: serde_json::Value::String(s.clone()),
                ..Default::default()
            });
        }
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                // Each item can be a message object or a string
                if let Some(s) = item.as_str() {
                    messages.push(crate::providers::traits::ChatMessage {
                        role: "user".to_string(),
                        content: serde_json::Value::String(s.to_string()),
                        ..Default::default()
                    });
                } else if let (Some(role), Some(content)) = (
                    item.get("role").and_then(|v| v.as_str()),
                    item.get("content"),
                ) {
                    messages.push(crate::providers::traits::ChatMessage {
                        role: role.to_string(),
                        content: content.clone(),
                        ..Default::default()
                    });
                }
            }
        }
        _ => {
            return Err(ctx
                .emit(GatewayError::TransformError(
                    "Missing or invalid 'input' field".into(),
                ))
                .into());
        }
    }

    // Content filter
    let content_filter = state.content_filter.load();
    if let Some(m) = content_filter.check(&messages) {
        match m.action {
            Action::Block => {
                tracing::warn!("Content filter blocked request: {m}");
                return Err(ctx
                    .emit(GatewayError::TransformError(format!(
                        "Request blocked by content filter: {m}"
                    )))
                    .into());
            }
            Action::Warn => tracing::warn!("Content filter warning: {m}"),
            Action::Log => tracing::info!("Content filter log: {m}"),
        }
    }

    // PII redaction — same pipeline the chat-completions and Anthropic
    // surfaces use, so /v1/responses doesn't leak emails / phones / IDs
    // upstream just because it's the third-class endpoint. Streaming
    // restoration runs through PiiStreamRestorer below; non-streaming
    // restoration runs against the converted response right before we
    // hand it back to the client.
    let pii_redactor = state.pii_redactor.load();
    let messages_for_audit = messages.clone();
    let (redacted_messages, redaction_ctx) = pii_redactor.redact_messages(&messages);

    let max_tokens = body
        .get("max_output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096) as u32;

    let request = crate::providers::traits::ChatCompletionRequest {
        model: mapped_model.clone(),
        messages: redacted_messages,
        temperature: body.get("temperature").and_then(|v| v.as_f64()),
        max_tokens: Some(max_tokens),
        stream: Some(is_stream),
        extra: serde_json::json!({}),
    };

    // Caller identity travels alongside the request, not inside it — see `CallCtx`.
    let call_ctx = crate::providers::traits::CallCtx::new(
        Some(trace_id.clone()),
        identity.user_id.clone(),
        identity.user_email.clone(),
    );

    // Route to provider — multi-route failover
    let router = state.router.load();
    let routes = router.route(&mapped_model).ok_or_else(|| {
        ctx.emit(GatewayError::ProviderError(format!(
            "No provider found for model: {mapped_model}"
        )))
    })?;

    let cache_fingerprint = crate::cache::ResponseCache::fingerprint(&request);

    if is_stream {
        let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
        let (entry, sel_record) = select_route_for_stream(routes, &sel_ctx)
            .await
            .map_err(|e| GatewayErrorResponse::from(ctx.emit(e)))?;

        let mut stream_request = request.clone();
        if let Some(ref upstream) = entry.upstream_model {
            stream_request.model = upstream.clone();
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

        // Post-invoke pipeline (see `proxy_chat_completion` for the
        // shared design). Responses (like Anthropic Messages) does
        // NOT cache — `cache_enabled: false`.
        let deps = crate::lifecycle::ChatPostInvokeDeps {
            state: state.clone(),
            pii_redactor: pii_redactor.clone(),
            request: crate::lifecycle::ChatRequestSnapshot {
                identity: identity.clone(),
                trace_id: trace_id.clone(),
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
            cache_enabled: false,
        };
        let pump_ctx =
            crate::lifecycle::ChatPumpContext::from_deps(&deps, request.messages.clone());
        let stream = super::super::protocol_relearn::open_stream_with_relearn(
            entry,
            stream_request,
            call_ctx.clone(),
            state.db.clone(),
        );
        // Stitch placeholders back together as chunks stream through.
        // Same restorer the chat-completions surface uses; no-op when
        // redaction_ctx is empty so the feature-off path stays free.
        let stream_restorer = Some(PiiStreamRestorer::new(&redaction_ctx));
        let mut http_response = launch_stream_pump(deps, pump_ctx, stream, stream_restorer);
        if let Ok(v) = trace_id.parse() {
            http_response.headers_mut().insert("x-trace-id", v);
        }
        Ok(http_response)
    } else {
        let sel_ctx = build_selection_ctx(&state, &mapped_model, identity.user_id.as_deref()).await;
        // See chat-completions handler for rationale: pre-prepare the
        // error-path body capture so the synchronous map_err closure
        // can move it in.
        let error_path_capture = prepare_body_capture(
            &state.dynamic_config,
            &pii_redactor,
            &state.blob_store,
            &trace_id,
            &messages_for_audit,
            None,
        )
        .await;
        let (chosen_entry, mut response, sel_record) =
            select_route_with_failover(routes, &request, &call_ctx, &sel_ctx)
                .await
                .map_err(|e| {
                    emit_gateway_error_log(
                        &state.audit,
                        &trace_id,
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

        // Restore original model name
        response.model = mapped_model.clone();

        // Output guardrails — see proxy_chat_completion for ordering.
        let model_cfg = router.config_for(&mapped_model);
        if let Err(e) = crate::output_guardrails::apply_output_guardrails(
            &response,
            &model_cfg.output_guardrails,
        ) {
            finalize_health(&state, &sel_record, false).await;
            return Err(ctx.emit(e).into());
        }

        // OpenAI Responses doesn't cache (same as Anthropic Messages
        // — its buffered branch never did, the streaming pump's
        // `cache_enabled: false` matches). Pipe through
        // `run_post_invoke` so audit emit / breaker accounting /
        // budget debit share one site with the other two surfaces.
        let deps = crate::lifecycle::ChatPostInvokeDeps {
            state: state.clone(),
            pii_redactor: pii_redactor.clone(),
            request: crate::lifecycle::ChatRequestSnapshot {
                identity: identity.clone(),
                trace_id: trace_id.clone(),
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
            cache_enabled: false,
        };
        let mut response = run_buffered_post_invoke(&deps, response).await;

        // Restore PII placeholders so the converted response carries
        // the original user data the model echoed back.
        pii_redactor.restore_response(&mut response, &redaction_ctx);

        let responses_format = convert_to_responses_format(&response);
        let mut http_response = Json(responses_format).into_response();
        if let Ok(v) = trace_id.parse() {
            http_response.headers_mut().insert("x-trace-id", v);
        }
        Ok(http_response)
    }
}

/// Convert an internal ChatCompletionResponse to OpenAI Responses API format.
fn convert_to_responses_format(
    resp: &crate::providers::traits::ChatCompletionResponse,
) -> serde_json::Value {
    let mut output = Vec::new();

    for choice in &resp.choices {
        let text = choice.message.content.as_str().unwrap_or("").to_string();
        output.push(serde_json::json!({
            "type": "message",
            "id": format!("msg_{}", uuid::Uuid::new_v4()),
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": text,
            }],
        }));
    }

    let (input_tokens, output_tokens) = resp
        .usage
        .as_ref()
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));

    serde_json::json!({
        "id": resp.id,
        "object": "response",
        "created_at": resp.created,
        "status": "completed",
        "model": resp.model,
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    })
}
