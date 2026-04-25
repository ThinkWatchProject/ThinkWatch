//! Integration tests for the authentication surfaces:
//! login / register / refresh / logout / change_password /
//! delete_account / register-key + signature middleware /
//! TOTP setup-and-verify / sso status / setup wizard.

use serde_json::Value;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn login_wrong_password_yields_401() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();

    let resp = con
        .post(
            "/api/auth/login",
            json!({"email": admin.user.email, "password": "obviously_wrong_pwd"}),
        )
        .await
        .unwrap();
    resp.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn login_unknown_email_yields_401() {
    let app = TestApp::spawn().await;
    let con = app.console_client();
    let resp = con
        .post(
            "/api/auth/login",
            json!({"email": "nobody@example.com", "password": "anything_with_8+_chars"}),
        )
        .await
        .unwrap();
    resp.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn login_progressive_lockout_after_repeated_failures() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();

    // 5 wrong attempts trip the lockout (per `login` handler).
    for _ in 0..5 {
        con.post(
            "/api/auth/login",
            json!({"email": admin.user.email, "password": "still_wrong_pw"}),
        )
        .await
        .unwrap();
    }
    // The next attempt — even with the correct password — should be
    // refused with the lockout-error 400.
    let resp = con
        .post(
            "/api/auth/login",
            json!({"email": admin.user.email, "password": admin.plaintext_password}),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status.as_u16(),
        400,
        "lockout should refuse even valid credentials, body={}",
        resp.text()
    );
    assert!(resp.text().to_lowercase().contains("lock"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn refresh_token_rotates_and_blacklists_previous() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let original_refresh = con
        .cookie("__Secure-refresh_token")
        .expect("refresh cookie set on login");

    // JWT `iat` is whole-seconds; refreshing within the same second
    // produces the byte-identical token. Wait until the next second
    // boundary so the rotation is observable.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // First refresh — should rotate (set a new cookie).
    con.post_empty("/api/auth/refresh")
        .await
        .unwrap()
        .assert_ok();
    let new_refresh = con
        .cookie("__Secure-refresh_token")
        .expect("refresh cookie set on rotation");
    assert_ne!(
        original_refresh, new_refresh,
        "refresh cookie should rotate"
    );

    // Replay the original refresh token in a body field — must be
    // blacklisted now.
    con.set_cookie("__Secure-refresh_token", "");
    con.clear_cookies();
    let replay = con
        .post(
            "/api/auth/refresh",
            json!({"refresh_token": original_refresh}),
        )
        .await
        .unwrap();
    replay.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn change_password_revokes_outstanding_refresh_tokens() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let stale_refresh = con
        .cookie("__Secure-refresh_token")
        .expect("refresh cookie present");

    let resp = con
        .post(
            "/api/auth/password",
            json!({
                "old_password": admin.plaintext_password,
                "new_password": "NewPass_word_123!"
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();

    // The stale refresh token must be rejected for further sessions.
    con.clear_cookies();
    let replay = con
        .post("/api/auth/refresh", json!({"refresh_token": stale_refresh}))
        .await
        .unwrap();
    replay.assert_status(401);

    // New password works.
    let new_login = con
        .post(
            "/api/auth/login",
            json!({"email": admin.user.email, "password": "NewPass_word_123!"}),
        )
        .await
        .unwrap();
    new_login.assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn revoke_sessions_invalidates_other_devices() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();

    let con_a = app.console_client();
    let con_b = app.console_client();
    let creds = json!({"email": admin.user.email, "password": admin.plaintext_password});
    con_a
        .post("/api/auth/login", creds.clone())
        .await
        .unwrap()
        .assert_ok();
    con_b
        .post("/api/auth/login", creds)
        .await
        .unwrap()
        .assert_ok();

    // Client A asks the server to revoke every other session.
    con_a
        .post_empty("/api/auth/revoke-sessions")
        .await
        .unwrap()
        .assert_ok();

    // Client B's refresh attempt now fails.
    let stale = con_b.cookie("__Secure-refresh_token").unwrap();
    con_b.clear_cookies();
    let replay = con_b
        .post("/api/auth/refresh", json!({"refresh_token": stale}))
        .await
        .unwrap();
    replay.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn signed_requests_pass_when_key_registered() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Generate the keypair and ship the public key to the server.
    let pk = con.enable_signing();
    con.post("/api/auth/register-key", json!({"public_key": pk}))
        .await
        .unwrap()
        .assert_ok();

    // A protected POST that previously needed neither auth nor sigs
    // now requires both. Use the change-password "no-op" path to
    // verify signing — wrong old password is fine, we only care
    // that the signature middleware accepted us.
    let resp = con
        .post(
            "/api/auth/password",
            json!({
                "old_password": admin.plaintext_password,
                "new_password": "AnotherStrong_Pwd1!",
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn signed_requests_reject_when_sig_headers_missing() {
    // With a public key registered, a subsequent mutating call that
    // *omits* the signature headers is rejected by the
    // verify_signature middleware (it sees a key but no proof and
    // refuses to assume the grace path).
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let pk = con.enable_signing();
    con.post("/api/auth/register-key", json!({"public_key": pk}))
        .await
        .unwrap()
        .assert_ok();

    // Signed POST → ok. Use /api/keys (a non-credential-mutating
    // endpoint that won't blow away the signing key on success).
    con.post(
        "/api/keys",
        json!({
            "name": "via signed request",
            "surfaces": ["ai_gateway"],
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    // Drop signing → identical call now fails authentication.
    con.disable_signing();
    let resp = con
        .post(
            "/api/keys",
            json!({
                "name": "without sigs",
                "surfaces": ["ai_gateway"],
            }),
        )
        .await
        .unwrap();
    resp.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn totp_setup_then_verify_then_login_requires_code() {
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": user.user.email, "password": user.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    // 1. Issue a TOTP secret.
    let setup: Value = con
        .post_empty("/api/auth/totp/setup")
        .await
        .unwrap()
        .json()
        .unwrap();
    let secret = setup["secret"]
        .as_str()
        .expect("totp secret in body")
        .to_string();

    // 2. Verify a freshly-generated code to enable TOTP.
    let code = think_watch_auth::totp::current_code(&secret, &user.user.email).unwrap();
    con.post("/api/auth/totp/verify-setup", json!({"code": code}))
        .await
        .unwrap()
        .assert_ok();

    // 3. Logout so the session is fresh.
    con.post_empty("/api/auth/logout")
        .await
        .unwrap()
        .assert_ok();

    // 4. Re-login with password only — must request TOTP.
    let stage1: Value = con
        .post(
            "/api/auth/login",
            json!({"email": user.user.email, "password": user.plaintext_password}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(stage1["totp_required"], serde_json::json!(true));

    // 5. Supply the code → success.
    let code = think_watch_auth::totp::current_code(&secret, &user.user.email).unwrap();
    con.post(
        "/api/auth/login",
        json!({
            "email": user.user.email,
            "password": user.plaintext_password,
            "totp_code": code,
        }),
    )
    .await
    .unwrap()
    .assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn delete_account_soft_deletes_and_blocks_login() {
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": user.user.email, "password": user.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    con.delete("/api/auth/account").await.unwrap().assert_ok();

    let con2 = app.console_client();
    let resp = con2
        .post(
            "/api/auth/login",
            json!({"email": user.user.email, "password": user.plaintext_password}),
        )
        .await
        .unwrap();
    resp.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn setup_status_and_initialize_flow() {
    let app = TestApp::spawn().await;
    let con = app.console_client();

    let st: Value = con.get("/api/setup/status").await.unwrap().json().unwrap();
    assert_eq!(st["initialized"], json!(false));
    assert_eq!(st["needs_setup"], json!(true));

    let resp: Value = con
        .post(
            "/api/setup/initialize",
            json!({
                "admin": {
                    "email": "owner@example.com",
                    "display_name": "Owner",
                    "password": "Owner_password_123!"
                },
                "site_name": "Test Site"
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(resp["admin_email"].as_str(), Some("owner@example.com"));
    assert!(resp["admin_id"].is_string());

    // Setup is now locked.
    let st2: Value = con.get("/api/setup/status").await.unwrap().json().unwrap();
    assert_eq!(st2["initialized"], json!(true));

    // A second initialize attempt is rejected.
    let dup = con
        .post(
            "/api/setup/initialize",
            json!({
                "admin": {
                    "email": "another@example.com",
                    "display_name": "Other",
                    "password": "Other_password_123!"
                }
            }),
        )
        .await
        .unwrap();
    assert!(matches!(dup.status.as_u16(), 400 | 403));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn sso_status_reports_disabled_by_default() {
    let app = TestApp::spawn().await;
    let con = app.console_client();
    let resp: Value = con
        .get("/api/auth/sso/status")
        .await
        .unwrap()
        .json()
        .unwrap();
    // Either `enabled: false` or `configured: false` — the shape
    // varies but both express the same thing.
    let off = resp.get("enabled").and_then(|v| v.as_bool()) == Some(false)
        || resp.get("configured").and_then(|v| v.as_bool()) == Some(false);
    assert!(off, "sso should be off by default: {resp}");
}
