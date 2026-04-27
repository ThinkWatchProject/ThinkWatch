//! Integration tests for the OIDC setup wizard's draft → test →
//! activate flow. Discovery / test-login round-trips against a real
//! provider are out of scope here — those need a live IdP and live
//! Got the wizard's state machine: draft persistence, test-result
//! invalidation, and the activation gate.

use serde_json::{Value, json};
use think_watch_test_support::prelude::*;

const OIDC_TEST_RESULT_KEY: &str = "oidc:test:result";

/// Helper — log in as super_admin and return a `TestClient`. We
/// deliberately *don't* call `enable_signing` + `register-key`:
/// once a public key is registered the verify_signature middleware
/// rejects unsigned GETs, but `TestClient` only signs mutating
/// methods. Sticking to the grace window keeps the tests readable
/// and still exercises the routes' auth + permission checks.
async fn admin_console(app: &TestApp) -> TestClient {
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

/// Stash a synthetic test-login result directly in Redis so we can
/// drive the activation gate without spinning up a real OIDC popup.
async fn write_fake_test_result(app: &TestApp, passed: bool) {
    let payload = json!({
        "passed": passed,
        "at": chrono::Utc::now().timestamp(),
        "error": if passed { Value::Null } else { json!("forced failure") },
        "claims_preview": json!({
            "subject": "sub-123",
            "email": "person@example.com",
            "name": "Person",
            "issuer": "https://accounts.google.com",
        }),
    });
    let _: Result<(), _> = fred::interfaces::KeysInterface::set::<(), _, _>(
        &app.state.redis,
        OIDC_TEST_RESULT_KEY,
        payload.to_string(),
        Some(fred::types::Expiration::EX(1800)),
        None,
        false,
    )
    .await;
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn default_state_is_empty_draft_and_disabled_active() {
    let app = TestApp::spawn().await;
    let con = admin_console(&app).await;

    let resp: Value = con
        .get("/api/admin/settings/oidc")
        .await
        .unwrap()
        .json()
        .unwrap();

    assert_eq!(resp["active"]["enabled"], json!(false));
    assert_eq!(resp["active"]["configured"], json!(false));
    assert!(resp["draft"].is_null());
    assert!(resp["test_result"].is_null());
    // The default redirect URL should always be derivable.
    assert!(
        resp["default_redirect_url"]
            .as_str()
            .is_some_and(|s| s.contains("/api/auth/sso/callback"))
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn draft_upsert_persists_then_returns_in_get() {
    let app = TestApp::spawn().await;
    let con = admin_console(&app).await;

    con.patch(
        "/api/admin/settings/oidc/draft",
        json!({
            "provider_preset": "google",
            "issuer_url": "https://accounts.google.com",
            "client_id": "client-abc",
            "client_secret": "very-secret-1234567890",
            "redirect_url": "http://localhost:3001/api/auth/sso/callback",
            "email_claim": "preferred_username",
            "name_claim": "name",
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    let resp: Value = con
        .get("/api/admin/settings/oidc")
        .await
        .unwrap()
        .json()
        .unwrap();

    assert_eq!(resp["draft"]["provider_preset"], json!("google"));
    assert_eq!(
        resp["draft"]["issuer_url"],
        json!("https://accounts.google.com")
    );
    assert_eq!(resp["draft"]["client_id"], json!("client-abc"));
    // `has_secret: true` instead of leaking the plaintext.
    assert_eq!(resp["draft"]["has_secret"], json!(true));
    assert_eq!(resp["draft"]["email_claim"], json!("preferred_username"));
    // Active stays untouched while the draft is pending.
    assert_eq!(resp["active"]["enabled"], json!(false));
    assert_eq!(resp["active"]["configured"], json!(false));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn draft_mutation_invalidates_pending_test_result() {
    let app = TestApp::spawn().await;
    let con = admin_console(&app).await;

    // Seed a draft + a passing test result.
    con.patch(
        "/api/admin/settings/oidc/draft",
        json!({
            "issuer_url": "https://accounts.google.com",
            "client_id": "abc",
            "client_secret": "shhh-1234567890",
        }),
    )
    .await
    .unwrap()
    .assert_ok();
    write_fake_test_result(&app, true).await;

    let before: Value = con
        .get("/api/admin/settings/oidc")
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(before["test_result"]["passed"], json!(true));

    // A subsequent draft edit must wipe the test result — we don't
    // want stale "tested OK" surviving a config change.
    con.patch(
        "/api/admin/settings/oidc/draft",
        json!({"client_id": "different-client"}),
    )
    .await
    .unwrap()
    .assert_ok();

    let after: Value = con
        .get("/api/admin/settings/oidc")
        .await
        .unwrap()
        .json()
        .unwrap();
    assert!(after["test_result"].is_null());
    assert_eq!(after["draft"]["client_id"], json!("different-client"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn activate_without_passing_test_returns_400() {
    let app = TestApp::spawn().await;
    let con = admin_console(&app).await;

    con.patch(
        "/api/admin/settings/oidc/draft",
        json!({
            "issuer_url": "https://accounts.google.com",
            "client_id": "abc",
            "client_secret": "shhh-1234567890",
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    // No test result — activation must refuse.
    let resp = con
        .post("/api/admin/settings/oidc/activate", json!({}))
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 400);

    // A *failed* test result still doesn't satisfy the gate.
    write_fake_test_result(&app, false).await;
    let resp = con
        .post("/api/admin/settings/oidc/activate", json!({}))
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 400);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn delete_draft_clears_draft_and_test_result() {
    let app = TestApp::spawn().await;
    let con = admin_console(&app).await;

    con.patch(
        "/api/admin/settings/oidc/draft",
        json!({"issuer_url": "https://accounts.google.com"}),
    )
    .await
    .unwrap()
    .assert_ok();
    write_fake_test_result(&app, true).await;

    con.delete("/api/admin/settings/oidc/draft")
        .await
        .unwrap()
        .assert_ok();

    let resp: Value = con
        .get("/api/admin/settings/oidc")
        .await
        .unwrap()
        .json()
        .unwrap();
    assert!(resp["draft"].is_null());
    assert!(resp["test_result"].is_null());
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn discover_without_issuer_returns_400() {
    let app = TestApp::spawn().await;
    let con = admin_console(&app).await;

    // No draft at all.
    let resp = con
        .post("/api/admin/settings/oidc/discover", json!({}))
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 400);

    // Draft missing issuer.
    con.patch(
        "/api/admin/settings/oidc/draft",
        json!({"client_id": "abc"}),
    )
    .await
    .unwrap()
    .assert_ok();
    let resp = con
        .post("/api/admin/settings/oidc/discover", json!({}))
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 400);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn toggle_enable_without_active_config_returns_400() {
    let app = TestApp::spawn().await;
    let con = admin_console(&app).await;

    // No active config exists yet — flipping enable=true must fail.
    let resp = con
        .patch("/api/admin/settings/oidc", json!({"enabled": true}))
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 400);
}
