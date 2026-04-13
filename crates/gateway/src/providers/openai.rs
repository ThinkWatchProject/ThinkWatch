use super::traits::*;
use crate::sse_parser::SseStreamExt;
use futures::Stream;
use futures::stream::StreamExt;
use std::pin::Pin;

pub struct OpenAiProvider {
    pub base_url: String,
    pub client: reqwest::Client,
    pub custom_headers: Vec<(String, String)>,
}

impl OpenAiProvider {
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

    fn resolve_headers(&self, request: &ChatCompletionRequest) -> Vec<(String, String)> {
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
}

impl AiProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let headers = self.resolve_headers(&request);
        let mut builder = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url));
        for (k, v) in &headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let resp = builder
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
        let headers = self.resolve_headers(&request);

        // Ensure stream is set to true in the outgoing request
        let mut request = request;
        request.stream = Some(true);

        Box::pin(async_stream::stream! {
            let mut builder = client
                .post(&url);
            for (k, v) in &headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            let resp = builder
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
