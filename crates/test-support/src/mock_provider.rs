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

/// The events of a streamed Responses answer, "ok". It ends in
/// `response.completed` with usage of 23 input and 5 output tokens, or,
/// when `failed`, in `response.failed` with the error "The model crashed."
pub fn responses_answer(model: &str, failed: bool) -> String {
    let event = |kind: &str, mut v: Value| {
        v["type"] = json!(kind);
        format!("event: {kind}\ndata: {v}\n\n")
    };
    let last = if failed {
        event(
            "response.failed",
            json!({"response": {
                "id": "resp_1", "model": model, "status": "failed",
                "error": {"code": "server_error", "message": "The model crashed."}
            }}),
        )
    } else {
        event(
            "response.completed",
            json!({"response": {
                "id": "resp_1", "model": model, "status": "completed",
                "output": [{"type": "message", "id": "msg_1", "role": "assistant",
                            "content": [{"type": "output_text", "text": "ok"}]}],
                "usage": {"input_tokens": 23, "output_tokens": 5, "total_tokens": 28}
            }}),
        )
    };
    [
        event(
            "response.created",
            json!({"response": {"id": "resp_1", "model": model, "status": "in_progress"}}),
        ),
        event(
            "response.output_text.delta",
            json!({"item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "ok"}),
        ),
        last,
    ]
    .concat()
}

/// An upstream that answers every `POST {path}` with `frames` as an event
/// stream, then keeps the stream open for `linger` before it ends it — as
/// the ChatGPT Codex backend often does after its last event. Returns its
/// base URL.
///
/// wiremock sends a body in one piece and ends it, so this is a server
/// of its own.
pub async fn sse_upstream_lingering(
    path: &str,
    frames: String,
    linger: std::time::Duration,
) -> String {
    let answer = axum::routing::post(move || {
        let frames = frames.clone();
        async move {
            let body = async_stream::stream! {
                yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from(frames));
                tokio::time::sleep(linger).await;
            };
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(body))
                .unwrap()
        }
    });
    let app = axum::Router::new().route(path, answer);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
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
