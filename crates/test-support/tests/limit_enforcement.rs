//! Where the limits used to leak, pinned shut:
//!
//!   - a limits store that cannot be read follows
//!     `security.rate_limit_fail_closed` — the database the limits are
//!     loaded from, and the Redis a budget is read from — instead of
//!     reading as "no limits", and `GET /v1/usage` does not report
//!     limits it could not load as none;
//!   - a route's `rpm_cap` / `tpm_cap` is enforced: a route at its cap
//!     is skipped, and with every route capped the request gets 429;
//!   - a request the key may not make (its model, its MCP tool) is
//!     refused before it counts against a request limit;
//!   - an answer from the response cache counts its tokens.

use fred::interfaces::{HashesInterface, KeysInterface};
use think_watch_common::limits::{
    RateLimitSubject, RateMetric, Surface as LimitSurface, budget, sliding,
};
use think_watch_test_support::client::TestResponse;
use think_watch_test_support::prelude::*;

const MODEL: &str = "gpt-test";

fn chat(model: &str) -> Json {
    json!({"model": model, "messages": [{"role": "user", "content": "x"}]})
}

/// A request the response cache does not answer (it keeps only
/// deterministic ones), so every one reaches route selection.
fn uncached(model: &str) -> Json {
    json!({"model": model, "temperature": 1,
           "messages": [{"role": "user", "content": "x"}]})
}

/// A user with gateway access, an OpenAI mock routed as `MODEL`, and a
/// key. Returns the key and the user.
async fn seed(app: &TestApp, allowed_models: Option<&[&str]>) -> (String, Uuid) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let mock = MockProvider::openai_chat_ok(MODEL).await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("enf-prov"),
        "openai",
        &mock.uri(),
        None,
    )
    .await
    .unwrap();
    Box::leak(Box::new(mock));
    fixtures::create_model_and_route(&app.db, provider.id, MODEL)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("enf-key"),
        &["ai_gateway"],
        allowed_models,
        None,
    )
    .await
    .unwrap();
    (key.plaintext, user.user.id)
}

fn gateway(app: &TestApp, key: &str) -> TestClient {
    let gw = app.gateway_client();
    gw.set_bearer(key);
    gw
}

fn retry_after(r: &TestResponse) -> u64 {
    r.headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("a 429 carries Retry-After, headers: {:?}", r.headers))
}

/// What a user's `requests` rule on `surface` has counted in its window.
async fn user_requests(app: &TestApp, user_id: Uuid, surface: LimitSurface) -> i64 {
    let rule = think_watch_common::limits::RateLimitRule {
        id: Uuid::nil(),
        subject_kind: RateLimitSubject::User,
        subject_id: user_id,
        surface,
        metric: RateMetric::Requests,
        window_secs: 60,
        max_count: 1,
        enabled: true,
        expires_at: None,
        reason: None,
        created_by: None,
    };
    sliding::current_count(
        &app.state.redis,
        &sliding::ResolvedRule::new(&rule, user_id),
    )
    .await
}

// ----------------------------------------------------------------------------
// The limits can't be loaded
// ----------------------------------------------------------------------------

/// Loading the limits fails: the rule table is gone from under the
/// running server.
async fn break_the_limits_table(app: &TestApp) {
    sqlx::query("DROP TABLE rate_limit_rules CASCADE")
        .execute(&app.db)
        .await
        .unwrap();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn limits_that_cannot_be_loaded_refuse_when_failing_closed() {
    let app = TestApp::spawn().await;
    let (key, _) = seed(&app, None).await;
    app.set_setting("security.rate_limit_fail_closed", json!(true))
        .await;
    let gw = gateway(&app, &key);
    gw.post("/v1/chat/completions", chat(MODEL))
        .await
        .unwrap()
        .assert_ok();

    break_the_limits_table(&app).await;
    let r = gw.post("/v1/chat/completions", chat(MODEL)).await.unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    assert_eq!(retry_after(&r), 30);
    let body: Json = r.json().unwrap();
    assert_eq!(body["error"]["message"], "Rate limited: limits_unavailable");

    // An Anthropic caller gets it in its own format.
    let r = gw
        .post(
            "/v1/messages",
            json!({"model": MODEL, "max_tokens": 8,
                   "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    let body: Json = r.json().unwrap();
    assert_eq!(body["type"], "error");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn limits_that_cannot_be_loaded_let_the_request_through_when_failing_open() {
    let app = TestApp::spawn().await;
    let (key, _) = seed(&app, None).await;
    app.set_setting("security.rate_limit_fail_closed", json!(false))
        .await;
    break_the_limits_table(&app).await;
    gateway(&app, &key)
        .post("/v1/chat/completions", chat(MODEL))
        .await
        .unwrap()
        .assert_ok();
}

/// `GET /v1/usage` reports the limits that hold the key. Limits that
/// can't be loaded leave it nothing true to report — an empty list would
/// say the key has none — so it is a 503 whichever way the setting goes,
/// while a model request follows the setting.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn v1_usage_with_limits_that_cannot_be_loaded_is_unavailable() {
    let app = TestApp::spawn().await;
    let (key, _) = seed(&app, None).await;
    let gw = gateway(&app, &key);
    let before = gw.get("/v1/usage").await.unwrap();
    before.assert_ok();

    break_the_limits_table(&app).await;
    for fail_closed in [false, true] {
        app.set_setting("security.rate_limit_fail_closed", json!(fail_closed))
            .await;
        let r = gw.get("/v1/usage").await.unwrap();
        assert_eq!(
            r.status.as_u16(),
            503,
            "fail_closed={fail_closed}: {}",
            r.text()
        );
        let body: Json = r.json().unwrap();
        assert_eq!(
            body["error"]["message"],
            "The key's limits and usage are unavailable."
        );
        let model = gw.post("/v1/chat/completions", chat(MODEL)).await.unwrap();
        let expected = if fail_closed { 429 } else { 200 };
        assert_eq!(model.status.as_u16(), expected, "body={}", model.text());
    }
}

// ----------------------------------------------------------------------------
// The budget can't be read
// ----------------------------------------------------------------------------

/// A daily budget whose counter Redis refuses to read as a number: the
/// key holds a hash, so GET fails with WRONGTYPE.
async fn unreadable_budget(app: &TestApp, user_id: Uuid) {
    fixtures::create_budget_cap(&app.db, "user", user_id, "daily", 1_000_000)
        .await
        .unwrap();
    let key = budget::build_key("user", user_id, "daily", chrono::Utc::now());
    let _: i64 = app.state.redis.hset(&key, ("x", 1)).await.unwrap();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_budget_that_cannot_be_read_refuses_when_failing_closed() {
    let app = TestApp::spawn().await;
    let (key, user_id) = seed(&app, None).await;
    app.set_setting("security.rate_limit_fail_closed", json!(true))
        .await;
    unreadable_budget(&app, user_id).await;
    let r = gateway(&app, &key)
        .post("/v1/chat/completions", chat(MODEL))
        .await
        .unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    assert_eq!(retry_after(&r), 30);
    let body: Json = r.json().unwrap();
    assert_eq!(body["error"]["message"], "Rate limited: budget_unavailable");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_budget_that_cannot_be_read_lets_the_request_through_when_failing_open() {
    let app = TestApp::spawn().await;
    let (key, user_id) = seed(&app, None).await;
    app.set_setting("security.rate_limit_fail_closed", json!(false))
        .await;
    unreadable_budget(&app, user_id).await;
    gateway(&app, &key)
        .post("/v1/chat/completions", chat(MODEL))
        .await
        .unwrap()
        .assert_ok();
}

// ----------------------------------------------------------------------------
// Route caps
// ----------------------------------------------------------------------------

async fn set_route_caps(app: &TestApp, provider_id: Uuid, rpm: Option<i32>, tpm: Option<i32>) {
    sqlx::query("UPDATE model_routes SET rpm_cap = $2, tpm_cap = $3 WHERE provider_id = $1")
        .bind(provider_id)
        .bind(rpm)
        .bind(tpm)
        .execute(&app.db)
        .await
        .unwrap();
}

async fn route_id(app: &TestApp, provider_id: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM model_routes WHERE provider_id = $1")
        .bind(provider_id)
        .fetch_one(&app.db)
        .await
        .unwrap()
}

/// One provider serving `model` through one route, with a key.
async fn one_route(app: &TestApp, model: &str) -> (Uuid, String) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let mock = MockProvider::openai_chat_ok(model).await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("cap-prov"),
        "openai",
        &mock.uri(),
        None,
    )
    .await
    .unwrap();
    Box::leak(Box::new(mock));
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("cap-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    (provider.id, key.plaintext)
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_route_at_its_request_cap_is_skipped_for_the_next() {
    let app = TestApp::spawn().await;
    let model = "cap-model";
    let capped = MockProvider::openai_chat_ok(model).await;
    let spare = MockProvider::openai_chat_ok(model).await;
    let p_capped = fixtures::create_provider(
        &app.db,
        &unique_name("capped"),
        "openai",
        &capped.uri(),
        None,
    )
    .await
    .unwrap();
    let p_spare =
        fixtures::create_provider(&app.db, &unique_name("spare"), "openai", &spare.uri(), None)
            .await
            .unwrap();
    // The spare route carries no traffic of its own (weight 0): it
    // serves only when the other cannot.
    fixtures::create_model_route(&app.db, p_capped.id, model, 100)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, p_spare.id, model, 0)
        .await
        .unwrap();
    set_route_caps(&app, p_capped.id, Some(1), None).await;
    app.rebuild_gateway_router().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("cap-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = gateway(&app, &key.plaintext);

    for _ in 0..3 {
        gw.post("/v1/chat/completions", uncached(model))
            .await
            .unwrap()
            .assert_ok();
    }
    assert_eq!(
        capped.received_requests().await.len(),
        1,
        "the capped route takes one request a minute"
    );
    assert_eq!(
        spare.received_requests().await.len(),
        2,
        "the next route serves the rest"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn every_route_at_its_cap_is_a_429_with_retry_after() {
    let app = TestApp::spawn().await;
    let model = "cap-all-model";
    let (provider, key) = one_route(&app, model).await;
    set_route_caps(&app, provider, Some(1), None).await;
    app.rebuild_gateway_router().await;
    let gw = gateway(&app, &key);

    gw.post("/v1/chat/completions", uncached(model))
        .await
        .unwrap()
        .assert_ok();

    let r = gw
        .post("/v1/chat/completions", uncached(model))
        .await
        .unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    let secs = retry_after(&r);
    assert!((1..=60).contains(&secs), "Retry-After {secs}");
    let body: Json = r.json().unwrap();
    assert_eq!(body["error"]["message"], "Rate limited: route:requests/1m");

    // A stream is refused the same way, before it starts.
    let mut streamed = uncached(model);
    streamed["stream"] = json!(true);
    let r = gw.post("/v1/chat/completions", streamed).await.unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    retry_after(&r);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_token_cap_counts_weighted_tokens_after_the_response() {
    let app = TestApp::spawn().await;
    let model = "cap-tokens-model";
    let (provider, key) = one_route(&app, model).await;
    // The mock answers 7 prompt + 3 completion tokens: 2×7 + 3×3 = 23
    // weighted, over a cap of 20 that the raw 10 would stay under.
    sqlx::query("UPDATE models SET input_weight = 2, output_weight = 3 WHERE model_id = $1")
        .bind(model)
        .execute(&app.db)
        .await
        .unwrap();
    set_route_caps(&app, provider, None, Some(20)).await;
    app.rebuild_gateway_router().await;
    let gw = gateway(&app, &key);

    gw.post("/v1/chat/completions", uncached(model))
        .await
        .unwrap()
        .assert_ok();
    let route = route_id(&app, provider).await;
    let counter = think_watch_gateway::route_caps::counter_key(route, RateMetric::Tokens);
    let buckets: std::collections::HashMap<String, i64> =
        app.state.redis.hgetall(&counter).await.unwrap();
    assert_eq!(buckets.values().sum::<i64>(), 23, "{buckets:?}");
    let requests = think_watch_gateway::route_caps::counter_key(route, RateMetric::Requests);
    let exists: i64 = app.state.redis.exists(&requests).await.unwrap();
    assert_eq!(
        exists, 0,
        "a route without a request cap counts no requests"
    );

    let r = gw
        .post("/v1/chat/completions", uncached(model))
        .await
        .unwrap();
    assert_eq!(r.status.as_u16(), 429, "body={}", r.text());
    let body: Json = r.json().unwrap();
    assert_eq!(body["error"]["message"], "Rate limited: route:tokens/1m");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_route_cap_scripts_fill_free_and_record() {
    let app = TestApp::spawn().await;
    think_watch_test_support::redis_scripts::exercise_the_route_cap_scripts(&app.state.redis).await;
}

// ----------------------------------------------------------------------------
// Access before limits
// ----------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_model_the_key_may_not_use_does_not_count_a_request() {
    let app = TestApp::spawn().await;
    let (key, user_id) = seed(&app, Some(&[MODEL])).await;
    fixtures::create_rate_limit_rule(&app.db, "user", user_id, "ai_gateway", "requests", 60, 1)
        .await
        .unwrap();
    let gw = gateway(&app, &key);

    for _ in 0..3 {
        let r = gw
            .post("/v1/chat/completions", chat("not-for-this-key"))
            .await
            .unwrap();
        assert_eq!(r.status.as_u16(), 400, "body={}", r.text());
        assert!(r.text().contains("is not allowed"), "{}", r.text());
    }
    assert_eq!(
        user_requests(&app, user_id, LimitSurface::AiGateway).await,
        0
    );

    gw.post("/v1/chat/completions", chat(MODEL))
        .await
        .unwrap()
        .assert_ok();
    assert_eq!(
        user_requests(&app, user_id, LimitSurface::AiGateway).await,
        1
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn an_mcp_tool_the_key_may_not_call_does_not_count_a_request() {
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
        &unique_name("order-mcp"),
        "ord",
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

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("order-key"),
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE api_keys SET allowed_mcp_tools = ARRAY['ord__allowed'] WHERE id = $1")
        .bind(key.row.id)
        .execute(&app.db)
        .await
        .unwrap();
    fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        user.user.id,
        "mcp_gateway",
        "requests",
        60,
        1,
    )
    .await
    .unwrap();
    let gw = gateway(&app, &key.plaintext);
    let call = |id: i64, tool: &str| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": tool, "arguments": {}}})
    };

    for id in 1..=3 {
        let r: Json = gw
            .post("/mcp", call(id, "ord__forbidden"))
            .await
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(r["error"]["message"], "Access denied for this tool", "{r}");
    }
    assert_eq!(
        user_requests(&app, user.user.id, LimitSurface::McpGateway).await,
        0
    );
    let r: Json = gw
        .post("/mcp", call(4, "ord__allowed"))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert!(r["result"].is_object(), "{r}");
    assert_eq!(
        user_requests(&app, user.user.id, LimitSurface::McpGateway).await,
        1
    );
}

// ----------------------------------------------------------------------------
// Cache hits
// ----------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_cached_answer_counts_its_tokens() {
    let app = TestApp::spawn().await;
    let (key, user_id) = seed(&app, None).await;
    let rule = fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        user_id,
        "ai_gateway",
        "tokens",
        60,
        1_000_000,
    )
    .await
    .unwrap();
    fixtures::create_budget_cap(&app.db, "user", user_id, "daily", 1_000_000)
        .await
        .unwrap();
    let gw = gateway(&app, &key);
    let deterministic = json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": "the same every time"}],
        "temperature": 0,
    });

    let tokens = || async {
        let rule = think_watch_common::limits::RateLimitRule {
            id: rule,
            subject_kind: RateLimitSubject::User,
            subject_id: user_id,
            surface: LimitSurface::AiGateway,
            metric: RateMetric::Tokens,
            window_secs: 60,
            max_count: 1_000_000,
            enabled: true,
            expires_at: None,
            reason: None,
            created_by: None,
        };
        let window = sliding::current_count(
            &app.state.redis,
            &sliding::ResolvedRule::new(&rule, user_id),
        )
        .await;
        let spent: Option<i64> = app
            .state
            .redis
            .get(budget::build_key(
                "user",
                user_id,
                "daily",
                chrono::Utc::now(),
            ))
            .await
            .unwrap();
        (window, spent.unwrap_or(0))
    };

    let first = gw
        .post("/v1/chat/completions", deterministic.clone())
        .await
        .unwrap();
    first.assert_ok();
    assert_eq!(first.headers["x-cache"], "MISS");
    let (window, spent) = tokens().await;
    assert!(window > 0, "the upstream answer counted");
    assert_eq!(window, spent);

    let hit = gw
        .post("/v1/chat/completions", deterministic.clone())
        .await
        .unwrap();
    hit.assert_ok();
    assert_eq!(hit.headers["x-cache"], "HIT");
    assert_eq!(
        tokens().await,
        (2 * window, 2 * spent),
        "the cached answer counts the tokens it records, like the call that made it"
    );
}
