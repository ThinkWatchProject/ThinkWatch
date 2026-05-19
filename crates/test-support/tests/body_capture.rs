//! End-to-end body-capture tests. Each one boots
//! `TestApp::spawn_with_clickhouse()`, drives a request through the
//! gateway, and asserts the audit pipeline landed the captured
//! request_body / response_body in `gateway_logs` correctly under
//! four interesting configurations: default (both captured), request
//! capture disabled (response only), max_bytes lowered (truncated),
//! and cache hit (from_cache status with cached response body).
//!
//! Like the other CH-dependent tests they're `#[ignore]` so
//! `cargo nextest run --workspace` skips them and `make test-it`
//! opts in.

use serde::Deserialize;
use serde_json::Value;
use think_watch_test_support::prelude::*;

const PROBE_PROMPT: &str = "tell me about clickhouse body capture";

/// CH row shape for the body-capture columns. `clickhouse::Row` is
/// derived so `fetch_optional` can bind into it positionally; the
/// columns must appear in the same order the SELECT lists them.
#[derive(Debug, Deserialize, clickhouse::Row)]
struct BodyRow {
    request_body: Option<String>,
    response_body: Option<String>,
    request_body_bytes: Option<u32>,
    response_body_bytes: Option<u32>,
    body_capture_status: Option<String>,
}

#[derive(Debug, Deserialize, clickhouse::Row)]
struct CacheRow {
    body_capture_status: Option<String>,
    response_body: Option<String>,
}

async fn seed_runtime(app: &TestApp) -> (String, uuid::Uuid) {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let mock = MockProvider::openai_chat_ok("gpt-test").await;
    let uri = mock.uri();
    Box::leak(Box::new(mock));

    let provider =
        fixtures::create_provider(&app.db, &unique_name("body-cap-prov"), "openai", &uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "gpt-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("body-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    (key.plaintext, user.user.id)
}

async fn drive_one_call(app: &TestApp, api_key: &str, content: &str) {
    let gw = app.gateway_client();
    gw.set_bearer(api_key);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": content}]}),
    )
    .await
    .unwrap()
    .assert_ok();
}

/// Poll `gateway_logs` for the user's most recent row, returning the
/// body columns. The audit pipeline flushes asynchronously so we wait
/// up to ~10 s before failing.
async fn wait_for_body_row(ch: &clickhouse::Client, user_id: uuid::Uuid) -> BodyRow {
    for _ in 0..200 {
        let row: Option<BodyRow> = ch
            .query(
                "SELECT request_body, response_body, request_body_bytes, \
                        response_body_bytes, body_capture_status \
                   FROM gateway_logs \
                  WHERE user_id = ? \
                  ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id.to_string())
            .fetch_optional()
            .await
            .expect("CH select");
        if let Some(r) = row
            // Wait for the audit flusher to actually land status —
            // until then `fetch_optional` returns None and we keep
            // polling.
            && r.body_capture_status.is_some()
        {
            return r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("gateway_logs body row never landed for user {user_id}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn body_capture_records_request_and_response_by_default() {
    let app = TestApp::spawn_with_clickhouse().await;
    let (api_key, user_id) = seed_runtime(&app).await;
    drive_one_call(&app, &api_key, PROBE_PROMPT).await;
    let ch = app
        .state
        .clickhouse
        .as_ref()
        .expect("clickhouse client wired up");
    let row = wait_for_body_row(ch, user_id).await;

    assert_eq!(
        row.body_capture_status.as_deref(),
        Some("captured"),
        "status should be `captured` under defaults"
    );

    let req_str = row
        .request_body
        .expect("request_body should be populated by default");
    assert!(
        req_str.contains(PROBE_PROMPT),
        "captured request_body should embed the exact prompt the caller sent"
    );
    let resp_str = row
        .response_body
        .expect("response_body should be populated by default");
    assert!(
        !resp_str.is_empty(),
        "response_body should hold the assembled completion JSON"
    );

    let r_bytes = row
        .request_body_bytes
        .expect("request_body_bytes should be set");
    assert_eq!(
        r_bytes as usize,
        req_str.len(),
        "byte count should match string length"
    );
    let s_bytes = row
        .response_body_bytes
        .expect("response_body_bytes should be set");
    assert_eq!(
        s_bytes as usize,
        resp_str.len(),
        "byte count should match string length"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn body_capture_disabled_toggle_writes_null_request() {
    let app = TestApp::spawn_with_clickhouse().await;
    fixtures::set_setting(&app.db, "audit.capture_request_bodies", Value::Bool(false))
        .await
        .unwrap();
    let (api_key, user_id) = seed_runtime(&app).await;
    drive_one_call(&app, &api_key, PROBE_PROMPT).await;
    let ch = app.state.clickhouse.as_ref().unwrap();
    let row = wait_for_body_row(ch, user_id).await;
    // Response capture still enabled → response_body populated,
    // status remains "captured" because at least one body landed.
    assert!(
        row.request_body.is_none(),
        "request_body should be NULL when capture is off"
    );
    assert!(
        row.request_body_bytes.is_none(),
        "byte count should be NULL when body is NULL"
    );
    assert!(
        row.response_body.is_some(),
        "response_body should still land when only request capture is off"
    );
    assert_eq!(row.body_capture_status.as_deref(), Some("captured"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn body_capture_truncates_when_over_max_bytes() {
    let app = TestApp::spawn_with_clickhouse().await;
    // 256 → tiny cap; the JSON-serialized messages array will far
    // exceed this so we should see the …[truncated] sentinel and
    // status = "truncated".
    fixtures::set_setting(&app.db, "audit.body_max_bytes", Value::from(256_i64))
        .await
        .unwrap();
    let (api_key, user_id) = seed_runtime(&app).await;
    drive_one_call(&app, &api_key, PROBE_PROMPT).await;
    let ch = app.state.clickhouse.as_ref().unwrap();
    let row = wait_for_body_row(ch, user_id).await;
    assert_eq!(row.body_capture_status.as_deref(), Some("truncated"));
    let req_str = row
        .request_body
        .expect("request_body should still be set, just truncated");
    assert!(
        req_str.ends_with("..."),
        "truncated request should carry the ellipsis sentinel, got: {req_str:?}"
    );
    let r_bytes = row.request_body_bytes.expect("byte count populated");
    assert!(
        (r_bytes as usize) <= 256,
        "truncated request must be within the configured cap, got {r_bytes}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn body_capture_marks_cache_hits_with_from_cache_status() {
    let app = TestApp::spawn_with_clickhouse().await;
    let (api_key, user_id) = seed_runtime(&app).await;
    // First call: cache miss, populates the slot.
    drive_one_call(&app, &api_key, PROBE_PROMPT).await;
    let ch = app.state.clickhouse.as_ref().unwrap();
    let _ = wait_for_body_row(ch, user_id).await;
    // Second IDENTICAL call: cache hit. Status should now be
    // "from_cache" with the cached response body still in place.
    drive_one_call(&app, &api_key, PROBE_PROMPT).await;
    // Poll until we see a from_cache row (the first row is "captured").
    for _ in 0..200 {
        let row: Option<CacheRow> = ch
            .query(
                "SELECT body_capture_status, response_body \
                   FROM gateway_logs \
                  WHERE user_id = ? AND body_capture_status = 'from_cache' \
                  ORDER BY created_at DESC LIMIT 1",
            )
            .bind(user_id.to_string())
            .fetch_optional()
            .await
            .expect("CH select");
        if let Some(r) = row {
            assert_eq!(r.body_capture_status.as_deref(), Some("from_cache"));
            assert!(
                r.response_body.is_some_and(|s| !s.is_empty()),
                "cache-hit row should carry the cached response body"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("from_cache row never landed for user {user_id}");
}
