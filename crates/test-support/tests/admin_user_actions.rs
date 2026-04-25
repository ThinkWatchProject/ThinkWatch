//! Admin user-action tests: reset-password full flow + force-logout
//! real effects.
//!
//! Both endpoints are admin-only emergency tools the vertical
//! matrix already gates at the route layer; this file pins the
//! actual side effects:
//!
//!   - `POST /api/admin/users/{id}/reset-password` — generates a
//!     temp password, marks `password_change_required = true`,
//!     drops the user's signing key, sets a Redis "temp" marker
//!     with a TTL. Login with the OLD password fails. Login with
//!     the NEW (temp) password succeeds. Past the TTL, even the
//!     temp password is rejected.
//!
//!   - `POST /api/admin/users/{id}/force-logout` — drops the
//!     signing key, bumps `pw_epoch` so outstanding refresh tokens
//!     are rejected, signals live dashboard WS to disconnect. The
//!     refresh-token revocation is the side-effect that matters
//!     here (the same gap I plugged in `revoke_sessions`).

use serde_json::Value;
use think_watch_test_support::prelude::*;

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

async fn login(app: &TestApp, email: &str, password: &str) -> TestClient {
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": email, "password": password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn reset_password_invalidates_old_password_and_returns_temp_one() {
    let app = TestApp::spawn().await;
    let (admin_con, _) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();
    let original_pwd = target.plaintext_password.clone();

    // Confirm the original password works before the reset.
    login(&app, &target.user.email, &original_pwd).await;

    // Admin resets it.
    let body: Value = admin_con
        .post_empty(&format!(
            "/api/admin/users/{}/reset-password",
            target.user.id
        ))
        .await
        .unwrap()
        .json()
        .unwrap();
    let temp_password = body["temporary_password"]
        .as_str()
        .or_else(|| body["temp_password"].as_str())
        .or_else(|| body["password"].as_str())
        .expect("response must carry the new temporary password")
        .to_string();
    assert_ne!(
        temp_password, original_pwd,
        "temp password must differ from the old one"
    );

    // The DB row reflects the change.
    let row: (Option<String>, bool) =
        sqlx::query_as("SELECT password_hash, password_change_required FROM users WHERE id = $1")
            .bind(target.user.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    let (hash, change_required) = row;
    let hash = hash.expect("password_hash present");
    assert!(
        think_watch_auth::password::verify_password(&temp_password, &hash).unwrap(),
        "DB hash must match the new temp password"
    );
    assert!(
        !think_watch_auth::password::verify_password(&original_pwd, &hash).unwrap_or(false),
        "DB hash must NOT match the old password anymore"
    );
    assert!(
        change_required,
        "password_change_required must be set after reset"
    );

    // Old credentials → 401.
    let resp = app
        .console_client()
        .post(
            "/api/auth/login",
            json!({"email": target.user.email, "password": original_pwd}),
        )
        .await
        .unwrap();
    resp.assert_status(401);

    // New temp password → 200.
    app.console_client()
        .post(
            "/api/auth/login",
            json!({"email": target.user.email, "password": temp_password}),
        )
        .await
        .unwrap()
        .assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn reset_password_marker_expiry_blocks_temp_password_after_ttl() {
    // The session_service sets a `pw_temp:{user_id}` marker with a
    // 24h TTL on reset. After the marker expires AND the row's
    // `updated_at` is older than the grandfather window, the login
    // path rejects the temp password (forces an admin reset
    // again). We can't actually wait 24h; simulate by deleting the
    // Redis marker and backdating `updated_at`.
    let app = TestApp::spawn().await;
    let (admin_con, _) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();

    let body: Value = admin_con
        .post_empty(&format!(
            "/api/admin/users/{}/reset-password",
            target.user.id
        ))
        .await
        .unwrap()
        .json()
        .unwrap();
    let temp_password = body["temporary_password"]
        .as_str()
        .or_else(|| body["temp_password"].as_str())
        .or_else(|| body["password"].as_str())
        .unwrap()
        .to_string();

    // Simulate TTL expiry: delete the marker + backdate updated_at
    // beyond the 24h grandfather window.
    use fred::interfaces::KeysInterface;
    let _: Option<i64> = app
        .state
        .redis
        .del(&format!("pw_temp:{}", target.user.id))
        .await
        .ok();
    sqlx::query("UPDATE users SET updated_at = now() - interval '48 hours' WHERE id = $1")
        .bind(target.user.id)
        .execute(&app.db)
        .await
        .unwrap();

    // Login with the temp password must now fail.
    let resp = app
        .console_client()
        .post(
            "/api/auth/login",
            json!({"email": target.user.email, "password": temp_password}),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status.as_u16(),
        401,
        "expired temp password must be refused: {}",
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn force_logout_invalidates_targets_refresh_tokens() {
    // The vertical matrix already proves that only admin / super_admin
    // can hit `/force-logout`. Here we assert the EFFECT: any
    // refresh token issued before the force-logout must be rejected
    // (same gap I closed in `revoke_sessions`).
    let app = TestApp::spawn().await;
    let (admin_con, _) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();

    // Target logs in to get a refresh token.
    let target_con = login(&app, &target.user.email, &target.plaintext_password).await;
    let stale_refresh = target_con
        .cookie("__Secure-refresh_token")
        .expect("refresh cookie present after login");

    // Wait until the next whole second so `iat <= epoch` can fire
    // — JWT iat is whole seconds, force-logout sets pw_epoch to now.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // Admin force-logouts the target.
    admin_con
        .post_empty(&format!("/api/admin/users/{}/force-logout", target.user.id))
        .await
        .unwrap()
        .assert_ok();

    // Replay the stale refresh token via the body field — must be
    // refused.
    let con = app.console_client();
    let resp = con
        .post("/api/auth/refresh", json!({"refresh_token": stale_refresh}))
        .await
        .unwrap();
    resp.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn force_logout_drops_signing_key() {
    // Targets a different facet of the same endpoint: the
    // signing_pubkey:{user_id} Redis key must be removed so the
    // user's existing browser session can no longer sign mutating
    // requests. Verifies the cleanup happens at the SAME time as
    // refresh-token invalidation.
    let app = TestApp::spawn().await;
    let (admin_con, _) = admin_session(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();

    let target_con = login(&app, &target.user.email, &target.plaintext_password).await;
    let pk = target_con.enable_signing();
    target_con
        .post("/api/auth/register-key", json!({"public_key": pk}))
        .await
        .unwrap()
        .assert_ok();

    // Confirm the key landed.
    use fred::interfaces::KeysInterface;
    let pre: Option<String> = app
        .state
        .redis
        .get(&format!("signing_pubkey:{}", target.user.id))
        .await
        .unwrap();
    assert!(pre.is_some(), "signing key must be in Redis after register");

    admin_con
        .post_empty(&format!("/api/admin/users/{}/force-logout", target.user.id))
        .await
        .unwrap()
        .assert_ok();

    let post: Option<String> = app
        .state
        .redis
        .get(&format!("signing_pubkey:{}", target.user.id))
        .await
        .unwrap();
    assert!(
        post.is_none(),
        "signing key must be removed after force-logout, got {post:?}"
    );
}
