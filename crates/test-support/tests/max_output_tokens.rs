//! A model's output cap (`models.max_output_tokens`), end to end.
//!
//! The cap is applied to the request, not measured on the answer: a caller
//! asking for more output tokens than the model allows is lowered to the
//! cap, one asking for less keeps its own, and one asking for none gets the
//! cap — unless the cap is more than the model's family is known to take
//! (8,192 tokens, or 32,000 for Claude), where a filled-in limit could be
//! refused and the model's own limit applies instead. The upstream stops
//! there by itself. Each format names the field its own way (`max_tokens`,
//! `max_completion_tokens`, `max_output_tokens`,
//! `generationConfig.maxOutputTokens`), and a request forwarded as sent is
//! capped as surely as a converted one.

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use think_watch_test_support::prelude::*;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};

const CAP: u64 = 64;

/// A Chat upstream answering in whichever form it was asked for.
async fn chat_upstream() -> MockProvider {
    let upstream = MockProvider {
        server: wiremock::MockServer::start().await,
    };
    upstream
        .mount(
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(|req: &Request| {
                    let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
                    let usage =
                        json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
                    if body["stream"] == true {
                        let chunk = |choices: Value, usage: Value| {
                            format!(
                                "data: {}\n\n",
                                json!({"id": "c", "object": "chat.completion.chunk", "created": 0,
                                       "model": "m", "choices": choices, "usage": usage})
                            )
                        };
                        let sse = [
                            chunk(
                                json!([{"index": 0, "delta": {"role": "assistant", "content": "hi"}, "finish_reason": null}]),
                                Value::Null,
                            ),
                            chunk(
                                json!([{"index": 0, "delta": {}, "finish_reason": "length"}]),
                                Value::Null,
                            ),
                            chunk(json!([]), usage),
                            "data: [DONE]\n\n".to_string(),
                        ]
                        .concat();
                        ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
                    } else {
                        ResponseTemplate::new(200).set_body_json(json!({
                            "id": "c", "object": "chat.completion", "created": 0, "model": "m",
                            "choices": [{"index": 0, "finish_reason": "length",
                                         "message": {"role": "assistant", "content": "hi"}}],
                            "usage": usage,
                        }))
                    }
                }),
        )
        .await;
    upstream
}

/// A key, and `model` routed to `upstream` (of `provider_type`), capped at
/// `cap` output tokens when there is one.
async fn seed(
    app: &TestApp,
    upstream: &str,
    provider_type: &str,
    model: &str,
    cap: Option<u64>,
) -> String {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider =
        fixtures::create_provider(&app.db, &unique_name("cap"), provider_type, upstream, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET max_output_tokens = $1 WHERE model_id = $2")
        .bind(cap.map(|c| c as i32))
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

/// The four HTTP surfaces, asking for `ask` output tokens (or none), as
/// `(name, path, body)`.
fn surfaces(model: &str, stream: bool, ask: Option<u64>) -> Vec<(&'static str, String, Value)> {
    let gemini = if stream {
        format!("/v1beta/models/{model}:streamGenerateContent?alt=sse")
    } else {
        format!("/v1beta/models/{model}:generateContent")
    };
    // Sampled at a temperature: a repeated Chat request is not answered
    // from the response cache.
    let mut chat = json!({"model": model, "stream": stream, "temperature": 0.5,
                          "messages": [{"role": "user", "content": "ping"}]});
    let mut messages = json!({"model": model, "stream": stream,
                              "messages": [{"role": "user", "content": "ping"}]});
    let mut responses = json!({"model": model, "stream": stream, "input": "ping"});
    let mut gemini_body = json!({"contents": [{"role": "user", "parts": [{"text": "ping"}]}]});
    if let Some(n) = ask {
        chat["max_completion_tokens"] = json!(n);
        messages["max_tokens"] = json!(n);
        responses["max_output_tokens"] = json!(n);
        gemini_body["generationConfig"] = json!({"maxOutputTokens": n});
    }
    vec![
        ("chat", "/v1/chat/completions".into(), chat),
        ("messages", "/v1/messages".into(), messages),
        ("responses", "/v1/responses".into(), responses),
        ("gemini", gemini, gemini_body),
    ]
}

/// The output limit a Chat request asks its upstream for.
fn chat_max(v: &Value) -> Option<u64> {
    v.get("max_completion_tokens")
        .or_else(|| v.get("max_tokens"))
        .and_then(Value::as_u64)
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_cap_lowers_a_larger_ask_and_fills_a_missing_one_on_every_surface() {
    let app = TestApp::spawn().await;
    let upstream = chat_upstream().await;
    let key = seed(&app, &upstream.uri(), "openai", "cap-every", Some(CAP)).await;

    // (what the caller asks for, what the upstream should see)
    for (ask, expect) in [(Some(1000), CAP), (None, CAP), (Some(16), 16)] {
        for stream in [false, true] {
            for (surface, path, body) in surfaces("cap-every", stream, ask) {
                let before = upstream.received_requests().await.len();
                let (status, text) = post(&app, &key, &path, &body).await;
                assert_eq!(status, 200, "{surface} stream={stream}: {text}");
                let sent: Value = upstream.received_requests().await[before]
                    .body_json()
                    .unwrap();
                assert_eq!(
                    chat_max(&sent),
                    Some(expect),
                    "{surface} stream={stream} ask={ask:?}: {sent}"
                );
            }
        }
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_request_forwarded_as_sent_is_capped_in_its_own_field() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::anthropic_messages_ok("cap-claude").await;
    let key = seed(&app, &upstream.uri(), "anthropic", "cap-claude", Some(CAP)).await;
    let (status, text) = post(
        &app,
        &key,
        "/v1/messages",
        &json!({"model": "cap-claude", "max_tokens": 4096,
                "messages": [{"role": "user", "content": "ping"}],
                "metadata": {"user_id": "u-1"}}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(sent["max_tokens"], CAP, "{sent}");
    // Forwarded as sent otherwise: what the conversion layer would drop
    // is still there.
    assert_eq!(sent["metadata"]["user_id"], "u-1", "{sent}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_websocket_turn_is_capped_too() {
    let app = TestApp::spawn().await;
    let upstream = chat_upstream().await;
    let key = seed(&app, &upstream.uri(), "openai", "cap-ws", Some(CAP)).await;

    let mut req = format!(
        "ws://{}/v1/responses",
        app.gateway_url.trim_start_matches("http://")
    )
    .into_client_request()
    .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type": "response.create", "model": "cap-ws", "input": "ping",
                   "max_output_tokens": 100000})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("an event within 10s")
            .expect("connection open")
            .expect("frame");
        let Message::Text(t) = next else { continue };
        let v: Value = serde_json::from_str(t.as_str()).unwrap();
        if matches!(
            v["type"].as_str(),
            Some("response.completed" | "response.failed" | "response.incomplete")
        ) {
            break;
        }
    }
    socket.close(None).await.unwrap();
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(chat_max(&sent), Some(CAP), "{sent}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_model_without_a_cap_leaves_the_request_alone() {
    let app = TestApp::spawn().await;
    let upstream = chat_upstream().await;
    let key = seed(&app, &upstream.uri(), "openai", "cap-none", None).await;
    let (status, text) = post(
        &app,
        &key,
        "/v1/chat/completions",
        &json!({"model": "cap-none", "messages": [{"role": "user", "content": "ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(chat_max(&sent), None, "{sent}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_model_api_sets_and_clears_the_cap() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let model_id = unique_name("cap-api");

    let created: Value = con
        .post(
            "/api/admin/models",
            json!({"model_id": model_id, "display_name": "Capped", "max_output_tokens": 4096}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(created["max_output_tokens"], 4096, "{created}");
    let id = created["id"].as_str().unwrap().to_string();

    let listed: Value = con
        .get(&format!("/api/admin/models?q={model_id}"))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(listed["items"][0]["max_output_tokens"], 4096, "{listed}");
    assert!(listed["items"][0].get("output_guardrails").is_none());

    // Absent leaves it as it was.
    let kept: Value = con
        .patch(
            &format!("/api/admin/models/{id}"),
            json!({"display_name": "Renamed"}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(kept["max_output_tokens"], 4096, "{kept}");
    // null clears it.
    let cleared: Value = con
        .patch(
            &format!("/api/admin/models/{id}"),
            json!({"max_output_tokens": null}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert!(cleared["max_output_tokens"].is_null(), "{cleared}");
    let set: Value = con
        .patch(
            &format!("/api/admin/models/{id}"),
            json!({"max_output_tokens": 2147483647_i64}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(set["max_output_tokens"], 2147483647_i64, "{set}");

    for bad in [json!(0), json!(-5), json!(2147483648_i64)] {
        let r = con
            .patch(
                &format!("/api/admin/models/{id}"),
                json!({"max_output_tokens": bad}),
            )
            .await
            .unwrap();
        assert_eq!(r.status.as_u16(), 400, "{bad}: {}", r.text());
    }

    // The length cap that this replaced is refused, not silently ignored.
    let r = con
        .patch(
            &format!("/api/admin/models/{id}"),
            json!({"output_guardrails": [{"type": "max_length", "max_chars": 4000}]}),
        )
        .await
        .unwrap();
    assert_eq!(r.status.as_u16(), 400, "{}", r.text());
    assert!(r.text().contains("max_output_tokens"), "{}", r.text());
    let r = con
        .post(
            "/api/admin/models",
            json!({"model_id": unique_name("cap-old"), "display_name": "Old",
                   "output_guardrails": [{"type": "max_length", "max_chars": 4000}]}),
        )
        .await
        .unwrap();
    assert_eq!(r.status.as_u16(), 400, "{}", r.text());
    // Asking for none passes.
    con.patch(
        &format!("/api/admin/models/{id}"),
        json!({"output_guardrails": []}),
    )
    .await
    .unwrap()
    .assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_cap_above_what_the_family_takes_is_not_filled_in() {
    // 25,000 tokens: what a 100,000-byte cap converts to, on a model whose
    // family the gateway only knows to take 8,192.
    let app = TestApp::spawn().await;
    let upstream = chat_upstream().await;
    let key = seed(&app, &upstream.uri(), "openai", "cap-loose", Some(25_000)).await;
    for (ask, expect) in [
        (None, None),
        (Some(30_000), Some(25_000)),
        (Some(100), Some(100)),
    ] {
        let mut body = json!({"model": "cap-loose", "temperature": 0.5,
                              "messages": [{"role": "user", "content": "ping"}]});
        if let Some(n) = ask {
            body["max_tokens"] = json!(n);
        }
        let before = upstream.received_requests().await.len();
        let (status, text) = post(&app, &key, "/v1/chat/completions", &body).await;
        assert_eq!(status, 200, "{ask:?}: {text}");
        let sent: Value = upstream.received_requests().await[before]
            .body_json()
            .unwrap();
        assert_eq!(chat_max(&sent), expect, "ask={ask:?}: {sent}");
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_conversion_that_must_write_a_limit_writes_the_lower_one() {
    // A Chat caller asking for no limit, routed to Anthropic, which requires
    // one: the conversion writes the family's figure, and the cap lowers it.
    let app = TestApp::spawn().await;
    let upstream = MockProvider::anthropic_messages_ok("cap-claude-conv").await;
    let key = seed(
        &app,
        &upstream.uri(),
        "anthropic",
        "cap-claude-conv",
        Some(CAP),
    )
    .await;
    let (status, text) = post(
        &app,
        &key,
        "/v1/chat/completions",
        &json!({"model": "cap-claude-conv", "messages": [{"role": "user", "content": "ping"}]}),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let sent: Value = upstream.received_requests().await[0].body_json().unwrap();
    assert_eq!(sent["max_tokens"], CAP, "{sent}");
}
