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
        .send(
            reqwest::Method::POST,
            "/api/auth/pow-challenge",
            Some(&json!({ "email": admin.user.email })),
        )
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
async fn pow_challenge_requires_email() {
    let app = TestApp::spawn().await;
    let con = app.console_client();

    // No body at all → 400 (deserialize fails on missing required field).
    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/pow-challenge",
            None::<&serde_json::Value>,
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        400,
        "pow-challenge without email must 400; got: {}",
        resp.text()
    );

    // Empty-object body → also 400 (email field missing).
    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/pow-challenge",
            Some(&json!({})),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        400,
        "pow-challenge with empty body must 400; got: {}",
        resp.text()
    );

    // Malformed email → 400 (validator rejects).
    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/pow-challenge",
            Some(&json!({ "email": "not-an-email" })),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        400,
        "pow-challenge with invalid email must 400; got: {}",
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn challenge_bound_to_email_a_rejected_for_email_b() {
    // Email-binding is the whole point of mixing email into the
    // hash: a stockpiled challenge for alice cannot be replayed
    // against bob even with a ground nonce.
    let app = TestApp::spawn().await;
    let alice = fixtures::create_admin_user(&app.db).await.unwrap();
    let bob = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();

    // Mint + grind a challenge bound to alice's email.
    let pow_for_alice = con.mint_and_grind_pow(&alice.user.email).await.unwrap();

    // Try to use it for bob. Server must reject. The metadata
    // email-mismatch check fires BEFORE the hash check, so we assert
    // on its specific error body — this pins which branch handled
    // the rejection. Without this, removing the email-binding hash
    // mix-in would still leave the test passing on the metadata
    // branch and the regression would go silent. (The pow.rs unit
    // tests cover the hash-only branch directly.)
    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/login",
            Some(&json!({
                "email": bob.user.email,
                "password": bob.plaintext_password,
                "pow": pow_for_alice,
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        400,
        "challenge ground for alice MUST NOT validate for bob; got: {}",
        resp.text()
    );
    let body = resp.text();
    assert!(
        body.contains("different account"),
        "metadata email-mismatch branch must surface the 'different account' \
         message — current body: {body}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn case_insensitive_email_round_trip_succeeds() {
    // Server normalizes email at mint (trim + lowercase) and at
    // verify (trim + lowercase). Test client mirrors that normalize.
    // Mint with an upper-cased + space-padded email; log in with a
    // lower-cased plain form. Should round-trip cleanly — guards
    // against any of those three sites dropping the normalize.
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();

    let canonical = admin.user.email.clone();
    let weird = format!("  {}  ", canonical.to_uppercase());

    let pow = con.mint_and_grind_pow(&weird).await.unwrap();
    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/login",
            Some(&json!({
                "email": canonical,
                "password": admin.plaintext_password,
                "pow": pow,
            })),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        200,
        "mint(upper+spaces) + login(lower) must succeed; got: {}",
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn adaptive_difficulty_escalates_after_subnet_failures() {
    // Fail login 10 times from the same subnet so the per-subnet
    // failure counter trips the first escalation step (difficulty
    // 19 → 21). Then mint a fresh challenge and assert the response
    // carries the elevated difficulty.
    //
    // We use a unique email per attempt so the per-EMAIL lockout
    // (which kicks in at 5 failures) doesn't 400 our later calls
    // before they reach the subnet-counter increment. The subnet
    // counter is per-subnet, NOT per-email, so unique emails still
    // aggregate into the same bucket. This mirrors the real attack
    // surface: a credential-stuffing botnet sweeping a stolen email
    // list from one /24.
    //
    // This is the marquee defense added in this change. Without
    // this e2e test, either side of the Redis key contract
    // (prefix, NX-then-INCR, GET-on-mint, threshold function) could
    // drift and the only symptom would be the wrong difficulty
    // showing up to users.
    let app = TestApp::spawn().await;
    let con = app.console_client();

    for i in 0..10 {
        // Each iteration uses a fresh email that doesn't exist in
        // the DB — the constant-time login path still runs the
        // failure branch (and thus the subnet INCR) because
        // password_valid stays false against the dummy hash.
        let email = format!("attacker-stuffing-{i}@example.test");
        let resp = con
            .post(
                "/api/auth/login",
                json!({
                    "email": email,
                    "password": "any-password-that-wont-match-1234",
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status,
            401,
            "wrong-password must 401; got: {}",
            resp.text()
        );
    }

    // Mint a fresh challenge — should now carry the elevated
    // difficulty (21, per `difficulty_for_subnet_failures(10)`).
    // Use yet another email so no per-email state leaks into the
    // mint path.
    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/pow-challenge",
            Some(&json!({ "email": "victim@example.test" })),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        200,
        "mint should succeed; got: {}",
        resp.text()
    );
    let body: serde_json::Value = resp.json().unwrap();
    let difficulty = body["difficulty"].as_u64().unwrap();
    assert_eq!(
        difficulty, 21,
        "10 subnet failures should escalate difficulty 19 → 21; \
         got difficulty={difficulty}, body={body}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn successful_login_clears_subnet_failure_counter() {
    // Decay model: a successful login from the subnet is strong
    // evidence the subnet is not actively being brute-forced.
    // Without this, an attacker can leave neighbours stuck at
    // difficulty 23 for the whole 10-min window even though the
    // /24 hosts a real user.
    //
    // Strategy: trip the escalation, log in successfully, then
    // re-mint and assert difficulty dropped back to base.
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();

    // Bump the subnet counter to 10 (first escalation tier).
    for i in 0..10 {
        let email = format!("decay-test-{i}@example.test");
        con.post(
            "/api/auth/login",
            json!({"email": email, "password": "wrong-password-here-1234"}),
        )
        .await
        .unwrap();
    }

    // Confirm escalation is active before we test the clear path.
    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/pow-challenge",
            Some(&json!({ "email": "probe@example.test" })),
        )
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(
        body["difficulty"].as_u64().unwrap(),
        21,
        "precondition: subnet should be at elevated difficulty"
    );

    // A successful login from this subnet must clear the counter.
    let resp = con
        .post(
            "/api/auth/login",
            json!({
                "email": admin.user.email,
                "password": admin.plaintext_password,
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();

    // Next mint should be back at base difficulty.
    let resp = con
        .send(
            reqwest::Method::POST,
            "/api/auth/pow-challenge",
            Some(&json!({ "email": "probe2@example.test" })),
        )
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(
        body["difficulty"].as_u64().unwrap(),
        19,
        "successful login should clear subnet counter; body: {body}"
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
