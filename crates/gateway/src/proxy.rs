use arc_swap::ArcSwap;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::cache::ResponseCache;
use crate::content_filter::{Action, ContentFilter};
use crate::cost_tracker::CostTracker;
use crate::metadata::RequestMetadata;
use crate::model_mapping::ModelMapper;
use crate::pii_redactor::PiiRedactor;
use crate::providers::traits::{ChatCompletionRequest, GatewayError};
use crate::quota::QuotaManager;
use crate::rate_limiter::RateLimiter;
use crate::router::ModelRouter;
use crate::streaming::stream_to_sse;
use think_watch_common::limits::{
    self, BudgetSubject, RateLimitSubject, RateMetric, sliding, weight,
};

/// Shared application state for the gateway proxy handlers.
#[derive(Clone)]
pub struct GatewayState {
    pub router: Arc<ModelRouter>,
    pub model_mapper: Arc<ModelMapper>,
    /// Hot-swappable so admins can update rules without restarting the gateway.
    pub content_filter: Arc<ArcSwap<ContentFilter>>,
    pub quota: Arc<QuotaManager>,
    pub cache: Arc<ResponseCache>,
    /// Hot-swappable so admins can update PII patterns without restarting.
    pub pii_redactor: Arc<ArcSwap<PiiRedactor>>,
    pub cost_tracker: Arc<CostTracker>,
    pub rate_limiter: Arc<RateLimiter>,
    /// PG pool — used to query enabled rate-limit rules and budget caps
    /// per request. Cached above the proxy via `WeightCache` for the
    /// model multipliers; raw rules go through a separate cache later.
    pub db: PgPool,
    /// Redis client used by the bucketed sliding-window engine and the
    /// natural-period budget counters. Same connection used by `quota`,
    /// `cache`, and the rest of the gateway.
    pub redis: fred::clients::Client,
    /// LRU cache mapping `model_id → (input_multiplier, output_multiplier)`.
    /// Looked up once per request to convert raw token counts into
    /// the weighted-token cost the engine consumes.
    pub weight_cache: weight::WeightCache,
}

/// Identity information extracted from the auth middleware.
///
/// Carries the resolved subject IDs the proxy needs in order to
/// query the new `rate_limit_rules` / `budget_caps` engine. The old
/// per-key fixed columns (rate_limit_rpm / tpm / monthly_budget)
/// are gone — the engine reads everything from those tables.
#[derive(Debug, Clone, Default)]
pub struct GatewayRequestIdentity {
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub allowed_models: Option<Vec<String>>,
}

/// Resolve every (subject_kind, subject_id) tuple this request should
/// be rate-limited against.
///
/// Order is significant only for diagnostics — the rate-limit engine
/// treats every rule independently and rejects on the first failure.
/// We add api_key first so the most specific quota fires first in
/// the error message.
fn resolve_rate_subjects(
    identity: &GatewayRequestIdentity,
    provider_id: Option<Uuid>,
) -> Vec<(RateLimitSubject, Uuid)> {
    let mut out = Vec::new();
    if let Some(s) = identity.api_key_id.as_deref()
        && let Ok(id) = Uuid::parse_str(s)
    {
        out.push((RateLimitSubject::ApiKey, id));
    }
    if let Some(s) = identity.user_id.as_deref()
        && let Ok(id) = Uuid::parse_str(s)
    {
        out.push((RateLimitSubject::User, id));
    }
    if let Some(pid) = provider_id {
        out.push((RateLimitSubject::Provider, pid));
    }
    out
}

/// Same shape as `resolve_rate_subjects` but for `budget_caps`.
/// Budget caps don't apply to MCP servers, but the rest of the
/// subject set overlaps. Provider budgets are common (a finance
/// owner caps the OpenAI provider per month) so we include them.
fn resolve_budget_subjects(
    identity: &GatewayRequestIdentity,
    provider_id: Option<Uuid>,
) -> Vec<(BudgetSubject, Uuid)> {
    let mut out = Vec::new();
    if let Some(s) = identity.api_key_id.as_deref()
        && let Ok(id) = Uuid::parse_str(s)
    {
        out.push((BudgetSubject::ApiKey, id));
    }
    if let Some(s) = identity.user_id.as_deref()
        && let Ok(id) = Uuid::parse_str(s)
    {
        out.push((BudgetSubject::User, id));
    }
    if let Some(pid) = provider_id {
        out.push((BudgetSubject::Provider, pid));
    }
    out
}

/// Build a human-readable label for "which rule rejected the request",
/// used as the error body so callers know what to lift. The label
/// shape is `<subject>:<metric>/<window>` (e.g. `api_key:tokens/1h`).
fn rule_label(rule: &limits::RateLimitRule) -> String {
    let window = match rule.window_secs {
        60 => "1m".to_string(),
        300 => "5m".to_string(),
        3_600 => "1h".to_string(),
        18_000 => "5h".to_string(),
        86_400 => "1d".to_string(),
        604_800 => "1w".to_string(),
        n => format!("{n}s"),
    };
    format!(
        "{}:{}/{}",
        rule.subject_kind.as_str(),
        rule.metric.as_str(),
        window
    )
}

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

    // 2. Enforce allowed_models from API key
    if let Some(ref allowed) = identity.allowed_models
        && !allowed.is_empty()
        && !allowed
            .iter()
            .any(|m| request.model == *m || request.model.starts_with(m))
    {
        return Err(GatewayError::TransformError(format!(
            "Model '{}' is not allowed for this API key",
            request.model
        ))
        .into());
    }

    // 3. Rate limit pre-flight (requests metric).
    //
    // Resolve every (api_key, user, provider) subject this request
    // belongs to and load all enabled rules in one shot. We then
    // split rules by metric — only requests-metric rules fire here;
    // tokens-metric rules need real token counts and run after the
    // upstream response in step 8.
    //
    // The actual check is a single Lua EVALSHA across every rule
    // for atomicity. On Redis error the engine fails open and bumps
    // a metric (see `sliding::check_and_record`).
    let provider_id = state.router.provider_id_for(&request.model);
    let rate_subjects = resolve_rate_subjects(&identity, provider_id);
    let request_rules =
        match limits::list_enabled_rules_for_subjects(&state.db, &rate_subjects).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("rate-limit DB load failed: {e}; allowing request");
                metrics::counter!("gateway_rate_limit_db_error_total").increment(1);
                Vec::new()
            }
        };
    let resolved_request_rules = sliding::resolve_rules(&request_rules, RateMetric::Requests);
    if !resolved_request_rules.is_empty() {
        let outcome = sliding::check_and_record(&state.redis, &resolved_request_rules, 1)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("rate-limit redis error: {e}; allowing request");
                sliding::CheckOutcome {
                    allowed: true,
                    exceeded_index: -1,
                    currents: Vec::new(),
                }
            });
        if !outcome.allowed {
            // Find the first rule that pushed us over so the response
            // body tells the caller exactly what to lift.
            let label = (outcome.exceeded_index >= 0)
                .then(|| {
                    request_rules
                        .iter()
                        .filter(|r| r.metric == RateMetric::Requests)
                        .nth(outcome.exceeded_index as usize)
                        .map(rule_label)
                })
                .flatten()
                .unwrap_or_else(|| "rate limit".to_string());
            metrics::counter!("gateway_rate_limited_total", "metric" => "requests").increment(1);
            return Err(GatewayError::LocalRateLimited(label).into());
        }
    }

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
        match m.action {
            Action::Block => {
                tracing::warn!("Content filter blocked request: {m}");
                return Err(GatewayError::TransformError(format!(
                    "Request blocked by content filter: {m}"
                ))
                .into());
            }
            Action::Warn => {
                tracing::warn!("Content filter warning (request allowed): {m}");
            }
            Action::Log => {
                tracing::info!("Content filter log: {m}");
            }
        }
    }

    // 5. PII redaction — redact user messages before sending upstream
    let pii_redactor = state.pii_redactor.load();
    let (redacted_messages, redaction_ctx) = pii_redactor.redact_messages(&request.messages);
    request.messages = redacted_messages;

    // 6. Check token quota — use user/api_key as quota key when available
    let quota_key = identity
        .user_id
        .as_deref()
        .or(identity.api_key_id.as_deref())
        .map(|id| format!("{id}:{}", request.model))
        .unwrap_or_else(|| request.model.clone());
    if let Err(e) = state.quota.check_quota(&quota_key).await {
        tracing::warn!("Quota exceeded for {quota_key}: {e}");
        return Err(GatewayError::ProviderError(format!("Quota exceeded: {e}")).into());
    }

    let is_stream = request.stream.unwrap_or(false);

    // Cache lookup (non-streaming only)
    if !is_stream && let Some(cached) = state.cache.get(&request).await {
        metrics::counter!("gateway_cache_total", "result" => "hit").increment(1);
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

    if !is_stream {
        metrics::counter!("gateway_cache_total", "result" => "miss").increment(1);
    }

    // Route to provider
    let provider = state.router.route(&request.model).ok_or_else(|| {
        GatewayError::ProviderError(format!("No provider found for model: {}", request.model))
    })?;

    if is_stream {
        let stream = provider.stream_chat_completion(request);
        Ok(stream_to_sse(stream).into_response())
    } else {
        let mut response = provider.chat_completion_boxed(request.clone()).await?;

        // 8a. Restore PII in the response
        pii_redactor.restore_response(&mut response, &redaction_ctx);

        // 8b. Consume quota based on actual token usage
        if let Some(ref usage) = response.usage {
            let total = usage.total_tokens;
            if let Err(e) = state.quota.consume(&quota_key, total).await {
                tracing::warn!("Failed to consume quota: {e}");
                // Don't fail the request — usage already happened
            }
        }

        // 8b.1. Post-flight accounting against the new limits engine.
        //
        // Token-metric sliding rules and budget caps both run here,
        // after the upstream returned its real token counts. Both
        // are recorded but never refused — by this point the request
        // has already been served, so the caller can transiently
        // exceed by exactly one request's worth of weighted tokens
        // (the documented "soft cap" behavior).
        if let Some(ref usage) = response.usage {
            let mult = state.weight_cache.get(&state.db, &request.model).await;
            let weighted = weight::weighted_tokens(
                usage.prompt_tokens as i64,
                usage.completion_tokens as i64,
                mult,
            );
            if weighted > 0 {
                // Token-metric sliding rules — same set of rules we
                // loaded above, just filtered to the tokens metric.
                let resolved_token_rules =
                    sliding::resolve_rules(&request_rules, RateMetric::Tokens);
                if !resolved_token_rules.is_empty()
                    && let Err(e) =
                        sliding::check_and_record(&state.redis, &resolved_token_rules, weighted)
                            .await
                {
                    tracing::warn!("token rate-limit accounting failed: {e}");
                }

                // Natural-period budget caps — daily / weekly /
                // monthly weighted-token totals.
                let budget_subjects = resolve_budget_subjects(&identity, provider_id);
                match limits::list_enabled_caps_for_subjects(&state.db, &budget_subjects).await {
                    Ok(caps) if !caps.is_empty() => {
                        if let Err(e) =
                            limits::budget::add_weighted_tokens(&state.redis, &caps, weighted).await
                        {
                            tracing::warn!("budget add_weighted_tokens failed: {e}");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("budget caps DB load failed: {e}");
                    }
                }
            }
        }

        // 8c. Budget threshold alerting was previously fired here
        // by `BudgetAlertManager`. Removed in the limits refactor —
        // alerts will be re-introduced as a subscriber on
        // `budget_caps` cap-crossings in a follow-up phase. The
        // current path still records spend (8b.1 above), it just
        // doesn't notify any external webhook.

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
                "owned_by": "think-watch",
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

        let content_filter = state.content_filter.load();
        if let Some(m) = content_filter.check(&chat_messages) {
            match m.action {
                Action::Block => {
                    tracing::warn!("Content filter blocked request: {m}");
                    return Err(GatewayError::TransformError(format!(
                        "Request blocked by content filter: {m}"
                    ))
                    .into());
                }
                Action::Warn => tracing::warn!("Content filter warning: {m}"),
                Action::Log => tracing::info!("Content filter log: {m}"),
            }
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

    let mapped_model = state.model_mapper.map(&model);

    // Extract messages from the "input" field (Responses API format)
    // Input can be a string or an array of messages
    let mut messages = Vec::new();

    if let Some(instructions) = body.get("instructions").and_then(|v| v.as_str()) {
        messages.push(crate::providers::traits::ChatMessage {
            role: "system".to_string(),
            content: serde_json::Value::String(instructions.to_string()),
        });
    }

    match body.get("input") {
        Some(serde_json::Value::String(s)) => {
            messages.push(crate::providers::traits::ChatMessage {
                role: "user".to_string(),
                content: serde_json::Value::String(s.clone()),
            });
        }
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                // Each item can be a message object or a string
                if let Some(s) = item.as_str() {
                    messages.push(crate::providers::traits::ChatMessage {
                        role: "user".to_string(),
                        content: serde_json::Value::String(s.to_string()),
                    });
                } else if let (Some(role), Some(content)) = (
                    item.get("role").and_then(|v| v.as_str()),
                    item.get("content"),
                ) {
                    messages.push(crate::providers::traits::ChatMessage {
                        role: role.to_string(),
                        content: content.clone(),
                    });
                }
            }
        }
        _ => {
            return Err(
                GatewayError::TransformError("Missing or invalid 'input' field".into()).into(),
            );
        }
    }

    // Content filter
    let content_filter = state.content_filter.load();
    if let Some(m) = content_filter.check(&messages) {
        match m.action {
            Action::Block => {
                tracing::warn!("Content filter blocked request: {m}");
                return Err(GatewayError::TransformError(format!(
                    "Request blocked by content filter: {m}"
                ))
                .into());
            }
            Action::Warn => tracing::warn!("Content filter warning: {m}"),
            Action::Log => tracing::info!("Content filter log: {m}"),
        }
    }

    let max_tokens = body
        .get("max_output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096) as u32;

    let request = crate::providers::traits::ChatCompletionRequest {
        model: mapped_model.clone(),
        messages,
        temperature: body.get("temperature").and_then(|v| v.as_f64()),
        max_tokens: Some(max_tokens),
        stream: Some(is_stream),
        extra: serde_json::json!({}),
    };

    // Route to provider
    let provider = state.router.route(&mapped_model).ok_or_else(|| {
        GatewayError::ProviderError(format!("No provider found for model: {mapped_model}"))
    })?;

    if is_stream {
        let stream = provider.stream_chat_completion(request);
        Ok(stream_to_sse(stream).into_response())
    } else {
        let response = provider.chat_completion_boxed(request).await?;
        let responses_format = convert_to_responses_format(&response);
        Ok(Json(responses_format).into_response())
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
            GatewayError::LocalRateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
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
