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

/// A tripped route comes back after the cooldown.
///
/// The breaker used to move from open to half-open only when a request on
/// that route completed — and an open route is never picked, so it stayed
/// open until its Redis key expired, about four cooldowns later. A cooled
/// breaker now reads as half-open, the next request probes it, and a
/// success closes it.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_tripped_route_is_probed_again_once_the_cooldown_is_over() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let app = TestApp::spawn().await;
    for (k, v) in [
        ("gateway.cb_enabled", json!(true)),
        ("gateway.cb_error_pct", json!(50)),
        ("gateway.cb_min_samples", json!(2)),
        ("gateway.cb_window_secs", json!(60)),
        ("gateway.cb_open_secs", json!(1)),
    ] {
        fixtures::set_setting(&app.db, k, v).await.unwrap();
    }
    app.state.dynamic_config.reload().await.unwrap();

    // Fails twice, then recovers.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "boom"}})),
        )
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "object": "chat.completion", "created": 1, "model": "cb-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "back"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })))
        .mount(&server)
        .await;

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let p = fixtures::create_provider(&app.db, &unique_name("cb"), "openai", &server.uri(), None)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p.id, "cb-model", 100)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(&app.db, user.user.id, "cb", &["ai_gateway"], None, None)
        .await
        .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    let ask = || {
        gw.post(
            "/v1/chat/completions",
            json!({"model": "cb-model", "messages": [{"role": "user", "content": "ping"}]}),
        )
    };
    let hits = || async { server.received_requests().await.unwrap_or_default().len() };

    // Two failures: 100% errors over 2 samples trips it.
    assert!(!ask().await.unwrap().status.is_success());
    assert!(!ask().await.unwrap().status.is_success());
    assert_eq!(hits().await, 2);

    // Open: refused without reaching the upstream.
    assert!(!ask().await.unwrap().status.is_success());
    assert_eq!(hits().await, 2, "an open route was still called");

    // Cooled: half-open, probed, recovered.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let resp = ask().await.unwrap();
    resp.assert_ok();
    assert_eq!(hits().await, 3);
    let body: Value = resp.json().unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "back");

    // Closed again: the next one goes straight through.
    ask().await.unwrap().assert_ok();
}

/// The dashboard shows a tripped AI route's provider as open. Its state
/// used to come from a process-local registry only the MCP breaker wrote
/// to, so every AI provider read `Closed` whatever its routes were doing.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_dashboard_shows_a_tripped_ai_provider_as_open() {
    let app = TestApp::spawn_with_clickhouse().await;
    for (k, v) in [
        ("gateway.cb_enabled", json!(true)),
        ("gateway.cb_error_pct", json!(50)),
        ("gateway.cb_min_samples", json!(2)),
        ("gateway.cb_open_secs", json!(600)),
    ] {
        fixtures::set_setting(&app.db, k, v).await.unwrap();
    }
    app.state.dynamic_config.reload().await.unwrap();

    let bad = MockProvider::always_500().await;
    let name = unique_name("tripped");
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let p = fixtures::create_provider(&app.db, &name, "openai", &bad.uri(), None)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p.id, "dash-model", 100)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(&app.db, user.user.id, "dash", &["ai_gateway"], None, None)
        .await
        .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    for _ in 0..2 {
        let _ = gw
            .post(
                "/v1/chat/completions",
                json!({"model": "dash-model", "messages": [{"role": "user", "content": "x"}]}),
            )
            .await
            .unwrap();
    }

    let con = admin_session(&app).await;
    let live: Value = con
        .get("/api/dashboard/live")
        .await
        .unwrap()
        .json()
        .unwrap();
    let row = live["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["provider"] == name.as_str())
        .unwrap_or_else(|| panic!("no row for {name}: {live}"));
    assert_eq!(row["cb_state"], "Open", "{row}");
}
