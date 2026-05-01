//! Integration tests for the console / admin API surface.
//!
//! Each test logs in as a super-admin (or developer where the
//! permission boundary needs probing) and drives a realistic CRUD
//! cycle. Permission failures return 403 — non-mutating reads return
//! the canonical shape so the frontend's TypeScript contracts stay
//! valid.

use serde_json::Value;
use think_watch_test_support::prelude::*;

/// List endpoints are split between two response shapes — `Vec<T>`
/// in the older endpoints and `{ "items": Vec<T> }` / `{ "data":
/// Vec<T> }` in the newer ones. This helper resolves both so the
/// individual tests don't have to care about which kind they hit.
fn pick_list(value: &Value) -> Option<&Vec<Value>> {
    value
        .as_array()
        .or_else(|| value.get("items").and_then(|v| v.as_array()))
        .or_else(|| value.get("data").and_then(|v| v.as_array()))
}

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

// ---------------------------------------------------------------------------
// API Keys CRUD
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn api_keys_create_list_get_rotate_revoke_cycle() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    // Create
    let created: Value = con
        .post(
            "/api/keys",
            json!({"name": "ci-key", "surfaces": ["ai_gateway"]}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let plaintext = created["key"].as_str().unwrap().to_string();
    assert!(plaintext.starts_with("tw-"), "key shape: {plaintext}");

    // List
    let list_resp = con.get("/api/keys").await.unwrap();
    list_resp.assert_ok();
    let list_body: Value = list_resp.json().unwrap();
    let entries = pick_list(&list_body).expect("api keys list");
    assert!(entries.iter().any(|e| e["id"] == created["id"]));

    // Get
    let single: Value = con
        .get(&format!("/api/keys/{id}"))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(single["name"], "ci-key");

    // Rotate
    let rotated: Value = con
        .post_empty(&format!("/api/keys/{id}/rotate"))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert!(rotated["key"].as_str().unwrap().starts_with("tw-"));
    assert_ne!(
        rotated["key"].as_str().unwrap(),
        plaintext,
        "rotation must mint a new plaintext"
    );

    // Revoke (delete) — sets is_active=false AND deleted_at=now(),
    // so the row vanishes from the default list and direct GET 404s.
    // The audit-window view (`?archived=true`) still surfaces it
    // until the retention sweep hard-deletes it ~30 days later.
    con.delete(&format!("/api/keys/{id}"))
        .await
        .unwrap()
        .assert_ok();

    con.get(&format!("/api/keys/{id}"))
        .await
        .unwrap()
        .assert_status(404);

    // Archived view should contain exactly this revoked key.
    let archived: Value = con
        .get("/api/keys?archived=true")
        .await
        .unwrap()
        .json()
        .unwrap();
    let row = archived["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"].as_str() == Some(&id))
        .expect("revoked key must appear under archived=true");
    assert_eq!(row["is_active"], json!(false));
    assert_eq!(row["disabled_reason"].as_str(), Some("revoked"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn developer_cannot_force_revoke_admin_keys() {
    let app = TestApp::spawn().await;
    // Two users: dev + admin.
    let dev = fixtures::create_random_user(&app.db).await.unwrap();
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();

    // Admin mints a key.
    let admin_key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "admin-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    // Dev logs in.
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": dev.user.email, "password": dev.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let resp = con
        .post(
            &format!("/api/admin/keys/{}/force-revoke", admin_key.row.id),
            json!({"reason": "test - perm check"}),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 401 | 403),
        "developer must not be allowed to force-revoke, got {}",
        resp.status
    );
}

// ---------------------------------------------------------------------------
// Users CRUD (admin)
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn admin_can_list_create_update_delete_users() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    let created: Value = con
        .post(
            "/api/admin/users",
            json!({
                "email": unique_email(),
                "display_name": "Created via test",
                "password": "InitialPwd_1234!",
                "is_active": true
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let new_id = created["id"].as_str().unwrap().to_string();

    let list: Value = con.get("/api/admin/users").await.unwrap().json().unwrap();
    let entries = pick_list(&list).expect("users list");
    assert!(entries.iter().any(|e| e["id"] == created["id"]));

    con.patch(
        &format!("/api/admin/users/{new_id}"),
        json!({"display_name": "Renamed"}),
    )
    .await
    .unwrap()
    .assert_ok();

    con.delete(&format!("/api/admin/users/{new_id}"))
        .await
        .unwrap()
        .assert_ok();

    // Soft-deleted: stays visible to admin lists with deleted flag.
    let post_delete: Value = con
        .get(&format!("/api/admin/users/{new_id}"))
        .await
        .unwrap()
        .json()
        .unwrap_or(Value::Null);
    if !post_delete.is_null() {
        // Admin endpoint may either 404 or return tombstoned shape.
        assert!(post_delete["deleted_at"].is_string() || post_delete["is_active"] == json!(false));
    }
}

// ---------------------------------------------------------------------------
// Roles + permissions
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn list_roles_includes_seeded_systems() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    let resp = con.get("/api/admin/roles").await.unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    let arr = pick_list(&body).expect("roles list");
    let names: Vec<&str> = arr.iter().filter_map(|r| r["name"].as_str()).collect();
    for required in [
        "super_admin",
        "admin",
        "team_manager",
        "developer",
        "viewer",
    ] {
        assert!(
            names.contains(&required),
            "missing seeded role {required}: {names:?}"
        );
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn permissions_endpoint_returns_known_keys() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let body: Value = con
        .get("/api/admin/permissions")
        .await
        .unwrap()
        .json()
        .unwrap();
    let arr = pick_list(&body).expect("permissions list");
    let keys: Vec<&str> = arr.iter().filter_map(|p| p["key"].as_str()).collect();
    assert!(
        keys.contains(&"api_keys:create"),
        "expected api_keys:create in permission catalog: {keys:?}"
    );
}

// ---------------------------------------------------------------------------
// Teams CRUD
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn teams_create_add_member_list_remove() {
    let app = TestApp::spawn().await;
    let (con, _admin) = admin_session(&app).await;
    let dev = fixtures::create_random_user(&app.db).await.unwrap();

    // Create
    let team: Value = con
        .post(
            "/api/admin/teams",
            json!({"name": unique_name("team"), "description": "ci-team"}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let team_id = team["id"].as_str().unwrap().to_string();

    // Add dev as member
    con.post(
        &format!("/api/admin/teams/{team_id}/members"),
        json!({"user_id": dev.user.id, "role": "member"}),
    )
    .await
    .unwrap()
    .assert_ok();

    // List members shows the dev.
    let members: Value = con
        .get(&format!("/api/admin/teams/{team_id}/members"))
        .await
        .unwrap()
        .json()
        .unwrap();
    let arr = pick_list(&members).expect("members list");
    assert!(
        arr.iter()
            .any(|m| m["user_id"] == json!(dev.user.id.to_string())
                || m["user_id"] == json!(dev.user.id)),
        "dev should be in member list: {arr:?}"
    );

    // Remove member.
    con.delete(&format!(
        "/api/admin/teams/{team_id}/members/{}",
        dev.user.id
    ))
    .await
    .unwrap()
    .assert_ok();
}

// ---------------------------------------------------------------------------
// Providers + models CRUD
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn providers_can_be_created_listed_deleted() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    let created: Value = con
        .post(
            "/api/admin/providers",
            json!({
                "name": unique_name("prov"),
                "display_name": "Test Provider",
                "provider_type": "openai",
                // SSRF guard rejects loopback / private networks. Use a
                // public-looking host so the validate_url check passes.
                // No request will actually reach this URL because the
                // gateway router isn't rebuilt in this test.
                // SSRF guard does a DNS resolve; pick a public host
                // that exists. No request actually leaves the test —
                // the gateway router isn't rebuilt afterwards.
                "base_url": "https://api.openai.com/v1",
                "config": {}
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let pid = created["id"].as_str().unwrap().to_string();

    let list_body: Value = con
        .get("/api/admin/providers")
        .await
        .unwrap()
        .json()
        .unwrap();
    let arr = pick_list(&list_body).expect("providers list");
    assert!(arr.iter().any(|p| p["id"] == created["id"]));

    con.delete(&format!("/api/admin/providers/{pid}"))
        .await
        .unwrap()
        .assert_ok();
}

// ---------------------------------------------------------------------------
// Settings get/update
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn settings_round_trip_updates_dynamic_config() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    let body = con.get("/api/admin/settings/system").await.unwrap();
    body.assert_ok();

    // Flip allow_registration via the bulk update endpoint.
    con.patch(
        "/api/admin/settings",
        json!({"settings": {"auth.allow_registration": true}}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Re-read via the public setup-status helper which calls
    // dynamic_config.is_initialized + reads the in-memory cache via
    // a simple GET. Direct DB read keeps the test independent of
    // the response shape exposed by /api/admin/settings.
    let row: Option<bool> =
        sqlx::query_scalar("SELECT (value::text = 'true') FROM system_settings WHERE key = $1")
            .bind("auth.allow_registration")
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(row, Some(true));
}

// ---------------------------------------------------------------------------
// Limits CRUD via console
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn limits_admin_creates_and_lists_user_rule() {
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let dev = fixtures::create_random_user(&app.db).await.unwrap();

    let path = format!("/api/admin/limits/user/{}/rules", dev.user.id);
    let created = con
        .post(
            &path,
            json!({
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 100,
                "enabled": true
            }),
        )
        .await
        .unwrap();
    created.assert_ok();

    let list: Value = con.get(&path).await.unwrap().json().unwrap();
    let arr = list
        .get("items")
        .and_then(|v| v.as_array())
        .or_else(|| list.as_array())
        .expect("rules list");
    assert!(
        arr.iter().any(|r| r["max_count"] == json!(100)),
        "expected max_count=100 rule: {arr:?}"
    );
}
