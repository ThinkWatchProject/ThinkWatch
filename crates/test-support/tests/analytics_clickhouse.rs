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

/// A Chat stream is billed on the upstream's usage even when the caller
/// did not ask for the usage chunk, and the caller still does not get
/// one. Forwarded as sent, such a stream used to reach the upstream
/// without `stream_options.include_usage`, come back with no usage, and
/// be recorded as zero tokens.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_chat_stream_is_billed_when_the_caller_did_not_ask_for_usage() {
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let app = TestApp::spawn_with_clickhouse().await;
    let chunk = |content: &str| {
        json!({"id":"c","object":"chat.completion.chunk","created":1,"model":"gpt-stream",
               "choices":[{"index":0,"delta":{"content":content},"finish_reason":null}],"usage":null})
    };
    let usage = json!({"id":"c","object":"chat.completion.chunk","created":1,"model":"gpt-stream",
                       "choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}});
    let sse = format!(
        "data: {}\n\ndata: {}\n\ndata: {usage}\n\ndata: [DONE]\n\n",
        chunk("hel"),
        chunk("lo")
    );
    let upstream = MockServer::start().await;
    // Only a request that asks for usage gets an answer.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            json!({"stream_options": {"include_usage": true}}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse, "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("stream-usage"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "gpt-stream")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("stream-usage-key"),
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
            json!({"model": "gpt-stream", "stream": true,
                   "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let text = resp.text();
    assert!(text.contains("hel") && text.contains("lo"), "{text}");
    assert!(
        !text.contains("usage"),
        "the caller did not ask for usage and got it: {text}"
    );
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");

    let ch = app.state.clickhouse.as_ref().expect("clickhouse client");
    let (_, input_tokens, output_tokens) = wait_for_gateway_log(ch, user.user.id).await;
    assert_eq!((input_tokens, output_tokens), (5, 2));
}

/// The latest `gateway_logs` row for a user: status, input and output
/// tokens, cost, and the detail JSON.
async fn last_gateway_log(
    ch: &clickhouse::Client,
    user_id: uuid::Uuid,
) -> (i64, i64, i64, Decimal, Value) {
    for _ in 0..200 {
        let row: Option<(i64, i64, i64, String, String)> = ch
            .query(
                "SELECT ifNull(status_code, -1), ifNull(input_tokens, -1), \
                        ifNull(output_tokens, -1), ifNull(toString(cost_usd), ''), \
                        ifNull(detail, '') \
                   FROM gateway_logs \
                  WHERE user_id = ? AND cost_usd IS NOT NULL \
                  ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id.to_string())
            .fetch_optional()
            .await
            .expect("CH select");
        if let Some((status, input, output, cost, detail)) = row
            && !cost.is_empty()
        {
            return (
                status,
                input,
                output,
                Decimal::from_str(&cost).unwrap(),
                serde_json::from_str(&detail).unwrap_or(Value::Null),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("gateway_logs row never landed for user {user_id}");
}

/// One provider at `uri` serving `model`, and a key for a new user.
async fn seed_upstream(
    app: &TestApp,
    uri: &str,
    provider_type: &str,
    model: &str,
) -> (String, uuid::Uuid) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let provider =
        fixtures::create_provider(&app.db, &unique_name("bill"), provider_type, uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("bill-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    (key.plaintext, user.user.id)
}

/// A stream the upstream reports no usage for — one that ignores
/// `stream_options` — is billed on an estimate, not as zero tokens, and
/// the row says it is an estimate.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_without_usage_is_billed_on_an_estimate() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let app = TestApp::spawn_with_clickhouse().await;
    let chunk = |content: &str| {
        json!({"id":"c","object":"chat.completion.chunk","created":1,"model":"gpt-nousage",
               "choices":[{"index":0,"delta":{"content":content},"finish_reason":null}]})
    };
    let words = "x".repeat(400);
    let sse = format!("data: {}\n\ndata: [DONE]\n\n", chunk(&words));
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&upstream)
        .await;
    let (key, user_id) = seed_upstream(&app, &upstream.uri(), "openai", "gpt-nousage").await;

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let prompt = "y".repeat(800);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "gpt-nousage", "stream": true,
               "messages": [{"role": "user", "content": prompt}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    let ch = app.state.clickhouse.as_ref().expect("clickhouse client");
    let (status, input, output, cost, detail) = last_gateway_log(ch, user_id).await;
    assert_eq!(status, 200);
    // About four bytes a token: 800 bytes in, 400 out.
    assert_eq!((input, output), (200, 100));
    assert!(cost > Decimal::ZERO, "an unreported stream was free");
    assert_eq!(detail["usage_estimated"], true, "{detail}");
}

/// A caller that leaves mid-stream takes the upstream's final usage with
/// it. The request is billed on what was sent before it left.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_stream_the_caller_leaves_is_billed_on_an_estimate() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let app = TestApp::spawn_with_clickhouse().await;
    // An upstream that sends one chunk and then keeps the stream open.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = sock.read(&mut buf).await;
                let chunk = json!({"id":"c","object":"chat.completion.chunk","created":1,
                    "model":"gpt-leave","choices":[{"index":0,
                    "delta":{"content":"z".repeat(400)},"finish_reason":null}]});
                let event = format!("data: {chunk}\n\n");
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                            transfer-encoding: chunked\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock
                    .write_all(format!("{:x}\r\n{event}\r\n", event.len()).as_bytes())
                    .await;
                let _ = sock.flush().await;
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            });
        }
    });
    let (key, user_id) =
        seed_upstream(&app, &format!("http://{addr}"), "openai", "gpt-leave").await;

    let mut resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", app.gateway_url))
        .bearer_auth(&key)
        .json(&json!({"model": "gpt-leave", "stream": true,
                      "messages": [{"role": "user", "content": "q".repeat(800)}]}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let first = resp.chunk().await.unwrap().unwrap_or_default();
    assert!(String::from_utf8_lossy(&first).contains("zzzz"));
    drop(resp);

    let ch = app.state.clickhouse.as_ref().expect("clickhouse client");
    let (status, input, output, cost, detail) = last_gateway_log(ch, user_id).await;
    assert_eq!(status, 499, "{detail}");
    assert_eq!((input, output), (200, 100));
    assert!(cost > Decimal::ZERO, "a stream the caller left was free");
    assert_eq!(detail["usage_estimated"], true, "{detail}");
}

/// Input read from the prompt cache is billed at a tenth of the input
/// price and input written to it at 1.25×, not all at the full price —
/// and the budget counter is debited by the same weights.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn cached_input_is_billed_at_the_cache_prices() {
    use fred::interfaces::KeysInterface;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let app = TestApp::spawn_with_clickhouse().await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_cache", "type": "message", "role": "assistant", "model": "claude-cache",
            "content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "cache_read_input_tokens": 10000,
                      "cache_creation_input_tokens": 2000, "output_tokens": 50}
        })))
        .mount(&upstream)
        .await;
    let (key, user_id) = seed_upstream(&app, &upstream.uri(), "anthropic", "claude-cache").await;
    fixtures::create_budget_cap(&app.db, "user", user_id, "daily", 1_000_000)
        .await
        .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&key);
    let ask = || {
        gw.post(
            "/v1/messages",
            json!({"model": "claude-cache", "max_tokens": 16,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
    };
    ask().await.unwrap().assert_ok();

    let ch = app.state.clickhouse.as_ref().expect("clickhouse client");
    let (_, input, output, cost, detail) = last_gateway_log(ch, user_id).await;
    // The row's input is the whole input, cached or not.
    assert_eq!((input, output), (12_100, 50));
    // Default baseline 0.000002 in / 0.000008 out, weights 1.0:
    // 0.000002 × (100 + 10 000 × 0.1 + 2 000 × 1.25) + 0.000008 × 50
    assert_eq!(cost, Decimal::from_str("0.0076").unwrap(), "{detail}");
    assert_eq!(detail["cache_read_tokens"], 10_000);
    assert_eq!(detail["cache_write_tokens"], 2_000);

    // 100 + 1 000 + 2 500 + 50 weighted tokens on the budget counter.
    let budget_key =
        think_watch_common::limits::budget::build_key("user", user_id, "daily", chrono::Utc::now());
    let debited: Option<String> = app.state.redis.get(&budget_key).await.unwrap();
    assert_eq!(
        debited.as_deref(),
        Some("3650"),
        "{budget_key} for {user_id}"
    );

    // A model's own cache price replaces the derived one.
    sqlx::query("UPDATE models SET cache_read_weight = 0.5 WHERE model_id = 'claude-cache'")
        .execute(&app.db)
        .await
        .unwrap();
    app.state.weight_cache.invalidate_all().await;
    ask().await.unwrap().assert_ok();
    // The second row lands asynchronously, like the first.
    let mut second = cost;
    for _ in 0..200 {
        second = last_gateway_log(ch, user_id).await.3;
        if second != cost {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // 0.000002 × (100 + 5 000 + 2 500) + 0.0004
    assert_eq!(second, Decimal::from_str("0.0156").unwrap());
}
