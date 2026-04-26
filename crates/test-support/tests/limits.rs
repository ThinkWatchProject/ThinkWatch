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
    // schema supports `subject_kind='api_key_lineage'` and the auth
    // middleware passes `api_key_id` through
    // `compute_effective_surface_constraints`, which resolves it to
    // `lineage_id` before binding. Two keys for the same user: one
    // carries a max_count=1 rule keyed on its lineage_id, the other
    // carries nothing. Each key must behave independently.
    let app = TestApp::spawn().await;
    let (key_a_plain, user_id) = seed_runtime(&app).await;

    // Locate key_a's lineage_id (created by `seed_runtime`).
    let key_a_lineage: uuid::Uuid =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT lineage_id FROM api_keys WHERE user_id = $1")
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
        "api_key_lineage",
        key_a_lineage,
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

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn api_key_rate_limit_survives_rotation_via_lineage_id() {
    // The whole point of `subject_kind = 'api_key_lineage'`: a rule
    // attached to the key's lineage must keep biting after rotation
    // without anyone copying the row forward. Concretely: create a
    // key, attach a max_count=1 rule keyed on its lineage_id, drive
    // one allowed request, rotate the key, then send a request under
    // the freshly-minted generation-2 plaintext — it must STILL be
    // rejected because the lineage counter has already reached its
    // ceiling.
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();

    let mock = MockProvider::openai_chat_ok("gpt-test").await;
    let uri = mock.uri();
    Box::leak(Box::new(mock));
    let provider =
        fixtures::create_provider(&app.db, &unique_name("rot-prov"), "openai", &uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "gpt-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    // Login as admin to use the public rotate endpoint.
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Generation 1: create a key for the regular user, attach the rule.
    let key_v1 = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("rot-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let key_v1_id = key_v1.row.id;
    let lineage_id = key_v1.row.lineage_id;
    assert_eq!(lineage_id, key_v1_id, "fresh key: lineage_id == id");

    fixtures::create_rate_limit_rule(
        &app.db,
        "api_key_lineage",
        lineage_id,
        "ai_gateway",
        "requests",
        60,
        1,
    )
    .await
    .unwrap();

    // First request under gen-1: allowed.
    let gw1 = app.gateway_client();
    gw1.set_bearer(&key_v1.plaintext);
    gw1.post(
        "/v1/chat/completions",
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "g1"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Rotate via the public endpoint so lineage_id propagation goes
    // through the same path the operator hits in the UI.
    let rotated: serde_json::Value = con
        .post_empty(&format!("/api/keys/{key_v1_id}/rotate"))
        .await
        .unwrap()
        .json()
        .unwrap();
    let plaintext_v2 = rotated["key"].as_str().unwrap().to_string();
    let key_v2_id: uuid::Uuid = uuid::Uuid::parse_str(rotated["id"].as_str().unwrap()).unwrap();
    assert_ne!(key_v2_id, key_v1_id, "rotation must mint a fresh id");

    // Sanity: gen-2 must inherit the SAME lineage_id.
    let lineage_v2: uuid::Uuid =
        sqlx::query_scalar("SELECT lineage_id FROM api_keys WHERE id = $1")
            .bind(key_v2_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(lineage_v2, lineage_id, "rotation must preserve lineage_id");

    // Generation 2 request — the lineage counter is already at 1/1
    // from gen-1, so this MUST be rejected. If the gateway were
    // resolving on `api_key.id` (the old subject_kind='api_key'
    // scheme) gen-2's fresh id would have a fresh counter and slip
    // through; the lineage_id resolution is what closes that gap.
    let gw2 = app.gateway_client();
    gw2.set_bearer(&plaintext_v2);
    let r = gw2
        .post(
            "/v1/chat/completions",
            json!({"model": "gpt-test", "messages": [{"role": "user", "content": "g2"}]}),
        )
        .await
        .unwrap();
    assert!(
        !r.status.is_success(),
        "gen-2 must inherit gen-1's exhausted lineage counter; got {}",
        r.status
    );
}
