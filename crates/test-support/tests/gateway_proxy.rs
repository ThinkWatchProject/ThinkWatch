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

/// The request shape Claude Code actually sends: `system` as an array of
/// blocks with a `cache_control` breakpoint, plus tools and a server tool.
///
/// Anthropic to Anthropic has no business being rewritten. The gateway
/// used to rebuild the request from a chat-shaped DTO, which read
/// `system` with `as_str()` — `None` for the array form — so Claude
/// Code's entire system prompt was dropped, along with every tool and
/// every prompt-cache breakpoint.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn anthropic_to_anthropic_forwards_the_request_as_sent() {
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
                "system": [
                    {"type": "text", "text": "You are Claude Code."},
                    {"type": "text", "text": "Project rules.",
                     "cache_control": {"type": "ephemeral"}}
                ],
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [
                    {"name": "Read", "description": "read a file",
                     "input_schema": {"type": "object",
                                      "properties": {"path": {"type": "string"}}}},
                    {"type": "web_search_20250305", "name": "web_search"}
                ],
                "tool_choice": {"type": "auto"},
                "metadata": {"user_id": "u-1"}
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();

    let sent = upstream.received_requests().await;
    assert_eq!(sent.len(), 1);
    let sent: Value = serde_json::from_slice(&sent[0].body).unwrap();

    // The system prompt, whole, in the shape it was sent.
    assert_eq!(sent["system"][0]["text"], "You are Claude Code.");
    assert_eq!(sent["system"][1]["text"], "Project rules.");
    assert_eq!(
        sent["system"][1]["cache_control"]["type"], "ephemeral",
        "a dropped breakpoint turns every cached prefix back into full-price input: {sent}"
    );
    // Tools, including the server tool no other format can express.
    assert_eq!(sent["tools"][0]["name"], "Read");
    assert_eq!(sent["tools"][1]["type"], "web_search_20250305");
    assert_eq!(sent["tool_choice"]["type"], "auto");
    assert_eq!(sent["metadata"]["user_id"], "u-1");
}

/// A request forwarded in its own format carries the caller's
/// `anthropic-beta`: its body can use a beta feature, and without the
/// header that turns it on the upstream refuses it.
///
/// And every Anthropic-bound request carries `anthropic-version`, which
/// the API requires. The mock does not check for it, so nothing else in
/// this suite would notice it missing — a real upstream would refuse
/// every request.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn anthropic_bound_requests_carry_the_headers_the_api_needs() {
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
    gw.set_header("anthropic-beta", "context-management-2025-06-27");
    gw.post(
        "/v1/messages",
        json!({"model": "claude-3-haiku-test", "max_tokens": 16,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    let sent = upstream.received_requests().await;
    let h = &sent.last().unwrap().headers;
    assert_eq!(
        h.get("anthropic-beta").and_then(|v| v.to_str().ok()),
        Some("context-management-2025-06-27")
    );
    assert!(
        h.get("anthropic-version").is_some(),
        "the API refuses a request without a version header"
    );
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

/// A conversation that went through a format conversion earlier carries
/// reasoning signatures the gateway wrote (`tw1.`). When a later turn of
/// it is forwarded as sent to Anthropic, those signatures go too, and
/// Anthropic refuses the whole request over them. They are taken out; a
/// signature Anthropic issued itself stays.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_passthrough_request_leaves_the_gateways_own_signatures_behind() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::anthropic_messages_ok("claude-carried").await;
    let api_key =
        seed_provider_and_key(&app, &upstream.uri(), "anthropic", "claude-carried", None).await;

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    let resp = gw
        .post(
            "/v1/messages",
            json!({
                "model": "claude-carried",
                "max_tokens": 16,
                "messages": [
                    {"role": "user", "content": "first"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "converted", "signature": "tw1.abc"},
                        {"type": "text", "text": "answer one"}
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "native", "signature": "EqQBCkgIBx"},
                        {"type": "text", "text": "answer two"}
                    ]},
                    {"role": "user", "content": "second"}
                ]
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();

    let sent = upstream.received_requests().await;
    assert_eq!(sent.len(), 1);
    let text = String::from_utf8_lossy(&sent[0].body).into_owned();
    assert!(
        !text.contains("tw1."),
        "a carried signature was forwarded: {text}"
    );
    let sent: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(sent["messages"][1]["content"][0]["text"], "answer one");
    assert_eq!(sent["messages"][2]["content"][0]["signature"], "EqQBCkgIBx");
}

/// A Responses request the gateway cannot convert — it continues a
/// conversation kept on OpenAI's servers and carries a compaction only
/// OpenAI can read, as Codex sends after compacting against OpenAI — is
/// still forwarded as sent to a route that speaks Responses. Only when
/// every route would have to convert it is it refused, saying why.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_request_only_openai_can_read_is_forwarded_to_a_responses_route() {
    use wiremock::matchers::{method, path};

    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("resp-model").await;
    upstream
        .mount(
            wiremock::Mock::given(method("POST"))
                .and(path("/v1/responses"))
                .respond_with(MockProvider::json(json!({
                    "id": "resp_2",
                    "object": "response",
                    "created_at": 1_700_000_000_i64,
                    "model": "resp-model",
                    "status": "completed",
                    "output": [{
                        "type": "message", "id": "msg_1", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "continued",
                                     "annotations": []}]
                    }],
                    "usage": {"input_tokens": 9, "output_tokens": 2, "total_tokens": 11}
                }))),
        )
        .await;
    let api_key = seed_provider_and_key(&app, &upstream.uri(), "openai", "resp-model", None).await;
    let ask = json!({
        "model": "resp-model",
        "previous_response_id": "resp_1",
        "input": [
            {"type": "compaction", "encrypted_content": "gAAAAABo"},
            {"type": "message", "role": "user", "content": "go on"}
        ]
    });
    let gw = app.gateway_client();
    gw.set_bearer(&api_key);

    // The route converts to Chat Completions, an OpenAI provider's
    // default: refused with the reason, and nothing sent.
    let resp = gw.post("/v1/responses", ask.clone()).await.unwrap();
    resp.assert_status(400);
    let body: Value = resp.json().unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("previous_response_id")),
        "{body}"
    );
    assert!(upstream.received_requests().await.is_empty());

    // A route that speaks Responses: forwarded as sent.
    sqlx::query(
        "UPDATE model_routes SET upstream_protocol = 'openai_responses' WHERE model_id = $1",
    )
    .bind("resp-model")
    .execute(&app.db)
    .await
    .unwrap();
    app.rebuild_gateway_router().await;
    let resp = gw.post("/v1/responses", ask).await.unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(
        body["output"][0]["content"][0]["text"], "continued",
        "{body}"
    );

    let sent = upstream.received_requests().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].url.path(), "/v1/responses");
    let sent: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(sent["previous_response_id"], "resp_1", "{sent}");
    assert_eq!(sent["input"][0]["encrypted_content"], "gAAAAABo", "{sent}");
}
