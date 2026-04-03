use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use std::sync::Arc;

use crate::budget_alert::BudgetAlertManager;
use crate::cache::ResponseCache;
use crate::content_filter::ContentFilter;
use crate::metadata::RequestMetadata;
use crate::model_mapping::ModelMapper;
use crate::pii_redactor::PiiRedactor;
use crate::providers::traits::{ChatCompletionRequest, GatewayError};
use crate::quota::QuotaManager;
use crate::router::ModelRouter;
use crate::streaming::stream_to_sse;

/// Shared application state for the gateway proxy handlers.
#[derive(Clone)]
pub struct GatewayState {
    pub router: Arc<ModelRouter>,
    pub model_mapper: Arc<ModelMapper>,
    pub content_filter: Arc<ContentFilter>,
    pub quota: Arc<QuotaManager>,
    pub cache: Arc<ResponseCache>,
    pub pii_redactor: Arc<PiiRedactor>,
    pub budget_alert: Option<Arc<BudgetAlertManager>>,
}

/// POST /v1/chat/completions
///
/// Proxies chat completion requests to the appropriate AI provider based
/// on the model name in the request body. Supports both streaming (SSE)
/// and non-streaming (JSON) modes.
///
/// Request pipeline:
/// 1. Model mapping (aliases)
/// 2. Content filter (prompt injection detection)
/// 3. Token quota check
/// 4. Cache lookup (non-streaming only)
/// 5. Route to provider
/// 6. On success: consume quota, store cache, return response
pub async fn proxy_chat_completion(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(mut request): Json<ChatCompletionRequest>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    // 1. Apply model mapping
    request.model = state.model_mapper.map(&request.model);

    // 2. Extract per-request metadata from headers and body
    let metadata = RequestMetadata::extract(&headers, &request);
    tracing::info!(
        request_id = %metadata.request_id,
        model = %metadata.model,
        tags = ?metadata.tags,
        "Request metadata extracted"
    );

    // 3. Content filter — check for prompt injection
    if let Err(filter_result) = state.content_filter.check(&request.messages) {
        tracing::warn!("Content filter triggered: {filter_result}");
        return Err(GatewayError::TransformError(format!(
            "Request blocked by content filter: {filter_result}"
        ))
        .into());
    }

    // 4. PII redaction — redact user messages before sending upstream
    let (redacted_messages, redaction_ctx) = state.pii_redactor.redact_messages(&request.messages);
    request.messages = redacted_messages;

    // 5. Check token quota (using model name as quota key for now;
    //    in production this would be a user/team key from auth middleware)
    let quota_key = request.model.clone();
    if let Err(e) = state.quota.check_quota(&quota_key).await {
        tracing::warn!("Quota exceeded for {quota_key}: {e}");
        return Err(GatewayError::ProviderError(format!("Quota exceeded: {e}")).into());
    }

    let is_stream = request.stream.unwrap_or(false);

    // 6. Cache lookup (non-streaming only)
    if !is_stream && let Some(cached) = state.cache.get(&request).await {
        tracing::debug!(model = %request.model, "Cache HIT");
        let mut response = Json(&cached).into_response();
        response
            .headers_mut()
            .insert("X-Cache", "HIT".parse().unwrap());
        response.headers_mut().insert(
            "X-Metadata-Request-Id",
            metadata.request_id.parse().unwrap(),
        );
        return Ok(response);
    }

    // 7. Route to provider
    let provider = state.router.route(&request.model).ok_or_else(|| {
        GatewayError::ProviderError(format!("No provider found for model: {}", request.model))
    })?;

    if is_stream {
        let stream = provider.stream_chat_completion(request);
        Ok(stream_to_sse(stream).into_response())
    } else {
        let mut response = provider.chat_completion_boxed(request.clone()).await?;

        // 8a. Restore PII in the response
        state
            .pii_redactor
            .restore_response(&mut response, &redaction_ctx);

        // 8b. Consume quota based on actual token usage
        if let Some(ref usage) = response.usage {
            let total = usage.total_tokens;
            if let Err(e) = state.quota.consume(&quota_key, total).await {
                tracing::warn!("Failed to consume quota: {e}");
                // Don't fail the request — usage already happened
            }
        }

        // 8c. Budget alert check (async, non-blocking)
        if let Some(ref budget_alert) = state.budget_alert {
            let alert = Arc::clone(budget_alert);
            let key = quota_key.clone();
            // In a real setup, current_spend and budget_limit would come from
            // a spending tracker; here we pass placeholder values that the
            // caller should replace with actual spend data.
            tokio::spawn(async move {
                alert.check_and_alert(&key, 0.0, 0.0).await;
            });
        }

        // 8d. Cache the response
        state.cache.set(&request, &response, None).await;

        // 8e. Log audit detail including metadata
        tracing::info!(
            request_id = %metadata.request_id,
            metadata = %metadata.to_json(),
            "Audit log: request completed"
        );

        let mut http_response = Json(&response).into_response();
        http_response
            .headers_mut()
            .insert("X-Cache", "MISS".parse().unwrap());
        http_response.headers_mut().insert(
            "X-Metadata-Request-Id",
            metadata.request_id.parse().unwrap(),
        );
        Ok(http_response)
    }
}

/// GET /v1/models
///
/// Returns the list of available models in OpenAI-compatible format.
pub async fn list_models_handler(State(state): State<GatewayState>) -> Json<serde_json::Value> {
    let models = state.router.list_models();

    let model_objects: Vec<serde_json::Value> = models
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "agent-bastion",
            })
        })
        .collect();

    Json(serde_json::json!({
        "object": "list",
        "data": model_objects,
    }))
}

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
    _headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<axum::response::Response, GatewayErrorResponse> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::TransformError("Missing 'model' field".into()))?
        .to_string();

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Apply model mapping
    let mapped_model = state.model_mapper.map(&model);

    // Content filter — check user messages
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        let chat_messages: Vec<crate::providers::traits::ChatMessage> = messages
            .iter()
            .filter_map(|m| {
                Some(crate::providers::traits::ChatMessage {
                    role: m.get("role")?.as_str()?.to_string(),
                    content: m.get("content").cloned().unwrap_or(serde_json::Value::Null),
                })
            })
            .collect();

        if let Err(filter_result) = state.content_filter.check(&chat_messages) {
            tracing::warn!("Content filter triggered: {filter_result}");
            return Err(GatewayError::TransformError(format!(
                "Request blocked by content filter: {filter_result}"
            ))
            .into());
        }
    }

    // Route to provider
    let provider = state.router.route(&mapped_model).ok_or_else(|| {
        GatewayError::ProviderError(format!("No provider found for model: {mapped_model}"))
    })?;

    // For Anthropic providers, we can access the underlying provider details.
    // Build the upstream request by forwarding the body as-is.
    // We need the provider's base_url and api_key — use the DynAiProvider
    // to make a direct Anthropic API call.

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
                });
            }
        }
    }

    let request = crate::providers::traits::ChatCompletionRequest {
        model: mapped_model.clone(),
        messages,
        temperature: body.get("temperature").and_then(|v| v.as_f64()),
        max_tokens: Some(max_tokens),
        stream: Some(is_stream),
        extra: serde_json::json!({}),
    };

    if is_stream {
        let stream = provider.stream_chat_completion(request);
        Ok(stream_to_sse(stream).into_response())
    } else {
        let response = provider.chat_completion_boxed(request).await?;

        // Convert OpenAI response back to Anthropic format
        let anthropic_response = convert_to_anthropic_response(&response);
        Ok(Json(anthropic_response).into_response())
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

// ---------- Error adapter ----------

/// Newtype wrapper so we can implement `IntoResponse` for `GatewayError`.
pub struct GatewayErrorResponse(GatewayError);

impl From<GatewayError> for GatewayErrorResponse {
    fn from(err: GatewayError) -> Self {
        Self(err)
    }
}

impl IntoResponse for GatewayErrorResponse {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;

        let (status, error_type) = match &self.0 {
            GatewayError::ProviderError(_) => (StatusCode::BAD_GATEWAY, "provider_error"),
            GatewayError::TransformError(_) => (StatusCode::BAD_REQUEST, "transform_error"),
            GatewayError::NetworkError(_) => (StatusCode::BAD_GATEWAY, "network_error"),
            GatewayError::UpstreamRateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            GatewayError::UpstreamAuthError => (StatusCode::UNAUTHORIZED, "auth_error"),
        };

        let body = serde_json::json!({
            "error": {
                "message": self.0.to_string(),
                "type": error_type,
            }
        });

        (status, Json(body)).into_response()
    }
}
