use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
    /// Caller identity for template header resolution. Not serialized
    /// to upstream — used only by the provider to resolve {{user_id}}
    /// and {{user_email}} in custom headers.
    #[serde(skip)]
    pub caller_user_id: Option<String>,
    #[serde(skip)]
    pub caller_user_email: Option<String>,
    /// Per-request trace id from the gateway entry layer. Forwarded
    /// to the upstream as `x-trace-id` so a single id correlates the
    /// downstream request, the gateway log, and the provider-side
    /// request trace (when the provider also threads it through).
    #[serde(skip)]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: serde_json::Value,
    /// Pass-through bucket for the rest of the OpenAI / Anthropic
    /// message envelope: `tool_call_id` (required when `role: "tool"`),
    /// `tool_calls` (assistant-side function invocations), `name`
    /// (legacy function-call / multi-user labelling), `refusal`,
    /// vendor annotations.
    ///
    /// Without this flatten, serde quietly drops anything we don't
    /// declare — the gateway then forwards a stripped message and
    /// the upstream 400s with `missing field "tool_call_id"` the
    /// first time the conversation uses tools, with no signal that
    /// the gateway ate the field on the way through.
    ///
    /// Construct with `..Default::default()` if only role + content
    /// matter so future additions to this struct don't ripple across
    /// every literal in the codebase.
    #[serde(flatten, default, skip_serializing_if = "is_empty_extras")]
    pub extra: serde_json::Value,
}

fn is_empty_extras(v: &serde_json::Value) -> bool {
    matches!(v, serde_json::Value::Null)
        || matches!(v, serde_json::Value::Object(o) if o.is_empty())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: serde_json::Value,
    pub finish_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Catch-all upstream failure that doesn't fit one of the more
    /// specific variants below. Prefer `ProviderHttpError` /
    /// `ProviderTimeout` / `ProviderInvalidResponse` when the cause
    /// is known so dashboards can split errors by class instead of
    /// regex'ing the message.
    #[error("Provider error: {0}")]
    ProviderError(String),
    /// Upstream returned a non-2xx, non-429, non-401 status. The
    /// status is kept structured so error-classifier metrics stay
    /// readable and the gateway can classify retry-eligible 5xx
    /// versus poison 4xx without parsing the message.
    #[error("Provider HTTP {status}: {message}")]
    ProviderHttpError { status: u16, message: String },
    /// Upstream took longer than the configured timeout. Distinct
    /// from a network drop because the request reached the upstream
    /// — only the response was missing in time.
    #[error("Provider timeout: {0}")]
    ProviderTimeout(String),
    /// Upstream responded but the body wasn't parseable as the
    /// expected schema (chat completion / messages / etc.). Almost
    /// always indicates an upstream incident or a model-specific
    /// quirk, and is poison for retries — failover should still
    /// happen but retry against the SAME upstream is pointless.
    #[error("Provider returned invalid response: {0}")]
    ProviderInvalidResponse(String),
    #[error("Request transform error: {0}")]
    TransformError(String),
    #[error("Network error: {0}")]
    NetworkError(String),
    /// Upstream returned 429. `retry_after_secs` captures the value
    /// parsed off the upstream's `Retry-After` header (delta-seconds
    /// form per RFC 7231) so we can echo it to our client and stop
    /// clients spinning into a tight retry loop while quota is still
    /// burning. `None` means the upstream didn't tell us — we pick a
    /// conservative default downstream.
    #[error("Rate limited by upstream")]
    UpstreamRateLimited { retry_after_secs: Option<u32> },
    #[error("Authentication failed with upstream")]
    UpstreamAuthError,
    /// Local rate limit / budget cap was hit. The String is the rule
    /// label so the response body can tell the caller WHICH limit
    /// fired (e.g. "user requests/5h", "api_key tokens/1d",
    /// "monthly budget"). Maps to 429 in `IntoResponse`.
    #[error("Rate limited: {0}")]
    LocalRateLimited(String),
}

impl GatewayError {
    /// Canonical HTTP status code for this error variant. Single source
    /// of truth shared between the response wire status
    /// (`GatewayErrorResponse::into_response`), the non-streaming log
    /// row writer, and the streaming `StreamOutcome::UpstreamError`
    /// path — drift between any of these would make the gateway_logs
    /// `status_code` field disagree with what the client saw, leading
    /// operators to chase phantom 502s for what was actually a 429.
    pub fn status_code(&self) -> i64 {
        match self {
            GatewayError::ProviderError(_) => 502,
            GatewayError::ProviderHttpError { status, .. } => i64::from(*status),
            GatewayError::ProviderTimeout(_) => 504,
            GatewayError::ProviderInvalidResponse(_) => 502,
            GatewayError::TransformError(_) => 400,
            GatewayError::NetworkError(_) => 502,
            GatewayError::UpstreamRateLimited { .. } | GatewayError::LocalRateLimited(_) => 429,
            GatewayError::UpstreamAuthError => 401,
        }
    }

    /// Short stable tag derived from the variant name. Used as a
    /// dashboard-friendly label (Prometheus value, gateway_logs
    /// `error_type` field). Never localize — operators grep on these.
    pub fn error_tag(&self) -> &'static str {
        match self {
            GatewayError::ProviderError(_) => "ProviderError",
            GatewayError::ProviderHttpError { .. } => "ProviderHttpError",
            GatewayError::ProviderTimeout(_) => "ProviderTimeout",
            GatewayError::ProviderInvalidResponse(_) => "ProviderInvalidResponse",
            GatewayError::TransformError(_) => "TransformError",
            GatewayError::NetworkError(_) => "NetworkError",
            GatewayError::UpstreamRateLimited { .. } => "UpstreamRateLimited",
            GatewayError::LocalRateLimited(_) => "LocalRateLimited",
            GatewayError::UpstreamAuthError => "UpstreamAuthError",
        }
    }

    /// Hint, in seconds, for `Retry-After` on a 429 response. For
    /// upstream limits we echo the upstream's own header when present;
    /// for local limits we fall back to a conservative 30s so naive
    /// clients don't spin into a tight retry loop while the bucket is
    /// still refilling. Capped at one hour to keep the header sane
    /// even when an upstream returns an absurd value.
    pub fn retry_after_secs(&self) -> Option<u32> {
        const HARD_CAP_SECS: u32 = 3600;
        const LOCAL_DEFAULT_SECS: u32 = 30;
        match self {
            GatewayError::UpstreamRateLimited { retry_after_secs } => {
                retry_after_secs.map(|s| s.min(HARD_CAP_SECS))
            }
            GatewayError::LocalRateLimited(_) => Some(LOCAL_DEFAULT_SECS),
            _ => None,
        }
    }
}

/// Parse RFC 7231 `Retry-After` (delta-seconds form). HTTP-date is
/// intentionally not supported — the absolute-time variant is
/// effectively unused by upstream LLM providers and would require
/// dragging in a date parser plus clock-skew handling for a vanishingly
/// rare path. Bad input silently maps to None, mirroring how a missing
/// header is treated; a malformed header is no better than no header.
pub fn parse_retry_after_seconds(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok()
}

/// Shared base for all AI providers. Holds the HTTP client, base URL,
/// and custom header templates. Previously each provider duplicated
/// these three fields and the identical `new()`, `with_custom_headers()`,
/// and `resolve_headers()` methods.
pub struct ProviderBase {
    pub base_url: String,
    pub client: reqwest::Client,
    pub custom_headers: Vec<(String, String)>,
}

impl ProviderBase {
    pub fn new(base_url: String) -> Self {
        // Wall-clock bounds on upstream HTTP. Without these a hung
        // upstream pins a connection forever; failover only retries
        // across routes, not within a stuck attempt. The 5-min total
        // is generous enough for slow LLM completions but cuts off
        // truly stuck calls; the 10s connect timeout is short because
        // a healthy upstream resolves and TCPs in well under that.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("reqwest client builder cannot fail on stable inputs");
        Self {
            base_url,
            client,
            custom_headers: Vec::new(),
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.custom_headers = headers;
        self
    }

    /// Resolve template variables (`{{user_id}}`, `{{user_email}}`) in
    /// custom header values using the caller identity from the request.
    pub fn resolve_headers(&self, request: &ChatCompletionRequest) -> Vec<(String, String)> {
        let uid = request.caller_user_id.as_deref().unwrap_or("");
        let email = request.caller_user_email.as_deref().unwrap_or("");
        self.custom_headers
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.replace("{{user_id}}", uid)
                        .replace("{{user_email}}", email),
                )
            })
            .collect()
    }

    /// Append the caller-resolved custom headers to a `RequestBuilder`.
    /// Centralizes what would otherwise be duplicated in every
    /// provider's `chat_completion` and `stream_chat_completion`.
    /// Also injects `x-trace-id` from the request's trace_id so the
    /// upstream log line and the gateway log line share a correlation
    /// id (OBS-01).
    pub fn apply_custom_headers(
        &self,
        builder: reqwest::RequestBuilder,
        request: &ChatCompletionRequest,
    ) -> reqwest::RequestBuilder {
        let mut builder = Self::apply_headers(builder, &self.resolve_headers(request));
        if let Some(ref trace_id) = request.trace_id {
            builder = builder.header("x-trace-id", trace_id.as_str());
        }
        builder
    }

    /// Append a pre-resolved header list to a `RequestBuilder`.
    /// Streaming providers resolve headers before spawning the
    /// `async_stream!` block (since `&self` can't cross the `'static`
    /// boundary) and call this from inside the stream.
    pub fn apply_headers(
        mut builder: reqwest::RequestBuilder,
        headers: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        builder
    }

    /// Validate an upstream response status and translate non-2xx
    /// outcomes into the canonical `GatewayError` variants
    /// (`UpstreamRateLimited` / `UpstreamAuthError` / `ProviderError`).
    /// On a 2xx response the response is returned unchanged so the
    /// caller can continue parsing the body.
    ///
    /// `provider_label` appears in the user-visible error message, so
    /// each provider passes its own friendly name (e.g. "OpenAI",
    /// "Anthropic", "Bedrock").
    pub async fn check_status(
        resp: reqwest::Response,
        provider_label: &str,
    ) -> Result<reqwest::Response, GatewayError> {
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Snag the upstream's own `Retry-After` (if it sent one)
            // so we can echo it to our client; without this, a client
            // with naive 3× retry policies just hammers the upstream
            // through the same quota window — observed in the field.
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after_seconds);
            return Err(GatewayError::UpstreamRateLimited { retry_after_secs });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(GatewayError::UpstreamAuthError);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GatewayError::ProviderError(format!(
                "{provider_label} returned {status}: {body}"
            )));
        }
        Ok(resp)
    }

    /// Wrap `RequestBuilder::send` to map transport-level failures into
    /// `GatewayError::NetworkError` so callers don't have to repeat the
    /// `.map_err(|e| GatewayError::NetworkError(e.to_string()))?` line.
    pub async fn send(builder: reqwest::RequestBuilder) -> Result<reqwest::Response, GatewayError> {
        builder
            .send()
            .await
            .map_err(|e| GatewayError::NetworkError(e.to_string()))
    }
}

pub trait AiProvider: Send + Sync {
    fn name(&self) -> &str;

    fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> impl std::future::Future<Output = Result<ChatCompletionResponse, GatewayError>> + Send;

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>;
}

#[cfg(test)]
mod chat_message_roundtrip_tests {
    use super::ChatMessage;

    /// Lock in the fix for "gateway eats `tool_call_id`". A `role:"tool"`
    /// reply MUST round-trip with its `tool_call_id` intact — without
    /// the `extra` flatten, serde silently drops the field and the
    /// upstream then 400s with "missing field `tool_call_id`".
    #[test]
    fn tool_call_id_roundtrips_through_flatten_extras() {
        let raw = serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_abc123",
            "content": "{\"result\":\"ok\"}"
        });
        let msg: ChatMessage = serde_json::from_value(raw).unwrap();
        let out = serde_json::to_value(&msg).unwrap();
        assert_eq!(out["role"], "tool");
        assert_eq!(out["tool_call_id"], "call_abc123");
        assert_eq!(out["content"], "{\"result\":\"ok\"}");
    }

    /// `assistant` messages with `tool_calls` (function-call style)
    /// also need flattening: the array of `{id, type, function}`
    /// triples must survive a round-trip so the upstream sees the
    /// same conversation the client built.
    #[test]
    fn assistant_tool_calls_roundtrip() {
        let raw = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_abc123",
                "type": "function",
                "function": { "name": "search", "arguments": "{}" }
            }]
        });
        let msg: ChatMessage = serde_json::from_value(raw.clone()).unwrap();
        let out = serde_json::to_value(&msg).unwrap();
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["tool_calls"], raw["tool_calls"]);
    }

    /// Plain `{role, content}` messages must serialize without a
    /// stray empty `extra` blob — otherwise upstreams strict about
    /// unknown fields would 400 on every vanilla user message.
    #[test]
    fn vanilla_message_has_no_extra_blob() {
        let msg = ChatMessage {
            role: "user".into(),
            content: serde_json::Value::String("hi".into()),
            ..Default::default()
        };
        let out = serde_json::to_value(&msg).unwrap();
        let obj = out.as_object().unwrap();
        assert_eq!(obj.len(), 2, "expected only role+content, got {obj:?}");
        assert!(obj.contains_key("role"));
        assert!(obj.contains_key("content"));
    }

    /// `name` (legacy function-call labelling, also used for
    /// multi-user scenarios) is another field operators have hit;
    /// pin it to the same passthrough.
    #[test]
    fn message_name_field_passes_through() {
        let raw = serde_json::json!({
            "role": "user",
            "name": "alice",
            "content": "hi"
        });
        let msg: ChatMessage = serde_json::from_value(raw).unwrap();
        let out = serde_json::to_value(&msg).unwrap();
        assert_eq!(out["name"], "alice");
    }
}
