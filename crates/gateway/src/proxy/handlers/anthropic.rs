//! `POST /v1/messages` — Anthropic Messages API passthrough.
//! Used by Claude Code and other tools that speak the Anthropic native
//! format. Internal pipeline routes via the same provider failover as
//! `/v1/chat/completions`; the response is then converted back to
//! Anthropic's wire shape.

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

/// POST /v1/messages
///
/// Anthropic Messages API passthrough. Used by Claude Code and other tools
/// that speak the Anthropic native format. Routes to the provider registered
/// for the requested model, forwarding the request as-is to the Anthropic
/// upstream (no format conversion needed).
///
/// This endpoint also applies content filtering, quota checks, and audit
/// logging, but does NOT do PII redaction or caching (complex content types).
pub async fn proxy_anthropic_messages(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<GatewayRequestIdentity>,
    Json(body): Json<serde_json::Value>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    // Honor x-trace-id when the caller pinned one — that's how a
    // client correlates this AI call with the MCP tools/call it
    // makes off the back of a tool-use response. Otherwise mint.
    let trace_id = resolve_trace_id(&headers);
    let session_id = resolve_session_id(&headers);
    let request_started_at = std::time::Instant::now();

    // Build LogCtx with model="(unknown)" up-front so a missing-model
    // body still emits an error row. We rebuild it once we know the
    // real model so subsequent emits attribute correctly.
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

    // Apply model mapping
    let mapped_model = state.model_mapper.map(&model);
    let ctx = LogCtx::new(
        &state.audit,
        &identity,
        &trace_id,
        session_id.as_deref(),
        &mapped_model,
        request_started_at,
    );

    // Pre-flight: rate-limit + budget peek + access control. Same
    // tower as `/v1/chat/completions` so a developer key can't dodge
    // their per-minute quota by switching surfaces.
    let preflight = run_preflight_stages(&state, &identity, &trace_id, &mapped_model).await?;

    // Content filter — check user messages
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        let chat_messages: Vec<crate::providers::traits::ChatMessage> = messages
            .iter()
            .filter_map(|m| {
                Some(crate::providers::traits::ChatMessage {
                    role: m.get("role")?.as_str()?.to_string(),
                    content: m.get("content").cloned().unwrap_or(serde_json::Value::Null),
                    ..Default::default()
                })
            })
            .collect();

        let content_filter = state.content_filter.load();
        if let Some(m) = content_filter.check(&chat_messages) {
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
    }

    // Route to provider — multi-route failover
    let router = state.router.load();
    let routes = router.route(&mapped_model).ok_or_else(|| {
        ctx.emit(GatewayError::ProviderError(format!(
            "No provider found for model: {mapped_model}"
        )))
    })?;

    // Convert to OpenAI format internally, let the provider handle the rest
    let max_tokens = body
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096) as u32;

    // Build a ChatCompletionRequest from the Anthropic body
    let mut messages = Vec::new();
    if let Some(system) = body.get("system").and_then(|v| v.as_str()) {
        messages.push(crate::providers::traits::ChatMessage {
            role: "system".to_string(),
            content: serde_json::Value::String(system.to_string()),
            ..Default::default()
        });
    }
    if let Some(msg_array) = body.get("messages").and_then(|v| v.as_array()) {
        for m in msg_array {
            if let (Some(role), Some(content)) =
                (m.get("role").and_then(|v| v.as_str()), m.get("content"))
            {
                messages.push(crate::providers::traits::ChatMessage {
                    role: role.to_string(),
                    content: content.clone(),
                    ..Default::default()
                });
            }
        }
    }

    // PII redaction — snapshot pre-redaction messages for the audit
    // body-capture pipeline. Same reasoning as the chat-completions
    // handler: the audit row needs to show what the user actually wrote.
    let pii_redactor = state.pii_redactor.load();
    let messages_for_audit = messages.clone();
    let (redacted_messages, redaction_ctx) = pii_redactor.redact_messages(&messages);

    let request = crate::providers::traits::ChatCompletionRequest {
        model: mapped_model.clone(),
        messages: redacted_messages,
        temperature: body.get("temperature").and_then(|v| v.as_f64()),
        max_tokens: Some(max_tokens),
        stream: Some(is_stream),
        extra: serde_json::json!({}),
        caller_user_id: identity.user_id.clone(),
        caller_user_email: identity.user_email.clone(),
        trace_id: Some(trace_id.clone()),
    };

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
        // shared design). Anthropic Messages does NOT cache —
        // `cache_enabled: false` — but otherwise the breaker / audit
        // / budget tail is identical.
        let deps = crate::lifecycle::ChatPostInvokeDeps {
            state: state.clone(),
            pii_redactor: pii_redactor.clone(),
            request: crate::lifecycle::ChatRequestSnapshot {
                identity: identity.clone(),
                trace_id: trace_id.clone(),
                session_id: session_id.clone(),
                mapped_model: mapped_model.clone(),
                messages_for_audit: messages_for_audit.clone(),
                request_for_cache: request.clone(),
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
        let stream = entry.provider.stream_chat_completion(stream_request);
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
            select_route_with_failover(routes, &request, &sel_ctx)
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

        // Output guardrails — same hook as the chat-completions surface;
        // see proxy_chat_completion for rationale on ordering vs. PII
        // restore and the streaming carve-out.
        let model_cfg = router.config_for(&mapped_model);
        if let Err(e) = crate::output_guardrails::apply_output_guardrails(
            &response,
            &model_cfg.output_guardrails,
        ) {
            finalize_health(&state, &sel_record, false).await;
            return Err(ctx.emit(e).into());
        }

        // Anthropic Messages doesn't cache (its buffered branch never
        // did, the streaming pump's `cache_enabled: false` matches).
        // Pipe the response through `run_post_invoke` so audit emit /
        // breaker accounting / budget debit share one site with the
        // chat completions surface.
        let deps = crate::lifecycle::ChatPostInvokeDeps {
            state: state.clone(),
            pii_redactor: pii_redactor.clone(),
            request: crate::lifecycle::ChatRequestSnapshot {
                identity: identity.clone(),
                trace_id: trace_id.clone(),
                session_id: session_id.clone(),
                mapped_model: mapped_model.clone(),
                messages_for_audit: messages_for_audit.clone(),
                request_for_cache: request.clone(),
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

        pii_redactor.restore_response(&mut response, &redaction_ctx);

        // Convert OpenAI response back to Anthropic format
        let anthropic_response = convert_to_anthropic_response(&response);
        let mut http_response = Json(anthropic_response).into_response();
        if let Ok(v) = trace_id.parse() {
            http_response.headers_mut().insert("x-trace-id", v);
        }
        Ok(http_response)
    }
}

/// Convert an OpenAI-format response back to Anthropic Messages API format.
fn convert_to_anthropic_response(
    resp: &crate::providers::traits::ChatCompletionResponse,
) -> serde_json::Value {
    let content: Vec<serde_json::Value> = resp
        .choices
        .iter()
        .map(|c| {
            let text = c.message.content.as_str().unwrap_or("").to_string();
            serde_json::json!({
                "type": "text",
                "text": text,
            })
        })
        .collect();

    let stop_reason = resp
        .choices
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .map(|r| match r {
            "stop" => "end_turn",
            "length" => "max_tokens",
            other => other,
        })
        .unwrap_or("end_turn");

    let (input_tokens, output_tokens) = resp
        .usage
        .as_ref()
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));

    serde_json::json!({
        "id": resp.id,
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    })
}
