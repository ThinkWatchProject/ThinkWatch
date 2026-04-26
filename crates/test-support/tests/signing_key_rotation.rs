//! ECDSA signing-key rotation contract.
//!
//! `verify_signature.rs` checks every mutating request against a
//! single public key per user, stored in Redis at
//! `signing_pubkey:{user_id}`. The user's browser rotates this key
//! on every login by calling `POST /api/auth/register-key`, which
//! overwrites whatever was there. The contract this test pins:
//!
//!   1. After overwriting the public key, signed requests using the
//!      OLD private key must be rejected with 401.
//!   2. The new key works for signed requests immediately, no
//!      session reset required.
//!   3. IP binding: when the operator opts into XFF-based IP
//!      resolution, a request signed with a key registered from
//!      one IP must be rejected when it arrives from a different
//!      IP — even if the signature itself is valid.
//!
//! `auth.rs::signed_request_round_trip` covers the happy path of
//! a single key. `admin_user_actions::force_logout_drops_signing_key`
//! covers admin-driven invalidation. This file completes the
//! picture for the user-driven rotation case.

use serde_json::Value;
use think_watch_test_support::prelude::*;

async fn admin_login(app: &TestApp) -> TestClient {
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

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn rotation_invalidates_old_signing_key() {
    let app = TestApp::spawn().await;
    let con = admin_login(&app).await;

    // Pick a mutating endpoint we know an admin is allowed to hit
    // and that requires signing — provider-test does the job, with
    // a dummy URL that fails downstream but only AFTER the signature
    // gate, so a 4xx body still proves the signature was accepted.
    // Use an admin POST that succeeds end-to-end so we can split
    // sig-fail (401) from "sig fine, handler said no" (any 2xx/4xx
    // that's not 401).
    //
    // `POST /api/dashboard/ws-ticket` is signed and idempotent —
    // perfect probe.

    // 1. Mint key A and register it with the server.
    let key_a = SignedKey::generate();
    con.set_signing_key(key_a.clone());
    con.post(
        "/api/auth/register-key",
        json!({"public_key": key_a.public_jwk()}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Signed request with key A — must succeed.
    let r = con.post_empty("/api/dashboard/ws-ticket").await.unwrap();
    r.assert_ok();
    let body: Value = r.json().unwrap();
    assert!(
        body["ticket"].as_str().is_some(),
        "control: key A must mint a ticket: {body}"
    );

    // 2. Mint key B and register it. This overwrites A in Redis.
    let key_b = SignedKey::generate();
    con.set_signing_key(key_b.clone());
    con.post(
        "/api/auth/register-key",
        json!({"public_key": key_b.public_jwk()}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Signed request with key B — must also succeed (no session
    // reset required between rotation and use).
    con.post_empty("/api/dashboard/ws-ticket")
        .await
        .unwrap()
        .assert_ok();

    // 3. Restore client to key A and try again. Redis no longer
    //    holds A's pubkey, so the verifier must reject.
    con.set_signing_key(key_a);
    let r = con.post_empty("/api/dashboard/ws-ticket").await.unwrap();
    assert_eq!(
        r.status.as_u16(),
        401,
        "rotation didn't invalidate the old key — A-signed request still passes after B was registered: status={} body={}",
        r.status,
        r.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn signing_key_ip_binding_rejects_request_from_different_ip() {
    // The verifier records the client IP on register-key and rejects
    // any signed request that arrives from a different IP. Default
    // resolution is "connection" (TCP peer), which is identical for
    // every test request, so flip to XFF mode for this test where we
    // can control X-Forwarded-For via TestClient.
    let app = TestApp::spawn().await;

    // Two settings are required to actually honor XFF: the source
    // toggle AND a non-empty trusted-proxy list (otherwise the
    // resolver falls back to the TCP peer IP to prevent spoofing —
    // see `extract_client_ip` in auth_guard.rs).
    fixtures::set_setting(&app.db, "security.client_ip_source", json!("xff"))
        .await
        .unwrap();
    fixtures::set_setting(&app.db, "security.trusted_proxies", json!("*"))
        .await
        .unwrap();
    app.state.dynamic_config.reload().await.unwrap();

    let con = admin_login(&app).await;
    let key = SignedKey::generate();
    let pk = key.public_jwk();
    con.set_signing_key(key);

    // Register key from IP A.
    con.set_forwarded_for("203.0.113.10");
    con.post("/api/auth/register-key", json!({"public_key": pk}))
        .await
        .unwrap()
        .assert_ok();

    // Same-IP signed request: control. Must succeed.
    con.post_empty("/api/dashboard/ws-ticket")
        .await
        .unwrap()
        .assert_ok();

    // Switch to a different "client IP" and try again. Signature
    // itself is still valid; binding check must reject anyway.
    con.set_forwarded_for("198.51.100.42");
    let r = con.post_empty("/api/dashboard/ws-ticket").await.unwrap();
    assert_eq!(
        r.status.as_u16(),
        401,
        "IP binding bypassed — request from new IP accepted with stale-IP key: status={} body={}",
        r.status,
        r.text()
    );
}
