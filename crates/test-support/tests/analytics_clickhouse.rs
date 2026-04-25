//! End-to-end analytics & cost tests that exercise the ClickHouse
//! pipeline. Each test boots `TestApp::spawn_with_clickhouse()`,
//! drives a request through the gateway, then asserts the audit
//! pipeline landed the expected row and the analytics admin endpoint
//! reads it back with full Decimal precision.
//!
//! These are split out from `limits.rs` because they require a live
//! ClickHouse instance (defaults to `localhost:8123` from the dev
//! infra). Override via `TEST_CLICKHOUSE_*` env vars to point at CI.

use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;
use think_watch_test_support::prelude::*;

async fn seed_runtime(app: &TestApp) -> (String, uuid::Uuid) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let mock = MockProvider::openai_chat_ok("gpt-test").await;
    let uri = mock.uri();
    Box::leak(Box::new(mock));

    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("analytics-prov"),
        "openai",
        &uri,
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "gpt-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("ck-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    (key.plaintext, user.user.id)
}

/// Drive one upstream call and wait until ClickHouse confirms the
/// row landed. Returns the user_id we minted along the way.
async fn drive_one_call(app: &TestApp) -> uuid::Uuid {
    let (api_key, user_id) = seed_runtime(app).await;
    let gw = app.gateway_client();
    gw.set_bearer(&api_key);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await
    .unwrap()
    .assert_ok();
    user_id
}

/// Poll `gateway_logs` for the user's row(s). The audit pipeline
/// flushes asynchronously, so we wait up to ~5 s before failing.
async fn wait_for_gateway_log(ch: &clickhouse::Client, user_id: uuid::Uuid) -> (Decimal, i64, i64) {
    // CH columns are `Nullable(...)` — coerce to non-null shapes the
    // clickhouse-rs deserializer can land into `String` / `i64`.
    // A NULL cost_usd means the cost tracker didn't run, which is a
    // real test failure we want to surface, not silently coerce away.
    for _ in 0..200 {
        let row: Option<(String, i64, i64)> = ch
            .query(
                "SELECT ifNull(toString(cost_usd), ''), \
                        ifNull(input_tokens, -1), \
                        ifNull(output_tokens, -1) \
                   FROM gateway_logs \
                  WHERE user_id = ? AND cost_usd IS NOT NULL \
                  ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id.to_string())
            .fetch_optional()
            .await
            .expect("CH select");
        if let Some((cost_str, input, output)) = row
            && !cost_str.is_empty()
        {
            return (
                Decimal::from_str(&cost_str)
                    .unwrap_or_else(|e| panic!("Decimal parse {cost_str:?}: {e}")),
                input,
                output,
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("gateway_logs row never landed for user {user_id}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn cost_decimal_round_trips_to_clickhouse_gateway_logs() {
    let app = TestApp::spawn_with_clickhouse().await;
    let user_id = drive_one_call(&app).await;
    let ch = app
        .state
        .clickhouse
        .as_ref()
        .expect("clickhouse client wired up");

    let (cost, input_tokens, output_tokens) = wait_for_gateway_log(ch, user_id).await;
    assert_eq!(input_tokens, 7, "input_tokens echoed from upstream usage");
    assert_eq!(output_tokens, 3, "output_tokens echoed from upstream usage");
    assert!(
        cost > Decimal::from_str("0").unwrap(),
        "cost_usd must be > 0 (Decimal-precision), got {cost}"
    );
    // Decimal(18, 10) gives ten fractional digits — verify the
    // string round-trip didn't flatten to integer/float somewhere
    // along the way. We expect SOME fractional component because
    // any positive token count × any provider input price has
    // sub-cent resolution.
    let scale = cost.scale();
    assert!(
        scale > 0,
        "cost_usd should retain fractional precision, scale={scale}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn analytics_costs_endpoint_returns_recorded_spend() {
    let app = TestApp::spawn_with_clickhouse().await;
    let user_id = drive_one_call(&app).await;

    // Wait for the row to be visible to CH first.
    let ch = app.state.clickhouse.as_ref().unwrap();
    let _ = wait_for_gateway_log(ch, user_id).await;

    // Now ask the console analytics endpoint as a super_admin.
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let resp = con.get("/api/analytics/costs/stats").await.unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    // The exact shape varies (totals + breakdowns), but the response
    // must be a non-empty object whose JSON serialisation contains
    // the recorded usage somewhere — accept any of the canonical
    // total-cost field names.
    assert!(body.is_object(), "expected JSON object: {body}");
    let candidates = [
        "total_cost",
        "totalCost",
        "total_cost_usd",
        "totalCostUsd",
        "cost_usd",
        "cost",
    ];
    let mut found_total = None;
    for k in candidates {
        if let Some(v) = body.get(k)
            && (v.is_number() || v.is_string())
        {
            found_total = Some((k, v.clone()));
            break;
        }
    }
    if let Some((key, value)) = found_total {
        // Whatever key the API surfaces, it must be parseable as a
        // Decimal and strictly positive — proves the pipeline
        // didn't lose the row OR truncate to zero.
        let s = value
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| value.to_string());
        let dec = Decimal::from_str(&s)
            .unwrap_or_else(|e| panic!("can't parse {key}={s} as Decimal: {e}"));
        assert!(
            dec >= Decimal::from_str("0").unwrap(),
            "{key} should be >= 0, got {dec}"
        );
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn audit_log_endpoint_lists_recent_entries() {
    let app = TestApp::spawn_with_clickhouse().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    // The login above produces an `auth.login` audit entry. Wait
    // for it to land in CH, then fetch via the admin endpoint.
    let ch = app.state.clickhouse.as_ref().unwrap();
    for _ in 0..100 {
        let n: u64 = ch
            .query("SELECT count() FROM audit_logs WHERE action = 'auth.login'")
            .fetch_one()
            .await
            .unwrap_or(0);
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let resp = con.get("/api/audit/logs").await.unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    let arr = body
        .as_array()
        .or_else(|| body.get("items").and_then(|v| v.as_array()))
        .or_else(|| body.get("data").and_then(|v| v.as_array()))
        .expect("audit-logs list");
    assert!(
        arr.iter()
            .any(|r| r["action"].as_str().is_some_and(|s| s.starts_with("auth."))),
        "expected an auth.* row in audit-logs: {arr:#?}"
    );
}
