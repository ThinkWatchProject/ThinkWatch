//! Rate-limit and budget-cap integration tests.
//!
//! ClickHouse-backed analytics (gateway_logs / cost_rollup_hourly) is
//! exercised separately in `analytics_clickhouse.rs`; here we only
//! drive the synchronous Postgres-side rules so the suite stays
//! runnable without a CH instance configured.

use think_watch_test_support::prelude::*;

/// Boots the stack, seeds an OpenAI mock + an active key, and
/// returns `(api_key_plaintext, user_id)`. Reused across the suite.
async fn seed_runtime(app: &TestApp) -> (String, uuid::Uuid) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let mock = MockProvider::openai_chat_ok("gpt-test").await;
    let uri = mock.uri();
    // Leak the mock so wiremock keeps serving for the lifetime of
    // the test. wiremock shuts down on Drop; the leak is per-test.
    Box::leak(Box::new(mock));

    let provider =
        fixtures::create_provider(&app.db, &unique_name("limit-prov"), "openai", &uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "gpt-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("limit-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    (key.plaintext, user.user.id)
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn user_rate_limit_caps_requests_per_minute() {
    let app = TestApp::spawn().await;
    let (api_key, user_id) = seed_runtime(&app).await;

    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 60, 2)
        .await
        .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);

    for _ in 0..2 {
        let r = gw
            .post(
                "/v1/chat/completions",
                json!({
                    "model": "gpt-test",
                    "messages": [{"role": "user", "content": "x"}]
                }),
            )
            .await
            .unwrap();
        r.assert_ok();
    }
    let r = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    assert!(
        !r.status.is_success(),
        "third request should be rate-limited, got {}",
        r.status
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn user_budget_cap_increments_redis_counter_post_flight() {
    // Budget caps are advisory in the current implementation — they
    // INCR a Redis counter and emit `budget.threshold_crossed` audit
    // entries when thresholds are crossed, but the gateway does not
    // pre-flight reject. This test pins that contract so a future
    // change to *enforce* caps is a deliberate, observable shift.
    let app = TestApp::spawn().await;
    let (api_key, user_id) = seed_runtime(&app).await;

    fixtures::create_budget_cap(&app.db, "user", user_id, "daily", 1_000)
        .await
        .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    // The post-flight worker INCRs `budget:user:{uid}:daily:{YYYY-MM-DD}`.
    // We don't know the exact key shape from outside, so just check
    // any matching key landed in Redis.
    use fred::interfaces::{ClientLike, KeysInterface};
    let keys: Vec<String> = {
        use fred::types::{ClusterHash, CustomCommand};
        let cmd = CustomCommand::new("KEYS", ClusterHash::FirstKey, false);
        app.state
            .redis
            .custom(cmd, vec![format!("budget:*{user_id}*")])
            .await
            .unwrap_or_default()
    };
    assert!(
        !keys.is_empty(),
        "expected a budget counter for user {user_id}, found none"
    );
    let val: Option<i64> = app
        .state
        .redis
        .get(&keys[0])
        .await
        .ok()
        .flatten()
        .and_then(|s: String| s.parse().ok());
    assert!(
        val.unwrap_or(0) > 0,
        "budget counter should be > 0 after a request, got {val:?}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn rate_limit_window_validation_rejects_off_grid_seconds() {
    // The persisted `rate_limit_rules.window_secs` is constrained to
    // a small allow-list (60, 300, 3600, …). Hand-inserted rules
    // outside that set must trip the startup validator. We exercise
    // the validator directly here — the pool is the per-test DB.
    let app = TestApp::spawn().await;

    sqlx::query(
        "INSERT INTO rate_limit_rules \
            (subject_kind, subject_id, surface, metric, window_secs, max_count, enabled) \
         VALUES ('user', '00000000-0000-0000-0000-000000000001', \
                 'ai_gateway', 'requests', 17, 1, true)",
    )
    .execute(&app.db)
    .await
    .unwrap();

    let res = think_watch_common::limits::validate_persisted(&app.db).await;
    assert!(
        res.is_err(),
        "validate_persisted must reject an off-grid window"
    );
    let msg = res.unwrap_err().to_string();
    assert!(msg.contains("window_secs"), "msg: {msg}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn api_key_scope_rate_limit_isolates_from_other_keys() {
    // Per-key rate-limit rules MUST fire on the gateway hot path —
    // schema supports `subject_kind='api_key'` and the auth
    // middleware passes `api_key_id` through
    // `compute_effective_surface_constraints`. Two keys for the
    // same user: one carries a max_count=1 rule, the other
    // carries nothing. Each key must behave independently.
    let app = TestApp::spawn().await;
    let (key_a_plain, user_id) = seed_runtime(&app).await;

    // Locate key_a's id (created by `seed_runtime`).
    let key_a_id: uuid::Uuid =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM api_keys WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&app.db)
            .await
            .unwrap();

    // Mint a second key for the same user, no rule attached.
    let key_b = fixtures::create_api_key(
        &app.db,
        user_id,
        &unique_name("free-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    fixtures::create_rate_limit_rule(
        &app.db,
        "api_key",
        key_a_id,
        "ai_gateway",
        "requests",
        60,
        1,
    )
    .await
    .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&key_a_plain);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await
    .unwrap()
    .assert_ok();
    let r = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    assert!(
        !r.status.is_success(),
        "key_a's max_count=1 api_key-scope rule must fire on the second call, got {}",
        r.status
    );

    // key_b shares the user_id but has no api_key-scope rule.
    gw.set_bearer(&key_b.plaintext);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await
    .unwrap()
    .assert_ok();
    gw.post(
        "/v1/chat/completions",
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await
    .unwrap()
    .assert_ok();
}
