//! Integration tests for the proof-of-work guard on `/api/auth/login`.
//!
//! TestClient::post auto-injects PoW for /api/auth/login (so the
//! ~70 existing login call sites don't need touching). These tests
//! drive the lower-level `send` helper directly to exercise the
//! reject paths the auto-inject would mask.

use serde_json::json;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn login_without_pow_field_400s() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();

    // Bypass the auto-inject by hitting `send` directly with a
    // body that *intentionally* omits `pow`. This is what a naive
    // attacker brute-forcing curl-style would send.
    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/login",
            Some(&json!({
                "email": admin.user.email,
                "password": admin.plaintext_password,
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        400,
        "missing pow must 400 with 'PoW required', got: {}",
        resp.text()
    );
    assert!(
        resp.text().to_lowercase().contains("proof-of-work"),
        "error body should explain PoW requirement: {}",
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn login_with_invalid_pow_nonce_400s() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();

    // Mint a real challenge but submit a garbage nonce. The handler
    // should GETDEL the challenge (it's been "used") and reject for
    // wrong nonce — so a follow-up retry with the same challenge_id
    // would also fail (challenge consumed).
    let challenge = con
        .post_empty("/api/auth/pow-challenge")
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .unwrap();
    let challenge_id = challenge["challenge_id"].as_str().unwrap();

    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/login",
            Some(&json!({
                "email": admin.user.email,
                "password": admin.plaintext_password,
                "pow": {
                    "challenge_id": challenge_id,
                    "nonce": "nope-not-a-valid-nonce",
                },
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        400,
        "invalid nonce must 400; got: {}",
        resp.text()
    );

    // Replay attempt with same challenge_id — must also 400 (the
    // verify path GETDEL'd it on the first call).
    let replay = con
        .send(
            reqwest::Method::POST,
            "/api/auth/login",
            Some(&json!({
                "email": admin.user.email,
                "password": admin.plaintext_password,
                "pow": {
                    "challenge_id": challenge_id,
                    "nonce": "anything",
                },
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        replay.status,
        400,
        "consumed challenge must reject the replay; got: {}",
        replay.text()
    );
    assert!(
        replay.text().to_lowercase().contains("expired")
            || replay.text().to_lowercase().contains("consumed"),
        "replay error should mention expired/consumed: {}",
        replay.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn login_with_unknown_challenge_id_400s() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();

    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/login",
            Some(&json!({
                "email": admin.user.email,
                "password": admin.plaintext_password,
                "pow": {
                    "challenge_id": "completely-fake-challenge-id",
                    "nonce": "anything",
                },
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        400,
        "unknown challenge_id should 400; got: {}",
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn login_with_valid_pow_succeeds() {
    // Happy path explicitly — even though every other login test in
    // the suite already drives this implicitly via post()'s
    // auto-inject, pin a direct assertion here so a regression in
    // mint_and_grind_pow doesn't silently break the entire suite at
    // once.
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();

    let resp = con
        .post(
            "/api/auth/login",
            json!({"email": admin.user.email, "password": admin.plaintext_password}),
        )
        .await
        .unwrap();
    resp.assert_ok();
}
