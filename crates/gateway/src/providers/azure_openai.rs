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
    pub base_url: String,
    pub api_key: String,
    pub api_version: String,
    pub client: reqwest::Client,
}

impl AzureOpenAiProvider {
    pub fn new(base_url: String, api_key: String, api_version: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            api_version: api_version.unwrap_or_else(|| "2024-12-01-preview".to_string()),
            client: reqwest::Client::new(),
        }
    }

    fn completions_url(&self, deployment: &str) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.base_url, deployment, self.api_version
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

        let resp = self
            .client
            .post(&url)
            .header("api-key", &self.api_key)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| GatewayError::NetworkError(e.to_string()))?;

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
                "Azure OpenAI returned {status}: {body}"
            )));
        }

        let response: ChatCompletionResponse = resp
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(e.to_string()))?;

        Ok(response)
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        let client = self.client.clone();
        let url = self.completions_url(&request.model);
        let api_key = self.api_key.clone();

        let mut stream_request = request;
        stream_request.stream = Some(true);

        Box::pin(async_stream::stream! {
            let resp = client
                .post(&url)
                .header("api-key", &api_key)
                .header("content-type", "application/json")
                .json(&stream_request)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    yield Err(GatewayError::NetworkError(e.to_string()));
                    return;
                }
            };

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                yield Err(GatewayError::ProviderError(format!(
                    "Azure OpenAI stream error: {body}"
                )));
                return;
            }

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
