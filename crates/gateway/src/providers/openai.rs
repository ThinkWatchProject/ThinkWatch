use super::traits::*;
use crate::sse_parser::SseStreamExt;
use futures::Stream;
use futures::stream::StreamExt;
use std::pin::Pin;

pub struct OpenAiProvider {
    pub base: ProviderBase,
}

impl OpenAiProvider {
    pub fn new(base_url: String) -> Self {
        Self {
            base: ProviderBase::new(base_url),
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.base = self.base.with_custom_headers(headers);
        self
    }

    fn completions_url(&self) -> String {
        format!("{}/v1/chat/completions", self.base.base_url)
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
        let builder = self.base.client.post(self.completions_url());
        let builder = self
            .base
            .apply_custom_headers(builder, &request)
            .json(&request);

        let resp = ProviderBase::send(builder).await?;
        let resp = ProviderBase::check_status(resp, "OpenAI").await?;

        resp.json::<ChatCompletionResponse>()
            .await
            .map_err(|e| GatewayError::ProviderError(e.to_string()))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        let client = self.base.client.clone();
        let url = self.completions_url();
        let custom_headers = self.base.resolve_headers(&request);

        // Ensure stream is set to true in the outgoing request
        let mut request = request;
        request.stream = Some(true);

        Box::pin(async_stream::stream! {
            let builder = ProviderBase::apply_headers(client.post(&url), &custom_headers)
                .json(&request);

            let resp = match ProviderBase::send(builder).await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };
            let resp = match ProviderBase::check_status(resp, "OpenAI").await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };

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
