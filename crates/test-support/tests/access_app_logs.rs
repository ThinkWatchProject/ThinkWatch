//! `access_logs` and `app_logs` read-endpoint shape contracts.
//!
//! Both tables are written by long-lived background pipelines:
//! `access_logs` from the `AccessLogLayer` middleware on every HTTP
//! hit, `app_logs` from `tracing::*` events when a CH layer is
//! attached. The READ endpoints (`GET /api/admin/access-logs` and
//! `GET /api/admin/app-logs`) had no integration coverage — a
//! schema rename or filter regression here would silently break
//! the unified log explorer's "Access" / "App" tabs.
//!
//! What this file pins:
//!   - access-logs envelope: `{items: [...], total: N}`. Every
//!     entry has method / path / status_code / latency_ms / port.
//!     Pin field names because the React table renders by name.
//!   - access-logs filter: `?method=POST` narrows correctly; only
//!     rows with that method come back.
//!   - app-logs envelope: `{items: [...], total: N}` with level /
//!     target / message / fields / span.
//!   - app-logs filter: `?level=error` narrows; `?q=needle` does
//!     a substring search across the message column.
//!
//! Access logs land via the production middleware just by driving
//! a couple HTTP requests. App logs aren't written by any
//! production source today (the tracing→CH layer isn't wired up
//! yet), so we plant rows directly via `state.audit.log()` with
//! `LogType::App` — same code path the future tracing layer will
//! use, just triggered by the test instead of by `tracing::info!`.

use serde_json::Value;
use think_watch_common::audit::{AuditEntry, LogType};
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

// ---------------------------------------------------------------------------
// access_logs
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn access_logs_endpoint_returns_recorded_http_traffic() {
    let app = TestApp::spawn_with_clickhouse().await;
    let con = admin_session(&app).await;

    // Drive one extra GET so we have at least one non-login row to
    // assert against.
    con.get("/api/auth/me").await.unwrap().assert_ok();

    // Wait for the access-log batches to flush.
    let ch = app.state.clickhouse.as_ref().unwrap();
    for _ in 0..200 {
        let n: u64 = ch
            .query("SELECT count() FROM access_logs")
            .fetch_one()
            .await
            .unwrap_or(0);
        if n >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let body: Value = con
        .get("/api/admin/access-logs?limit=20")
        .await
        .unwrap()
        .json()
        .unwrap();

    // Envelope shape — `items` + `total`. The React table reads
    // both by name; a rename white-screens the Access tab.
    let items = body["items"]
        .as_array()
        .expect("access_logs response must have an `items` array");
    assert!(
        body["total"].as_u64().is_some(),
        "access_logs response must have a numeric `total`: {body}"
    );
    assert!(
        !items.is_empty(),
        "access_logs has no items even after driving HTTP traffic: {body}"
    );

    // Every row exposes the columns the table renders.
    let row = &items[0];
    for k in [
        "id",
        "method",
        "path",
        "status_code",
        "latency_ms",
        "port",
        "created_at",
    ] {
        assert!(
            row.get(k).is_some(),
            "access_log row missing field {k}: {row}"
        );
    }
    assert!(
        row["method"].as_str().is_some(),
        "method should be a string: {row}"
    );
    assert!(
        row["status_code"].as_u64().is_some(),
        "status_code should be numeric: {row}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn access_logs_method_filter_narrows_results() {
    let app = TestApp::spawn_with_clickhouse().await;
    let con = admin_session(&app).await;

    // Drive both a GET and a POST so both buckets exist. Pick a
    // POST that doesn't invalidate the session (logout would 401
    // every subsequent admin call) — ws-ticket mints into Redis
    // and leaves the cookie alone.
    con.get("/api/auth/me").await.unwrap().assert_ok();
    con.post_empty("/api/dashboard/ws-ticket")
        .await
        .unwrap()
        .assert_ok();

    // Wait for both rows to land.
    let ch = app.state.clickhouse.as_ref().unwrap();
    for _ in 0..200 {
        let methods: Vec<String> = ch
            .query("SELECT toString(method) FROM access_logs")
            .fetch_all()
            .await
            .unwrap_or_default();
        if methods.contains(&"GET".into()) && methods.contains(&"POST".into()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let body: Value = con
        .get("/api/admin/access-logs?method=POST&limit=200")
        .await
        .unwrap()
        .json()
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "method=POST filter returned 0 rows even though we drove a POST: {body}"
    );
    for row in items {
        assert_eq!(
            row["method"], "POST",
            "method=POST filter must only return POST rows; got {row}"
        );
    }
}

// ---------------------------------------------------------------------------
// app_logs
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn app_logs_endpoint_returns_planted_runtime_entries() {
    let app = TestApp::spawn_with_clickhouse().await;
    let con = admin_session(&app).await;

    // Plant rows via the same audit path the future tracing→CH
    // layer will use. Field repurposing is documented inside
    // `flush_app` in `crates/common/src/audit.rs`:
    //   action → level
    //   resource → target
    //   resource_id → message
    //   detail → fields (JSON-stringified)
    //   user_agent → span
    let unique_marker = unique_name("apl-needle");
    let unique_target = unique_name("test::module");
    for level in ["info", "warn", "error"] {
        app.state.audit.log(
            AuditEntry::new(level)
                .log_type(LogType::App)
                .resource(&unique_target)
                .resource_id(format!("{level} log row containing {unique_marker}"))
                .detail(json!({"k": "v", "level_dup": level})),
        );
    }

    let ch = app.state.clickhouse.as_ref().unwrap();
    for _ in 0..200 {
        let n: u64 = ch
            .query("SELECT count() FROM app_logs WHERE target = ?")
            .bind(&unique_target)
            .fetch_one()
            .await
            .unwrap_or(0);
        if n >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Plain list — three rows we just planted should be among
    // them. Use a generous limit to dodge other test rows.
    let body: Value = con
        .get(&format!(
            "/api/admin/app-logs?target={unique_target}&limit=50"
        ))
        .await
        .unwrap()
        .json()
        .unwrap();
    let items = body["items"]
        .as_array()
        .expect("app_logs envelope must have `items`");
    assert!(
        body["total"].as_u64().is_some(),
        "app_logs response must have numeric `total`: {body}"
    );
    assert_eq!(
        items.len(),
        3,
        "should see exactly the three planted rows: {body}"
    );
    for k in ["id", "level", "target", "message", "created_at"] {
        assert!(
            items[0].get(k).is_some(),
            "app_log row missing field {k}: {}",
            items[0]
        );
    }
    // `fields` round-trips from JSON-stringified to a JSON value
    // shape (the response struct re-parses it into serde Value).
    assert!(
        items[0]["fields"].is_object(),
        "fields must deserialize to a JSON object on the response side: {}",
        items[0]
    );

    // Level filter — one of the three should match `error`.
    let body: Value = con
        .get(&format!(
            "/api/admin/app-logs?target={unique_target}&level=error"
        ))
        .await
        .unwrap()
        .json()
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "level=error filter should return only the error-level row: {body}"
    );
    assert_eq!(items[0]["level"], "error");

    // q substring search hits the unique marker we embedded in
    // every message.
    let body: Value = con
        .get(&format!(
            "/api/admin/app-logs?target={unique_target}&q={unique_marker}"
        ))
        .await
        .unwrap()
        .json()
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        3,
        "q=<marker> should hit all three rows that contain it: {body}"
    );
}
