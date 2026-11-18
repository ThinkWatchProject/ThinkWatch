//! Real Redis decoding and HTTP admission/recovery through the assembled app.

use fred::prelude::*;
use think_watch_common::resources;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn redis_body_reads_preserve_bytes_and_reservations_until_the_last_clone() {
    let app = TestApp::spawn().await;
    let key = unique_name("resource-body");
    let data = b"\0\xff\"cached JSON\"\xe4\xb8\xad";
    let _: () = app
        .state
        .redis
        .set(&key, data.as_slice(), None, None, false)
        .await
        .unwrap();
    let budget = resources::body_budget();
    let baseline = budget.used();
    let body = resources::cached_body(&app.state.redis, &key)
        .await
        .unwrap();
    assert_eq!(&body[..], data);
    assert_eq!(
        budget.used(),
        baseline + data.len() * resources::BODY_HEADROOM
    );
    let copy = body.clone();
    drop(body);
    assert!(budget.used() > baseline);
    drop(copy);
    assert_eq!(budget.used(), baseline);

    let mut occupied = budget.reserve();
    assert!(occupied.grow(resources::policy().body_bytes - baseline));
    assert!(
        resources::cached_body(&app.state.redis, &key)
            .await
            .is_none()
    );
    drop(occupied);
    assert!(
        resources::cached_body(&app.state.redis, &key)
            .await
            .is_some()
    );
    let _: () = app.state.redis.del(&key).await.unwrap();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn http_body_pressure_is_retryable_and_releases_without_affecting_health() {
    let app = TestApp::spawn().await;
    let model = "admission-test";
    let upstream = MockProvider::openai_chat_ok(model).await;
    let provider = fixtures::create_provider(&app.db, model, "openai", &upstream.uri(), None)
        .await
        .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let key = fixtures::create_api_key(&app.db, user.user.id, model, &["ai_gateway"], None, None)
        .await
        .unwrap();

    let budget = resources::body_budget();
    let mut occupied = budget.reserve();
    assert!(occupied.grow(resources::policy().body_bytes - budget.used()));
    let gateway = app.gateway_client();
    let request = json!({"model":model,"messages":[{"role":"user","content":"hello"}]});
    let response = gateway
        .post("/v1/chat/completions", &request)
        .await
        .unwrap();
    response.assert_status(401);
    gateway.get("/health/live").await.unwrap().assert_ok();

    gateway.set_bearer(&key.plaintext);
    let response = gateway
        .post("/v1/chat/completions", &request)
        .await
        .unwrap();
    response.assert_status(429);
    assert_eq!(response.headers["retry-after"], "1");
    drop(occupied);
    gateway
        .post("/v1/chat/completions", &request)
        .await
        .unwrap()
        .assert_ok();
}
