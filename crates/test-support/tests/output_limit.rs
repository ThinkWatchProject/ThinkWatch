//! A model's length cap (`output_guardrails: [{"type": "max_length"}]`)
//! at the gateway, on every surface a caller can use.
//!
//! A whole answer over the cap is withheld and replaced by an error. A
//! stream is measured as it goes: the frame that crosses the cap is not
//! sent, what came before it is, and the stream ends with an error in the
//! caller's own format — for a Gemini caller without `alt=sse`, as the
//! last element of a well-formed JSON array. The cap counts bytes.
//!
//! The upstream streams "hi " then "there" (8 bytes) and answers whole
//! with "hello world" (11 bytes); a cap of 4 lets "hi " through and cuts
//! at "there".

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use think_watch_test_support::prelude::*;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// A key, and `model` routed to `upstream` (an OpenAI Chat upstream) with
/// a byte cap of `max`.
async fn seed(app: &TestApp, upstream: &str, model: &str, max: usize) -> String {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider =
        fixtures::create_provider(&app.db, &unique_name("cap"), "openai", upstream, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET output_guardrails = $1::jsonb WHERE model_id = $2")
        .bind(json!([{"type": "max_length", "max_chars": max}]))
        .bind(model)
        .execute(&app.db)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    fixtures::create_api_key(&app.db, user.user.id, "cap", &["ai_gateway"], None, None)
        .await
        .unwrap()
        .plaintext
}

/// The four HTTP surfaces, as `(name, path, body)` for `model`.
fn surfaces(model: &str, stream: bool) -> Vec<(&'static str, String, Value)> {
    let gemini = if stream {
        format!("/v1beta/models/{model}:streamGenerateContent?alt=sse")
    } else {
        format!("/v1beta/models/{model}:generateContent")
    };
    vec![
        (
            "chat",
            "/v1/chat/completions".into(),
            json!({"model": model, "stream": stream,
                   "messages": [{"role": "user", "content": "ping"}]}),
        ),
        (
            "messages",
            "/v1/messages".into(),
            json!({"model": model, "stream": stream, "max_tokens": 64,
                   "messages": [{"role": "user", "content": "ping"}]}),
        ),
        (
            "responses",
            "/v1/responses".into(),
            json!({"model": model, "stream": stream, "input": "ping"}),
        ),
        (
            "gemini",
            gemini,
            json!({"contents": [{"role": "user", "parts": [{"text": "ping"}]}]}),
        ),
    ]
}

async fn post(app: &TestApp, key: &str, path: &str, body: &Value) -> (u16, String) {
    let mut req = reqwest::Client::new()
        .post(format!("{}{path}", app.gateway_url))
        .json(body);
    req = if path.starts_with("/v1beta/") {
        req.header("x-goog-api-key", key)
    } else {
        req.bearer_auth(key)
    };
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap())
}

/// `(event, data)` for each SSE frame whose data is JSON.
fn frames(body: &str) -> Vec<(Option<String>, Value)> {
    body.split("\n\n")
        .filter_map(|block| {
            let mut event = None;
            let mut data = None;
            for line in block.lines() {
                if let Some(e) = line.strip_prefix("event: ") {
                    event = Some(e.to_string());
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = serde_json::from_str(d).ok();
                }
            }
            Some((event, data?))
        })
        .collect()
}

/// The answer's text in whichever format a frame or element is in.
fn text_in(v: &Value) -> String {
    let parts = [
        v.pointer("/choices/0/delta/content"),
        v.pointer("/delta/text"),
        (v["type"] == "response.output_text.delta")
            .then(|| v.get("delta"))
            .flatten(),
    ];
    let mut out: String = parts
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if let Some(ps) = v
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    {
        out.extend(ps.iter().filter_map(|p| p["text"].as_str()));
    }
    out
}

/// Whether the last frame is the stream's error, in `surface`'s format.
fn ends_in_error(surface: &str, fs: &[(Option<String>, Value)]) -> bool {
    let Some((event, data)) = fs.last() else {
        return false;
    };
    match surface {
        "messages" => event.as_deref() == Some("error") && data["type"] == "error",
        "responses" => data["type"] == "response.failed",
        _ => data.get("error").is_some(),
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_over_the_cap_is_cut_in_every_callers_format() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("cap-stream").await;
    let key = seed(&app, &upstream.uri(), "cap-stream", 4).await;

    for (surface, path, body) in surfaces("cap-stream", true) {
        let (status, text) = post(&app, &key, &path, &body).await;
        // Headers went out before the answer did.
        assert_eq!(status, 200, "{surface}: {text}");
        let fs = frames(&text);
        let said: String = fs.iter().map(|(_, v)| text_in(v)).collect();
        assert_eq!(said, "hi ", "{surface}: {text}");
        assert!(ends_in_error(surface, &fs), "{surface}: {text}");
        assert!(text.contains("max_length"), "{surface}: {text}");
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_gemini_json_array_stream_over_the_cap_ends_with_an_error_element() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("cap-array").await;
    let key = seed(&app, &upstream.uri(), "cap-array", 4).await;

    let (status, text) = post(
        &app,
        &key,
        "/v1beta/models/cap-array:streamGenerateContent",
        &json!({"contents": [{"role": "user", "parts": [{"text": "ping"}]}]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let elements: Vec<Value> =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("not a JSON array ({e}): {text}"));
    let said: String = elements.iter().map(text_in).collect();
    assert_eq!(said, "hi ", "{text}");
    let last = elements.last().unwrap();
    assert!(
        last["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("max_length")),
        "{text}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_under_the_cap_is_untouched() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("cap-roomy").await;
    let key = seed(&app, &upstream.uri(), "cap-roomy", 100).await;

    for (surface, path, body) in surfaces("cap-roomy", true) {
        let (status, text) = post(&app, &key, &path, &body).await;
        assert_eq!(status, 200, "{surface}: {text}");
        let fs = frames(&text);
        let said: String = fs.iter().map(|(_, v)| text_in(v)).collect();
        assert_eq!(said, "hi there", "{surface}: {text}");
        assert!(!ends_in_error(surface, &fs), "{surface}: {text}");
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_whole_answer_over_the_cap_is_withheld_in_every_callers_format() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("cap-whole").await;
    let key = seed(&app, &upstream.uri(), "cap-whole", 4).await;

    for (surface, path, body) in surfaces("cap-whole", false) {
        let (status, text) = post(&app, &key, &path, &body).await;
        assert!(!(200..300).contains(&status), "{surface}: {status} {text}");
        assert!(text.contains("max_length"), "{surface}: {text}");
        assert!(!text.contains("hello world"), "{surface}: {text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v.get("error").is_some(), "{surface}: {text}");
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_cap_counts_bytes_not_characters() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider {
        server: wiremock::MockServer::start().await,
    };
    upstream
        .mount(
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id": "c", "object": "chat.completion", "created": 0, "model": "cap-cjk",
                    "choices": [{"index": 0, "finish_reason": "stop",
                                 "message": {"role": "assistant", "content": "你好"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))),
        )
        .await;
    // Two characters, six bytes.
    let key = seed(&app, &upstream.uri(), "cap-cjk", 5).await;
    let (status, text) = post(
        &app,
        &key,
        "/v1/chat/completions",
        &json!({"model": "cap-cjk", "messages": [{"role": "user", "content": "ping"}]}),
    )
    .await;
    assert!(!(200..300).contains(&status), "{status} {text}");
    assert!(text.contains("6 chars > 5 cap"), "{text}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_websocket_turn_over_the_cap_fails_and_the_connection_stays() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("cap-ws").await;
    let key = seed(&app, &upstream.uri(), "cap-ws", 4).await;

    let mut req = format!(
        "ws://{}/v1/responses",
        app.gateway_url.trim_start_matches("http://")
    )
    .into_client_request()
    .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(req).await.unwrap();

    for _ in 0..2 {
        socket
            .send(Message::Text(
                json!({"type": "response.create", "model": "cap-ws", "input": "ping"}).to_string(),
            ))
            .await
            .unwrap();
        let mut events: Vec<Value> = Vec::new();
        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
                .await
                .expect("an event within 10s")
                .expect("connection open")
                .expect("frame");
            let Message::Text(t) = next else { continue };
            let v: Value = serde_json::from_str(t.as_str()).unwrap();
            let done = matches!(
                v["type"].as_str(),
                Some("response.completed" | "response.failed")
            );
            events.push(v);
            if done {
                break;
            }
        }
        let said: String = events.iter().map(text_in).collect();
        assert_eq!(said, "hi ", "{events:?}");
        let last = events.last().unwrap();
        assert_eq!(last["type"], "response.failed", "{events:?}");
    }
    socket.close(None).await.unwrap();
}
