//! End-to-end coverage for real chunk-by-chunk SSE pass-through on
//! the MCP gateway. Spins up a tiny axum upstream that emits three
//! SSE events with 100 ms gaps and calls `tools/call` through the
//! gateway with `Accept: text/event-stream`. The unit tests in
//! `mcp-gateway/src/proxy.rs::passthrough_tests` cover the parser in
//! isolation; this file proves the wired-up gateway forwards chunks
//! AS THEY ARRIVE end-to-end — buffering would collapse the
//! inter-event gaps and fail the timing assertion.

use axum::{
    Router,
    response::sse::{Event, Sse},
    routing::post,
};
use bytes::Bytes;
use futures::StreamExt;
use std::convert::Infallible;
use std::time::{Duration, Instant};
use think_watch_test_support::prelude::*;

/// Upstream handler that emits three JSON-RPC SSE events with 100 ms
/// gaps. No `keep_alive` so the gateway's chunk parser sees exactly
/// three `\n\n`-delimited events (no `:keepalive` comments).
async fn delayed_sse_handler() -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let body = async_stream::stream! {
        let envelopes = [
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":33}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"pct":66}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":"done"}}"#,
        ];
        for (i, payload) in envelopes.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            yield Ok::<Event, Infallible>(Event::default().data(*payload));
        }
    };
    Sse::new(body)
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn chunks_forwarded_as_they_arrive() {
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();

    // Bind on a random port and spawn the tiny upstream.
    let upstream_router = Router::new().route("/mcp", post(delayed_sse_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, upstream_router).await;
    });
    let upstream_url = format!("http://{upstream_addr}/mcp");

    // Persist + register the MCP server. Anonymous shape so the
    // proxy's credential resolver short-circuits to None — the
    // upstream sees no Authorization header, which matches our
    // mock handler's expectations.
    let server_id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("stream-test"),
        "stream",
        &upstream_url,
        fixtures::McpServerOpts::default(),
    )
    .await
    .unwrap();
    let server_row = sqlx::query_as::<_, think_watch_common::models::McpServer>(
        "SELECT * FROM mcp_servers WHERE id = $1",
    )
    .bind(server_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    let registered = think_watch_server::mcp_runtime::build_registered_server(
        &app.db,
        &server_row,
        &app.state.config.encryption_key,
    )
    .await
    .unwrap();
    app.state.mcp_registry.register(registered).await;

    // Mint an API key with mcp_gateway surface.
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "stream-key",
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    // Raw reqwest call — TestApp's signed-client wrapper buffers
    // the body, so we go direct here to read bytes incrementally.
    let client = reqwest::Client::new();
    let url = format!("{}/mcp", app.gateway_url);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "stream__anything", "arguments": {} }
    });
    let started = Instant::now();
    let resp = client
        .post(&url)
        .bearer_auth(&key.plaintext)
        .header("Accept", "text/event-stream")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "expected 2xx from /mcp, got {}: {:?}",
        resp.status(),
        resp.text().await
    );
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.contains("text/event-stream"),
        "expected SSE content-type, got {content_type:?}"
    );

    // Read chunks one at a time, recording arrival timestamps per
    // completed event. We treat every `\n\n`-delimited block that
    // carries at least one `data:` line as an event.
    let mut event_timestamps: Vec<Duration> = Vec::new();
    let mut text_buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes: Bytes = chunk.unwrap();
        let s = String::from_utf8_lossy(&bytes);
        text_buf.push_str(&s);
        while let Some(idx) = text_buf.find("\n\n") {
            let block: String = text_buf.drain(..idx + 2).collect();
            if block.lines().any(|l| l.starts_with("data:")) {
                event_timestamps.push(started.elapsed());
            }
        }
    }

    assert!(
        event_timestamps.len() >= 3,
        "expected ≥3 SSE events from the timeline, got {}: {event_timestamps:?}",
        event_timestamps.len()
    );

    // The decisive assertion: span between the first and last event
    // must reflect the upstream's 100 ms gaps. Buffering would
    // collapse them to near-zero (the proxy would flush all three
    // at once after the upstream completed).
    let first = *event_timestamps.first().unwrap();
    let last = *event_timestamps.last().unwrap();
    let span = last - first;
    assert!(
        span >= Duration::from_millis(150),
        "events arrived bunched (span={span:?}, timestamps={event_timestamps:?}); \
         expected ≥150 ms span across 3 events with 100 ms upstream gaps — buffering regression?"
    );
}
