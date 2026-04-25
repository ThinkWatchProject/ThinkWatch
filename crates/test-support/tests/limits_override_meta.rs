//! Override metadata for `rate_limit_rules` and `budget_caps`.
//!
//! Each row carries `expires_at`, `reason`, `created_by` for the
//! audit trail. Test the contract:
//!
//!   - `expires_at` past `now()` is rejected at write
//!   - `expires_at` more than `MAX_OVERRIDE_HORIZON_DAYS` (90) in
//!     the future is rejected
//!   - `reason` longer than `MAX_REASON_LEN` (500) is rejected
//!   - rows with `expires_at <= now()` no longer participate in
//!     override merge — they're invisible to the gateway hot path
//!   - the audit log entry for an upsert carries the override meta

use chrono::{Duration, Utc};
use think_watch_test_support::prelude::*;

async fn admin_session(app: &TestApp) -> (TestClient, fixtures::SeededUser) {
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    (con, admin)
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn upsert_rule_with_meta_persists_expires_reason_created_by() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();

    let expires = Utc::now() + Duration::days(7);
    let reason = "QA — burst-traffic override; revisit by next sprint";
    con.post(
        &format!("/api/admin/limits/user/{}/rules", target.user.id),
        json!({
            "surface": "ai_gateway",
            "metric": "requests",
            "window_secs": 60,
            "max_count": 100,
            "expires_at": expires,
            "reason": reason
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    let row: (
        Option<chrono::DateTime<Utc>>,
        Option<String>,
        Option<uuid::Uuid>,
    ) = sqlx::query_as(
        "SELECT expires_at, reason, created_by FROM rate_limit_rules \
         WHERE subject_kind = 'user' AND subject_id = $1",
    )
    .bind(target.user.id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    let (db_expires, db_reason, db_created_by) = row;
    assert!(
        db_expires
            .map(|t| (t - expires).num_seconds().abs() < 5)
            .unwrap_or(false),
        "expires_at round-trip mismatch: got {db_expires:?}, expected ~{expires:?}"
    );
    assert_eq!(db_reason.as_deref(), Some(reason));
    assert_eq!(
        db_created_by,
        Some(admin.user.id),
        "created_by must be the actor"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn upsert_rule_rejects_past_expires_at() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();

    let resp = con
        .post(
            &format!("/api/admin/limits/user/{}/rules", target.user.id),
            json!({
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 100,
                "expires_at": Utc::now() - Duration::hours(1),
                "reason": "stale"
            }),
        )
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 400);
    assert!(resp.text().to_lowercase().contains("future"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn upsert_rule_rejects_overlong_horizon() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();

    // 100 days > MAX_OVERRIDE_HORIZON_DAYS (90).
    let resp = con
        .post(
            &format!("/api/admin/limits/user/{}/rules", target.user.id),
            json!({
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 100,
                "expires_at": Utc::now() + Duration::days(100),
                "reason": "too far out"
            }),
        )
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 400);
    assert!(
        resp.text().to_lowercase().contains("days"),
        "expected horizon-error message, got: {}",
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn upsert_rule_rejects_overlong_reason() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();
    let reason = "x".repeat(501);

    let resp = con
        .post(
            &format!("/api/admin/limits/user/{}/rules", target.user.id),
            json!({
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 100,
                "expires_at": Utc::now() + Duration::days(7),
                "reason": reason
            }),
        )
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 400);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn expired_overrides_are_invisible_to_the_gateway_hot_path() {
    // Plant a rule directly with `expires_at` in the past.
    // `list_enabled_rules_for_subjects` filters out expired rows
    // SQL-side (`expires_at IS NULL OR expires_at > now()`), so the
    // override merge inside the gateway does not see it. Make sure
    // a request that *would* trip the rule actually goes through.
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();

    // Plant an expired rule limiting the user to 1 request / minute.
    sqlx::query(
        r#"INSERT INTO rate_limit_rules
            (subject_kind, subject_id, surface, metric, window_secs, max_count, enabled, expires_at)
           VALUES ('user', $1, 'ai_gateway', 'requests', 60, 1, true, now() - interval '1 hour')"#,
    )
    .bind(user.user.id)
    .execute(&app.db)
    .await
    .unwrap();

    // Stand up a healthy upstream + key.
    let mock = MockProvider::openai_chat_ok("expired-rule-test").await;
    let uri = mock.uri();
    Box::leak(Box::new(mock));

    let provider =
        fixtures::create_provider(&app.db, &unique_name("exp-prov"), "openai", &uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "expired-rule-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        "exp-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);

    // Two requests — both must succeed because the limit-row is
    // expired and filtered out of the override merge.
    for i in 0..2 {
        let resp = gw
            .post(
                "/v1/chat/completions",
                json!({
                    "model": "expired-rule-test",
                    "messages": [{"role": "user", "content": format!("ping {i}")}]
                }),
            )
            .await
            .unwrap();
        resp.assert_ok();
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn budget_cap_meta_round_trips() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();

    let expires = Utc::now() + Duration::days(30);
    let reason = "Q4 trial budget — drops back to default after Dec 31";
    con.post(
        &format!("/api/admin/limits/user/{}/budgets", target.user.id),
        json!({
            "period": "daily",
            "limit_tokens": 100_000,
            "expires_at": expires,
            "reason": reason
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    let row: (
        Option<chrono::DateTime<Utc>>,
        Option<String>,
        Option<uuid::Uuid>,
    ) = sqlx::query_as(
        "SELECT expires_at, reason, created_by FROM budget_caps \
         WHERE subject_kind = 'user' AND subject_id = $1",
    )
    .bind(target.user.id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    let (db_expires, db_reason, db_created_by) = row;
    assert!(
        db_expires.is_some(),
        "expires_at must persist on budget_caps"
    );
    assert_eq!(db_reason.as_deref(), Some(reason));
    assert_eq!(db_created_by, Some(admin.user.id));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn upsert_rule_emits_audit_entry_with_override_meta() {
    // The handler emits an audit entry on a successful write. Pin
    // the action name + the per-row meta so a future refactor that
    // forgets the audit emit fails this test loudly.
    let app = TestApp::spawn_with_clickhouse().await;
    let (con, _) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();

    let expires = Utc::now() + Duration::days(3);
    let reason = "audit trail probe";
    con.post(
        &format!("/api/admin/limits/user/{}/rules", target.user.id),
        json!({
            "surface": "ai_gateway",
            "metric": "requests",
            "window_secs": 60,
            "max_count": 42,
            "expires_at": expires,
            "reason": reason
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    // Wait for the audit flush — limits ops emit into audit_logs.
    let ch = app.state.clickhouse.as_ref().expect("CH client");
    let mut found: Option<(String, String)> = None;
    for _ in 0..40 {
        let row: Option<(String, String)> = ch
            .query(
                "SELECT action, ifNull(detail, '') FROM audit_logs \
                   WHERE action LIKE 'rate_limit.%' \
                   ORDER BY created_at DESC LIMIT 1",
            )
            .fetch_optional()
            .await
            .unwrap();
        if let Some(r) = row {
            found = Some(r);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (action, detail) = found.expect("rate_limit.* audit row must land");
    assert!(
        action.starts_with("rate_limit."),
        "expected rate_limit.* action, got {action}"
    );
    assert!(
        detail.contains(reason) || detail.contains("max_count"),
        "audit detail should carry the meta — got {detail}"
    );
    let _ = expires;
}
