//! Webhook outbox retry / backoff / DLQ tests.
//!
//! The happy-path delivery is already covered in `background_tasks.rs`.
//! Here we drive the failure path: a forwarder pointed at a
//! receiver that returns 500 enqueues a row in `webhook_outbox`,
//! the drain reschedules it with exponential backoff, and after
//! `MAX_OUTBOX_ATTEMPTS` (24) the row is finally retired.

use chrono::{Duration, Utc};
use serde_json::json;
use think_watch_common::audit::AuditEntry;
use think_watch_test_support::prelude::*;
use uuid::Uuid;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn install_forwarder(app: &TestApp, url: &str) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO log_forwarders (name, forwarder_type, config, log_types, enabled)
           VALUES ($1, 'webhook', $2, ARRAY['audit']::text[], true) RETURNING id"#,
    )
    .bind(unique_name("outbox"))
    .bind(serde_json::json!({"url": url}))
    .fetch_one(&app.db)
    .await
    .unwrap();
    app.state.audit.reload_forwarders().await;
    id
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn delivery_failure_enqueues_outbox_row_then_drain_redelivers() {
    let app = TestApp::spawn().await;

    // Receiver that 500s on the FIRST request, 200s afterwards.
    // wiremock's mock priority makes the more-specific (count-bounded)
    // matcher win; place the 500 mock first with `up_to_n_times(1)`.
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&receiver)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let forwarder_id = install_forwarder(&app, &receiver.uri()).await;

    // Emit an audit entry — first attempt 500s and lands in outbox.
    app.state
        .audit
        .log(AuditEntry::new("test.outbox_retry").resource("integration_test"));

    // Wait for the outbox row to appear.
    for _ in 0..50 {
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webhook_outbox WHERE forwarder_id = $1")
                .bind(forwarder_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // The outbox scheduler sets `next_attempt_at` to ~30s from now
    // on the first failure. Tests can't wait that long, so we
    // backdate it manually before driving drain_once.
    sqlx::query(
        "UPDATE webhook_outbox SET next_attempt_at = now() - interval '1 second' \
         WHERE forwarder_id = $1",
    )
    .bind(forwarder_id)
    .execute(&app.db)
    .await
    .unwrap();

    // First drain — receiver returns 200 this time, row should be gone.
    app.state.audit.drain_webhook_outbox_once().await.unwrap();

    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webhook_outbox WHERE forwarder_id = $1")
            .bind(forwarder_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "successful redelivery should delete the row");

    // Receiver saw both calls.
    assert!(
        receiver.received_requests().await.unwrap_or_default().len() >= 2,
        "expected initial + retry"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn drain_bumps_attempts_and_doubles_backoff_on_repeated_failures() {
    let app = TestApp::spawn().await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&receiver)
        .await;
    let forwarder_id = install_forwarder(&app, &receiver.uri()).await;

    app.state
        .audit
        .log(AuditEntry::new("test.outbox_backoff").resource("integration_test"));
    for _ in 0..50 {
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webhook_outbox WHERE forwarder_id = $1")
                .bind(forwarder_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Drive 3 drain passes, backdating before each so the row is
    // due. After each pass `attempts` should grow and the next
    // schedule gap should roughly double.
    let mut prior_gap_secs: Option<i64> = None;
    for pass in 1..=3 {
        sqlx::query(
            "UPDATE webhook_outbox SET next_attempt_at = now() - interval '1 second' \
             WHERE forwarder_id = $1",
        )
        .bind(forwarder_id)
        .execute(&app.db)
        .await
        .unwrap();

        app.state.audit.drain_webhook_outbox_once().await.unwrap();

        let row: (i32, chrono::DateTime<Utc>) = sqlx::query_as(
            "SELECT attempts, next_attempt_at FROM webhook_outbox WHERE forwarder_id = $1",
        )
        .bind(forwarder_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
        let attempts = row.0;
        let gap = (row.1 - Utc::now()).num_seconds();
        assert!(
            attempts >= pass,
            "attempts should bump per pass, got {attempts} after pass {pass}"
        );
        if let Some(prev) = prior_gap_secs {
            // Doubling — allow ±25% slack for clock drift.
            assert!(
                gap as f64 >= prev as f64 * 1.5,
                "gap should ~double each pass: prev={prev}s now={gap}s"
            );
        }
        prior_gap_secs = Some(gap);
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn drain_drops_row_after_max_attempts() {
    // After 24 failed attempts the drain should give up and the row
    // should disappear from the outbox so the table doesn't grow
    // forever on a chronically broken receiver.
    let app = TestApp::spawn().await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&receiver)
        .await;
    let forwarder_id = install_forwarder(&app, &receiver.uri()).await;

    // Plant a row directly with attempts = 23, due now — the next
    // drain attempt will be #24, which should retire it.
    let payload = json!({
        "id": Uuid::new_v4().to_string(),
        "log_type": "audit",
        "action": "test.outbox_max",
        "created_at": Utc::now().to_rfc3339(),
    });
    sqlx::query(
        r#"INSERT INTO webhook_outbox
            (forwarder_id, payload, attempts, next_attempt_at, last_error)
           VALUES ($1, $2, 23, $3, 'priming for cap test')"#,
    )
    .bind(forwarder_id)
    .bind(&payload)
    .bind(Utc::now() - Duration::seconds(1))
    .execute(&app.db)
    .await
    .unwrap();

    app.state.audit.drain_webhook_outbox_once().await.unwrap();

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_outbox WHERE forwarder_id = $1")
        .bind(forwarder_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(n, 0, "outbox row past MAX_OUTBOX_ATTEMPTS must be removed");
}
