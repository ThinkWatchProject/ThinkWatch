//! The Responses API over a WebSocket: `GET /v1/responses` upgraded, one
//! `response.create` text frame per turn, the stream's events back as
//! frames.
//!
//! Each turn runs the HTTP pipeline, so the guarantees pinned here are
//! the ones a pipe between two sockets would lose: the key is checked on
//! the upgrade, limits apply per turn, every turn is converted to the
//! route's format, billed and logged, and a refusal is a `response.failed`
//! frame that leaves the connection usable.

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use think_watch_test_support::prelude::*;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn seed(app: &TestApp, upstream: &str, model: &str) -> (Uuid, String) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(&app.db, &unique_name("ws"), "openai", upstream, None)
        .await
        .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(&app.db, user.user.id, "w", &["ai_gateway"], None, None)
        .await
        .unwrap();
    (user.user.id, key.plaintext)
}

fn ws_url(app: &TestApp) -> String {
    format!(
        "ws://{}/v1/responses",
        app.gateway_url.trim_start_matches("http://")
    )
}

async fn connect(app: &TestApp, key: &str) -> Socket {
    let mut req = ws_url(app).into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("upgrade accepted");
    socket
}

fn create(model: &str, input: &str) -> Message {
    Message::Text(json!({"type": "response.create", "model": model, "input": input}).to_string())
}

/// Read events until the turn ends (`response.completed` or
/// `response.failed`), and return them all.
async fn turn(socket: &mut Socket) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("an event within 10s")
            .expect("connection open")
            .expect("frame");
        let Message::Text(t) = next else { continue };
        let v: Value = serde_json::from_str(t.as_str()).expect("every frame is JSON");
        let done = matches!(
            v["type"].as_str(),
            Some("response.completed" | "response.failed")
        );
        events.push(v);
        if done {
            return events;
        }
    }
}

fn text_of(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .filter_map(|e| e["delta"].as_str())
        .collect()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn turns_on_one_connection_are_answered_billed_and_logged() {
    let app = TestApp::spawn_with_clickhouse().await;
    let upstream = MockProvider::openai_chat_stream_ok("ws-model").await;
    let (user_id, key) = seed(&app, &upstream.uri(), "ws-model").await;

    let mut socket = connect(&app, &key).await;
    for _ in 0..2 {
        socket.send(create("ws-model", "hi")).await.unwrap();
        let events = turn(&mut socket).await;
        assert_eq!(events[0]["type"], "response.created", "{events:?}");
        let last = events.last().unwrap();
        assert_eq!(last["type"], "response.completed", "{events:?}");
        assert_eq!(last["response"]["model"], "ws-model", "{events:?}");
        assert_eq!(text_of(&events), "hi there", "{events:?}");
    }
    socket.close(None).await.unwrap();

    // Each turn was a request to the route's upstream, converted to its
    // format.
    let sent = upstream.received_requests().await;
    assert_eq!(sent.len(), 2);
    let body: Value = sent[0].body_json().unwrap();
    assert_eq!(body["model"], "ws-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][0]["content"], "hi");

    // And each has its own billed audit row.
    let ch = app.state.clickhouse.as_ref().expect("CH wired up");
    let mut rows = Vec::new();
    for _ in 0..100 {
        rows = ch
            .query(
                "SELECT ifNull(input_tokens, -1), ifNull(output_tokens, -1), ifNull(status_code, -1) \
                   FROM gateway_logs WHERE user_id = ?",
            )
            .bind(user_id.to_string())
            .fetch_all::<(i64, i64, i64)>()
            .await
            .expect("CH query");
        if rows.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(rows, vec![(5, 4, 200), (5, 4, 200)], "gateway_logs rows");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_upgrade_needs_a_valid_key() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("ws-nokey").await;
    seed(&app, &upstream.uri(), "ws-nokey").await;

    let err = tokio_tungstenite::connect_async(ws_url(&app))
        .await
        .expect_err("refused without a key");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(r) => assert_eq!(r.status(), 401),
        other => panic!("expected an HTTP refusal, got {other}"),
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn limits_apply_per_turn_and_a_refusal_keeps_the_connection() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("ws-limited").await;
    let (user_id, key) = seed(&app, &upstream.uri(), "ws-limited").await;
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 60, 1)
        .await
        .unwrap();

    let mut socket = connect(&app, &key).await;
    socket.send(create("ws-limited", "one")).await.unwrap();
    let first = turn(&mut socket).await;
    assert_eq!(first.last().unwrap()["type"], "response.completed");

    socket.send(create("ws-limited", "two")).await.unwrap();
    let second = turn(&mut socket).await;
    let failed = second.last().unwrap();
    assert_eq!(failed["type"], "response.failed", "{second:?}");
    assert_eq!(
        failed["response"]["error"]["code"], "rate_limit_exceeded",
        "{second:?}"
    );
    assert_eq!(upstream.received_requests().await.len(), 1);

    // A frame that is not a turn is refused the same way, and the
    // connection is still there after it.
    socket
        .send(Message::Text(json!({"type": "session.update"}).to_string()))
        .await
        .unwrap();
    let refused = turn(&mut socket).await;
    assert_eq!(refused.last().unwrap()["type"], "response.failed");
    socket.send(Message::Ping(vec![1])).await.unwrap();
}
