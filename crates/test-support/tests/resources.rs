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
    let budget = resources::body_budget();
    let mut occupied = budget.reserve();
    assert!(occupied.grow(resources::policy().body_bytes - budget.used()));
    let gateway = app.gateway_client();
    let response = gateway
        .post("/v1/chat/completions", json!({"model":"test"}))
        .await
        .unwrap();
    response.assert_status(429);
    assert_eq!(response.headers["retry-after"], "1");
    drop(occupied);
    let response = gateway
        .post("/v1/chat/completions", json!({"model":"test"}))
        .await
        .unwrap();
    response.assert_status(401);
    let health = gateway.get("/health/live").await.unwrap();
    health.assert_ok();
}
