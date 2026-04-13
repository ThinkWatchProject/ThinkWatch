use super::openai::OpenAiProvider;
use super::traits::*;
use futures::Stream;
use std::pin::Pin;

/// Custom provider that proxies to any OpenAI-compatible endpoint.
pub struct CustomProvider {
    inner: OpenAiProvider,
    provider_name: String,
}

impl CustomProvider {
    pub fn new(name: String, base_url: String, api_key: String) -> Self {
        Self {
            inner: OpenAiProvider::new(base_url, api_key),
            provider_name: name,
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.inner = self.inner.with_custom_headers(headers);
        self
    }
}

impl AiProvider for CustomProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        self.inner.chat_completion(request).await
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        self.inner.stream_chat_completion(request)
    }
}
