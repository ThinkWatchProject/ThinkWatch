//! Cache-hit + streaming-cancellation contract for the AI gateway.
//!
//! `gateway_proxy.rs::openai_chat_completion_streaming_relays_sse`
//! covers the streaming MISS path. This file pins the cache-hit and
//! cancellation branches that aren't exercised elsewhere:
//!
//!   - non-streaming cache hit: identical deterministic
//!     (`temperature=0`) requests reach the upstream once; the second
//!     call gets `X-Cache: HIT` and an identical body without
//!     touching the upstream
//!   - streaming cache hit: the proxy assembles the upstream's SSE
//!     into a `ChatCompletionResponse`, caches it, and on a follow-up
//!     `stream=true` request re-emits it as a single-chunk SSE with
//!     the same `X-Cache: HIT` marker — no second upstream call
//!   - non-deterministic requests (`temperature > 0`) are NOT cached;
//!     two calls must each reach the upstream
//!   - streaming client disconnect: dropping the consumer mid-stream
//!     fires the `on_done` callback with `ClientCancelled`, which
//!     emits a `gateway_logs` row with `status_code = 499` and
//!     `stream_outcome = client_cancelled` (the marker the trace UI
//!     and dashboards split on)

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Seed a developer + provider + model + API key, return the bearer.
async fn seed(app: &TestApp, upstream_url: &str, model: &str) -> String {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("cache-prov"),
        "openai",
        upstream_url,
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "cache-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    key.plaintext
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn cache_hit_skips_upstream_and_sets_x_cache_header() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("cache-hit-test").await;
    let bearer = seed(&app, &upstream.uri(), "cache-hit-test").await;

    let gw = app.gateway_client();
    gw.set_bearer(&bearer);

    let body = json!({
        "model": "cache-hit-test",
        "messages": [{"role": "user", "content": "deterministic ping"}],
        "temperature": 0
    });

    let first = gw.post("/v1/chat/completions", body.clone()).await.unwrap();
    first.assert_ok();
    let first_body: Value = first.json().unwrap();
    assert_eq!(
        first
            .headers
            .get("x-cache")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "MISS",
        "first identical call must be a cache MISS"
    );

    let second = gw.post("/v1/chat/completions", body.clone()).await.unwrap();
    second.assert_ok();
    assert_eq!(
        second
            .headers
            .get("x-cache")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "HIT",
        "second identical call must be a cache HIT"
    );
    let second_body: Value = second.json().unwrap();
    assert_eq!(
        second_body["choices"][0]["message"]["content"],
        first_body["choices"][0]["message"]["content"],
        "cached body should match the first response verbatim"
    );

    // Upstream should have seen exactly one request despite two
    // identical client calls. This is the whole point of the cache.
    let received = upstream.received_requests().await;
    assert_eq!(
        received.len(),
        1,
        "cache hit must skip the upstream — got {} upstream requests",
        received.len()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn temperature_nonzero_request_is_not_cached() {
    // The cache key only commits when `temperature == 0` (or absent)
    // — anything that asks for sampled output skips the cache so two
    // identical prompts can produce two genuinely different replies.
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("nondet-test").await;
    let bearer = seed(&app, &upstream.uri(), "nondet-test").await;

    let gw = app.gateway_client();
    gw.set_bearer(&bearer);

    let body = json!({
        "model": "nondet-test",
        "messages": [{"role": "user", "content": "sampling please"}],
        "temperature": 0.7
    });
    gw.post("/v1/chat/completions", body.clone())
        .await
        .unwrap()
        .assert_ok();
    gw.post("/v1/chat/completions", body)
        .await
        .unwrap()
        .assert_ok();

    let received = upstream.received_requests().await;
    assert_eq!(
        received.len(),
        2,
        "temperature>0 must NOT be cached — both calls should hit upstream"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn streaming_cache_hit_replays_assembled_sse() {
    // The streaming MISS path consumes upstream chunks, lets the
    // client see them, and the on_done callback assembles them into
    // a `ChatCompletionResponse` it stashes in the cache. A follow-up
    // streaming request with the SAME body then gets re-emitted as
    // `data: <json>\n\ndata: [DONE]\n\n` — a single-chunk SSE — and
    // the upstream is NOT contacted.
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("stream-cache-test").await;
    let bearer = seed(&app, &upstream.uri(), "stream-cache-test").await;

    let gw = app.gateway_client();
    gw.set_bearer(&bearer);

    let body = json!({
        "model": "stream-cache-test",
        "messages": [{"role": "user", "content": "streamy"}],
        "stream": true,
        "temperature": 0
    });

    // First request — MISS, body should contain the assembled chunks.
    let first = gw.post("/v1/chat/completions", body.clone()).await.unwrap();
    first.assert_ok();
    let first_body = first.text();
    assert!(
        first_body.contains("[DONE]"),
        "MISS path must terminate the SSE with [DONE]: {first_body}"
    );

    // The cache write happens in the post-flight `on_done` callback,
    // which is spawned off the response stream — not synchronous with
    // the response body. Wait briefly for it to land before firing the
    // second request. 50 * 50ms = 2.5s upper bound; in practice the
    // callback runs in single-digit ms after the [DONE] token.
    for _ in 0..50 {
        let probe = gw.post("/v1/chat/completions", body.clone()).await.unwrap();
        if probe.headers.get("x-cache").and_then(|v| v.to_str().ok()) == Some("HIT") {
            // Found a HIT — assert the rest of the contract on this response.
            let txt = probe.text();
            assert!(
                txt.contains("[DONE]"),
                "HIT replay must still emit [DONE]: {txt}"
            );
            assert!(
                txt.contains("\"object\":\"chat.completion\""),
                "HIT replay should serialize the assembled ChatCompletionResponse: {txt}"
            );
            // Upstream got exactly ONE call across all client requests.
            // (The MockProvider wraps the SSE upstream — count its hits.)
            let received = upstream.received_requests().await;
            assert_eq!(
                received.len(),
                1,
                "streaming cache hit must skip upstream — got {} upstream calls",
                received.len()
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("streaming response was never cached — second request never observed X-Cache: HIT");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn streaming_client_disconnect_emits_cancelled_gateway_log() {
    // Drop the SSE consumer mid-flight. The `on_done` callback in the
    // streaming branch must fire with `StreamOutcome::ClientCancelled`,
    // which writes a gateway_logs row with `status_code = 499` and
    // `stream_outcome = client_cancelled` in the detail blob.
    //
    // Recipe: an upstream that holds the response open (long initial
    // delay) so the gateway's SSE body stream is parked waiting on
    // the first chunk when the client times out.
    let app = TestApp::spawn_with_clickhouse().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();

    // Slow upstream — 5s delay before any chunk; we'll drop after ~150ms.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(
                    b"data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
                    "text/event-stream",
                )
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&server)
        .await;

    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("cancel-prov"),
        "openai",
        &server.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "cancel-stream")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "cancel-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    // Raw reqwest with an aggressive total-request timeout — when it
    // fires, the in-flight request future is dropped, which closes
    // the gateway-side TCP connection. The gateway's SSE body future
    // is dropped, which drops `done_tx`, which the spawned on_done
    // task picks up as ClientCancelled.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(150))
        .build()
        .unwrap();
    let url = format!("{}/v1/chat/completions", app.gateway_url);
    let body = serde_json::json!({
        "model": "cancel-stream",
        "messages": [{"role": "user", "content": "stop me"}],
        "stream": true,
        "temperature": 0.7
    });
    let result = client
        .post(&url)
        .bearer_auth(&key.plaintext)
        .json(&body)
        .send()
        .await;
    // Either we time out (expected) or we got a partial response that
    // we now drop. Both end with the gateway seeing a disconnect.
    drop(result);

    // Wait for the cancelled row to land in ClickHouse. The audit
    // pipeline batches with a small flush interval; give it a few
    // seconds.
    let ch = app.state.clickhouse.as_ref().expect("CH wired up");
    let mut found: Option<(i64, String)> = None;
    for _ in 0..100 {
        let row: Option<(i64, String)> = ch
            .query(
                "SELECT ifNull(status_code, -1), ifNull(detail, '') FROM gateway_logs \
                   WHERE user_id = ? ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user.user.id.to_string())
            .fetch_optional()
            .await
            .expect("CH query");
        if let Some(r) = row {
            found = Some(r);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (status, detail) = found.expect("gateway_logs cancelled row never landed");
    assert_eq!(
        status, 499,
        "client cancel must record status_code=499 (Nginx convention), got {status}; detail={detail}"
    );
    assert!(
        detail.contains("client_cancelled"),
        "detail must carry stream_outcome=client_cancelled marker, got {detail}"
    );
}
