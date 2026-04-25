//! Dashboard live-stats WebSocket schema contract.
//!
//! `GET /api/dashboard/ws` is a WebSocket that pushes a fresh
//! `DashboardLive` snapshot on connect and every 4 seconds after.
//! No integration test had ever opened it before this file: the
//! frontend's "live mode" works in dev and prod but a quiet schema
//! drift (rename `rpm_buckets`, drop `recent_logs`) would white-
//! screen the dashboard with no test signal.
//!
//! Auth is two-step:
//!   1. `POST /api/dashboard/ws-ticket` with a normal authenticated
//!      session → returns `{ticket: <opaque>}` valid for 30s.
//!   2. Open `ws://…/api/dashboard/ws?ticket=<opaque>`. Handler
//!      atomically GETDELs the ticket from Redis; missing ticket =
//!      4xx, replay = 4xx.
//!
//! What this file pins:
//!   - The auth two-step: missing ticket is rejected, a valid one
//!     opens the channel.
//!   - The first-push payload is a JSON object containing all four
//!     `DashboardLive` fields (providers / rpm_buckets / recent_logs
//!     / max_rpm_limit) — the dashboard reads them by name.
//!   - `rpm_buckets` is a 30-element array (= last 30 minutes).
//!   - Single-use semantics: redeeming the same ticket twice fails
//!     the second time.

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use think_watch_test_support::prelude::*;
use tokio_tungstenite::tungstenite::Message;

async fn admin_ws_ticket(app: &TestApp) -> (TestClient, String) {
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let body: Value = con
        .post_empty("/api/dashboard/ws-ticket")
        .await
        .unwrap()
        .json()
        .unwrap();
    let ticket = body["ticket"]
        .as_str()
        .expect("ws-ticket response must carry `ticket`")
        .to_string();
    (con, ticket)
}

fn ws_url(console_url: &str, ticket: &str) -> String {
    // Console URL is `http://127.0.0.1:PORT`; tungstenite wants `ws://…`.
    let base = console_url
        .strip_prefix("http://")
        .map(|s| format!("ws://{s}"))
        .unwrap_or_else(|| console_url.to_string());
    format!("{base}/api/dashboard/ws?ticket={ticket}")
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn ws_first_push_carries_full_dashboardlive_envelope() {
    let app = TestApp::spawn().await;
    let (_con, ticket) = admin_ws_ticket(&app).await;

    let (mut socket, _resp) = tokio_tungstenite::connect_async(ws_url(&app.console_url, &ticket))
        .await
        .expect("WS upgrade failed");

    // The handler pushes an initial snapshot synchronously after
    // accepting the connection. Cap the wait so a regression that
    // drops the initial push (and only relies on the 4s ticker)
    // fails fast.
    let msg = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
        .await
        .expect("first WS frame did not arrive within 10s")
        .expect("WS stream ended before first frame")
        .expect("WS frame error");

    let text = match msg {
        Message::Text(t) => t,
        Message::Binary(b) => String::from_utf8(b).expect("binary frame is UTF-8 JSON"),
        other => panic!("unexpected WS frame: {other:?}"),
    };

    let body: Value = serde_json::from_str(&text).expect("first push must be JSON");

    // Pin the DashboardLive envelope. Each field is consumed by name
    // from the React layer — a silent rename here white-screens the
    // dashboard without a test signal.
    for k in ["providers", "rpm_buckets", "recent_logs", "max_rpm_limit"] {
        assert!(
            body.get(k).is_some(),
            "DashboardLive missing field {k}: {body}"
        );
    }

    let buckets = body["rpm_buckets"]
        .as_array()
        .expect("rpm_buckets must be an array");
    assert_eq!(
        buckets.len(),
        30,
        "rpm_buckets is a fixed 30-minute window (one entry per minute)"
    );
    assert!(
        buckets.iter().all(|v| v.is_number()),
        "rpm_buckets entries must be u64-shaped numbers: {buckets:?}"
    );

    assert!(
        body["providers"].is_array(),
        "providers must be an array (possibly empty on a fresh deploy)"
    );
    assert!(
        body["recent_logs"].is_array(),
        "recent_logs must be an array (possibly empty on a fresh deploy)"
    );
    // max_rpm_limit is Option<i32> — null on no api keys, number otherwise.
    assert!(
        body["max_rpm_limit"].is_null() || body["max_rpm_limit"].is_number(),
        "max_rpm_limit must be null OR a number, got {}",
        body["max_rpm_limit"]
    );

    let _ = socket.send(Message::Close(None)).await;
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn ws_rejects_connection_with_missing_ticket() {
    // Without a ticket query param the handler must reject the
    // upgrade (or close immediately after accepting). Otherwise an
    // unauthenticated client would receive sensitive data — provider
    // health, recent log rows including user_email, etc.
    let app = TestApp::spawn().await;
    let url = format!(
        "{}/api/dashboard/ws",
        app.console_url.replace("http://", "ws://")
    );
    let result = tokio_tungstenite::connect_async(&url).await;
    match result {
        Err(_) => {} // upgrade rejected — ideal
        Ok((mut socket, _)) => {
            // Some servers accept the upgrade then close immediately.
            // That's also acceptable as long as no payload arrives.
            let msg =
                tokio::time::timeout(std::time::Duration::from_millis(500), socket.next()).await;
            // Either the timeout fires (no frame), or we get a Close
            // frame, or the stream ended. Anything else is a leak.
            match msg {
                Err(_) => {}
                Ok(None) => {}
                Ok(Some(Ok(Message::Close(_)))) => {}
                Ok(Some(other)) => {
                    panic!("unticketed connection received a payload — auth bypass? {other:?}")
                }
            }
        }
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn ws_ticket_is_single_use() {
    // Reusing the same ticket on a second connection must fail. The
    // ticket consumes via Redis GETDEL on the WS handler; a leaked
    // ticket reused later would otherwise be a free re-auth bypass.
    let app = TestApp::spawn().await;
    let (_con, ticket) = admin_ws_ticket(&app).await;

    // First connection consumes the ticket.
    let (mut first, _) = tokio_tungstenite::connect_async(ws_url(&app.console_url, &ticket))
        .await
        .expect("first WS open should succeed");
    // Drain initial snapshot to confirm the channel actually opened.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), first.next()).await;
    let _ = first.send(Message::Close(None)).await;

    // Second connection with the SAME ticket: either the handshake
    // fails or the server closes immediately without a payload.
    let result = tokio_tungstenite::connect_async(ws_url(&app.console_url, &ticket)).await;
    match result {
        Err(_) => {}
        Ok((mut socket, _)) => {
            let msg =
                tokio::time::timeout(std::time::Duration::from_millis(500), socket.next()).await;
            match msg {
                Err(_) | Ok(None) | Ok(Some(Ok(Message::Close(_)))) => {}
                Ok(Some(other)) => {
                    panic!("ticket replay yielded a snapshot — single-use broken: {other:?}")
                }
            }
        }
    }
}
