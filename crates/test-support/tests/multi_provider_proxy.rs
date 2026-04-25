//! Per-provider proxy contract: Google Gemini + Azure OpenAI.
//!
//! `gateway_proxy.rs` covers the OpenAI and Anthropic happy paths.
//! This file fills in the rest of the supported providers — each
//! one has its own URL shape, request envelope, and response
//! conversion that the OpenAI-compatible gateway must round-trip
//! correctly:
//!
//!   - Google Gemini: `POST {base}/v1beta/models/{model}:generateContent`
//!     with `{contents: [...], systemInstruction?, generationConfig?}`
//!     → response with `candidates[].content.parts[].text` and
//!     `usageMetadata`. Streaming uses `:streamGenerateContent?alt=sse`.
//!   - Azure OpenAI: `POST {base}/openai/deployments/{deployment}/chat/completions?api-version=…`
//!     with the OpenAI envelope verbatim → OpenAI response verbatim.
//!     The "deployment" in the URL is the gateway's `model` string.
//!
//! AWS Bedrock is intentionally NOT covered here: its endpoint is
//! hard-coded to `bedrock-runtime.{region}.amazonaws.com` (no
//! `base_url` override knob), so wiremock can't intercept it
//! without DNS hijacking or production code changes. A unit test
//! for `convert_to_bedrock` / `convert_from_bedrock` would still
//! be valuable — out of scope for this commit.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn seed_provider_key(
    app: &TestApp,
    upstream_url: &str,
    provider_type: &str,
    model: &str,
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
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("multi-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    key.plaintext
}

// ---------------------------------------------------------------------------
// Google Gemini
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn google_gemini_chat_completion_round_trips_through_envelope_translation() {
    let app = TestApp::spawn().await;

    // Mock the Gemini upstream. The gateway computes the URL as
    // `{base}/v1beta/models/{model}:generateContent`, so we mount the
    // matcher on that exact path.
    let model = "gemini-test-pro";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1beta/models/{model}:generateContent")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "hi from gemini"}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 9,
                "candidatesTokenCount": 4,
                "totalTokenCount": 13
            }
        })))
        .mount(&server)
        .await;

    let bearer = seed_provider_key(&app, &server.uri(), "google", model, None).await;
    let gw = app.gateway_client();
    gw.set_bearer(&bearer);

    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": "be brief"},
                    {"role": "user", "content": "say hi"}
                ]
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();

    // Response: gateway converts Gemini's shape back into OpenAI's.
    let body: Value = resp.json().unwrap();
    assert_eq!(body["choices"][0]["message"]["role"], "assistant");
    assert_eq!(body["choices"][0]["message"]["content"], "hi from gemini");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["prompt_tokens"], 9);
    assert_eq!(body["usage"]["completion_tokens"], 4);
    assert_eq!(body["usage"]["total_tokens"], 13);
    assert_eq!(body["model"], model);

    // Request envelope on the wire: must be Gemini-shaped, not OpenAI.
    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(received.len(), 1, "exactly one upstream call expected");
    let upstream_body: Value =
        serde_json::from_slice(&received[0].body).expect("upstream body is JSON");
    // The system prompt is hoisted out of `messages` into `systemInstruction`.
    assert_eq!(
        upstream_body["systemInstruction"]["parts"][0]["text"], "be brief",
        "system message must be hoisted to systemInstruction: {upstream_body}"
    );
    // `messages: [{role:user,content:"say hi"}]` becomes
    // `contents: [{role:"user", parts:[{text:"say hi"}]}]`.
    assert_eq!(upstream_body["contents"][0]["role"], "user");
    assert_eq!(upstream_body["contents"][0]["parts"][0]["text"], "say hi");
    // OpenAI's `messages` field must NOT leak through.
    assert!(
        upstream_body.get("messages").is_none(),
        "OpenAI envelope leaked to Gemini upstream: {upstream_body}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn google_gemini_streaming_relays_sse_through_translation() {
    let app = TestApp::spawn().await;
    let model = "gemini-stream-pro";

    // Gemini SSE chunks the gateway will parse + convert.
    let chunks = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi \"}]},\"finishReason\":null}]}\n\n\
                  data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"there\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7}}\n\n";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{model}:streamGenerateContent"
        )))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(chunks.as_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let bearer = seed_provider_key(&app, &server.uri(), "google", model, None).await;
    let gw = app.gateway_client();
    gw.set_bearer(&bearer);

    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({
                "model": model,
                "messages": [{"role": "user", "content": "ping"}],
                "stream": true
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body = resp.text();
    // Translated chunks must reach the client in OpenAI SSE shape and
    // terminate with `[DONE]`.
    assert!(body.contains("[DONE]"), "missing [DONE] terminator: {body}");
    assert!(
        body.contains("\"role\":\"assistant\"") || body.contains("\"content\":\"hi"),
        "client never saw a translated content chunk: {body}"
    );
    // Upstream URL was the streaming variant, with `alt=sse` query.
    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(received.len(), 1);
    let url = received[0].url.to_string();
    assert!(
        url.contains(":streamGenerateContent") && url.contains("alt=sse"),
        "streaming variant must hit :streamGenerateContent?alt=sse, got {url}"
    );
}

// ---------------------------------------------------------------------------
// Azure OpenAI
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn azure_openai_chat_completion_uses_deployment_url_shape() {
    let app = TestApp::spawn().await;
    let deployment = "gpt-4o-azure-test";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/openai/deployments/{deployment}/chat/completions"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-azure",
            "object": "chat.completion",
            "created": 1_700_000_000_i64,
            "model": deployment,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "azure says hi"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 4, "completion_tokens": 5, "total_tokens": 9}
        })))
        .mount(&server)
        .await;

    let bearer = seed_provider_key(
        &app,
        &server.uri(),
        "azure_openai",
        deployment,
        Some(json!({"api_version": "2024-12-01-preview"})),
    )
    .await;
    let gw = app.gateway_client();
    gw.set_bearer(&bearer);

    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({
                "model": deployment,
                "messages": [{"role": "user", "content": "ping"}]
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "azure says hi");
    assert_eq!(body["usage"]["total_tokens"], 9);

    // Wire shape: deployment-style URL with the `api-version` query
    // string. The body is OpenAI-flavoured (Azure OpenAI takes the
    // same envelope).
    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(received.len(), 1);
    let url = received[0].url.to_string();
    assert!(
        url.contains(&format!(
            "/openai/deployments/{deployment}/chat/completions"
        )),
        "deployment URL shape required, got {url}"
    );
    assert!(
        url.contains("api-version=2024-12-01-preview"),
        "api-version query param must round-trip from provider config: {url}"
    );
    let upstream_body: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(upstream_body["model"], deployment);
    assert_eq!(
        upstream_body["messages"][0]["content"], "ping",
        "OpenAI envelope must round-trip verbatim to Azure: {upstream_body}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn azure_openai_uses_default_api_version_when_unset() {
    // Operators who omit `api_version` from the provider config must
    // still get a working call — the provider has a baked-in default
    // (`2024-12-01-preview`). This pins that default so a refactor
    // that drops it (or silently changes the year) breaks loudly.
    let app = TestApp::spawn().await;
    let deployment = "gpt-4o-default-ver";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/openai/deployments/{deployment}/chat/completions"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x",
            "object": "chat.completion",
            "created": 1_700_000_000_i64,
            "model": deployment,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&server)
        .await;

    let bearer = seed_provider_key(&app, &server.uri(), "azure_openai", deployment, None).await;
    let gw = app.gateway_client();
    gw.set_bearer(&bearer);

    gw.post(
        "/v1/chat/completions",
        json!({
            "model": deployment,
            "messages": [{"role": "user", "content": "x"}]
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    let received = server.received_requests().await.unwrap_or_default();
    assert_eq!(received.len(), 1);
    let url = received[0].url.to_string();
    assert!(
        url.contains("api-version="),
        "Azure URL must always carry an api-version: {url}"
    );
}
