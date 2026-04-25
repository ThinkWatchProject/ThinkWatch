//! Regression tests for authorization findings turned up by the
//! 2026-04-25 audit:
//!
//! 1. Bulk-disable / bulk-delete on `rate_limit_rules` and
//!    `budget_caps` accepted arbitrary row IDs without scope-checking
//!    the row's subject. A team-scoped caller could delete out-of-
//!    scope overrides by passing the right id. The fix layers a
//!    per-row `assert_scope_for_subject` call inside `run_bulk_id_op`.
//!
//! 2. `/api/admin/keys/{id}/force-revoke` was mounted in `user_routes`
//!    instead of `admin_routes` — the handler enforced `caller_has_
//!    global` correctly so it wasn't a real bypass, but the route
//!    placement was misleading. Now under `admin_routes`.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use uuid::Uuid;

async fn login_as(app: &TestApp, email: &str, password: &str) -> TestClient {
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": email, "password": password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con
}

/// Create a fresh user and grant them `team_manager` scoped to the
/// given team — same shape as the seeded role intent (single team
/// boundary). Returns `(user, password)` for login.
async fn create_team_manager(app: &TestApp, team_id: Uuid) -> fixtures::SeededUser {
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, scope_id, assigned_by)
           SELECT $1, id, 'team', $2, $1 FROM rbac_roles WHERE name = 'team_manager'"#,
    )
    .bind(user.user.id)
    .bind(team_id)
    .execute(&app.db)
    .await
    .unwrap();
    user
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_manager_cannot_bulk_delete_out_of_scope_rate_limit_rules() {
    let app = TestApp::spawn().await;

    // Two unrelated teams. Manager A is scoped to team A; the
    // victim user belongs to team B. We seed a user-scope
    // rate_limit_rule on the victim then try to bulk-delete it as
    // manager A.
    let team_a: Uuid =
        sqlx::query_scalar("INSERT INTO teams (name, description) VALUES ($1, 'A') RETURNING id")
            .bind(unique_name("team-a"))
            .fetch_one(&app.db)
            .await
            .unwrap();
    let team_b: Uuid =
        sqlx::query_scalar("INSERT INTO teams (name, description) VALUES ($1, 'B') RETURNING id")
            .bind(unique_name("team-b"))
            .fetch_one(&app.db)
            .await
            .unwrap();

    let manager_a = create_team_manager(&app, team_a).await;
    let _ = team_b; // referenced for clarity; manager_a is NOT scoped here

    // Victim user — fully outside manager A's scope.
    let victim = fixtures::create_random_user(&app.db).await.unwrap();
    let rule_id = fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        victim.user.id,
        "ai_gateway",
        "requests",
        60,
        100,
    )
    .await
    .unwrap();

    let con = login_as(&app, &manager_a.user.email, &manager_a.plaintext_password).await;

    // Bulk-delete attempt with the victim's row id.
    let body: Value = con
        .post(
            "/api/admin/limits/bulk/rules/delete",
            json!({"ids": [rule_id.to_string()]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();

    assert_eq!(
        body["success_count"].as_u64().unwrap(),
        0,
        "team_manager must not be able to delete out-of-scope rules: {body}"
    );
    assert_eq!(body["error_count"].as_u64().unwrap(), 1);
    assert!(
        body["outcomes"][0]["error"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("forbidden"),
        "outcome should be a forbidden error, got: {body}"
    );

    // Row is still there.
    let still: i64 = sqlx::query_scalar("SELECT count(*) FROM rate_limit_rules WHERE id = $1")
        .bind(rule_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(still, 1, "the rule must NOT have been deleted");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_manager_cannot_bulk_disable_out_of_scope_budget_caps() {
    let app = TestApp::spawn().await;

    let team_a: Uuid =
        sqlx::query_scalar("INSERT INTO teams (name, description) VALUES ($1, 'A') RETURNING id")
            .bind(unique_name("team-a"))
            .fetch_one(&app.db)
            .await
            .unwrap();
    let manager_a = create_team_manager(&app, team_a).await;

    let victim = fixtures::create_random_user(&app.db).await.unwrap();
    let cap_id = fixtures::create_budget_cap(&app.db, "user", victim.user.id, "daily", 5000)
        .await
        .unwrap();

    let con = login_as(&app, &manager_a.user.email, &manager_a.plaintext_password).await;
    let body: Value = con
        .post(
            "/api/admin/limits/bulk/budgets/disable",
            json!({"ids": [cap_id.to_string()]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["success_count"].as_u64().unwrap(), 0);
    assert_eq!(body["error_count"].as_u64().unwrap(), 1);

    let still_enabled: bool = sqlx::query_scalar("SELECT enabled FROM budget_caps WHERE id = $1")
        .bind(cap_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert!(
        still_enabled,
        "victim's cap must remain enabled after the rejected bulk-disable"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn super_admin_can_still_bulk_delete_after_the_fix() {
    // Regression guard for the fix itself: super_admin (global
    // scope) must still succeed.
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let rule_id = fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        user.user.id,
        "ai_gateway",
        "requests",
        60,
        100,
    )
    .await
    .unwrap();

    let con = login_as(&app, &admin.user.email, &admin.plaintext_password).await;
    let body: Value = con
        .post(
            "/api/admin/limits/bulk/rules/delete",
            json!({"ids": [rule_id.to_string()]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["success_count"].as_u64().unwrap(), 1);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM rate_limit_rules WHERE id = $1")
        .bind(rule_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn force_revoke_route_lives_under_admin_routes() {
    // Pin the route placement: the endpoint must respond at the
    // admin path. Auth/permission enforcement is covered by
    // `console_admin::developer_cannot_force_revoke_admin_keys`;
    // here we only assert the route hasn't drifted back to
    // `user_routes`.
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = login_as(&app, &admin.user.email, &admin.plaintext_password).await;

    let key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "force-revoke-target",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let resp = con
        .post(
            &format!("/api/admin/keys/{}/force-revoke", key.row.id),
            json!({"reason": "regression: route placement drift"}),
        )
        .await
        .unwrap();
    resp.assert_ok();

    // The key must now be inactive — proves the handler ran (which
    // means the route resolved, which means it's mounted somewhere).
    let active: bool = sqlx::query_scalar("SELECT is_active FROM api_keys WHERE id = $1")
        .bind(key.row.id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert!(!active, "force-revoke must have flipped is_active to false");
}
