//! `wiremock`-backed fakes for upstream AI providers (OpenAI,
//! Anthropic, Google, Bedrock, Azure). Each helper returns a
//! [`MockProvider`] you can hand to [`crate::fixtures::create_provider`]
//! to wire it into the gateway router.
//!
//! The bodies returned are deliberately minimal — they're enough for
//! the proxy's response parser to compute usage / cost without
//! pulling in the full upstream contract. Tests that need a richer
//! shape can mount additional `Mock` rules on the underlying
//! `MockServer` (exposed via `MockProvider::server`).

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A wiremock `MockServer` plus a tracker for invocation counts that
/// integration tests can inspect.
pub struct MockProvider {
    pub server: MockServer,
}

impl MockProvider {
    pub fn uri(&self) -> String {
        self.server.uri()
    }

    pub async fn received_requests(&self) -> Vec<wiremock::Request> {
        self.server.received_requests().await.unwrap_or_default()
    }

    /// Stand up an OpenAI-flavoured mock. `model` is the upstream
    /// model name used in the response body (the gateway echoes it
    /// to clients). The response includes a `usage` block so the
    /// cost tracker has tokens to log.
    pub async fn openai_chat_ok(model: &str) -> Self {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 1_700_000_000_i64,
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hello world"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [{"id": model, "object": "model"}]
            })))
            .mount(&server)
            .await;
        Self { server }
    }

    /// Stand up an OpenAI-flavoured streaming mock — emits SSE
    /// `data: {…}` chunks, finishing with `data: [DONE]\n\n`.
    pub async fn openai_chat_stream_ok(model: &str) -> Self {
        let server = MockServer::start().await;
        let chunks = openai_sse_chunks(model);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(chunks, "text/event-stream"))
            .mount(&server)
            .await;
        Self { server }
    }

    /// Anthropic Messages API non-streaming success.
    pub async fn anthropic_messages_ok(model: &str) -> Self {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [{"type": "text", "text": "hi"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 5, "output_tokens": 4}
            })))
            .mount(&server)
            .await;
        Self { server }
    }

    /// Generic upstream that always returns 500 — used to test the
    /// gateway's circuit-breaker / failover / retry logic.
    pub async fn always_500() -> Self {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "boom"}})),
            )
            .mount(&server)
            .await;
        Self { server }
    }

    /// Mount an arbitrary mock for tests that need a custom shape.
    pub async fn mount(&self, mock: Mock) {
        mock.mount(&self.server).await;
    }

    /// Convenience JSON helper.
    pub fn json(value: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(value)
    }
}

fn openai_sse_chunks(model: &str) -> Vec<u8> {
    let chunk = |delta: Value| {
        format!(
            "data: {}\n\n",
            json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 1_700_000_000_i64,
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
            })
        )
    };
    let final_chunk = format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_i64,
            "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 4, "total_tokens": 9}
        })
    );
    let mut buf = String::new();
    buf.push_str(&chunk(json!({"role": "assistant"})));
    buf.push_str(&chunk(json!({"content": "hi "})));
    buf.push_str(&chunk(json!({"content": "there"})));
    buf.push_str(&final_chunk);
    buf.push_str("data: [DONE]\n\n");
    buf.into_bytes()
}
