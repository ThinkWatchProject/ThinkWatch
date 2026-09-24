//! `security.totp_required` is enforced on console sessions.
//!
//! A user who has not enrolled TOTP still signs in, but the session
//! only reaches the enrollment endpoints (`/api/auth/me`, the TOTP
//! status / setup / verify-setup, `register-key`, logout); everything
//! else answers 403 with the `totp_enrollment_required` type. The gate
//! is decided per request, so enrolling lifts it on the same session.
//! API keys are not sessions and are not held at enrollment.
//!
//! SSO sign-in is covered in `admin_access.rs`, next to its mock
//! identity provider.

use serde_json::Value;
use think_watch_test_support::prelude::*;

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

fn assert_held_at_enrollment(resp: &think_watch_test_support::client::TestResponse, what: &str) {
    assert_eq!(resp.status.as_u16(), 403, "{what}: {}", resp.text());
    let body: Value = resp.json().unwrap();
    assert_eq!(
        body["error"]["type"], "totp_enrollment_required",
        "{what}: {body}"
    );
}

/// Run the real setup → verify-setup exchange with a code computed
/// from the secret the server hands back.
async fn enroll(con: &TestClient, email: &str) {
    let setup: Value = con
        .post_empty("/api/auth/totp/setup")
        .await
        .unwrap()
        .json()
        .unwrap();
    let secret = setup["secret"].as_str().unwrap();
    let code = think_watch_auth::totp::current_code(secret, email).unwrap();
    con.post("/api/auth/totp/verify-setup", json!({"code": code}))
        .await
        .unwrap()
        .assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn an_unenrolled_session_reaches_only_enrollment_until_it_enrolls() {
    let app = TestApp::spawn().await;
    app.set_setting("security.totp_required", json!(true)).await;
    // A super-admin: the requirement has no exemptions.
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = login(&app, &admin).await;

    // What the enrollment screen needs.
    let me: Value = con.get("/api/auth/me").await.unwrap().json().unwrap();
    assert_eq!(me["email"], admin.user.email.as_str());
    assert_eq!(me["totp_enrollment_required"], true, "{me}");
    let status: Value = con
        .get("/api/auth/totp/status")
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(status, json!({"enabled": false, "required": true}));

    // Everything else, reads and writes, user and admin routes.
    for path in [
        "/api/keys",
        "/api/dashboard/stats",
        "/api/health",
        "/api/admin/settings",
        "/api/admin/users",
    ] {
        assert_held_at_enrollment(&con.get(path).await.unwrap(), path);
    }
    assert_held_at_enrollment(
        &con.post(
            "/api/keys",
            json!({"name": "blocked", "surfaces": ["ai_gateway"]}),
        )
        .await
        .unwrap(),
        "POST /api/keys",
    );
    assert_held_at_enrollment(
        &con.post(
            "/api/auth/password",
            json!({"old_password": admin.plaintext_password, "new_password": "Another-Passw0rd!"}),
        )
        .await
        .unwrap(),
        "POST /api/auth/password",
    );

    enroll(&con, &admin.user.email).await;

    // Same session, no re-login.
    con.get("/api/keys").await.unwrap().assert_ok();
    con.get("/api/admin/settings").await.unwrap().assert_ok();
    let me: Value = con.get("/api/auth/me").await.unwrap().json().unwrap();
    assert_eq!(me["totp_enrollment_required"], false, "{me}");

    // Disabling while the setting is on would only lead straight back
    // to enrollment, so it is refused.
    con.post(
        "/api/auth/totp/disable",
        json!({"old_password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_status(400);

    // Logout stays reachable (checked on a fresh unenrolled session).
    let other = fixtures::create_random_user(&app.db).await.unwrap();
    let con = login(&app, &other).await;
    con.post_empty("/api/auth/logout")
        .await
        .unwrap()
        .assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn switching_the_setting_on_holds_sessions_that_already_exist() {
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let con = login(&app, &user).await;
    con.get("/api/keys").await.unwrap().assert_ok();

    app.set_setting("security.totp_required", json!(true)).await;
    assert_held_at_enrollment(&con.get("/api/keys").await.unwrap(), "after switch-on");

    app.set_setting("security.totp_required", json!(false))
        .await;
    con.get("/api/keys").await.unwrap().assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn an_enrolled_user_is_unaffected() {
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let con = login(&app, &user).await;
    enroll(&con, &user.user.email).await;

    app.set_setting("security.totp_required", json!(true)).await;
    con.get("/api/keys").await.unwrap().assert_ok();
    let me: Value = con.get("/api/auth/me").await.unwrap().json().unwrap();
    assert_eq!(me["totp_enrollment_required"], false, "{me}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn with_the_setting_off_an_unenrolled_user_is_unrestricted() {
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let con = login(&app, &user).await;

    con.get("/api/keys").await.unwrap().assert_ok();
    let me: Value = con.get("/api/auth/me").await.unwrap().json().unwrap();
    assert_eq!(me["totp_enrollment_required"], false, "{me}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn api_keys_are_not_held_at_enrollment() {
    let app = TestApp::spawn().await;
    app.set_setting("security.totp_required", json!(true)).await;

    let upstream = MockProvider::openai_chat_ok("gpt-4o-mini-test").await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("openai-totp"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "gpt-4o-mini-test")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    // The key's owner has never enrolled.
    let owner = fixtures::create_random_user(&app.db).await.unwrap();

    let gateway_key =
        fixtures::create_api_key(&app.db, owner.user.id, "gw", &["ai_gateway"], None, None)
            .await
            .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(gateway_key.plaintext);
    gw.post(
        "/v1/chat/completions",
        json!({
            "model": "gpt-4o-mini-test",
            "messages": [{"role": "user", "content": "ping"}]
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    let console_key =
        fixtures::create_api_key(&app.db, owner.user.id, "console", &["console"], None, None)
            .await
            .unwrap();
    let con = app.console_client();
    con.set_bearer(console_key.plaintext);
    con.get("/api/keys").await.unwrap().assert_ok();
}
