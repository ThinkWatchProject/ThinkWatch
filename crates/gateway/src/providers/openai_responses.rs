//! OpenAI **Responses** API adapter (`POST {base}/v1/responses`).
//!
//! Distinct from [`super::openai::OpenAiProvider`], which speaks Chat
//! Completions. Some upstreams expose a model on one dialect and not
//! the other — an aggregator that answers `/v1/responses` for its newer
//! models while 400-ing them on `/v1/chat/completions` is exactly the
//! case `UpstreamProtocol` exists to route around.
//!
//! Note the symmetry with `proxy::handlers::responses`, which converts
//! the *inbound* Responses format into the internal request. This is
//! the outbound half of the same mapping.

use super::traits::*;
use crate::sse_parser::SseStreamExt;
use futures::Stream;
use futures::stream::StreamExt;
use std::pin::Pin;

pub struct OpenAiResponsesProvider {
    pub base: ProviderBase,
}

impl OpenAiResponsesProvider {
    pub fn new(base_url: String) -> Self {
        Self {
            base: ProviderBase::new(base_url),
        }
    }

    pub fn with_custom_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.base = self.base.with_custom_headers(headers);
        self
    }

    fn responses_url(&self) -> String {
        format!("{}/v1/responses", self.base.base_url)
    }
}

/// Internal request → Responses request body.
///
/// `messages` map onto `input` items one-for-one; the Responses API
/// accepts the same `{role, content}` shape Chat Completions uses, so
/// tool calls and multimodal content parts ride through untouched in
/// `ChatMessage::extra` / `content`.
fn convert_request(request: &ChatCompletionRequest, stream: bool) -> serde_json::Value {
    let input: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            let mut item = serde_json::json!({ "role": m.role, "content": m.content });
            // Preserve tool_call_id / tool_calls / name / refusal —
            // the same passthrough contract `ChatMessage::extra` has
            // on the Chat Completions path.
            if let (Some(obj), Some(extra)) = (item.as_object_mut(), m.extra.as_object()) {
                for (k, v) in extra {
                    obj.insert(k.clone(), v.clone());
                }
            }
            item
        })
        .collect();

    let mut body = serde_json::json!({
        "model": request.model,
        "input": input,
    });
    let obj = body.as_object_mut().expect("just built as an object");
    if stream {
        obj.insert("stream".into(), serde_json::Value::Bool(true));
    }
    if let Some(t) = request.temperature {
        obj.insert("temperature".into(), serde_json::json!(t));
    }
    // Responses renamed the cap; Chat Completions' `max_tokens` is not
    // accepted and upstreams reject it as an unknown field.
    if let Some(m) = request.max_tokens {
        obj.insert("max_output_tokens".into(), serde_json::json!(m));
    }
    // Anything the caller sent that we don't model (tools, reasoning
    // effort, structured-output schemas) rides through as-is — dropping
    // it silently is how a gateway breaks tool use.
    if let Some(extra) = request.extra.as_object() {
        for (k, v) in extra {
            if k == "max_tokens" || k == "stream" {
                continue;
            }
            obj.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    body
}

/// Concatenate every `output_text` part of a Responses reply. Reasoning
/// items and tool calls carry no `output_text`, so they contribute
/// nothing here and stay out of the assistant message body.
fn extract_output_text(resp: &serde_json::Value) -> String {
    let Some(output) = resp.get("output").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut text = String::new();
    for item in output {
        let Some(parts) = item.get("content").and_then(|v| v.as_array()) else {
            continue;
        };
        for part in parts {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                text.push_str(t);
            }
        }
    }
    text
}

/// Responses reply → internal response.
fn convert_response(resp: serde_json::Value, request_model: &str) -> ChatCompletionResponse {
    let usage = resp.get("usage").map(|u| {
        let prompt = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let completion = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: u
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or((prompt + completion) as u64) as u32,
        }
    });

    ChatCompletionResponse {
        id: resp
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        object: "chat.completion".to_string(),
        created: resp
            .get("created_at")
            .and_then(|v| v.as_i64())
            .unwrap_or_default(),
        model: resp
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(request_model)
            .to_string(),
        choices: vec![Choice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String(extract_output_text(&resp)),
                ..Default::default()
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage,
    }
}

impl AiProvider for OpenAiResponsesProvider {
    fn name(&self) -> &str {
        "openai_responses"
    }

    async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let body = convert_request(&request, false);
        let builder = self
            .base
            .client
            .post(self.responses_url())
            .header("content-type", "application/json");
        let builder = self.base.apply_custom_headers(builder, &ctx).json(&body);

        let resp = ProviderBase::send(builder).await?;
        let resp = ProviderBase::check_status(resp, "OpenAI Responses").await?;
        let raw: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| GatewayError::ProviderError(e.to_string()))?;
        Ok(convert_response(raw, &request.model))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
        ctx: CallCtx,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        let client = self.base.client.clone();
        let url = self.responses_url();
        let headers = self.base.resolve_headers(&ctx);
        let model = request.model.clone();
        let body = convert_request(&request, true);

        Box::pin(async_stream::stream! {
            let builder = client
                .post(&url)
                .header("content-type", "application/json");
            let builder = ProviderBase::apply_headers(builder, &headers).json(&body);

            let resp = match ProviderBase::send(builder).await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };
            let resp = match ProviderBase::check_status(resp, "OpenAI Responses").await {
                Ok(r) => r,
                Err(e) => { yield Err(e); return; }
            };

            let mut event_stream = resp.bytes_stream().sse_events();
            // The Responses stream carries the id on the first event
            // only; later deltas reference it implicitly, so thread it
            // through for the chunks we synthesize.
            let mut response_id = String::new();

            while let Some(event_result) = event_stream.next().await {
                let event = match event_result {
                    Ok(e) => e,
                    Err(e) => { yield Err(GatewayError::ProviderError(e.to_string())); return; }
                };
                let data = event.data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                if response_id.is_empty()
                    && let Some(id) = parsed
                        .get("response")
                        .and_then(|r| r.get("id"))
                        .and_then(|v| v.as_str())
                {
                    response_id = id.to_string();
                }

                match event.event.as_str() {
                    "response.output_text.delta" => {
                        let Some(delta) = parsed.get("delta").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        yield Ok(ChatCompletionChunk {
                            id: response_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created: 0,
                            model: model.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: serde_json::json!({"content": delta}),
                                finish_reason: None,
                            }],
                            usage: None,
                        });
                    }
                    // Terminal event — carries the final usage totals,
                    // which the accounting stage needs to bill the call.
                    "response.completed" => {
                        let usage = parsed
                            .get("response")
                            .and_then(|r| r.get("usage"))
                            .map(|u| {
                                let prompt = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let completion = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                Usage {
                                    prompt_tokens: prompt,
                                    completion_tokens: completion,
                                    total_tokens: u
                                        .get("total_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or((prompt + completion) as u64) as u32,
                                }
                            });
                        yield Ok(ChatCompletionChunk {
                            id: response_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created: 0,
                            model: model.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: serde_json::json!({}),
                                finish_reason: Some("stop".to_string()),
                            }],
                            usage,
                        });
                        return;
                    }
                    "response.failed" | "error" => {
                        let message = parsed
                            .get("response")
                            .and_then(|r| r.get("error"))
                            .or_else(|| parsed.get("error"))
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "upstream reported a failed response".to_string());
                        yield Err(GatewayError::ProviderError(message));
                        return;
                    }
                    _ => {}
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("hi".to_string()),
                ..Default::default()
            }],
            temperature: None,
            max_tokens: Some(16),
            stream: None,
            extra: serde_json::json!({"tools": [{"type": "web_search"}]}),
        }
    }

    #[test]
    fn request_uses_responses_field_names_and_keeps_unmodelled_fields() {
        let body = convert_request(&req("gpt-5"), false);
        // `max_tokens` is not a Responses field — upstreams reject it.
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["max_output_tokens"], 16);
        assert_eq!(body["input"][0]["role"], "user");
        // Tool definitions must survive the hop or tool use silently breaks.
        assert_eq!(body["tools"][0]["type"], "web_search");
    }

    #[test]
    fn response_text_parts_concatenate_into_the_assistant_message() {
        let raw = serde_json::json!({
            "id": "resp_1",
            "model": "gpt-5",
            "created_at": 42,
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "he"},
                    {"type": "output_text", "text": "llo"}
                ]}
            ],
            "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
        });
        let converted = convert_response(raw, "fallback");
        assert_eq!(converted.model, "gpt-5");
        assert_eq!(converted.choices[0].message.content, "hello");
        let usage = converted.usage.unwrap();
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (3, 2));
    }

    #[test]
    fn missing_usage_and_output_degrade_to_an_empty_answer_not_a_panic() {
        let converted = convert_response(serde_json::json!({"id": "resp_2"}), "fallback");
        assert_eq!(converted.model, "fallback");
        assert_eq!(converted.choices[0].message.content, "");
        assert!(converted.usage.is_none());
    }
}
