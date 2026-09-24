//! Per-route upstream protocol: probing, relearning, invalidation.
//!
//! The case these tests model is a single aggregator that serves
//! several model families over one host and one credential while
//! exposing a *different* API per family — `anthropic.*` answers only
//! on `/v1/messages`, everything else on `/v1/chat/completions`. One
//! provider record has to cover both, and the admin must never be asked
//! which is which.
//!
//! The upstream here is a wiremock configured exactly that way: it
//! 400s an Anthropic model on the Chat Completions path with the same
//! wording real aggregators use.

use serde_json::{Value, json};
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mount a `/v1/chat/completions` that rejects anything the upstream
/// only serves on the Messages API, and a `/v1/messages` that answers.
async fn mount_family_split_upstream(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "validation_error",
            "message": "The model 'anthropic.claude-test' does not support the \
                        '/v1/chat/completions' API",
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_probe",
            "type": "message",
            "role": "assistant",
            "model": "anthropic.claude-test",
            "content": [{"type": "text", "text": "hi from messages"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 2},
        })))
        .mount(server)
        .await;
}

async fn route_protocol(app: &TestApp, model_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT upstream_protocol FROM model_routes WHERE model_id = $1",
    )
    .bind(model_id)
    .fetch_one(&app.db)
    .await
    .unwrap()
}

async fn probe_verdict(app: &TestApp, provider_id: uuid::Uuid, model: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT status FROM provider_model_probes WHERE provider_id = $1 AND upstream_model = $2",
    )
    .bind(provider_id)
    .bind(model)
    .fetch_optional(&app.db)
    .await
    .unwrap()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn importing_a_route_probes_the_dialect_the_upstream_actually_wants() {
    // The admin picks models and presses import. Nothing in that flow
    // asks which API each model speaks — the gateway works it out.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let server = MockServer::start().await;
    mount_family_split_upstream(&server).await;

    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("split-upstream"),
        "custom",
        &server.uri(),
        None,
    )
    .await
    .unwrap();

    let model = unique_name("anthropic.claude-test");
    let resp = con
        .post(
            "/api/admin/model-routes/batch",
            json!({
                "provider_id": provider.id,
                "items": [{"upstream": model}],
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();

    assert_eq!(
        route_protocol(&app, &model).await.as_deref(),
        Some("anthropic_messages"),
        "import must record the dialect the upstream answered on, before the route exists"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_model_the_upstream_refuses_is_never_imported() {
    // The failure this exists to prevent: an admin imports a catalog,
    // the routes look fine, and the dead ones are discovered one failed
    // call at a time.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let server = MockServer::start().await;
    let refused = unique_name("openai.gpt-refused");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": format!("The model '{refused}' does not support the '/v1/chat/completions' API"),
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": format!("The model '{refused}' does not support the '/v1/responses' API"),
        })))
        .mount(&server)
        .await;

    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("refusing-upstream"),
        "custom",
        &server.uri(),
        None,
    )
    .await
    .unwrap();

    let resp = con
        .post(
            "/api/admin/model-routes/batch",
            json!({
                "provider_id": provider.id,
                "items": [{"upstream": refused}],
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();

    assert_eq!(
        body["created"], 0,
        "a refused model must not become a route"
    );
    assert_eq!(
        body["skipped"][0]["upstream"], refused,
        "the admin selected it, so the response must say it was skipped: {body}"
    );
    assert!(
        body["skipped"][0]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("does not support"),
        "the skip reason must carry the upstream's own wording: {body}"
    );

    let routes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM model_routes WHERE provider_id = $1")
            .bind(provider.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(routes, 0);
    assert_eq!(
        probe_verdict(&app, provider.id, &refused).await.as_deref(),
        Some("unavailable"),
        "the verdict must be cached so a repeat import costs nothing"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn rechecking_a_provider_revisits_a_previously_refused_model() {
    // Verdicts never expire and nothing polls the upstream, so this
    // action is the only way back for a model that has since been
    // enabled there.
    let app = TestApp::try_spawn_with(SpawnOptions {
        url_validator: Some(permissive_url_validator()),
        ..Default::default()
    })
    .await
    .expect("spawn");
    let con = admin_session(&app).await;
    let server = MockServer::start().await;
    let model = "qwen.qwen3-test";
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": model}],
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })))
        .mount(&server)
        .await;

    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("recheck-upstream"),
        "custom",
        &server.uri(),
        None,
    )
    .await
    .unwrap();
    // Stale verdict from when the upstream refused this model.
    sqlx::query(
        "INSERT INTO provider_model_probes (provider_id, upstream_model, status, error)
         VALUES ($1, $2, 'unavailable', 'refused earlier')",
    )
    .bind(provider.id)
    .bind(model)
    .execute(&app.db)
    .await
    .unwrap();

    let resp = con
        .post(
            &format!("/api/admin/providers/{}/recheck-models", provider.id),
            json!({}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(
        body["available"], 1,
        "recheck must overwrite the stale verdict: {body}"
    );

    assert_eq!(
        probe_verdict(&app, provider.id, model).await.as_deref(),
        Some("ok")
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_route_on_the_wrong_dialect_recovers_and_remembers() {
    // Upstreams move models between APIs. A route that was right
    // yesterday starts failing — the caller should never see it.
    let app = TestApp::spawn().await;
    let server = MockServer::start().await;
    mount_family_split_upstream(&server).await;

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("split-upstream"),
        "custom",
        &server.uri(),
        None,
    )
    .await
    .unwrap();
    let model = unique_name("anthropic.claude-test");
    fixtures::create_model_and_route(&app.db, provider.id, &model)
        .await
        .unwrap();
    // The fixture leaves `upstream_protocol` NULL, so the route starts
    // on the provider type's default — Chat Completions, which this
    // upstream rejects for this model.
    assert_eq!(route_protocol(&app, &model).await, None);
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("proto-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);

    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({"model": model, "messages": [{"role": "user", "content": "say hi"}]}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"], "hi from messages",
        "the caller gets a real answer, not the upstream's dialect complaint"
    );

    assert_eq!(
        route_protocol(&app, &model).await.as_deref(),
        Some("anthropic_messages"),
        "the working dialect must be persisted so later requests skip the retry"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn changing_a_providers_endpoint_discards_what_was_learned_about_the_old_one() {
    // A learned dialect describes one specific upstream. Point the
    // provider somewhere else and it's an assumption about a host that
    // may not even exist any more.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("repoint"),
        "custom",
        "https://api.example.com",
        None,
    )
    .await
    .unwrap();
    let model = unique_name("anthropic.claude-test");
    fixtures::create_model_and_route(&app.db, provider.id, &model)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE model_routes SET upstream_protocol = 'anthropic_messages' WHERE model_id = $1",
    )
    .bind(&model)
    .execute(&app.db)
    .await
    .unwrap();

    // Rotating the credential counts the same as re-pointing the URL:
    // both mean the dialect was learned against something that may no
    // longer be what's on the other end.
    let resp = con
        .patch(
            &format!("/api/admin/providers/{}", provider.id),
            json!({"headers": [{"key": "x-api-key", "value": "rotated-secret"}]}),
        )
        .await
        .unwrap();
    resp.assert_ok();

    assert_eq!(
        route_protocol(&app, &model).await,
        None,
        "a changed endpoint must clear the learned dialect so it gets rediscovered"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_streaming_request_recovers_from_the_wrong_dialect_too() {
    // A stream can't be retried once bytes have reached the client, but
    // a dialect rejection arrives before the first chunk. A client that
    // only ever streams must still get an answer — and must still teach
    // the route the right dialect, or it would pay the failed attempt
    // on every request forever.
    let app = TestApp::spawn().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "The model 'anthropic.claude-test' does not support the \
                        '/v1/chat/completions' API",
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: message_start\n\
                     data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\
                     \"model\":\"anthropic.claude-test\",\"usage\":{\"input_tokens\":3,\
                     \"output_tokens\":0}}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\
                     \"delta\":{\"type\":\"text_delta\",\"text\":\"streamed\"}}\n\n\
                     event: message_stop\n\
                     data: {\"type\":\"message_stop\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("split-upstream"),
        "custom",
        &server.uri(),
        None,
    )
    .await
    .unwrap();
    let model = unique_name("anthropic.claude-test");
    fixtures::create_model_and_route(&app.db, provider.id, &model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("proto-stream-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);

    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({
                "model": model,
                "messages": [{"role": "user", "content": "say hi"}],
                "stream": true,
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body = resp.text();
    assert!(
        body.contains("streamed"),
        "the caller must receive the retried stream's content: {body}"
    );

    // The relearn write happens after the response completes, so give
    // it a moment to land rather than racing it.
    let mut learned = None;
    for _ in 0..100 {
        learned = route_protocol(&app, &model).await;
        if learned.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        learned.as_deref(),
        Some("anthropic_messages"),
        "a streaming recovery must persist the dialect like the buffered one does"
    );
}
