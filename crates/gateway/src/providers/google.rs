use super::traits::*;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

pub struct GoogleProvider {
    pub base: ProviderBase,
}

impl GoogleProvider {
    pub fn new(base_url: String) -> Self {
        Self {
            base: ProviderBase::new(base_url),
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.base = self.base.with_custom_headers(headers);
        self
    }
}

// ---------- Gemini API types ----------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    prompt_token_count: Option<u32>,
    candidates_token_count: Option<u32>,
    total_token_count: Option<u32>,
}

// ---------- Conversion ----------

fn convert_request(req: &ChatCompletionRequest) -> GeminiRequest {
    let mut system_instruction: Option<GeminiContent> = None;
    let mut contents = Vec::new();

    for msg in &req.messages {
        let text = match &msg.content {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        if msg.role == "system" {
            system_instruction = Some(GeminiContent {
                role: None,
                parts: vec![GeminiPart { text: Some(text) }],
            });
        } else {
            let role = match msg.role.as_str() {
                "assistant" => "model",
                _ => "user",
            };
            contents.push(GeminiContent {
                role: Some(role.to_string()),
                parts: vec![GeminiPart { text: Some(text) }],
            });
        }
    }

    GeminiRequest {
        contents,
        system_instruction,
        generation_config: Some(GeminiGenerationConfig {
            temperature: req.temperature,
            max_output_tokens: req.max_tokens,
        }),
    }
}

fn convert_response(resp: GeminiResponse, model: &str) -> ChatCompletionResponse {
    let (text, finish_reason) = resp
        .candidates
        .and_then(|c| c.into_iter().next())
        .map(|c| {
            let text = c
                .content
                .and_then(|content| {
                    content
                        .parts
                        .into_iter()
                        .filter_map(|p| p.text)
                        .collect::<Vec<_>>()
                        .first()
                        .cloned()
                })
                .unwrap_or_default();
            let reason = c.finish_reason.map(|r| match r.as_str() {
                "STOP" => "stop".to_string(),
                "MAX_TOKENS" => "length".to_string(),
                other => other.to_lowercase(),
            });
            (text, reason)
        })
        .unwrap_or_default();

    let usage = resp.usage_metadata.map(|u| Usage {
        prompt_tokens: u.prompt_token_count.unwrap_or(0),
        completion_tokens: u.candidates_token_count.unwrap_or(0),
        total_tokens: u.total_token_count.unwrap_or(0),
    });

    ChatCompletionResponse {
        id: format!("gemini-{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp(),
        model: model.to_string(),
        choices: vec![Choice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String(text),
            },
            finish_reason,
        }],
        usage,
    }
}

// ---------- AiProvider ----------

impl AiProvider for GoogleProvider {
    fn name(&self) -> &str {
        "google"
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let model = request.model.clone();
        let gemini_req = convert_request(&request);

        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base.base_url, model
        );

        let builder = self
            .base
            .apply_custom_headers(self.base.client.post(&url), &request)
            .json(&gemini_req);

        let resp = ProviderBase::send(builder).await?;
        let resp = ProviderBase::check_status(resp, "Gemini").await?;

        let gemini_resp: GeminiResponse = resp
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(e.to_string()))?;

        Ok(convert_response(gemini_resp, &model))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        let client = self.base.client.clone();
        let base_url = self.base.base_url.clone();
        let model = request.model.clone();
        let headers = self.base.resolve_headers(&request);

        let gemini_req = convert_request(&request);

        Box::pin(async_stream::stream! {
            let url = format!(
                "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
                base_url, model
            );

            let builder = ProviderBase::apply_headers(client.post(&url), &headers).json(&gemini_req);

            let resp = match ProviderBase::send(builder).await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };
            let resp = match ProviderBase::check_status(resp, "Gemini").await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };

            use crate::sse_parser::SseStreamExt;
            use futures::StreamExt;
            let mut stream = resp.bytes_stream().sse_events();
            let chunk_id = format!("gemini-{}", uuid::Uuid::new_v4());

            while let Some(event) = stream.next().await {
                let event = match event {
                    Ok(e) => e,
                    Err(e) => {
                        yield Err(GatewayError::NetworkError(e.to_string()));
                        return;
                    }
                };

                let data = event.data.trim().to_string();
                if data.is_empty() || data == "[DONE]" {
                    break;
                }

                if let Ok(gemini_resp) = serde_json::from_str::<GeminiResponse>(&data) {
                    let text = gemini_resp
                        .candidates
                        .and_then(|c| c.into_iter().next())
                        .and_then(|c| c.content)
                        .and_then(|content| content.parts.into_iter().next())
                        .and_then(|p| p.text)
                        .unwrap_or_default();

                    yield Ok(ChatCompletionChunk {
                        id: chunk_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created: chrono::Utc::now().timestamp(),
                        model: model.clone(),
                        choices: vec![ChunkChoice {
                            index: 0,
                            delta: serde_json::json!({"content": text}),
                            finish_reason: None,
                        }],
                        usage: None,
                    });
                }
            }
        })
    }
}
