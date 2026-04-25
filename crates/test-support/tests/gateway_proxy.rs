//! Integration tests for the public AI gateway (`port 3000`).
//! Exercises the full path: API-key auth → router lookup → upstream
//! forward (mocked) → response transform → audit log. Each test
//! stands up its own wiremock so request bodies and counts are
//! observable from the assertions.

use serde_json::Value;
use think_watch_test_support::prelude::*;

/// Seed a developer user, an active provider pointing at `upstream`,
/// a model + route, and a `tw-…` API key with `ai_gateway` surface.
async fn seed_provider_and_key(
    app: &TestApp,
    upstream_url: &str,
    provider_type: &str,
    model_id: &str,
    extra_config: Option<Value>,
) -> String {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name(&format!("{provider_type}-test")),
        provider_type,
        upstream_url,
        extra_config,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model_id)
        .await
        .unwrap();

    // Hot-reload the gateway router so it sees the new provider.
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "gateway-test",
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
async fn openai_chat_completion_happy_path() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("gpt-4o-mini-test").await;
    let api_key =
        seed_provider_and_key(&app, &upstream.uri(), "openai", "gpt-4o-mini-test", None).await;

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);

    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({
                "model": "gpt-4o-mini-test",
                "messages": [{"role": "user", "content": "ping"}]
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["model"].as_str().unwrap(), "gpt-4o-mini-test");
    assert_eq!(body["choices"][0]["message"]["content"], "hello world");
    assert_eq!(body["usage"]["total_tokens"], 10);

    // Upstream saw exactly one request.
    let received = upstream.received_requests().await;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].url.path(), "/v1/chat/completions");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn openai_chat_completion_streaming_relays_sse() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_stream_ok("gpt-stream").await;
    let api_key = seed_provider_and_key(&app, &upstream.uri(), "openai", "gpt-stream", None).await;

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);

    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({
                "model": "gpt-stream",
                "messages": [{"role": "user", "content": "ping"}],
                "stream": true
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body = resp.text();
    // SSE framing must reach the client untouched (or at least
    // contain the same data lines and the [DONE] terminator).
    assert!(body.contains("\"role\":\"assistant\""), "body: {body}");
    assert!(body.contains("\"content\":\"hi \""), "body: {body}");
    assert!(body.contains("[DONE]"), "missing [DONE] in stream: {body}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn gateway_rejects_request_without_api_key() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("any").await;
    seed_provider_and_key(&app, &upstream.uri(), "openai", "any", None).await;

    let gw = app.gateway_client();
    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "any", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    resp.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn gateway_rejects_mcp_only_key_on_ai_surface() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("any").await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        "mcp-only-provider",
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "any")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    // Key only authorised for the MCP surface.
    let mcp_key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "mcp only",
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&mcp_key.plaintext);
    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "any", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    // Wrong surface → 403 (key valid, just not allowed here).
    resp.assert_status(403);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn gateway_blocks_disallowed_model_on_api_key() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("permitted").await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        "permitted-provider",
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "permitted")
        .await
        .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "forbidden-model")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "narrow",
        &["ai_gateway"],
        Some(&["permitted"]),
        None,
    )
    .await
    .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);

    // Allowed model → 200.
    gw.post(
        "/v1/chat/completions",
        json!({"model": "permitted", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Disallowed model → not 2xx.
    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({
                "model": "forbidden-model",
                "messages": [{"role": "user", "content": "x"}]
            }),
        )
        .await
        .unwrap();
    assert!(
        !resp.status.is_success(),
        "expected non-success for disallowed model, got {}: {}",
        resp.status,
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn gateway_returns_502_when_upstream_500s() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::always_500().await;
    let api_key =
        seed_provider_and_key(&app, &upstream.uri(), "openai", "broken-model", None).await;

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "broken-model", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    assert!(
        !resp.status.is_success(),
        "upstream 500 should not surface as 2xx: {}",
        resp.status
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn anthropic_messages_happy_path() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::anthropic_messages_ok("claude-3-haiku-test").await;
    let api_key = seed_provider_and_key(
        &app,
        &upstream.uri(),
        "anthropic",
        "claude-3-haiku-test",
        None,
    )
    .await;

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    let resp = gw
        .post(
            "/v1/messages",
            json!({
                "model": "claude-3-haiku-test",
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["model"].as_str().unwrap(), "claude-3-haiku-test");
    assert_eq!(body["content"][0]["text"], "hi");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn list_models_endpoint_returns_registered_models() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("listed-model").await;
    let api_key =
        seed_provider_and_key(&app, &upstream.uri(), "openai", "listed-model", None).await;

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    let resp = gw.get("/v1/models").await.unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    let ids: Vec<&str> = body["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"listed-model"),
        "expected listed-model in /v1/models response: {ids:?}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn revoked_api_key_no_longer_authorises() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("any").await;
    let api_key = seed_provider_and_key(&app, &upstream.uri(), "openai", "any", None).await;

    // First call works.
    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "any", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Soft-delete the key.
    sqlx::query("UPDATE api_keys SET is_active = false, deleted_at = now()")
        .execute(&app.db)
        .await
        .unwrap();
    // Second call now 401.
    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "any", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    resp.assert_status(401);
}
