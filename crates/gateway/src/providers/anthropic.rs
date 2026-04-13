use super::traits::*;
use crate::sse_parser::SseStreamExt;
use futures::Stream;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

pub struct AnthropicProvider {
    pub base_url: String,
    pub client: reqwest::Client,
    pub custom_headers: Vec<(String, String)>,
}

impl AnthropicProvider {
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

// ---------- Anthropic-native request/response types ----------

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    id: String,
    model: String,
    content: Vec<AnthropicContentBlock>,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// ---------- Anthropic SSE event types ----------

#[derive(Debug, Deserialize)]
struct MessageStartEvent {
    message: MessageStartMessage,
}

#[derive(Debug, Deserialize)]
struct MessageStartMessage {
    id: String,
    model: String,
    _usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct ContentBlockDeltaEvent {
    index: u32,
    delta: ContentBlockDelta,
}

#[derive(Debug, Deserialize)]
struct ContentBlockDelta {
    #[serde(rename = "type")]
    _delta_type: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaEvent {
    delta: MessageDelta,
    usage: Option<MessageDeltaUsage>,
}

#[derive(Debug, Deserialize)]
struct MessageDelta {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaUsage {
    output_tokens: u32,
}

// ---------- Conversion helpers ----------

fn convert_request(req: ChatCompletionRequest) -> AnthropicRequest {
    let mut system_text: Option<String> = None;
    let mut messages = Vec::new();

    for msg in &req.messages {
        if msg.role == "system" {
            // Extract system messages into the top-level system field
            let text = match &msg.content {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            system_text = Some(match system_text {
                Some(existing) => format!("{existing}\n{text}"),
                None => text,
            });
        } else {
            // Map roles: user->user, assistant->assistant, anything else pass through
            let role = match msg.role.as_str() {
                "user" => "user".to_string(),
                "assistant" => "assistant".to_string(),
                other => other.to_string(),
            };
            messages.push(AnthropicMessage {
                role,
                content: msg.content.clone(),
            });
        }
    }

    let max_tokens = req.max_tokens.unwrap_or(4096);

    AnthropicRequest {
        model: req.model.clone(),
        max_tokens,
        messages,
        system: system_text,
        temperature: req.temperature,
        stream: req.stream,
    }
}

fn convert_response(resp: AnthropicResponse) -> ChatCompletionResponse {
    let text = resp
        .content
        .iter()
        .filter_map(|block| {
            if block.content_type == "text" {
                block.text.clone()
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("");

    let finish_reason = resp.stop_reason.map(|r| match r.as_str() {
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "stop_sequence" => "stop".to_string(),
        other => other.to_string(),
    });

    let total = resp.usage.input_tokens + resp.usage.output_tokens;

    ChatCompletionResponse {
        id: resp.id,
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp(),
        model: resp.model,
        choices: vec![Choice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String(text),
            },
            finish_reason,
        }],
        usage: Some(Usage {
            prompt_tokens: resp.usage.input_tokens,
            completion_tokens: resp.usage.output_tokens,
            total_tokens: total,
        }),
    }
}

fn map_anthropic_stop_reason(reason: &str) -> String {
    match reason {
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "stop_sequence" => "stop".to_string(),
        other => other.to_string(),
    }
}

// ---------- AiProvider implementation ----------

impl AiProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let headers = self.resolve_headers(&request);
        let anthropic_req = convert_request(request);

        let mut builder = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        for (k, v) in &headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let resp = builder
            .json(&anthropic_req)
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
                "Anthropic returned {status}: {body}"
            )));
        }

        let anthropic_resp: AnthropicResponse = resp
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(e.to_string()))?;

        Ok(convert_response(anthropic_resp))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        let client = self.client.clone();
        let url = format!("{}/v1/messages", self.base_url);
        let headers = self.resolve_headers(&request);

        let mut anthropic_req = convert_request(request);
        anthropic_req.stream = Some(true);

        Box::pin(async_stream::stream! {
            let mut builder = client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json");
            for (k, v) in &headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            let resp = builder
                .json(&anthropic_req)
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
                    "Anthropic returned {status}: {body}"
                )));
                return;
            }

            let mut event_stream = resp.bytes_stream().sse_events();

            // Track message-level state from message_start
            let mut message_id = String::new();
            let mut model = String::new();

            while let Some(event_result) = event_stream.next().await {
                match event_result {
                    Ok(event) => {
                        let event_type = event.event.as_str();
                        let data = event.data.trim().to_string();

                        if data.is_empty() {
                            continue;
                        }

                        match event_type {
                            "message_start" => {
                                if let Ok(ev) = serde_json::from_str::<MessageStartEvent>(&data) {
                                    message_id = ev.message.id.clone();
                                    model = ev.message.model.clone();

                                    // Emit an initial chunk with role delta
                                    let chunk = ChatCompletionChunk {
                                        id: message_id.clone(),
                                        object: "chat.completion.chunk".to_string(),
                                        created: chrono::Utc::now().timestamp(),
                                        model: model.clone(),
                                        choices: vec![ChunkChoice {
                                            index: 0,
                                            delta: serde_json::json!({"role": "assistant"}),
                                            finish_reason: None,
                                        }],
                                        usage: None,
                                    };
                                    yield Ok(chunk);
                                }
                            }
                            "content_block_delta" => {
                                if let Ok(ev) = serde_json::from_str::<ContentBlockDeltaEvent>(&data)
                                    && let Some(text) = &ev.delta.text {
                                        let chunk = ChatCompletionChunk {
                                            id: message_id.clone(),
                                            object: "chat.completion.chunk".to_string(),
                                            created: chrono::Utc::now().timestamp(),
                                            model: model.clone(),
                                            choices: vec![ChunkChoice {
                                                index: ev.index,
                                                delta: serde_json::json!({"content": text}),
                                                finish_reason: None,
                                            }],
                                            usage: None,
                                        };
                                        yield Ok(chunk);
                                    }
                            }
                            "message_delta" => {
                                if let Ok(ev) = serde_json::from_str::<MessageDeltaEvent>(&data) {
                                    let finish_reason = ev.delta.stop_reason
                                        .as_deref()
                                        .map(map_anthropic_stop_reason);

                                    let usage = ev.usage.map(|u| Usage {
                                        prompt_tokens: 0,
                                        completion_tokens: u.output_tokens,
                                        total_tokens: u.output_tokens,
                                    });

                                    let chunk = ChatCompletionChunk {
                                        id: message_id.clone(),
                                        object: "chat.completion.chunk".to_string(),
                                        created: chrono::Utc::now().timestamp(),
                                        model: model.clone(),
                                        choices: vec![ChunkChoice {
                                            index: 0,
                                            delta: serde_json::json!({}),
                                            finish_reason,
                                        }],
                                        usage,
                                    };
                                    yield Ok(chunk);
                                }
                            }
                            "message_stop" => {
                                // Stream is complete
                                break;
                            }
                            // Ignore ping, content_block_start, content_block_stop, etc.
                            _ => {}
                        }
                    }
                    Err(e) => {
                        yield Err(GatewayError::ProviderError(format!(
                            "Anthropic SSE stream error: {e}"
                        )));
                        break;
                    }
                }
            }
        })
    }
}
