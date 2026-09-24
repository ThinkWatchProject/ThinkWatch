//! Errors reach each client in the shape its own SDK reads.
//!
//! An Anthropic SDK looks for `{"type":"error","error":{"type",…}}`, a
//! Responses client dispatches on `response.failed` and skips anything
//! else, a Chat client reads `{"error":{"message","type"}}`. Before, every
//! surface got the Chat shape — for the whole body and in a stream — so
//! an Anthropic or Responses client saw a gateway refusal as a response
//! it could not parse, or as a stream that simply stopped.

use serde_json::Value;
use think_watch_test_support::prelude::*;

/// A key allowed on the AI gateway, and `model` routed to an OpenAI
/// upstream that answers every request with a 500.
async fn failing_route(app: &TestApp, model: &str) -> (MockProvider, String) {
    let upstream = MockProvider::always_500().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("broken"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(&app.db, user.user.id, "e", &["ai_gateway"], None, None)
        .await
        .unwrap();
    (upstream, key.plaintext)
}

/// The `data:` payloads of an SSE body, with their event names.
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

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn an_anthropic_client_gets_an_anthropic_error_body() {
    let app = TestApp::spawn().await;
    let (_upstream, key) = failing_route(&app, "err-shape-a").await;
    let gw = app.gateway_client();
    gw.set_bearer(&key);

    let resp = gw
        .post(
            "/v1/messages",
            json!({"model": "err-shape-a", "max_tokens": 16,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await
        .unwrap();
    resp.assert_status(500);
    let body: Value = resp.json().unwrap();
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "api_error", "{body}");
    assert!(body["error"]["message"].is_string(), "{body}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_refusal_before_routing_is_in_the_callers_format_too() {
    let app = TestApp::spawn().await;
    let (_upstream, key) = failing_route(&app, "err-shape-known").await;
    let gw = app.gateway_client();
    gw.set_bearer(&key);

    // No route for this model: refused before any upstream is called.
    let resp = gw
        .post(
            "/v1/messages",
            json!({"model": "err-shape-nowhere", "max_tokens": 16,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await
        .unwrap();
    assert!(!resp.status.is_success());
    let body: Value = resp.json().unwrap();
    assert_eq!(body["type"], "error", "{body}");
    assert!(body["error"]["type"].is_string(), "{body}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_chat_client_gets_openais_error_types() {
    let app = TestApp::spawn().await;
    let (_upstream, key) = failing_route(&app, "err-shape-c").await;
    let gw = app.gateway_client();
    gw.set_bearer(&key);

    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "err-shape-c", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await
        .unwrap();
    resp.assert_status(500);
    let body: Value = resp.json().unwrap();
    assert_eq!(body["error"]["type"], "server_error", "{body}");
    assert!(body.get("type").is_none(), "{body}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_responses_stream_that_fails_ends_with_response_failed() {
    let app = TestApp::spawn().await;
    let (_upstream, key) = failing_route(&app, "err-shape-r").await;
    let gw = app.gateway_client();
    gw.set_bearer(&key);

    let resp = gw
        .post(
            "/v1/responses",
            json!({"model": "err-shape-r", "stream": true, "input": "hi"}),
        )
        .await
        .unwrap();
    let text = resp.text();
    let fs = frames(&text);
    let (event, data) = fs.last().unwrap_or_else(|| panic!("no frames in {text}"));
    assert_eq!(event.as_deref(), Some("response.failed"), "{text}");
    assert_eq!(data["type"], "response.failed", "{text}");
    assert_eq!(data["response"]["status"], "failed", "{text}");
    assert!(data["response"]["error"]["message"].is_string(), "{text}");
    // The caller's model, like every other frame it receives.
    assert_eq!(data["response"]["model"], "err-shape-r", "{text}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn an_anthropic_stream_that_fails_ends_with_an_error_event() {
    let app = TestApp::spawn().await;
    let (_upstream, key) = failing_route(&app, "err-shape-as").await;
    let gw = app.gateway_client();
    gw.set_bearer(&key);

    let resp = gw
        .post(
            "/v1/messages",
            json!({"model": "err-shape-as", "max_tokens": 16, "stream": true,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await
        .unwrap();
    let text = resp.text();
    let fs = frames(&text);
    let (event, data) = fs.last().unwrap_or_else(|| panic!("no frames in {text}"));
    assert_eq!(event.as_deref(), Some("error"), "{text}");
    assert_eq!(data["type"], "error", "{text}");
    assert_eq!(data["error"]["type"], "api_error", "{text}");
}
