//! Integration tests for the limits-bulk admin endpoints. Six
//! handlers, zero coverage before this file: a slip in their auth
//! gate or per-row outcome accounting could let an admin
//! accidentally apply (or fail to undo) overrides across many
//! subjects.
//!
//! Each test uses a super-admin session — the per-subject scope
//! check inside the handlers exercises authorisation at the row
//! level, not just at the top of the request.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use uuid::Uuid;

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

/// Three users + one api_key, in a Vec the bulk handlers can target.
async fn seed_subjects(app: &TestApp) -> (Vec<Uuid>, Uuid) {
    let mut users = Vec::with_capacity(3);
    for _ in 0..3 {
        let u = fixtures::create_random_user(&app.db).await.unwrap();
        users.push(u.user.id);
    }
    let api_key_owner = fixtures::create_random_user(&app.db).await.unwrap();
    let key = fixtures::create_api_key(
        &app.db,
        api_key_owner.user.id,
        "bulk-target",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    (users, key.row.id)
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn bulk_apply_rule_writes_one_row_per_subject() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let (users, _) = seed_subjects(&app).await;

    let body: Value = con
        .post(
            "/api/admin/limits/bulk/rules",
            json!({
                "targets": users.iter().map(|id| json!({"kind": "user", "id": id.to_string()})).collect::<Vec<_>>(),
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 99,
                "expires_at": chrono::Utc::now() + chrono::Duration::days(7),
                "reason": "bulk override test"
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["success_count"].as_u64().unwrap(), 3);
    assert_eq!(body["error_count"].as_u64().unwrap(), 0);

    // Each subject got exactly one rate_limit_rules row.
    for uid in &users {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM rate_limit_rules WHERE subject_kind = 'user' AND subject_id = $1",
        )
        .bind(uid)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(n, 1, "user {uid} should have exactly one rule");
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn bulk_apply_rule_rejects_past_expires_at() {
    // Override-meta validator pre-flights the spec ONCE before the
    // per-row loop, so a bogus `expires_at` must short-circuit
    // with 400 and leave the DB untouched.
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let (users, _) = seed_subjects(&app).await;

    let resp = con
        .post(
            "/api/admin/limits/bulk/rules",
            json!({
                "targets": users.iter().map(|id| json!({"kind": "user", "id": id.to_string()})).collect::<Vec<_>>(),
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 99,
                // 1h in the past — must trip "expires_at must be in the future"
                "expires_at": chrono::Utc::now() - chrono::Duration::hours(1),
                "reason": "stale override that should be rejected"
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status.as_u16(),
        400,
        "expected 400, got {}",
        resp.status
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rate_limit_rules WHERE subject_kind = 'user' AND subject_id = ANY($1)",
    )
    .bind(&users)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(n, 0, "no rules should have been persisted");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn bulk_apply_cap_writes_budget_rows() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let (users, _) = seed_subjects(&app).await;

    let body: Value = con
        .post(
            "/api/admin/limits/bulk/budgets",
            json!({
                "targets": users.iter().map(|id| json!({"kind": "user", "id": id.to_string()})).collect::<Vec<_>>(),
                "period": "daily",
                "limit_tokens": 5000,
                "expires_at": chrono::Utc::now() + chrono::Duration::days(30),
                "reason": "monthly bulk cap test override"
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["success_count"].as_u64().unwrap(), 3);

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM budget_caps WHERE subject_kind = 'user' AND subject_id = ANY($1)",
    )
    .bind(&users)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(n, 3);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn bulk_disable_then_delete_rules_round_trip() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let (users, _) = seed_subjects(&app).await;

    // Seed one rule per user directly.
    let mut rule_ids = Vec::new();
    for uid in &users {
        let id = fixtures::create_rate_limit_rule(
            &app.db,
            "user",
            *uid,
            "ai_gateway",
            "requests",
            60,
            100,
        )
        .await
        .unwrap();
        rule_ids.push(id);
    }

    // Disable all 3 in one call.
    let body: Value = con
        .post(
            "/api/admin/limits/bulk/rules/disable",
            json!({"ids": rule_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["success_count"].as_u64().unwrap(), 3);

    let disabled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rate_limit_rules WHERE id = ANY($1) AND enabled = false",
    )
    .bind(&rule_ids)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(disabled, 3);

    // Then bulk-delete.
    let body: Value = con
        .post(
            "/api/admin/limits/bulk/rules/delete",
            json!({"ids": rule_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["success_count"].as_u64().unwrap(), 3);

    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM rate_limit_rules WHERE id = ANY($1)")
            .bind(&rule_ids)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn bulk_disable_then_delete_caps_round_trip() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let (users, _) = seed_subjects(&app).await;

    let mut cap_ids = Vec::new();
    for uid in &users {
        let id = fixtures::create_budget_cap(&app.db, "user", *uid, "daily", 5000)
            .await
            .unwrap();
        cap_ids.push(id);
    }

    let body: Value = con
        .post(
            "/api/admin/limits/bulk/budgets/disable",
            json!({"ids": cap_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["success_count"].as_u64().unwrap(), 3);

    let disabled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM budget_caps WHERE id = ANY($1) AND enabled = false",
    )
    .bind(&cap_ids)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(disabled, 3);

    let body: Value = con
        .post(
            "/api/admin/limits/bulk/budgets/delete",
            json!({"ids": cap_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["success_count"].as_u64().unwrap(), 3);

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM budget_caps WHERE id = ANY($1)")
        .bind(&cap_ids)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn bulk_endpoints_require_rate_limits_write_permission() {
    // A developer (no `rate_limits:write`) hitting any bulk
    // endpoint must be rejected before any side-effect.
    let app = TestApp::spawn().await;
    let dev = fixtures::create_random_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": dev.user.email, "password": dev.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    let (users, _) = seed_subjects(&app).await;

    let resp = con
        .post(
            "/api/admin/limits/bulk/rules",
            json!({
                "targets": users.iter().map(|id| json!({"kind": "user", "id": id.to_string()})).collect::<Vec<_>>(),
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 99,
            }),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "developer must not be allowed to bulk-apply rules, got {}",
        resp.status
    );

    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM rate_limit_rules WHERE subject_id = ANY($1)")
            .bind(&users)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(n, 0, "no rule rows should have been persisted");
}
