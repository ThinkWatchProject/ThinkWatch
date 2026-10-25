//! Sending a request again after the upstream failed for the moment.
//!
//! Every candidate route tried and the last one overloaded, failing with a
//! server error or its connection broken, before any of the answer went
//! to the client: the gateway waits and sends again, within
//! `gateway.retry_wait_secs` of the request's arrival. A model with one
//! route — and a stream — goes back to the same route. Refusals are the
//! answer as before, and nothing is sent again once part of a stream went
//! out.
//!
//! The harness turns retries off (see `TestApp::try_spawn_with`); each
//! test here turns them on. Upstreams that say how long to wait
//! (`retry-after-ms`) keep the tests quick; one test waits out the
//! backoff itself.

use serde_json::Value;
use std::time::{Duration, Instant};
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A model routed to `uri`, and a key that may call it.
async fn seed(app: &TestApp, uri: &str, provider_type: &str, model: &str) -> String {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider =
        fixtures::create_provider(&app.db, &unique_name("retry"), provider_type, uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("retry-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap()
    .plaintext
}

async fn with_retries(secs: i64) -> TestApp {
    let app = TestApp::spawn().await;
    app.set_setting("gateway.retry_wait_secs", json!(secs))
        .await;
    app
}

/// An overloaded upstream that says to come back in 250 ms.
fn overloaded() -> ResponseTemplate {
    ResponseTemplate::new(503)
        .insert_header("retry-after-ms", "250")
        .set_body_json(json!({"error": {"message": "overloaded"}}))
}

fn chat_ok(model: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-retry",
        "object": "chat.completion",
        "created": 1_700_000_000_i64,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello world"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    }))
}

/// `first` for the first `times` requests, `then` after.
async fn upstream(
    at: &str,
    first: ResponseTemplate,
    times: u64,
    then: ResponseTemplate,
) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(at))
        .respond_with(first)
        .up_to_n_times(times)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(at))
        .respond_with(then)
        .mount(&server)
        .await;
    server
}

async fn sent(server: &MockServer) -> usize {
    server.received_requests().await.unwrap_or_default().len()
}

fn chat(model: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream, "messages": [{"role": "user", "content": "hi"}]})
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_whole_answer_goes_out_again_after_an_overload() {
    let app = with_retries(30).await;
    let server = upstream("/v1/chat/completions", overloaded(), 2, chat_ok("retry-1")).await;
    let key = seed(&app, &server.uri(), "openai", "retry-1").await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw
        .post("/v1/chat/completions", chat("retry-1", false))
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hello world");
    assert_eq!(sent(&server).await, 3, "two overloads, then the answer");
}

/// Without a word from the upstream, the first retry waits two seconds,
/// give or take a fifth.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn without_a_word_from_the_upstream_the_retry_waits_out_the_backoff() {
    let app = with_retries(30).await;
    let bad_gateway = ResponseTemplate::new(502).set_body_string("bad gateway");
    let server = upstream("/v1/chat/completions", bad_gateway, 1, chat_ok("retry-2")).await;
    let key = seed(&app, &server.uri(), "openai", "retry-2").await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let started = Instant::now();
    gw.post("/v1/chat/completions", chat("retry-2", false))
        .await
        .unwrap()
        .assert_ok();
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(1600), "{took:?}");
    assert_eq!(sent(&server).await, 2);
}

/// An upstream that says to come back at once is not asked again at
/// once: a zero wait is no word at all, and the backoff applies.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_wait_of_zero_from_the_upstream_still_waits_out_the_backoff() {
    let app = with_retries(30).await;
    let now = ResponseTemplate::new(503)
        .insert_header("retry-after-ms", "0")
        .insert_header("retry-after", "0")
        .set_body_json(json!({"error": {"message": "overloaded"}}));
    let server = upstream("/v1/chat/completions", now, 1, chat_ok("retry-zero")).await;
    let key = seed(&app, &server.uri(), "openai", "retry-zero").await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let started = Instant::now();
    gw.post("/v1/chat/completions", chat("retry-zero", false))
        .await
        .unwrap()
        .assert_ok();
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(1600), "{took:?}");
    assert_eq!(sent(&server).await, 2);
}

/// A refusal — a bad request, the upstream's rate limit — is the answer:
/// the upstream is asked once.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_refusal_is_not_sent_again() {
    let app = with_retries(30).await;
    for (status, model) in [
        (400u16, "retry-400"),
        (429, "retry-429"),
        (404, "retry-404"),
    ] {
        let refusal = ResponseTemplate::new(status)
            .insert_header("retry-after-ms", "250")
            .set_body_json(json!({"error": {"message": "no"}}));
        let server = upstream("/v1/chat/completions", refusal, 1, chat_ok(model)).await;
        let key = seed(&app, &server.uri(), "openai", model).await;

        let gw = app.gateway_client();
        gw.set_bearer(&key);
        let resp = gw
            .post("/v1/chat/completions", chat(model, false))
            .await
            .unwrap();
        resp.assert_status(status);
        assert_eq!(sent(&server).await, 1, "{status}");
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_zero_wait_never_sends_again() {
    let app = with_retries(0).await;
    let server = upstream(
        "/v1/chat/completions",
        overloaded(),
        1,
        chat_ok("retry-off"),
    )
    .await;
    let key = seed(&app, &server.uri(), "openai", "retry-off").await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw
        .post("/v1/chat/completions", chat("retry-off", false))
        .await
        .unwrap();
    resp.assert_status(503);
    assert_eq!(sent(&server).await, 1);
}

/// The retries stop when the next wait would end past the deadline, and
/// the last failure is the answer.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_retries_stop_at_the_deadline() {
    let app = with_retries(1).await;
    let slow_down = || {
        ResponseTemplate::new(503)
            .insert_header("retry-after-ms", "400")
            .set_body_json(json!({"error": {"message": "overloaded"}}))
    };
    let server = upstream("/v1/chat/completions", slow_down(), 100, slow_down()).await;
    let key = seed(&app, &server.uri(), "openai", "retry-deadline").await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let started = Instant::now();
    let resp = gw
        .post("/v1/chat/completions", chat("retry-deadline", false))
        .await
        .unwrap();
    resp.assert_status(503);
    assert!(started.elapsed() < Duration::from_secs(3));
    // 0 ms, ~400 ms, ~800 ms; a wait to ~1200 ms ends past the deadline.
    let n = sent(&server).await;
    assert!((2..=3).contains(&n), "{n}");
}

/// Two routes failing for the moment: both are asked again, and the one
/// that has recovered answers.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn every_route_that_failed_for_the_moment_is_asked_again() {
    let app = with_retries(30).await;
    let down = upstream("/v1/chat/completions", overloaded(), 100, overloaded()).await;
    let back = upstream(
        "/v1/chat/completions",
        overloaded(),
        1,
        chat_ok("retry-two"),
    )
    .await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    for server in [&down, &back] {
        let p = fixtures::create_provider(
            &app.db,
            &unique_name("retry-two"),
            "openai",
            &server.uri(),
            None,
        )
        .await
        .unwrap();
        fixtures::create_model_route(&app.db, p.id, "retry-two", 100)
            .await
            .unwrap();
    }
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("retry-two-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    gw.post("/v1/chat/completions", chat("retry-two", false))
        .await
        .unwrap()
        .assert_ok();
    assert_eq!(sent(&back).await, 2, "failed once, answered the retry");
    assert!(sent(&down).await >= 1);
}

/// A stream whose upstream cannot open goes out again: the client gets
/// one whole answer.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_that_cannot_open_goes_out_again() {
    let app = with_retries(30).await;
    let chunk = |delta: Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"id": "c", "object": "chat.completion.chunk", "created": 1,
                   "model": "retry-stream",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        )
    };
    let answer = [
        chunk(json!({"role": "assistant"}), Value::Null),
        chunk(json!({"content": "hello"}), Value::Null),
        chunk(json!({}), json!("stop")),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let ok = ResponseTemplate::new(200).set_body_raw(answer, "text/event-stream");
    let server = upstream("/v1/chat/completions", overloaded(), 1, ok).await;
    let key = seed(&app, &server.uri(), "openai", "retry-stream").await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw
        .post("/v1/chat/completions", chat("retry-stream", true))
        .await
        .unwrap();
    resp.assert_ok();
    let text = resp.text();
    assert!(text.contains("hello"), "{text}");
    assert!(!text.contains("overloaded"), "{text}");
    assert_eq!(text.matches("\"role\":\"assistant\"").count(), 1, "{text}");
    assert_eq!(sent(&server).await, 2);
}

fn anthropic_event(kind: &str, v: Value) -> String {
    format!("event: {kind}\ndata: {v}\n\n")
}

fn message_start(model: &str) -> String {
    anthropic_event(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": null,
            "usage": {"input_tokens": 12, "output_tokens": 1}
        }}),
    )
}

fn overload_event() -> String {
    anthropic_event(
        "error",
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    )
}

fn text_block(text: &str) -> String {
    [
        anthropic_event(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ),
        anthropic_event(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": text}}),
        ),
    ]
    .concat()
}

fn messages(model: &str) -> Value {
    json!({"model": model, "max_tokens": 100, "stream": true,
           "messages": [{"role": "user", "content": "hi"}]})
}

/// Anthropic opens its stream and then reports an overload, before any
/// content: the opening was held back, the stream goes out again, and
/// the client sees one answer.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_overloaded_before_any_content_goes_out_again() {
    let app = with_retries(30).await;
    let model = "retry-claude";
    let failed = [message_start(model), overload_event()].concat();
    let answer = [
        message_start(model),
        text_block("Hello"),
        anthropic_event(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        anthropic_event(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 2}}),
        ),
        anthropic_event("message_stop", json!({"type": "message_stop"})),
    ]
    .concat();
    let server = upstream(
        "/v1/messages",
        ResponseTemplate::new(200).set_body_raw(failed, "text/event-stream"),
        1,
        ResponseTemplate::new(200).set_body_raw(answer, "text/event-stream"),
    )
    .await;
    let key = seed(&app, &server.uri(), "anthropic", model).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw.post("/v1/messages", messages(model)).await.unwrap();
    resp.assert_ok();
    let text = resp.text();
    assert_eq!(text.matches("event: message_start").count(), 1, "{text}");
    assert!(text.contains("Hello"), "{text}");
    assert!(text.contains("event: message_stop"), "{text}");
    assert!(!text.contains("overloaded_error"), "{text}");
    assert_eq!(sent(&server).await, 2);
}

/// Part of the answer went out before the overload: the client gets it
/// and the error, and the upstream is not asked again.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_that_failed_after_content_is_not_sent_again() {
    let app = with_retries(30).await;
    let model = "retry-claude-late";
    let failed = [message_start(model), text_block("Hel"), overload_event()].concat();
    let server = upstream(
        "/v1/messages",
        ResponseTemplate::new(200).set_body_raw(failed.clone(), "text/event-stream"),
        1,
        ResponseTemplate::new(200).set_body_raw(failed, "text/event-stream"),
    )
    .await;
    let key = seed(&app, &server.uri(), "anthropic", model).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw.post("/v1/messages", messages(model)).await.unwrap();
    resp.assert_ok();
    let text = resp.text();
    assert!(text.contains("Hel"), "{text}");
    assert!(text.contains("overloaded_error"), "{text}");
    assert_eq!(sent(&server).await, 1);
}

/// A stream the upstream refuses before any content — a bad request —
/// is the answer: the opening and the refusal reach the client, once.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_refused_before_any_content_is_not_sent_again() {
    let app = with_retries(30).await;
    let model = "retry-claude-refused";
    let refused = [
        message_start(model),
        anthropic_event(
            "error",
            json!({"type": "error", "error": {"type": "invalid_request_error",
                                               "message": "prompt is too long"}}),
        ),
    ]
    .concat();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(refused, "text/event-stream"))
        .mount(&server)
        .await;
    let key = seed(&app, &server.uri(), "anthropic", model).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw.post("/v1/messages", messages(model)).await.unwrap();
    resp.assert_ok();
    let text = resp.text();
    assert_eq!(text.matches("event: message_start").count(), 1, "{text}");
    assert!(text.contains("prompt is too long"), "{text}");
    assert_eq!(sent(&server).await, 1);
}

/// While a stream waits to go out again, a Chat client hears a keep-alive
/// comment after 15 seconds of silence; a Gemini client reading SSE does
/// not — the google-genai SDK reads every line as JSON.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn only_non_gemini_clients_get_keep_alives_while_a_stream_waits() {
    let app = with_retries(60).await;
    let chunk = |model: &str, delta: Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"id": "c", "object": "chat.completion.chunk", "created": 1, "model": model,
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        )
    };
    let stream_of = |model: &str| {
        [
            chunk(model, json!({"content": "hello"}), Value::Null),
            chunk(model, json!({}), json!("stop")),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat()
    };
    // Overloaded once, saying to come back in 16 s: longer than the
    // keep-alive interval.
    let busy = || {
        ResponseTemplate::new(503)
            .insert_header("retry-after-ms", "16000")
            .set_body_json(json!({"error": {"message": "overloaded"}}))
    };
    let mut keys = Vec::new();
    for model in ["retry-ka-chat", "retry-ka-gemini"] {
        let ok = ResponseTemplate::new(200).set_body_raw(stream_of(model), "text/event-stream");
        let server = upstream("/v1/chat/completions", busy(), 1, ok).await;
        keys.push((seed(&app, &server.uri(), "openai", model).await, server));
    }

    let chat = async {
        reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", app.gateway_url))
            .bearer_auth(&keys[0].0)
            .json(&chat("retry-ka-chat", true))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    let gemini = async {
        reqwest::Client::new()
            .post(format!(
                "{}/v1beta/models/retry-ka-gemini:streamGenerateContent?alt=sse",
                app.gateway_url
            ))
            .header("x-goog-api-key", &keys[1].0)
            .json(&json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    let (chat, gemini) = tokio::join!(chat, gemini);

    assert!(chat.contains(": keep-alive\n\n"), "{chat}");
    assert!(chat.contains("hello"), "{chat}");
    assert!(gemini.contains("hello"), "{gemini}");
    for line in gemini.lines().filter(|l| !l.is_empty()) {
        let data = line
            .strip_prefix("data: ")
            .unwrap_or_else(|| panic!("not a data line: {line:?}\n{gemini}"));
        serde_json::from_str::<Value>(data).unwrap_or_else(|e| panic!("{e}: {data}"));
    }
    for (_, server) in &keys {
        assert_eq!(sent(server).await, 2);
    }
}
