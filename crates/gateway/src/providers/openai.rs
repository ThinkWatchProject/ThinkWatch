use super::traits::*;
use crate::sse_parser::SseStreamExt;
use futures::stream::StreamExt;
use futures::Stream;
use std::pin::Pin;

pub struct OpenAiProvider {
    pub base_url: String,
    pub api_key: String,
    pub client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url,
            api_key,
            client: reqwest::Client::new(),
        }
    }
}

impl AiProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
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
                "OpenAI returned {status}: {body}"
            )));
        }

        resp.json::<ChatCompletionResponse>()
            .await
            .map_err(|e| GatewayError::ProviderError(e.to_string()))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        let client = self.client.clone();
        let url = format!("{}/v1/chat/completions", self.base_url);
        let api_key = self.api_key.clone();

        // Ensure stream is set to true in the outgoing request
        let mut request = request;
        request.stream = Some(true);

        Box::pin(async_stream::stream! {
            let resp = client
                .post(&url)
                .bearer_auth(&api_key)
                .json(&request)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    yield Err(GatewayError::NetworkError(e.to_string()));
                    return;
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                yield Err(GatewayError::ProviderError(format!(
                    "OpenAI returned {status}: {body}"
                )));
                return;
            }

            let mut event_stream = resp.bytes_stream().sse_events();

            while let Some(event_result) = event_stream.next().await {
                match event_result {
                    Ok(event) => {
                        let data = event.data.trim().to_string();

                        // OpenAI signals end of stream with [DONE]
                        if data == "[DONE]" {
                            break;
                        }

                        if data.is_empty() {
                            continue;
                        }

                        match serde_json::from_str::<ChatCompletionChunk>(&data) {
                            Ok(chunk) => yield Ok(chunk),
                            Err(e) => {
                                tracing::warn!("Failed to parse SSE chunk: {e}, data: {data}");
                                // Skip unparseable chunks rather than breaking the stream
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(GatewayError::ProviderError(format!(
                            "SSE stream error: {e}"
                        )));
                        break;
                    }
                }
            }
        })
    }
}
