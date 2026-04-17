use super::traits::*;
use crate::sse_parser::SseStreamExt;
use futures::Stream;
use futures::stream::StreamExt;
use std::pin::Pin;

/// Azure OpenAI provider.
///
/// Uses the same request/response format as OpenAI, but with a different
/// URL structure and authentication header.
///
/// URL pattern: `{base_url}/openai/deployments/{model}/chat/completions?api-version={api_version}`
/// Auth header: `api-key: {api_key}` (not `Authorization: Bearer`)
pub struct AzureOpenAiProvider {
    pub base: ProviderBase,
    pub api_version: String,
}

impl AzureOpenAiProvider {
    pub fn new(base_url: String, api_version: Option<String>) -> Self {
        let mut base = ProviderBase::new(base_url);
        base.base_url = base.base_url.trim_end_matches('/').to_string();
        Self {
            base,
            api_version: api_version.unwrap_or_else(|| "2024-12-01-preview".to_string()),
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.base = self.base.with_custom_headers(headers);
        self
    }

    fn completions_url(&self, deployment: &str) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.base.base_url, deployment, self.api_version
        )
    }
}

impl AiProvider for AzureOpenAiProvider {
    fn name(&self) -> &str {
        "azure_openai"
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        // In Azure, the "model" field is the deployment name
        let url = self.completions_url(&request.model);
        let builder = self
            .base
            .client
            .post(&url)
            .header("content-type", "application/json");
        let builder = self
            .base
            .apply_custom_headers(builder, &request)
            .json(&request);

        let resp = ProviderBase::send(builder).await?;
        let resp = ProviderBase::check_status(resp, "Azure OpenAI").await?;

        resp.json::<ChatCompletionResponse>()
            .await
            .map_err(|e| GatewayError::ProviderError(e.to_string()))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        let client = self.base.client.clone();
        let url = self.completions_url(&request.model);
        let headers = self.base.resolve_headers(&request);

        let mut stream_request = request;
        stream_request.stream = Some(true);

        Box::pin(async_stream::stream! {
            let builder = client.post(&url).header("content-type", "application/json");
            let builder = ProviderBase::apply_headers(builder, &headers).json(&stream_request);

            let resp = match ProviderBase::send(builder).await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };
            let resp = match ProviderBase::check_status(resp, "Azure OpenAI").await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };

            let mut event_stream = resp.bytes_stream().sse_events();

            while let Some(event_result) = event_stream.next().await {
                match event_result {
                    Ok(event) => {
                        let data = event.data.trim();
                        if data == "[DONE]" {
                            break;
                        }
                        if data.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<ChatCompletionChunk>(data) {
                            Ok(chunk) => yield Ok(chunk),
                            Err(e) => {
                                tracing::warn!("Failed to parse Azure OpenAI chunk: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(GatewayError::ProviderError(format!(
                            "Azure OpenAI SSE error: {e}"
                        )));
                        break;
                    }
                }
            }
        })
    }
}
