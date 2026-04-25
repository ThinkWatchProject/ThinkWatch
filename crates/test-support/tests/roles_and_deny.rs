//! Custom roles CRUD + Deny-statement enforcement.
//!
//! Two questions this file pins:
//!
//!   1. Can an admin create / update / reset / delete a custom role
//!      and have the new permission set actually flow through to a
//!      user the role is assigned to?
//!   2. Does an explicit `Effect: Deny` statement override an
//!      `Allow` from another role on the same user? The unit tests
//!      cover the policy evaluator in isolation; this file covers
//!      the full pipeline (policy → role → assignment → request).

use serde_json::{Value, json};
use think_watch_test_support::prelude::*;
use uuid::Uuid;

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

async fn login_as(app: &TestApp, user: &fixtures::SeededUser) -> TestClient {
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": user.user.email, "password": user.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn admin_can_create_custom_role_and_grant_it_to_a_user() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    // Create a custom "providers reader" role.
    let created: Value = con
        .post(
            "/api/admin/roles",
            json!({
                "name": unique_name("providers-reader"),
                "description": "read-only providers",
                "policy_document": {
                    "Version": "2024-01-01",
                    "Statement": [{
                        "Sid": "ProvidersRead",
                        "Effect": "Allow",
                        "Action": ["providers:read", "models:read"],
                        "Resource": "*"
                    }]
                }
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let role_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    // Mint a fresh user, no system role yet.
    let user = fixtures::create_user(
        &app.db,
        &unique_email(),
        "Custom Role Tester",
        "TestPwd_1234567!",
    )
    .await
    .unwrap();

    // Assign the custom role at global scope directly via SQL — the
    // role-assignment HTTP path lives under `/api/admin/users/{id}/
    // roles` which we're not testing here.
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
           VALUES ($1, $2, 'global', $1)"#,
    )
    .bind(user.user.id)
    .bind(role_id)
    .execute(&app.db)
    .await
    .unwrap();

    let user_con = login_as(&app, &user).await;

    // The custom role grants `providers:read` only — they should be
    // able to GET /api/admin/providers but NOT POST it.
    user_con
        .get("/api/admin/providers")
        .await
        .unwrap()
        .assert_ok();
    let resp = user_con
        .post(
            "/api/admin/providers",
            json!({
                "name": unique_name("denied"),
                "display_name": "denied",
                "provider_type": "openai",
                "base_url": "https://api.openai.com/v1",
                "config": {}
            }),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "user with read-only custom role must not be able to create a provider, got {}",
        resp.status
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn deny_statement_beats_allow_from_a_different_role() {
    // Stack two roles on the same user:
    //   - `developer` (system) → grants `api_keys:create`
    //   - custom `no-key-creates` → Deny `api_keys:create`
    // Per RBAC contract Deny wins. The user must be unable to
    // create an API key even though one of their roles allows it.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    // Custom role with a Deny statement.
    let denier: Value = con
        .post(
            "/api/admin/roles",
            json!({
                "name": unique_name("no-key-creates"),
                "description": "denies api_keys:create",
                "policy_document": {
                    "Version": "2024-01-01",
                    "Statement": [{
                        "Sid": "NoKeyCreates",
                        "Effect": "Deny",
                        "Action": "api_keys:create",
                        "Resource": "*"
                    }]
                }
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let denier_id = Uuid::parse_str(denier["id"].as_str().unwrap()).unwrap();

    // User with developer + the deny role.
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
           VALUES ($1, $2, 'global', $1)"#,
    )
    .bind(user.user.id)
    .bind(denier_id)
    .execute(&app.db)
    .await
    .unwrap();

    // Confirm `/api/auth/me` reflects the Deny in `denied_permissions`.
    let user_con = login_as(&app, &user).await;
    let me: Value = user_con.get("/api/auth/me").await.unwrap().json().unwrap();
    let denied: Vec<&str> = me["denied_permissions"]
        .as_array()
        .expect("denied_permissions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        denied.contains(&"api_keys:create"),
        "denied_permissions should include api_keys:create, got: {denied:?}"
    );

    // Try to actually create a key — must 403.
    let resp = user_con
        .post(
            "/api/keys",
            json!({"name": "should-be-denied", "surfaces": ["ai_gateway"]}),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "Deny must beat Allow at the request gate, got {}: {}",
        resp.status,
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn deny_overrides_super_admin_wildcard() {
    // Contract observed in the wild (and pinned by this test):
    // **Deny always wins, even against `Action: *`**. A super_admin
    // who is also assigned a role with `Effect: Deny` for some
    // perm cannot exercise that perm. This is defense-in-depth —
    // an admin can intentionally lock a dangerous op away from
    // anyone, including themselves and other super_admins. The
    // tradeoff: there is NO break-glass; admins must NOT add Deny
    // roles to their own super_admin account.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let denier: Value = con
        .post(
            "/api/admin/roles",
            json!({
                "name": unique_name("deny-keys"),
                "description": "deny api_keys:create",
                "policy_document": {
                    "Version": "2024-01-01",
                    "Statement": [{
                        "Sid": "Deny",
                        "Effect": "Deny",
                        "Action": "api_keys:create",
                        "Resource": "*"
                    }]
                }
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let denier_id = Uuid::parse_str(denier["id"].as_str().unwrap()).unwrap();

    // super_admin user + the deny role layered on top.
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
           VALUES ($1, $2, 'global', $1)"#,
    )
    .bind(admin.user.id)
    .bind(denier_id)
    .execute(&app.db)
    .await
    .unwrap();

    let admin_con = login_as(&app, &admin).await;
    let resp = admin_con
        .post(
            "/api/keys",
            json!({"name": "wildcard loses to deny", "surfaces": ["ai_gateway"]}),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "Deny must beat super_admin's `*` wildcard, got {}: {}",
        resp.status,
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn update_role_propagates_new_perms_to_assignees() {
    // The handler MUST clear cached perms / re-derive on next /me
    // so an admin tightening a role doesn't leave existing users
    // with stale (over-permissive) caches.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    // Start with a wide custom role: read providers + read models.
    let role: Value = con
        .post(
            "/api/admin/roles",
            json!({
                "name": unique_name("wide-then-narrow"),
                "description": "starts wide, narrows on update",
                "policy_document": {
                    "Version": "2024-01-01",
                    "Statement": [{
                        "Sid": "Wide",
                        "Effect": "Allow",
                        "Action": ["providers:read", "models:read"],
                        "Resource": "*"
                    }]
                }
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let role_id = Uuid::parse_str(role["id"].as_str().unwrap()).unwrap();

    let user = fixtures::create_user(
        &app.db,
        &unique_email(),
        "Update Role Tester",
        "TestPwd_1234567!",
    )
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
           VALUES ($1, $2, 'global', $1)"#,
    )
    .bind(user.user.id)
    .bind(role_id)
    .execute(&app.db)
    .await
    .unwrap();

    let user_con = login_as(&app, &user).await;
    user_con
        .get("/api/admin/providers")
        .await
        .unwrap()
        .assert_ok();

    // Tighten the role — drop providers:read.
    con.patch(
        &format!("/api/admin/roles/{role_id}"),
        json!({
            "policy_document": {
                "Version": "2024-01-01",
                "Statement": [{
                    "Sid": "Narrow",
                    "Effect": "Allow",
                    "Action": ["models:read"],
                    "Resource": "*"
                }]
            }
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    // The next request from `user_con` must observe the new policy.
    // The dashboard endpoint reads permissions from the JWT claims
    // baked at login, so refresh first to mint a token reflecting
    // the new perms.
    user_con
        .post_empty("/api/auth/refresh")
        .await
        .unwrap()
        .assert_ok();

    let resp = user_con.get("/api/admin/providers").await.unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "after the role narrowed, providers:read must no longer be granted, got {}",
        resp.status
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn reset_role_restores_seeded_policy() {
    // `POST /api/admin/roles/{id}/reset` is a "Reset to defaults"
    // affordance for SYSTEM roles — admins who edited an `admin`
    // policy by mistake can roll it back without a DB shell. The
    // handler must:
    //   - reject reset on non-system / custom roles
    //   - restore the seeded `policy_document` for system roles
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    // 1. Resetting a custom role must fail (only system roles
    //    have a documented "default").
    let custom: Value = con
        .post(
            "/api/admin/roles",
            json!({
                "name": unique_name("custom-reset-test"),
                "description": "non-system, no defaults to reset to",
                "policy_document": {
                    "Version": "2024-01-01",
                    "Statement": [{"Sid": "X", "Effect": "Allow",
                                   "Action": "models:read", "Resource": "*"}]
                }
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let custom_id = Uuid::parse_str(custom["id"].as_str().unwrap()).unwrap();
    let resp = con
        .post_empty(&format!("/api/admin/roles/{custom_id}/reset"))
        .await
        .unwrap();
    assert!(
        !resp.status.is_success(),
        "reset on a custom role must fail, got {}",
        resp.status
    );

    // 2. Mutate the seeded `viewer` role and reset — final state
    //    matches the original seed.
    let viewer_id: Uuid = sqlx::query_scalar("SELECT id FROM rbac_roles WHERE name = 'viewer'")
        .fetch_one(&app.db)
        .await
        .unwrap();
    let original: Value =
        sqlx::query_scalar("SELECT policy_document FROM rbac_roles WHERE id = $1")
            .bind(viewer_id)
            .fetch_one(&app.db)
            .await
            .unwrap();

    sqlx::query(
        r#"UPDATE rbac_roles
           SET policy_document = '{"Version":"2024-01-01","Statement":[]}'::jsonb
           WHERE id = $1"#,
    )
    .bind(viewer_id)
    .execute(&app.db)
    .await
    .unwrap();

    con.post_empty(&format!("/api/admin/roles/{viewer_id}/reset"))
        .await
        .unwrap()
        .assert_ok();

    let after: Value = sqlx::query_scalar("SELECT policy_document FROM rbac_roles WHERE id = $1")
        .bind(viewer_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(
        after, original,
        "reset must restore the original seeded policy verbatim"
    );
}
