//! Trace / SLO / chargeback admin endpoint shapes.
//!
//! Three tightly-coupled read endpoints sit on top of the
//! ClickHouse-backed analytics tables. Each one has a public
//! response shape the frontend (and downstream procurement / SRE
//! tooling) reads, so a quiet schema change here breaks dashboards
//! without anyone noticing in the API tests:
//!
//!   - `GET /api/admin/trace/{trace_id}` — joins `gateway_logs`,
//!     `mcp_logs`, `audit_logs`, `app_logs` rows by trace_id and
//!     returns a `{trace_id, events: [...]}` envelope.
//!   - `GET /api/admin/slo?hours=…` — returns
//!     `{window_hours, total_requests, error_requests, error_rate,
//!     p50_ms, p95_ms, p99_ms}`. Hours-arg clamps to {1, 24, 168}.
//!   - `GET /api/admin/chargeback.csv` — CSV with the header
//!     `cost_center,model_id,cost_usd,total_tokens,request_count`
//!     and one row per `(cost_center, model_id)` aggregation.
//!
//! Each test drives one real upstream call so there's a row to
//! aggregate, then asserts the response shape end-to-end.

use serde_json::Value;
use think_watch_test_support::prelude::*;

async fn admin_session(app: &TestApp) -> TestClient {
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con
}

async fn drive_one_call(app: &TestApp, model: &str) -> (uuid::Uuid, uuid::Uuid, String) {
    // Returns (user_id, api_key_id, trace_id).
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let upstream = MockProvider::openai_chat_ok(model).await;
    let uri = upstream.uri();
    Box::leak(Box::new(upstream));

    let provider =
        fixtures::create_provider(&app.db, &unique_name("trace-prov"), "openai", &uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, model)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("trace-key"),
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
            json!({"model": model, "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    // X-Metadata-Request-Id is the trace_id the proxy stamps onto
    // the gateway_logs row. Take it off the success response.
    let trace_id = resp
        .headers
        .get("x-metadata-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .expect("X-Metadata-Request-Id on success response");
    (user.user.id, key.row.id, trace_id)
}

// ---------------------------------------------------------------------------
// Trace
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn trace_endpoint_returns_gateway_event_for_completed_request() {
    let app = TestApp::spawn_with_clickhouse().await;
    let (_uid, _kid, trace_id) = drive_one_call(&app, "trace-test").await;
    let con = admin_session(&app).await;

    // The audit pipeline batches into CH; poll until the gateway row
    // surfaces in the trace endpoint.
    let mut events: Vec<Value> = Vec::new();
    for _ in 0..200 {
        let body: Value = con
            .get(&format!("/api/admin/trace/{trace_id}"))
            .await
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(
            body["trace_id"], trace_id,
            "response must echo the trace_id"
        );
        events = body["events"].as_array().cloned().unwrap_or_default();
        if events.iter().any(|e| e["kind"] == "gateway") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let gateway = events
        .iter()
        .find(|e| e["kind"] == "gateway")
        .unwrap_or_else(|| {
            panic!("no `gateway` event in trace response after 10s; events={events:#?}")
        });
    assert_eq!(gateway["subject"], "trace-test");
    assert_eq!(gateway["status"], "200");
    assert!(
        gateway["duration_ms"].as_i64().unwrap_or(-1) >= 0,
        "duration_ms must be a non-negative i64: {gateway}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn trace_endpoint_rejects_overlong_id() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    // 129 chars: just over the 128-char cap.
    let long_id = "a".repeat(129);
    let resp = con
        .get(&format!("/api/admin/trace/{long_id}"))
        .await
        .unwrap();
    resp.assert_status(400);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn trace_endpoint_returns_empty_events_for_unknown_id() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let body: Value = con
        .get(&format!("/api/admin/trace/{}", uuid::Uuid::new_v4()))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert!(
        body["events"].as_array().unwrap().is_empty(),
        "unknown trace_id must return empty events, got {body}"
    );
}

// ---------------------------------------------------------------------------
// SLO
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn slo_snapshot_returns_full_envelope() {
    let app = TestApp::spawn_with_clickhouse().await;
    let _ = drive_one_call(&app, "slo-test").await;
    let con = admin_session(&app).await;

    // Wait for the row to land so total_requests > 0.
    let mut last_body: Value = Value::Null;
    for _ in 0..200 {
        let body: Value = con
            .get("/api/admin/slo?hours=24")
            .await
            .unwrap()
            .json()
            .unwrap();
        if body["total_requests"].as_u64().unwrap_or(0) > 0 {
            last_body = body;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let body = if last_body.is_null() {
        // Last-ditch fetch so the assertion error includes the body.
        con.get("/api/admin/slo?hours=24")
            .await
            .unwrap()
            .json()
            .unwrap()
    } else {
        last_body
    };

    // Pin the full response envelope. A field rename / removal is a
    // dashboard regression even if the underlying CH math is fine.
    assert_eq!(body["window_hours"], 24);
    assert!(
        body["total_requests"].as_u64().unwrap() >= 1,
        "expected ≥1 request after drive_one_call: {body}"
    );
    for k in ["error_requests", "error_rate", "p50_ms", "p95_ms", "p99_ms"] {
        assert!(
            body.get(k).is_some_and(|v| v.is_number()),
            "{k} must be present and numeric in: {body}"
        );
    }
    // p50 ≤ p95 ≤ p99 — a CH quantile regression that swaps fields
    // is otherwise silent.
    let (p50, p95, p99) = (
        body["p50_ms"].as_f64().unwrap(),
        body["p95_ms"].as_f64().unwrap(),
        body["p99_ms"].as_f64().unwrap(),
    );
    assert!(
        p50 <= p95 && p95 <= p99,
        "percentiles must be monotone non-decreasing: p50={p50} p95={p95} p99={p99}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn slo_snapshot_clamps_unsupported_hours_to_24() {
    let app = TestApp::spawn_with_clickhouse().await;
    let con = admin_session(&app).await;

    let body: Value = con
        .get("/api/admin/slo?hours=99999")
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        body["window_hours"], 24,
        "out-of-range hours must clamp to 24, got {body}"
    );
}

// ---------------------------------------------------------------------------
// Chargeback
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn chargeback_csv_groups_by_cost_center() {
    let app = TestApp::spawn_with_clickhouse().await;
    let (_uid, key_id, _trace) = drive_one_call(&app, "chargeback-test").await;

    // Tag the api key with a cost center AFTER the call, so the
    // gateway_logs row references this key by id and the PG enrichment
    // step in the chargeback handler picks up the tag.
    sqlx::query("UPDATE api_keys SET cost_center = $1 WHERE id = $2")
        .bind("audit-team")
        .bind(key_id)
        .execute(&app.db)
        .await
        .unwrap();

    let con = admin_session(&app).await;

    // Wait for the gateway_logs row to land in CH, then fetch the CSV.
    let mut csv = String::new();
    for _ in 0..200 {
        let resp = con.get("/api/admin/chargeback.csv").await.unwrap();
        resp.assert_ok();
        // Content-Type must be text/csv — pin it so a refactor that
        // accidentally JSON-ifies the response is caught.
        let ctype = resp
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ctype.starts_with("text/csv"),
            "chargeback must return text/csv, got {ctype}"
        );
        csv = resp.text();
        if csv.contains("audit-team") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let mut lines = csv.lines();
    let header = lines.next().expect("CSV must have a header line");
    assert_eq!(
        header, "cost_center,model_id,cost_usd,total_tokens,request_count",
        "chargeback header is part of the public contract"
    );
    let row = lines
        .find(|l| l.starts_with("audit-team,"))
        .unwrap_or_else(|| panic!("no audit-team row in:\n{csv}"));
    let cells: Vec<&str> = row.split(',').collect();
    assert_eq!(cells.len(), 5, "5 columns expected, got {row:?}");
    assert_eq!(cells[1], "chargeback-test");
    assert!(
        cells[3].parse::<u64>().unwrap_or(0) >= 1,
        "total_tokens should aggregate ≥ 1 from the upstream usage block: {row}"
    );
    assert!(
        cells[4].parse::<u64>().unwrap_or(0) >= 1,
        "request_count should be ≥ 1: {row}"
    );
}
