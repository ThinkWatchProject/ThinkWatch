//! Gemini-format clients: `POST /v1beta/models/{model}:generateContent`
//! and `:streamGenerateContent`, keyed by `x-goog-api-key` or `?key=`.
//!
//! The request runs the same pipeline as the other three surfaces —
//! routing, conversion to whatever the route speaks, billing, the audit
//! row — so these tests pin what is Gemini-specific: the model and the
//! stream flag come from the path, the stream comes back as SSE with
//! `alt=sse` and as one JSON array without it, a Gemini upstream gets the
//! request as sent, and errors are in Gemini's shape.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// A key allowed on the AI gateway, and `model` routed to `upstream` as
/// a provider of `provider_type`.
async fn seed(app: &TestApp, upstream: &str, provider_type: &str, model: &str) -> (Uuid, String) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider =
        fixtures::create_provider(&app.db, &unique_name("gem"), provider_type, upstream, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(&app.db, user.user.id, "g", &["ai_gateway"], None, None)
        .await
        .unwrap();
    (user.user.id, key.plaintext)
}

fn gemini_request() -> Value {
    json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]})
}

/// All the text parts of a Gemini response or stream chunk.
fn text_of(v: &Value) -> String {
    v["candidates"][0]["content"]["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default()
}

async fn post(
    app: &TestApp,
    path_and_query: &str,
    key_header: Option<&str>,
    body: &Value,
) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{}{path_and_query}", app.gateway_url))
        .json(body);
    if let Some(k) = key_header {
        req = req.header("x-goog-api-key", k);
    }
    req.send().await.unwrap()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_gemini_client_is_answered_in_gemini_format_and_billed() {
    let app = TestApp::spawn_with_clickhouse().await;
    let upstream = MockProvider::openai_chat_ok("gem-chat").await;
    let (user_id, key) = seed(&app, &upstream.uri(), "openai", "gem-chat").await;

    let resp = post(
        &app,
        "/v1beta/models/gem-chat:generateContent",
        Some(&key),
        &gemini_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(text_of(&body), "hello world", "{body}");
    assert_eq!(body["modelVersion"], "gem-chat", "{body}");
    assert_eq!(body["usageMetadata"]["promptTokenCount"], 7, "{body}");

    // The upstream got a Chat request naming the routed model.
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(sent["model"], "gem-chat");
    assert_eq!(sent["messages"][0]["content"], "hi");

    // Billed and logged like any other request.
    let ch = app.state.clickhouse.as_ref().expect("CH wired up");
    let mut row = None;
    for _ in 0..100 {
        row = ch
            .query(
                "SELECT ifNull(input_tokens, -1), ifNull(output_tokens, -1), ifNull(status_code, -1) \
                   FROM gateway_logs WHERE user_id = ? ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id.to_string())
            .fetch_optional::<(i64, i64, i64)>()
            .await
            .expect("CH query");
        if row.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(row, Some((7, 3, 200)), "gateway_logs row");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_gemini_stream_with_alt_sse_is_sse_and_the_key_may_be_in_the_query() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("gem-sse").await;
    let (_, key) = seed(&app, &upstream.uri(), "openai", "gem-sse").await;

    let resp = post(
        &app,
        &format!("/v1beta/models/gem-sse:streamGenerateContent?alt=sse&key={key}"),
        None,
        &gemini_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    let text = resp.text().await.unwrap();
    let chunks: Vec<Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    let said: String = chunks.iter().map(text_of).collect();
    assert_eq!(said, "hi there", "{text}");
    assert!(
        chunks
            .iter()
            .all(|c| c["modelVersion"] == "gem-sse" || c.get("modelVersion").is_none()),
        "{text}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_gemini_stream_without_alt_sse_is_one_json_array() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("gem-arr").await;
    let (_, key) = seed(&app, &upstream.uri(), "openai", "gem-arr").await;

    let resp = post(
        &app,
        "/v1beta/models/gem-arr:streamGenerateContent",
        Some(&key),
        &gemini_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "application/json"
    );
    let body: Value = resp.json().await.unwrap();
    let chunks = body
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {body}"));
    let said: String = chunks.iter().map(text_of).collect();
    assert_eq!(said, "hi there", "{body}");
}

/// Same format both sides: the request goes out as the caller sent it,
/// to the routed model's path, asking for SSE, without the caller's key.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_gemini_upstream_gets_the_request_as_sent() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider {
        server: wiremock::MockServer::start().await,
    };
    upstream
        .mount(
            Mock::given(method("POST"))
                .and(path("/v1beta/models/gem-native:streamGenerateContent"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(
                    concat!(
                        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"native\"}]}}],",
                        "\"modelVersion\":\"gemini-upstream-001\"}\r\n\r\n",
                        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\" reply\"}]},\"finishReason\":\"STOP\"}],",
                        "\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":2,\"totalTokenCount\":6}}\r\n\r\n",
                    ),
                    "text/event-stream",
                )),
        )
        .await;
    let (_, key) = seed(&app, &upstream.uri(), "google", "gem-native").await;

    // A field the conversion layer has no place for: it survives only
    // because the request is forwarded, not rebuilt.
    let mut request = gemini_request();
    request["safetySettings"] =
        json!([{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}]);
    let resp = post(
        &app,
        &format!("/v1beta/models/gem-native:streamGenerateContent?key={key}"),
        None,
        &request,
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let chunks = body
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {body}"));
    let said: String = chunks.iter().map(text_of).collect();
    assert_eq!(said, "native reply", "{body}");
    assert_eq!(chunks[0]["modelVersion"], "gem-native", "{body}");

    let sent = upstream.received_requests().await;
    let last = sent.last().unwrap();
    assert_eq!(
        last.url.query(),
        Some("alt=sse"),
        "the caller's key stays here"
    );
    let sent_body: Value = last.body_json().unwrap();
    assert_eq!(sent_body["safetySettings"], request["safetySettings"]);
    assert!(
        sent_body.get("model").is_none(),
        "Gemini names the model in the path"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_gemini_client_gets_gemini_errors() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::always_500().await;
    let (_, key) = seed(&app, &upstream.uri(), "openai", "gem-broken").await;

    let resp = post(
        &app,
        "/v1beta/models/gem-broken:generateContent",
        Some(&key),
        &gemini_request(),
    )
    .await;
    assert_eq!(resp.status(), 500);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], 500, "{body}");
    assert_eq!(body["error"]["status"], "INTERNAL", "{body}");

    // An action that is not a generation.
    let resp = post(
        &app,
        "/v1beta/models/gem-broken:countTokens",
        Some(&key),
        &gemini_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["status"], "INVALID_ARGUMENT", "{body}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_gemini_request_without_a_key_is_refused() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("gem-nokey").await;
    seed(&app, &upstream.uri(), "openai", "gem-nokey").await;

    let resp = post(
        &app,
        "/v1beta/models/gem-nokey:generateContent?key=tw-not-a-real-key",
        None,
        &gemini_request(),
    )
    .await;
    assert_eq!(resp.status(), 401);
    assert!(upstream.received_requests().await.is_empty());
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn gemini_clients_list_models_in_their_shape() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("gem-listed").await;
    let (_, key) = seed(&app, &upstream.uri(), "openai", "gem-listed").await;

    let resp = reqwest::Client::new()
        .get(format!("{}/v1beta/models", app.gateway_url))
        .header("x-goog-api-key", &key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let names: Vec<&str> = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert!(names.contains(&"models/gem-listed"), "{body}");
}
