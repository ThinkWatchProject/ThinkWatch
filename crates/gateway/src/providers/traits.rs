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
    #[error("Provider error: {0}")]
    ProviderError(String),
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
