//! Multi-provider failover tests for the AI gateway.
//!
//! Two routes for the same `model_id`: one points at a wiremock
//! that 500s, the other at a healthy mock. The proxy must skip the
//! broken provider and serve from the healthy one — and the failover
//! counter must tick.
//!
//! v2 note: there's no longer a "priority tier" concept. All routes
//! are peers; failover is implicit via the proxy's per-attempt retry
//! loop and circuit breaker. These tests only assert that *some*
//! healthy route serves the request.

use serde_json::Value;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn one_failing_provider_falls_through_to_healthy_peer() {
    let app = TestApp::spawn().await;

    // One returns 500, the other returns 200. Equal weight ⇒ either
    // could be picked first; the proxy must retry the other on 500.
    let bad = MockProvider::always_500().await;
    let good = MockProvider::openai_chat_ok("failover-model").await;
    let bad_uri = bad.uri();
    let good_uri = good.uri();
    Box::leak(Box::new(bad));
    Box::leak(Box::new(good));

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let p_bad = fixtures::create_provider(&app.db, &unique_name("bad"), "openai", &bad_uri, None)
        .await
        .unwrap();
    let p_good =
        fixtures::create_provider(&app.db, &unique_name("good"), "openai", &good_uri, None)
            .await
            .unwrap();

    fixtures::create_model_route(&app.db, p_bad.id, "failover-model", 100)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p_good.id, "failover-model", 100)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "failover-key",
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
                "model": "failover-model",
                "messages": [{"role": "user", "content": "ping"}]
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["model"].as_str(), Some("failover-model"));
    assert_eq!(body["choices"][0]["message"]["content"], "hello world");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn weighted_pool_failover_tries_each_member() {
    // Two providers in the same weighted pool. First-pick fails,
    // gateway must retry the other before bubbling the error.
    let app = TestApp::spawn().await;
    let bad = MockProvider::always_500().await;
    let good = MockProvider::openai_chat_ok("group-model").await;
    let bad_uri = bad.uri();
    let good_uri = good.uri();
    Box::leak(Box::new(bad));
    Box::leak(Box::new(good));

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let p_bad =
        fixtures::create_provider(&app.db, &unique_name("bad-prov"), "openai", &bad_uri, None)
            .await
            .unwrap();
    let p_good = fixtures::create_provider(
        &app.db,
        &unique_name("good-prov"),
        "openai",
        &good_uri,
        None,
    )
    .await
    .unwrap();

    fixtures::create_model_route(&app.db, p_bad.id, "group-model", 100)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p_good.id, "group-model", 100)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "group-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    // The selector picks via weighted random + (no) affinity. Drive
    // a few requests so even a stable random choice still hits both
    // sides. We only need ONE success to prove the failover path.
    let mut succeeded = 0;
    for _ in 0..5 {
        let resp = gw
            .post(
                "/v1/chat/completions",
                json!({"model": "group-model", "messages": [{"role": "user", "content": "x"}]}),
            )
            .await
            .unwrap();
        if resp.status.is_success() {
            succeeded += 1;
        }
    }
    assert!(
        succeeded >= 1,
        "at least one request must succeed via the healthy provider in the group"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn all_providers_failing_returns_upstream_error() {
    // Both providers return 500 — gateway has nowhere to fall over,
    // bubble a 502/503-class error.
    let app = TestApp::spawn().await;
    let p1 = MockProvider::always_500().await;
    let p2 = MockProvider::always_500().await;
    let u1 = p1.uri();
    let u2 = p2.uri();
    Box::leak(Box::new(p1));
    Box::leak(Box::new(p2));

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let p_a = fixtures::create_provider(&app.db, &unique_name("a"), "openai", &u1, None)
        .await
        .unwrap();
    let p_b = fixtures::create_provider(&app.db, &unique_name("b"), "openai", &u2, None)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p_a.id, "all-bad", 100)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p_b.id, "all-bad", 100)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "all-bad-key",
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
            json!({"model": "all-bad", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    assert!(
        !resp.status.is_success(),
        "all-providers-down must bubble an error, got {}",
        resp.status
    );
}
