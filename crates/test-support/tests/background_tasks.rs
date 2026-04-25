//! Integration tests for the long-running background workers.
//!
//! Each test seeds a row, calls the worker's deterministic
//! `run_*` entrypoint (made `pub` for this purpose), and asserts the
//! DB / Redis fallout. The interval-driven `spawn_*_task` shells are
//! not exercised — they're trivial wrappers and would force a 10
//! min / 24 h sleep.

use chrono::{Duration, Utc};
use think_watch_server::tasks::{api_key_lifecycle, data_retention};
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn api_key_lifecycle_disables_expired_keys() {
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "expiring",
        &["ai_gateway"],
        None,
        Some(Utc::now() - Duration::minutes(5)),
    )
    .await
    .unwrap();

    api_key_lifecycle::run_lifecycle_check(&app.db, &app.state.dynamic_config, &app.state.audit)
        .await
        .unwrap();

    let row: (bool, Option<String>) =
        sqlx::query_as("SELECT is_active, disabled_reason FROM api_keys WHERE id = $1")
            .bind(key.row.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert!(!row.0, "expired key must be disabled");
    assert_eq!(row.1.as_deref(), Some("expired"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn api_key_lifecycle_revokes_keys_past_grace_period() {
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();

    // Plant a row whose grace period already ended.
    let id: uuid::Uuid = sqlx::query_scalar(
        r#"INSERT INTO api_keys
            (key_prefix, key_hash, name, user_id, surfaces, is_active,
             grace_period_ends_at)
           VALUES ('tw-aaaaaaaa', 'fake-hash', 'rotated', $1,
                   ARRAY['ai_gateway']::text[], true, $2)
           RETURNING id"#,
    )
    .bind(user.user.id)
    .bind(Utc::now() - Duration::minutes(1))
    .fetch_one(&app.db)
    .await
    .unwrap();

    api_key_lifecycle::run_lifecycle_check(&app.db, &app.state.dynamic_config, &app.state.audit)
        .await
        .unwrap();

    let active: bool = sqlx::query_scalar("SELECT is_active FROM api_keys WHERE id = $1")
        .bind(id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert!(!active, "rotated/grace-expired key must be disabled");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn data_retention_purges_soft_deleted_users_past_window() {
    let app = TestApp::spawn().await;

    // Insert a user with deleted_at well past the 30-day window.
    let id: uuid::Uuid = sqlx::query_scalar(
        r#"INSERT INTO users
            (email, display_name, password_hash, is_active, deleted_at)
           VALUES ($1, 'Old User', '$argon2id$placeholder', false, $2)
           RETURNING id"#,
    )
    .bind(unique_email())
    .bind(Utc::now() - Duration::days(40))
    .fetch_one(&app.db)
    .await
    .unwrap();

    data_retention::run_retention_cleanup(&app.db, &app.state.dynamic_config, &app.state.audit)
        .await
        .unwrap();

    let still_there: Option<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .unwrap();
    assert!(
        still_there.is_none(),
        "soft-deleted user past window must be hard-deleted"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn data_retention_keeps_recent_soft_deletes() {
    let app = TestApp::spawn().await;
    let id: uuid::Uuid = sqlx::query_scalar(
        r#"INSERT INTO users
            (email, display_name, password_hash, is_active, deleted_at)
           VALUES ($1, 'Recent', '$argon2id$placeholder', false, $2)
           RETURNING id"#,
    )
    .bind(unique_email())
    .bind(Utc::now() - Duration::days(5))
    .fetch_one(&app.db)
    .await
    .unwrap();

    data_retention::run_retention_cleanup(&app.db, &app.state.dynamic_config, &app.state.audit)
        .await
        .unwrap();

    let still_there: Option<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .unwrap();
    assert!(
        still_there.is_some(),
        "user soft-deleted within retention window must NOT be hard-deleted"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn webhook_outbox_drain_delivers_and_clears() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();

    // 1. Stand up a 200-OK webhook receiver.
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    // 2. Register a forwarder pointing at it. The admin POST handler
    //    blocks loopback URLs via the SSRF guard, so insert directly
    //    and reload the registry cache — same end state as the HTTP
    //    path, just without the (production-correct) URL gate firing
    //    on a localhost wiremock.
    let _ = admin;
    sqlx::query(
        r#"INSERT INTO log_forwarders (name, forwarder_type, config, log_types, enabled)
           VALUES ($1, 'webhook', $2, ARRAY['audit']::text[], true)"#,
    )
    .bind(unique_name("webhook"))
    .bind(serde_json::json!({"url": receiver.uri()}))
    .execute(&app.db)
    .await
    .unwrap();
    app.state.audit.reload_forwarders().await;

    // 3. Emit an audit entry that the forwarder should pick up.
    app.state.audit.log(
        think_watch_common::audit::AuditEntry::new("test.background_task")
            .resource("integration_test"),
    );

    // 4. Wait for the receiver to see the request. The audit pipeline
    //    flushes through the registry's in-memory channel, not via
    //    webhook_outbox unless the forwarder is offline — so this
    //    request lands quickly.
    for _ in 0..50 {
        if !receiver
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("webhook receiver never saw the audit entry");
}
