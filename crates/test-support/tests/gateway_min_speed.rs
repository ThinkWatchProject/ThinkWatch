//! Cutting a stream its upstream generates too slowly.
//!
//! A provider with `min_speed_tokens_per_sec` set has its streams judged
//! once a second from the first token on: below the minimum over the last
//! `min_speed_window_secs` (pauses of 5 s or more left out), the stream is
//! cut with a 503 error in the client's format, the upstream's connection
//! dropped and the route charged a failure. The upstreams here are paced
//! servers (`sse_upstream_paced`), so each test takes as long as its
//! stream: the windows are the shortest allowed.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;
use think_watch_test_support::mock_provider::sse_upstream_paced;
use think_watch_test_support::prelude::*;

/// A model routed to `uri` under a provider that cuts streams below
/// `tokens_per_sec` over `window_secs`; a key that may call it, and the
/// route's id.
async fn seed(
    app: &TestApp,
    uri: &str,
    provider_type: &str,
    model: &str,
    tokens_per_sec: i32,
    window_secs: i32,
) -> (String, uuid::Uuid) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider =
        fixtures::create_provider(&app.db, &unique_name("slow"), provider_type, uri, None)
            .await
            .unwrap();
    sqlx::query(
        "UPDATE providers SET min_speed_tokens_per_sec = $2, min_speed_window_secs = $3 \
         WHERE id = $1",
    )
    .bind(provider.id)
    .bind(tokens_per_sec)
    .bind(window_secs)
    .execute(&app.db)
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    let route_id: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM model_routes WHERE provider_id = $1")
            .bind(provider.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("slow-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap()
    .plaintext;
    (key, route_id)
}

fn chat_chunk(model: &str, delta: Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-slow",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_i64,
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    )
}

/// A Chat stream: the opening, then `tokens` ASCII tokens of text in each
/// `(wait, tokens)` piece, then the end.
fn chat_stream(model: &str, pieces: &[(f64, usize)]) -> Vec<(Duration, String)> {
    let mut out = vec![(
        Duration::ZERO,
        chat_chunk(model, json!({"role": "assistant"}), None),
    )];
    for (wait, tokens) in pieces {
        out.push((
            Duration::from_secs_f64(*wait),
            chat_chunk(model, json!({"content": "abcd".repeat(*tokens)}), None),
        ));
    }
    out.push((
        Duration::ZERO,
        [
            chat_chunk(model, json!({}), Some("stop")),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat(),
    ));
    out
}

fn chat(model: &str) -> Value {
    json!({"model": model, "stream": true, "messages": [{"role": "user", "content": "hi"}]})
}

/// The route's failures in the breaker's window, once the stream's tail
/// has recorded it.
async fn errors_on(app: &TestApp, route_id: uuid::Uuid) -> u32 {
    use think_watch_gateway::health::{CircuitBreakerConfig, HealthTracker};
    let tracker = HealthTracker::new(app.state.redis.clone());
    let cfg = CircuitBreakerConfig::load(&app.state.dynamic_config).await;
    for _ in 0..50 {
        let h = tracker.snapshot(route_id, cfg).await;
        if h.total > 0 {
            return h.errors;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the stream's outcome was never recorded against the route");
}

/// One token a second against a minimum of five: cut about five seconds
/// after the first token, with a Chat error the client can read, the
/// upstream hung up on, and a failure on the route.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_slower_than_the_minimum_is_cut() {
    let app = TestApp::spawn().await;
    let model = "slow-chat";
    let pieces: Vec<(f64, usize)> = (0..30)
        .map(|i| (if i == 0 { 0.0 } else { 1.0 }, 1))
        .collect();
    let (uri, hung_up) =
        sse_upstream_paced("/v1/chat/completions", chat_stream(model, &pieces)).await;
    let (key, route_id) = seed(&app, &uri, "openai", model, 5, 5).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let started = Instant::now();
    let resp = gw.post("/v1/chat/completions", chat(model)).await.unwrap();
    let took = started.elapsed();
    resp.assert_ok();
    let text = resp.text();
    assert!(
        text.contains("abcd"),
        "the answer so far reaches the client: {text}"
    );
    assert!(
        text.contains("tokens per second over the last 5 seconds, below the minimum of 5."),
        "{text}"
    );
    assert!(!text.contains("[DONE]"), "{text}");
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(12),
        "cut after {took:?}"
    );
    // The error frame is Chat's, with a 503's class.
    let error: Value = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find(|v| v.get("error").is_some())
        .expect("an error frame");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("Upstream `")),
        "{error}"
    );

    assert_eq!(errors_on(&app, route_id).await, 1);
    for _ in 0..20 {
        if hung_up.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        hung_up.load(Ordering::SeqCst),
        "the upstream was not hung up on"
    );
}

/// An Anthropic client is told with Anthropic's error for a 503,
/// `overloaded_error`, which its SDK retries.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn an_anthropic_client_is_told_the_upstream_is_overloaded() {
    let app = TestApp::spawn().await;
    let model = "slow-claude";
    let event = |v: Value| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
    let mut pieces = vec![(
        Duration::ZERO,
        [
            event(json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "model": model,
                "content": [], "stop_reason": null,
                "usage": {"input_tokens": 12, "output_tokens": 1}
            }})),
            event(json!({"type": "content_block_start", "index": 0,
                         "content_block": {"type": "text", "text": ""}})),
        ]
        .concat(),
    )];
    for _ in 0..30 {
        pieces.push((
            Duration::from_secs(1),
            event(json!({"type": "content_block_delta", "index": 0,
                         "delta": {"type": "text_delta", "text": "abcd"}})),
        ));
    }
    let (uri, _) = sse_upstream_paced("/v1/messages", pieces).await;
    let (key, _) = seed(&app, &uri, "anthropic", model, 5, 5).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw
        .post(
            "/v1/messages",
            json!({"model": model, "max_tokens": 100, "stream": true,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let text = resp.text();
    assert!(text.contains("event: error"), "{text}");
    assert!(text.contains("overloaded_error"), "{text}");
    assert!(text.contains("below the minimum of 5."), "{text}");
}

/// Eight tokens a second against a minimum of five runs to its end.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_at_speed_runs_to_its_end() {
    let app = TestApp::spawn().await;
    let model = "fast-chat";
    let pieces: Vec<(f64, usize)> = (0..16)
        .map(|i| (if i == 0 { 0.0 } else { 0.5 }, 4))
        .collect();
    let (uri, hung_up) =
        sse_upstream_paced("/v1/chat/completions", chat_stream(model, &pieces)).await;
    let (key, route_id) = seed(&app, &uri, "openai", model, 5, 5).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw.post("/v1/chat/completions", chat(model)).await.unwrap();
    resp.assert_ok();
    let text = resp.text();
    assert!(text.contains("[DONE]"), "{text}");
    assert!(!text.contains("tokens per second"), "{text}");
    assert_eq!(errors_on(&app, route_id).await, 0);
    assert!(!hung_up.load(Ordering::SeqCst));
}

/// A long pause is the upstream not generating, not generating slowly:
/// eight tokens a second for 6 s, 8 s of silence, then on. Counting the
/// pause, the last 10 s would drop below three a second around 13 s.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_long_pause_does_not_cut_the_stream() {
    let app = TestApp::spawn().await;
    let model = "pausing-chat";
    let mut pieces: Vec<(f64, usize)> = (0..13)
        .map(|i| (if i == 0 { 0.0 } else { 0.5 }, 4))
        .collect();
    pieces.push((8.0, 4));
    pieces.extend((0..4).map(|_| (0.5, 4)));
    let (uri, _) = sse_upstream_paced("/v1/chat/completions", chat_stream(model, &pieces)).await;
    let (key, route_id) = seed(&app, &uri, "openai", model, 3, 10).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw.post("/v1/chat/completions", chat(model)).await.unwrap();
    resp.assert_ok();
    let text = resp.text();
    assert!(text.contains("[DONE]"), "{text}");
    assert!(!text.contains("tokens per second"), "{text}");
    assert_eq!(errors_on(&app, route_id).await, 0);
}

/// Without a minimum, a slow stream runs as it always did.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn without_a_minimum_a_slow_stream_is_left_alone() {
    let app = TestApp::spawn().await;
    let model = "slow-unlimited";
    let pieces: Vec<(f64, usize)> = (0..7)
        .map(|i| (if i == 0 { 0.0 } else { 1.0 }, 1))
        .collect();
    let (uri, _) = sse_upstream_paced("/v1/chat/completions", chat_stream(model, &pieces)).await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(&app.db, &unique_name("slow"), "openai", &uri, None)
        .await
        .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("slow-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap()
    .plaintext;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let resp = gw.post("/v1/chat/completions", chat(model)).await.unwrap();
    resp.assert_ok();
    assert!(resp.text().contains("[DONE]"), "{}", resp.text());
}

/// The console takes the minimum in range, turns it off with `null`, and
/// refuses one out of range.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_console_sets_and_clears_a_providers_minimum() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("speed"),
        "openai",
        "https://api.openai.com",
        None,
    )
    .await
    .unwrap();
    let at = format!("/api/admin/providers/{}", provider.id);

    let got = con.get(&at).await.unwrap();
    got.assert_ok();
    let v: Value = got.json().unwrap();
    assert_eq!(v["min_speed_tokens_per_sec"], Value::Null);
    assert_eq!(v["min_speed_window_secs"], 20);

    let set = con
        .patch(
            &at,
            json!({"min_speed_tokens_per_sec": 15, "min_speed_window_secs": 30}),
        )
        .await
        .unwrap();
    set.assert_ok();
    let v: Value = set.json().unwrap();
    assert_eq!(v["min_speed_tokens_per_sec"], 15);
    assert_eq!(v["min_speed_window_secs"], 30);

    // Another edit leaves it as it is.
    let renamed = con
        .patch(&at, json!({"display_name": "Renamed"}))
        .await
        .unwrap();
    renamed.assert_ok();
    assert_eq!(
        renamed.json::<Value>().unwrap()["min_speed_tokens_per_sec"],
        15
    );

    for bad in [
        json!({"min_speed_tokens_per_sec": 0}),
        json!({"min_speed_tokens_per_sec": 1001}),
        json!({"min_speed_window_secs": 4}),
        json!({"min_speed_window_secs": 121}),
    ] {
        con.patch(&at, bad.clone())
            .await
            .unwrap()
            .assert_status(400);
    }

    let cleared = con
        .patch(&at, json!({"min_speed_tokens_per_sec": null}))
        .await
        .unwrap();
    cleared.assert_ok();
    let v: Value = cleared.json().unwrap();
    assert_eq!(v["min_speed_tokens_per_sec"], Value::Null);
    assert_eq!(v["min_speed_window_secs"], 30);
}
