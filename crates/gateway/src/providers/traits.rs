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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: serde_json::Value,
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
    #[error("Rate limited by upstream")]
    UpstreamRateLimited,
    #[error("Authentication failed with upstream")]
    UpstreamAuthError,
    /// Local rate limit / budget cap was hit. The String is the rule
    /// label so the response body can tell the caller WHICH limit
    /// fired (e.g. "user requests/5h", "api_key tokens/1d",
    /// "monthly budget"). Maps to 429 in `IntoResponse`.
    #[error("Rate limited: {0}")]
    LocalRateLimited(String),
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
        Self {
            base_url,
            client: reqwest::Client::new(),
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
    pub fn apply_custom_headers(
        &self,
        builder: reqwest::RequestBuilder,
        request: &ChatCompletionRequest,
    ) -> reqwest::RequestBuilder {
        Self::apply_headers(builder, &self.resolve_headers(request))
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
            return Err(GatewayError::UpstreamRateLimited);
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
