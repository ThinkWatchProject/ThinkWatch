//! Horizontal authorization matrix — same-role cross-tenant
//! access attempts.
//!
//! "Vertical" tests in `authz_vertical.rs` cover privilege
//! escalation (lower role → admin endpoint). This file covers the
//! orthogonal axis: peers at the same role tier trying to read or
//! mutate each other's data. Two patterns:
//!
//!   1. Developer Alice → Developer Bob's `/api/keys/{id}`
//!      (GET / PATCH / DELETE / rotate, force-revoke).
//!   2. team_manager scoped to Team A → Team B's resources
//!      (PATCH / DELETE team, members CRUD, role assignments,
//!      limits inside Team B's user).
//!
//! All tests boot one TestApp and reuse role clients to stay
//! under the per-IP login rate limit (30 / minute).

use serde_json::Value;
use think_watch_test_support::prelude::*;
use uuid::Uuid;

async fn login(app: &TestApp, user: &fixtures::SeededUser) -> TestClient {
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

// ---------------------------------------------------------------------------
// Pattern 1 — developer Alice cannot touch developer Bob's API keys
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn developer_cannot_read_anothers_api_key() {
    let app = TestApp::spawn().await;
    let alice = fixtures::create_random_user(&app.db).await.unwrap();
    let bob = fixtures::create_random_user(&app.db).await.unwrap();
    let bob_key = fixtures::create_api_key(
        &app.db,
        bob.user.id,
        "bobs-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let con = login(&app, &alice).await;
    let resp = con
        .get(&format!("/api/keys/{}", bob_key.row.id))
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403 | 404),
        "Alice must not read Bob's key, got {}: {}",
        resp.status,
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn developer_cannot_patch_anothers_api_key() {
    let app = TestApp::spawn().await;
    let alice = fixtures::create_random_user(&app.db).await.unwrap();
    let bob = fixtures::create_random_user(&app.db).await.unwrap();
    let bob_key = fixtures::create_api_key(
        &app.db,
        bob.user.id,
        "bobs-key-patch",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let con = login(&app, &alice).await;
    let resp = con
        .patch(
            &format!("/api/keys/{}", bob_key.row.id),
            json!({"name": "alice-renamed-bobs-key"}),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403 | 404),
        "Alice must not rename Bob's key, got {}",
        resp.status
    );

    let still: String = sqlx::query_scalar("SELECT name FROM api_keys WHERE id = $1")
        .bind(bob_key.row.id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(still, "bobs-key-patch", "Bob's key name was mutated");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn developer_cannot_delete_anothers_api_key() {
    let app = TestApp::spawn().await;
    let alice = fixtures::create_random_user(&app.db).await.unwrap();
    let bob = fixtures::create_random_user(&app.db).await.unwrap();
    let bob_key = fixtures::create_api_key(
        &app.db,
        bob.user.id,
        "bobs-key-delete",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let con = login(&app, &alice).await;
    let resp = con
        .delete(&format!("/api/keys/{}", bob_key.row.id))
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403 | 404),
        "Alice must not delete Bob's key, got {}",
        resp.status
    );

    let active: bool = sqlx::query_scalar("SELECT is_active FROM api_keys WHERE id = $1")
        .bind(bob_key.row.id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert!(active, "Bob's key was deactivated by Alice");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn developer_cannot_rotate_anothers_api_key() {
    let app = TestApp::spawn().await;
    let alice = fixtures::create_random_user(&app.db).await.unwrap();
    let bob = fixtures::create_random_user(&app.db).await.unwrap();
    let bob_key = fixtures::create_api_key(
        &app.db,
        bob.user.id,
        "bobs-key-rotate",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let con = login(&app, &alice).await;
    let resp = con
        .post_empty(&format!("/api/keys/{}/rotate", bob_key.row.id))
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403 | 404),
        "Alice must not rotate Bob's key, got {}",
        resp.status
    );

    // Hash must be unchanged.
    let hash: String = sqlx::query_scalar("SELECT key_hash FROM api_keys WHERE id = $1")
        .bind(bob_key.row.id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(hash, bob_key.row.key_hash, "Bob's key hash was rotated");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn developer_can_still_read_own_api_key() {
    // Regression guard for the cross-tenant fixes — owners must
    // continue to access their own resources.
    let app = TestApp::spawn().await;
    let alice = fixtures::create_random_user(&app.db).await.unwrap();
    let alice_key = fixtures::create_api_key(
        &app.db,
        alice.user.id,
        "alice-own-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let con = login(&app, &alice).await;
    let body: Value = con
        .get(&format!("/api/keys/{}", alice_key.row.id))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["name"].as_str(), Some("alice-own-key"));
}

// ---------------------------------------------------------------------------
// Pattern 2 — team_manager scoped to Team A cannot touch Team B
// ---------------------------------------------------------------------------

async fn make_team(db: &sqlx::PgPool, name_prefix: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO teams (name, description) VALUES ($1, 'matrix') RETURNING id")
        .bind(unique_name(name_prefix))
        .fetch_one(db)
        .await
        .unwrap()
}

async fn add_to_team(db: &sqlx::PgPool, team_id: Uuid, user_id: Uuid) {
    sqlx::query("INSERT INTO team_members (team_id, user_id) VALUES ($1, $2)")
        .bind(team_id)
        .bind(user_id)
        .execute(db)
        .await
        .unwrap();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_manager_a_cannot_patch_team_b() {
    let app = TestApp::spawn().await;
    let team_a = make_team(&app.db, "team-a").await;
    let team_b = make_team(&app.db, "team-b").await;
    let manager_a = fixtures::create_user_with_role(&app.db, "team_manager", "team", Some(team_a))
        .await
        .unwrap();

    let con = login(&app, &manager_a).await;
    let resp = con
        .patch(
            &format!("/api/admin/teams/{team_b}"),
            json!({"description": "manager A trying to rename team B"}),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "manager A must not patch team B, got {}",
        resp.status
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_manager_a_cannot_add_member_to_team_b() {
    let app = TestApp::spawn().await;
    let team_a = make_team(&app.db, "team-a").await;
    let team_b = make_team(&app.db, "team-b").await;
    let manager_a = fixtures::create_user_with_role(&app.db, "team_manager", "team", Some(team_a))
        .await
        .unwrap();
    let outsider = fixtures::create_random_user(&app.db).await.unwrap();

    let con = login(&app, &manager_a).await;
    let resp = con
        .post(
            &format!("/api/admin/teams/{team_b}/members"),
            json!({"user_id": outsider.user.id, "role": "member"}),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "manager A must not add members to team B, got {}",
        resp.status
    );

    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM team_members WHERE team_id = $1 AND user_id = $2")
            .bind(team_b)
            .bind(outsider.user.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(n, 0, "outsider must not have been added to team B");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_manager_a_cannot_remove_member_from_team_b() {
    let app = TestApp::spawn().await;
    let team_a = make_team(&app.db, "team-a").await;
    let team_b = make_team(&app.db, "team-b").await;
    let manager_a = fixtures::create_user_with_role(&app.db, "team_manager", "team", Some(team_a))
        .await
        .unwrap();
    let bob = fixtures::create_random_user(&app.db).await.unwrap();
    add_to_team(&app.db, team_b, bob.user.id).await;

    let con = login(&app, &manager_a).await;
    let resp = con
        .delete(&format!(
            "/api/admin/teams/{team_b}/members/{}",
            bob.user.id
        ))
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "manager A must not remove members from team B, got {}",
        resp.status
    );

    let still: i64 =
        sqlx::query_scalar("SELECT count(*) FROM team_members WHERE team_id = $1 AND user_id = $2")
            .bind(team_b)
            .bind(bob.user.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(still, 1, "Bob was removed from team B by manager A");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_manager_a_can_add_member_to_own_team() {
    // Regression — manager A keeps the canonical write inside their
    // own team. team_manager doesn't have `teams:update` (renaming
    // a team's catalog entry is an admin op), but they do hold
    // `team_members:write` for the team they're scoped to.
    let app = TestApp::spawn().await;
    let team_a = make_team(&app.db, "team-a-self").await;
    let manager_a = fixtures::create_user_with_role(&app.db, "team_manager", "team", Some(team_a))
        .await
        .unwrap();
    let new_member = fixtures::create_random_user(&app.db).await.unwrap();

    let con = login(&app, &manager_a).await;
    let resp = con
        .post(
            &format!("/api/admin/teams/{team_a}/members"),
            json!({"user_id": new_member.user.id}),
        )
        .await
        .unwrap();
    resp.assert_ok();

    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM team_members WHERE team_id = $1 AND user_id = $2")
            .bind(team_a)
            .bind(new_member.user.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(n, 1, "manager A must be able to add to their own team");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_manager_a_cannot_write_limits_for_team_b_user() {
    // Single-row limits writes go through `assert_scope_for_subject`
    // which (for "user" subjects) requires global scope. A
    // team-scoped manager should never reach the SQL path.
    let app = TestApp::spawn().await;
    let team_a = make_team(&app.db, "team-a").await;
    let manager_a = fixtures::create_user_with_role(&app.db, "team_manager", "team", Some(team_a))
        .await
        .unwrap();
    let team_b_user = fixtures::create_random_user(&app.db).await.unwrap();

    let con = login(&app, &manager_a).await;
    let resp = con
        .post(
            &format!("/api/admin/limits/user/{}/rules", team_b_user.user.id),
            json!({
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 1
            }),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "manager A must not write limits for an out-of-scope user, got {}",
        resp.status
    );

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM rate_limit_rules WHERE subject_id = $1")
        .bind(team_b_user.user.id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(n, 0, "rule was persisted despite the rejected request");
}
