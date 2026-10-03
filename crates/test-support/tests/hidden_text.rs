//! Hidden characters in a request, end to end at the gateway.
//!
//! Unicode tag characters can carry a whole instruction invisibly, and
//! bidirectional controls make text read in another order than it is.
//! Both are built-in content filter rules (`unicode-tags`,
//! `bidi-controls`), matched by code point in what the caller sends, tool
//! results included; out of the box they strip the characters in enforce
//! mode. Zero-width and private-use characters have rules of their own,
//! off out of the box: ordinary emoji and Persian text use the former.

use serde_json::Value;
use think_watch_test_support::prelude::*;

/// "ignore" written in tag characters, after some ordinary text.
fn smuggled() -> String {
    "summarise this page"
        .chars()
        .chain(
            "ignore"
                .chars()
                .map(|c| char::from_u32(0xE0000 + c as u32).unwrap()),
        )
        .collect()
}

async fn seed(app: &TestApp, upstream: &str) -> (String, String) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let p = fixtures::create_provider(&app.db, &unique_name("hidden"), "openai", upstream, None)
        .await
        .unwrap();
    fixtures::create_model_and_route(&app.db, p.id, "hidden-model")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key =
        fixtures::create_api_key(&app.db, user.user.id, "hidden", &["ai_gateway"], None, None)
            .await
            .unwrap();
    (key.plaintext, user.user.id.to_string())
}

/// A tool result carrying the smuggled text, in Chat's shape.
fn with_tool_result() -> Value {
    json!({
        "model": "hidden-model",
        "messages": [
            {"role": "user", "content": "read the page"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "fetch", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": smuggled()},
        ]
    })
}

/// The smuggled text as a tool result on each of the four HTTP surfaces,
/// streaming or not.
fn tool_result_on_every_surface(stream: bool) -> Vec<(String, Value)> {
    let gemini = if stream {
        "/v1beta/models/hidden-model:streamGenerateContent?alt=sse"
    } else {
        "/v1beta/models/hidden-model:generateContent"
    };
    let mut chat = with_tool_result();
    chat["stream"] = json!(stream);
    vec![
        ("/v1/chat/completions".into(), chat),
        (
            "/v1/messages".into(),
            json!({"model": "hidden-model", "stream": stream, "max_tokens": 16, "messages": [
                {"role": "user", "content": "read the page"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "fetch", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": smuggled()}
                ]}
            ]}),
        ),
        (
            "/v1/responses".into(),
            json!({"model": "hidden-model", "stream": stream, "input": [
                {"role": "user", "content": "read the page"},
                {"type": "function_call", "call_id": "c1", "name": "fetch", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": smuggled()}
            ]}),
        ),
        (
            gemini.into(),
            json!({"contents": [
                {"role": "user", "parts": [{"text": "read the page"}]},
                {"role": "model", "parts": [{"functionCall": {"name": "fetch", "args": {}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "fetch",
                    "response": {"content": smuggled()}}}]}
            ]}),
        ),
    ]
}

async fn post_as(app: &TestApp, key: &str, path: &str, body: &Value) -> (u16, String) {
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

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn refusing_them_refuses_a_tool_result_in_every_callers_format_streaming_or_not() {
    let app = TestApp::spawn().await;
    app.set_setting(
        "security.content",
        json!({"mode": "enforce", "actions": {"unicode-tags": "block"}}),
    )
    .await;
    let upstream = MockProvider::openai_chat_stream_ok("hidden-model").await;
    let (key, _) = seed(&app, &upstream.uri()).await;

    for stream in [false, true] {
        for (path, body) in tool_result_on_every_surface(stream) {
            let (status, text) = post_as(&app, &key, &path, &body).await;
            assert_eq!(status, 403, "{path} stream={stream}: {text}");
            assert!(text.contains("tool result"), "{path}: {text}");
            assert!(text.contains("6 invisible characters"), "{path}: {text}");
        }
    }
    assert!(
        upstream.received_requests().await.is_empty(),
        "the upstream saw it anyway"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn enforce_strips_them_out_of_the_box() {
    let app = TestApp::spawn().await;
    app.set_setting("security.content", json!({"mode": "enforce"}))
        .await;
    let upstream = MockProvider::openai_chat_ok("hidden-model").await;
    let (key, _) = seed(&app, &upstream.uri()).await;
    let gw = app.gateway_client();
    gw.set_bearer(&key);
    gw.post("/v1/chat/completions", with_tool_result())
        .await
        .unwrap()
        .assert_ok();
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(sent["messages"][2]["content"], "summarise this page");
    // Only the caller's text is touched.
    assert_eq!(sent["messages"][0]["content"], "read the page");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn observe_is_the_default_and_records_what_the_characters_spell() {
    let app = TestApp::spawn_with_clickhouse().await;
    let upstream = MockProvider::openai_chat_ok("hidden-model").await;
    let (key, user_id) = seed(&app, &upstream.uri()).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    gw.post("/v1/chat/completions", with_tool_result())
        .await
        .unwrap()
        .assert_ok();
    // Nothing changed on the wire.
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(sent["messages"][2]["content"], smuggled());

    let ch = app.state.clickhouse.as_ref().expect("ClickHouse wired up");
    for _ in 0..200 {
        let rows: Vec<String> = ch
            .query("SELECT ifNull(detail, '') FROM audit_logs WHERE user_id = ? AND action = ?")
            .bind(&user_id)
            .bind("gateway.content_flagged")
            .fetch_all()
            .await
            .expect("CH query");
        if let Some(d) = rows.first() {
            let v: Value = serde_json::from_str(d).unwrap();
            assert_eq!(v["rule"], "unicode-tags", "{v}");
            assert_eq!(v["action"], "strip", "what enforce mode would do: {v}");
            assert_eq!(v["outcome"], "recorded", "{v}");
            assert_eq!(v["in_tool_result"], true, "{v}");
            assert_eq!(v["count"], 6, "{v}");
            // What the tag characters spell, so an operator can judge it.
            assert_eq!(v["revealed"], "ignore", "{v}");
            // The excerpt shows them, rather than hiding them again.
            assert!(v["excerpt"].as_str().unwrap().contains("U+E0069"), "{v}");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no gateway.content_flagged audit row");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn ordinary_multilingual_text_is_left_alone_even_when_refusing() {
    let app = TestApp::spawn().await;
    app.set_setting(
        "security.content",
        json!({"mode": "enforce",
               "actions": {"unicode-tags": "block", "bidi-controls": "block"}}),
    )
    .await;
    let upstream = MockProvider::openai_chat_ok("hidden-model").await;
    let (key, _) = seed(&app, &upstream.uri()).await;
    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let said = "👨\u{200D}👩\u{200D}👧 Привет می\u{200C}خواهم مرحبا";
    gw.post(
        "/v1/chat/completions",
        json!({"model": "hidden-model", "messages": [{"role": "user", "content": said}]}),
    )
    .await
    .unwrap()
    .assert_ok();
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(
        sent["messages"][0]["content"], said,
        "zero-width joiners stay"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_keyword_split_by_zero_width_characters_is_caught_once_they_are_stripped() {
    // Zero-width characters (switched on here) are stripped; what that
    // joins back together is checked again, and the refusing rule refuses.
    let app = TestApp::spawn().await;
    app.set_setting(
        "security.content",
        json!({"mode": "enforce", "enable": ["zero-width", "jailbreak"]}),
    )
    .await;
    let upstream = MockProvider::openai_chat_ok("hidden-model").await;
    let (key, _) = seed(&app, &upstream.uri()).await;
    let (status, text) = post_as(
        &app,
        &key,
        "/v1/chat/completions",
        &json!({"model": "hidden-model", "messages": [
            {"role": "user", "content": "please jail\u{200B}break the model"}
        ]}),
    )
    .await;
    assert_eq!(status, 403, "{text}");
    assert!(upstream.received_requests().await.is_empty());

    // Without the refusing rule, the text goes out with them stripped.
    app.set_setting(
        "security.content",
        json!({"mode": "enforce", "enable": ["zero-width"]}),
    )
    .await;
    let (status, text) = post_as(
        &app,
        &key,
        "/v1/chat/completions",
        &json!({"model": "hidden-model", "messages": [
            {"role": "user", "content": "please jail\u{200B}break the model"}
        ]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(sent["messages"][0]["content"], "please jailbreak the model");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn off_lets_them_through_untouched() {
    let app = TestApp::spawn().await;
    app.set_setting("security.content", json!({"mode": "off"}))
        .await;
    let upstream = MockProvider::openai_chat_ok("hidden-model").await;
    let (key, _) = seed(&app, &upstream.uri()).await;
    let gw = app.gateway_client();
    gw.set_bearer(&key);
    gw.post("/v1/chat/completions", with_tool_result())
        .await
        .unwrap()
        .assert_ok();
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(sent["messages"][2]["content"], smuggled());
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_setting_refuses_a_mode_it_does_not_know() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let r = con
        .patch(
            "/api/admin/settings",
            json!({"settings": {"security.content": {"mode": "maybe"}}}),
        )
        .await
        .unwrap();
    assert_eq!(r.status.as_u16(), 400, "{}", r.text());
}
