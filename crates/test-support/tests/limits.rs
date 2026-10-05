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
    // Post-flight side of the budget pipeline: a successful upstream
    // call INCRs the Redis counter via `record_usage` (which calls
    // `post_flight_account`) and emits `budget.threshold_crossed`
    // audit entries when thresholds are crossed. The complementary
    // pre-call rejection path is covered by
    // `user_budget_cap_pre_call_rejects_when_already_exhausted` —
    // this test pins the INCR-on-success contract that drives the
    // counter that the pre-call peek reads.
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
async fn user_budget_cap_pre_call_rejects_when_already_exhausted() {
    // Pre-call side of the budget pipeline: `check_budget` peeks the
    // Redis counter and short-circuits with 429 when current >= limit
    // BEFORE the upstream call fires. We seed the counter past the cap
    // directly so the test doesn't depend on the cost-tracker emitting
    // a specific weighted-token count per request (different upstream
    // mocks land different token totals; the rejection path's contract
    // is purely about peek-vs-limit, not about how the counter got
    // there).
    use chrono::Utc;
    use fred::interfaces::KeysInterface;
    use think_watch_common::limits::budget;

    let app = TestApp::spawn().await;
    let (api_key, user_id) = seed_runtime(&app).await;

    // Small cap, then seed Redis well past it. Daily period so the
    // key TTL doesn't expire during the test.
    let cap_limit: i64 = 100;
    fixtures::create_budget_cap(&app.db, "user", user_id, "daily", cap_limit)
        .await
        .unwrap();

    let key = budget::build_key("user", user_id, "daily", Utc::now());
    // EXPIRE is set by the post-flight path; for the test it's
    // sufficient to drop a raw value — the peek is a plain GET.
    let _: () = app
        .state
        .redis
        .set(&key, cap_limit + 5, None, None, false)
        .await
        .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    let resp = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();

    // 429 because `check_budget` -> `S::budget_exceeded_response` ->
    // `GatewayError::LocalRateLimited` -> 429 (see gateway/providers/
    // traits.rs::status_code, which lumps budget exhaustion under the
    // rate-limit status family per LocalRateLimited's docstring).
    assert_eq!(
        resp.status.as_u16(),
        429,
        "expected 429 when budget cap is already at-or-past limit, \
         got {} body={:?}",
        resp.status,
        resp.body
    );

    // Counter must NOT have been incremented — the request was
    // rejected pre-call, so post_flight_account never ran. Anything
    // past `cap_limit + 5` would mean we let an upstream call through.
    let after: Option<i64> = app
        .state
        .redis
        .get(&key)
        .await
        .ok()
        .flatten()
        .and_then(|s: String| s.parse().ok());
    assert_eq!(
        after,
        Some(cap_limit + 5),
        "budget counter must stay at the seeded value when the request \
         was rejected pre-call; any increment means an upstream call \
         leaked past `check_budget`"
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
    // middleware loads the key lineage's rules through
    // `compute_key_surface_constraints`. Two keys for the same user: one
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

// ----------------------------------------------------------------------------
// Enforcement: token limits, key counters, refusals that charge nothing,
// and the Retry-After a refusal carries.
// ----------------------------------------------------------------------------

fn chat() -> Json {
    json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}]})
}

/// `current` for every rule and cap on a subject, from the console's
/// usage endpoint — what an operator sees.
async fn usage(con: &TestClient, kind: &str, id: Uuid) -> (Vec<i64>, Vec<i64>) {
    let r = con
        .get(&format!("/api/admin/limits/{kind}/{id}/usage"))
        .await
        .unwrap();
    r.assert_ok();
    let v: Json = r.json().unwrap();
    let currents = |field: &str| -> Vec<i64> {
        v[field]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["current"].as_i64().unwrap())
            .collect()
    };
    (currents("rules"), currents("caps"))
}

fn retry_after(r: &think_watch_test_support::client::TestResponse) -> u64 {
    r.headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("a 429 carries Retry-After, headers: {:?}", r.headers))
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_token_limit_refuses_once_the_window_is_used_up() {
    // A request costs ~10 weighted tokens against the mock. A limit of 5
    // lets the first request through (nothing was used yet), records all
    // of what it used even though that overshoots, and refuses the next.
    let app = TestApp::spawn().await;
    let (api_key, user_id) = seed_runtime(&app).await;
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "tokens", 60, 5)
        .await
        .unwrap();
    let con = admin_session(&app).await;

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    gw.post("/v1/chat/completions", chat())
        .await
        .unwrap()
        .assert_ok();

    let (rules, _) = usage(&con, "user", user_id).await;
    assert!(
        rules[0] > 5,
        "the whole use is recorded, past the limit; got {rules:?}"
    );

    let r = gw.post("/v1/chat/completions", chat()).await.unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    let secs = retry_after(&r);
    assert!((1..=60).contains(&secs), "Retry-After {secs}");
    assert_eq!(
        usage(&con, "user", user_id).await.0,
        rules,
        "a refused request records nothing"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn every_request_limit_applies_when_there_are_several() {
    let app = TestApp::spawn().await;
    let (api_key, user_id) = seed_runtime(&app).await;
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 60, 2)
        .await
        .unwrap();
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 300, 5)
        .await
        .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    for _ in 0..2 {
        gw.post("/v1/chat/completions", chat())
            .await
            .unwrap()
            .assert_ok();
    }
    let r = gw.post("/v1/chat/completions", chat()).await.unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
}

/// A key and its owner, each with limits of their own.
async fn second_key(app: &TestApp, user_id: Uuid) -> fixtures::SeededApiKey {
    fixtures::create_api_key(
        &app.db,
        user_id,
        &unique_name("limit-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_keys_limits_count_on_the_key_not_on_its_owner() {
    let app = TestApp::spawn().await;
    let (_, user_id) = seed_runtime(&app).await;
    let key_a = second_key(&app, user_id).await;
    let key_b = second_key(&app, user_id).await;
    for key in [&key_a, &key_b] {
        fixtures::create_rate_limit_rule(
            &app.db,
            "api_key_lineage",
            key.row.lineage_id,
            "ai_gateway",
            "requests",
            60,
            1,
        )
        .await
        .unwrap();
    }
    let con = admin_session(&app).await;
    let gw = app.gateway_client();

    gw.set_bearer(&key_a.plaintext);
    gw.post("/v1/chat/completions", chat())
        .await
        .unwrap()
        .assert_ok();
    // Key B has a counter of its own: A's request does not use it up.
    gw.set_bearer(&key_b.plaintext);
    gw.post("/v1/chat/completions", chat())
        .await
        .unwrap()
        .assert_ok();
    gw.set_bearer(&key_a.plaintext);
    let r = gw.post("/v1/chat/completions", chat()).await.unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());

    // The console reads the counter the gateway writes.
    assert_eq!(usage(&con, "api_key", key_a.row.id).await.0, vec![1]);
    assert_eq!(usage(&con, "api_key", key_b.row.id).await.0, vec![1]);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_key_limit_does_not_lift_its_owners() {
    // The owner may make one request a minute; the key's own limit of
    // five does not raise that.
    let app = TestApp::spawn().await;
    let (_, user_id) = seed_runtime(&app).await;
    let key = second_key(&app, user_id).await;
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 60, 1)
        .await
        .unwrap();
    fixtures::create_rate_limit_rule(
        &app.db,
        "api_key_lineage",
        key.row.lineage_id,
        "ai_gateway",
        "requests",
        60,
        5,
    )
    .await
    .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    gw.post("/v1/chat/completions", chat())
        .await
        .unwrap()
        .assert_ok();
    let r = gw.post("/v1/chat/completions", chat()).await.unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_key_budget_counts_on_the_key() {
    let app = TestApp::spawn().await;
    let (_, user_id) = seed_runtime(&app).await;
    let key = second_key(&app, user_id).await;
    fixtures::create_budget_cap(
        &app.db,
        "api_key_lineage",
        key.row.lineage_id,
        "daily",
        1_000_000,
    )
    .await
    .unwrap();
    let con = admin_session(&app).await;

    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    gw.post("/v1/chat/completions", chat())
        .await
        .unwrap()
        .assert_ok();
    let (_, caps) = usage(&con, "api_key", key.row.id).await;
    assert!(
        caps[0] > 0,
        "the key's budget counted the request: {caps:?}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_request_refused_by_a_budget_charges_no_request_limit() {
    use fred::interfaces::KeysInterface;
    use think_watch_common::limits::budget;

    let app = TestApp::spawn().await;
    let (api_key, user_id) = seed_runtime(&app).await;
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 60, 2)
        .await
        .unwrap();
    fixtures::create_budget_cap(&app.db, "user", user_id, "daily", 100)
        .await
        .unwrap();
    let spent = budget::build_key("user", user_id, "daily", chrono::Utc::now());
    let _: () = app
        .state
        .redis
        .set(&spent, 105, None, None, false)
        .await
        .unwrap();
    let con = admin_session(&app).await;

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    for _ in 0..3 {
        let r = gw.post("/v1/chat/completions", chat()).await.unwrap();
        assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    }
    assert_eq!(usage(&con, "user", user_id).await.0, vec![0]);

    // With the budget freed, both requests the limit allows go through.
    let _: () = app.state.redis.del(&spent).await.unwrap();
    for _ in 0..2 {
        gw.post("/v1/chat/completions", chat())
            .await
            .unwrap()
            .assert_ok();
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_request_refused_by_a_token_limit_charges_no_request_limit() {
    let app = TestApp::spawn().await;
    let (api_key, user_id) = seed_runtime(&app).await;
    let requests =
        fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 60, 5)
            .await
            .unwrap();
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "tokens", 300, 1)
        .await
        .unwrap();
    let con = admin_session(&app).await;

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    gw.post("/v1/chat/completions", chat())
        .await
        .unwrap()
        .assert_ok();
    for _ in 0..3 {
        let r = gw.post("/v1/chat/completions", chat()).await.unwrap();
        assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    }

    let r: Json = con
        .get(&format!("/api/admin/limits/user/{user_id}/usage"))
        .await
        .unwrap()
        .json()
        .unwrap();
    let requests_used = r["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["rule_id"] == json!(requests))
        .map(|x| x["current"].as_i64().unwrap());
    assert_eq!(requests_used, Some(1), "usage: {r}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_rate_limit_says_when_its_window_frees() {
    let app = TestApp::spawn().await;
    let (api_key, user_id) = seed_runtime(&app).await;
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 300, 1)
        .await
        .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    gw.post("/v1/chat/completions", chat())
        .await
        .unwrap()
        .assert_ok();
    let r = gw.post("/v1/chat/completions", chat()).await.unwrap();
    assert_eq!(r.status.as_u16(), 429);
    // The one request leaves the five-minute window in under five
    // minutes, and not before the window has nearly run its course.
    let secs = retry_after(&r);
    assert!((290..=300).contains(&secs), "Retry-After {secs}");
    assert!(
        r.headers.get("x-should-retry").is_none(),
        "a window frees by itself; SDK retries are welcome"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_spent_budget_says_when_its_period_ends_and_not_to_retry() {
    use chrono::{Datelike, TimeZone, Utc};
    use fred::interfaces::KeysInterface;
    use think_watch_common::limits::budget;

    let app = TestApp::spawn().await;
    let (api_key, user_id) = seed_runtime(&app).await;
    fixtures::create_budget_cap(&app.db, "user", user_id, "monthly", 100)
        .await
        .unwrap();
    let now = Utc::now();
    let spent = budget::build_key("user", user_id, "monthly", now);
    let _: () = app
        .state
        .redis
        .set(&spent, 100, None, None, false)
        .await
        .unwrap();
    let (y, m) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    let next_month = Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).unwrap();
    let expected = (next_month - now).num_seconds();

    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    // The Anthropic surface, to see the refusal in the caller's format.
    let r = gw
        .post(
            "/v1/messages",
            json!({"model": "gpt-test", "max_tokens": 16,
                   "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    let secs = retry_after(&r) as i64;
    assert!(
        (expected - 5..=expected + 5).contains(&secs),
        "Retry-After {secs}, the month ends in {expected}"
    );
    assert_eq!(
        r.headers
            .get("x-should-retry")
            .and_then(|v| v.to_str().ok()),
        Some("false")
    );
    let body: Json = r.json().unwrap();
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "rate_limit_error", "{body}");
}

// ----------------------------------------------------------------------------
// The scripts themselves, on a clock the test sets.
// ----------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_limit_scripts_fill_free_and_record() {
    let app = TestApp::spawn().await;
    think_watch_test_support::redis_scripts::exercise_the_limit_scripts(&app.state.redis).await;
}

// ----------------------------------------------------------------------------
// MCP: a key's limits count on the key there too.
// ----------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_keys_mcp_limits_count_on_the_key_not_on_its_owner() {
    use axum::{Json as AxumJson, Router, routing::post};

    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();

    let upstream = Router::new().route(
        "/mcp",
        post(|AxumJson(req): AxumJson<Json>| async move {
            AxumJson(json!({"jsonrpc": "2.0", "id": req["id"], "result": {"content": []}}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, upstream).await;
    });
    let server_id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("limits-mcp"),
        "lim",
        &format!("http://{addr}/mcp"),
        fixtures::McpServerOpts::default(),
    )
    .await
    .unwrap();
    let row = sqlx::query_as::<_, think_watch_common::models::McpServer>(
        "SELECT * FROM mcp_servers WHERE id = $1",
    )
    .bind(server_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    let registered = think_watch_server::mcp_runtime::build_registered_server(
        &app.db,
        &row,
        &app.state.config.encryption_key,
    )
    .await
    .unwrap();
    app.state.mcp_registry.register(registered).await;

    let mut keys = Vec::new();
    for _ in 0..2 {
        let key = fixtures::create_api_key(
            &app.db,
            user.user.id,
            &unique_name("mcp-key"),
            &["mcp_gateway"],
            None,
            None,
        )
        .await
        .unwrap();
        fixtures::create_rate_limit_rule(
            &app.db,
            "api_key_lineage",
            key.row.lineage_id,
            "mcp_gateway",
            "requests",
            60,
            1,
        )
        .await
        .unwrap();
        keys.push(key);
    }
    let con = admin_session(&app).await;

    let gw = app.gateway_client();
    let call = |id: i64| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": "lim__anything", "arguments": {}}})
    };
    let mut answers = Vec::new();
    for (id, key) in [(1, &keys[0]), (2, &keys[1]), (3, &keys[0])] {
        gw.set_bearer(&key.plaintext);
        let r = gw.post("/mcp", call(id)).await.unwrap();
        r.assert_ok();
        answers.push(r.json::<Json>().unwrap());
    }
    assert!(answers[0]["result"].is_object(), "{}", answers[0]);
    assert!(
        answers[1]["result"].is_object(),
        "the second key has its own counter: {}",
        answers[1]
    );
    assert_eq!(
        answers[2]["error"]["message"], "Rate limited: api_key_lineage:requests/1m",
        "{}",
        answers[2]
    );
    assert_eq!(usage(&con, "api_key", keys[0].row.id).await.0, vec![1]);
}
