//! TOTP recovery codes — single-consume contract.
//!
//! `auth.rs::totp_setup_then_verify_then_login_requires_code`
//! covers the QR-code login path. This file pins the recovery
//! code path:
//!
//!   - 10 codes are minted at setup
//!   - the user can log in with any unused code as a substitute
//!     for the TOTP digits
//!   - a code is **single-use** — re-using the same code in a
//!     follow-up login fails
//!   - concurrent logins racing the same code can't both succeed
//!     (the CAS UPDATE in the login handler enforces atomicity)

use serde_json::Value;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn recovery_code_grants_login_then_is_consumed() {
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

    // Enable TOTP — capture the recovery codes the setup response
    // hands back.
    let setup: Value = con
        .post_empty("/api/auth/totp/setup")
        .await
        .unwrap()
        .json()
        .unwrap();
    let codes: Vec<String> = setup["recovery_codes"]
        .as_array()
        .expect("recovery_codes array in setup response")
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert_eq!(codes.len(), 10, "setup should mint 10 recovery codes");

    let secret = setup["secret"].as_str().unwrap();
    let code = think_watch_auth::totp::current_code(secret, &user.user.email).unwrap();
    con.post("/api/auth/totp/verify-setup", json!({"code": code}))
        .await
        .unwrap()
        .assert_ok();
    con.post_empty("/api/auth/logout")
        .await
        .unwrap()
        .assert_ok();

    // First recovery code redeems for a successful login.
    let target_code = codes[0].clone();
    let resp = app
        .console_client()
        .post(
            "/api/auth/login",
            json!({
                "email": user.user.email,
                "password": user.plaintext_password,
                "totp_code": target_code
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();

    // The DB must reflect that the code was consumed (one fewer
    // entry in users.totp_recovery_codes).
    let stored: Option<String> =
        sqlx::query_scalar("SELECT totp_recovery_codes FROM users WHERE id = $1")
            .bind(user.user.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    let remaining: Vec<String> = serde_json::from_str(&stored.unwrap()).unwrap();
    assert_eq!(
        remaining.len(),
        9,
        "exactly one code must have been consumed"
    );
    assert!(
        !remaining.contains(&target_code),
        "consumed code must NOT remain in the list"
    );

    // Same code reused → must fail.
    let replay = app
        .console_client()
        .post(
            "/api/auth/login",
            json!({
                "email": user.user.email,
                "password": user.plaintext_password,
                "totp_code": target_code
            }),
        )
        .await
        .unwrap();
    replay.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn concurrent_login_with_same_recovery_code_only_one_wins() {
    // The login handler does a CAS UPDATE — only succeed if
    // `totp_recovery_codes` matches what we read. Two concurrent
    // logins racing the same code must end with exactly one 200
    // and one 401.
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();

    // Bootstrap TOTP through the public endpoints once.
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": user.user.email, "password": user.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    let setup: Value = con
        .post_empty("/api/auth/totp/setup")
        .await
        .unwrap()
        .json()
        .unwrap();
    let secret = setup["secret"].as_str().unwrap().to_string();
    let codes: Vec<String> = setup["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let code_now = think_watch_auth::totp::current_code(&secret, &user.user.email).unwrap();
    con.post("/api/auth/totp/verify-setup", json!({"code": code_now}))
        .await
        .unwrap()
        .assert_ok();

    // Both racers try to consume codes[0].
    let target = codes[0].clone();
    let app_url = app.console_url.clone();
    let email_a = user.user.email.clone();
    let email_b = user.user.email.clone();
    let pwd_a = user.plaintext_password.clone();
    let pwd_b = user.plaintext_password.clone();
    let target_a = target.clone();
    let target_b = target.clone();
    let racer_a = tokio::spawn(async move {
        let con = TestClient::new(app_url);
        con.post(
            "/api/auth/login",
            json!({"email": email_a, "password": pwd_a, "totp_code": target_a}),
        )
        .await
    });
    let app_url = app.console_url.clone();
    let racer_b = tokio::spawn(async move {
        let con = TestClient::new(app_url);
        con.post(
            "/api/auth/login",
            json!({"email": email_b, "password": pwd_b, "totp_code": target_b}),
        )
        .await
    });
    let (a, b) = (
        racer_a.await.unwrap().unwrap(),
        racer_b.await.unwrap().unwrap(),
    );
    let (a_ok, b_ok) = (a.status.is_success(), b.status.is_success());
    assert!(
        a_ok ^ b_ok,
        "exactly one racer must win the recovery-code redemption: a={} b={}",
        a.status,
        b.status
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn recovery_codes_are_unique() {
    // Defensive — `generate_recovery_codes(10)` dedups internally.
    // A duplicate would let one redemption silently consume two
    // copies, leaving a "dead" entry that confuses both the user
    // and the audit trail. Exercise the generator through the HTTP
    // path so any future change to the count or alphabet is
    // observable.
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
    let setup: Value = con
        .post_empty("/api/auth/totp/setup")
        .await
        .unwrap()
        .json()
        .unwrap();
    let codes: Vec<String> = setup["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let unique: std::collections::HashSet<_> = codes.iter().collect();
    assert_eq!(unique.len(), codes.len(), "recovery codes must be unique");
}
