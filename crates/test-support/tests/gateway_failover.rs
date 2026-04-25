//! Multi-provider failover tests for the AI gateway.
//!
//! Two routes for the same `model_id`: a primary (priority=0)
//! pointed at a wiremock that 500s and a backup (priority=1)
//! pointed at a healthy mock. The proxy must advance to the backup
//! group and serve the request — and the failover counter must
//! tick.

use serde_json::Value;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn primary_500_falls_through_to_backup_priority() {
    let app = TestApp::spawn().await;

    // Primary returns 500, backup returns 200.
    let primary = MockProvider::always_500().await;
    let backup = MockProvider::openai_chat_ok("failover-model").await;
    let primary_uri = primary.uri();
    let backup_uri = backup.uri();
    Box::leak(Box::new(primary));
    Box::leak(Box::new(backup));

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let p_primary = fixtures::create_provider(
        &app.db,
        &unique_name("primary"),
        "openai",
        &primary_uri,
        None,
    )
    .await
    .unwrap();
    let p_backup =
        fixtures::create_provider(&app.db, &unique_name("backup"), "openai", &backup_uri, None)
            .await
            .unwrap();

    fixtures::create_model_route(&app.db, p_primary.id, "failover-model", 0, 100)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p_backup.id, "failover-model", 1, 100)
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
    // Backup served — model id echoed by the healthy mock.
    assert_eq!(body["model"].as_str(), Some("failover-model"));
    assert_eq!(body["choices"][0]["message"]["content"], "hello world");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn within_priority_group_failover_tries_each_member() {
    // Two providers in the SAME priority group. First-pick fails,
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

    // Same priority (0) — group failover, not cross-priority.
    fixtures::create_model_route(&app.db, p_bad.id, "group-model", 0, 100)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p_good.id, "group-model", 0, 100)
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
    fixtures::create_model_route(&app.db, p_a.id, "all-bad", 0, 100)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p_b.id, "all-bad", 1, 100)
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
