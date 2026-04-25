//! Smoke test that boots the full server stack and exercises the
//! happy path of register → login → /me → logout. Proves the test
//! harness is functional before the per-surface tests pile on.

use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn boot_and_healthcheck() {
    let app = TestApp::spawn().await;

    let gw = app.gateway_client();
    gw.get("/health/live").await.unwrap().assert_ok();

    let con = app.console_client();
    con.get("/health/live").await.unwrap().assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn register_login_me_logout_cycle() {
    let app = TestApp::spawn().await;
    // Public registration is locked off by default; flip the flag
    // before driving the public-register flow.
    fixtures::set_setting(&app.db, "auth.allow_registration", json!(true))
        .await
        .unwrap();
    app.state.dynamic_config.reload().await.unwrap();

    let email = unique_email();
    let con = app.console_client();
    let resp = con
        .post(
            "/api/auth/register",
            json!({
                "email": email,
                "display_name": "Tester",
                "password": "Test_password_12345!"
            }),
        )
        .await
        .unwrap();
    resp.assert_status(200);
    // The cookie jar should now hold the access cookie.
    assert!(
        con.cookie("__Host-access_token").is_some(),
        "register should have set the access cookie"
    );

    let me = con.get("/api/auth/me").await.unwrap();
    me.assert_ok();
    let me_json: Json = me.json().unwrap();
    assert_eq!(me_json["email"].as_str().unwrap(), email);

    let logout = con.post_empty("/api/auth/logout").await.unwrap();
    logout.assert_ok();
    assert!(
        con.cookie("__Host-access_token").is_none(),
        "logout should have cleared the access cookie"
    );

    // /me without a cookie → 401
    let me2 = con.get("/api/auth/me").await.unwrap();
    me2.assert_status(401);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn login_round_trips_after_seed() {
    let app = TestApp::spawn().await;

    let admin = fixtures::create_admin_user(&app.db).await.unwrap();

    let con = app.console_client();
    let resp = con
        .post(
            "/api/auth/login",
            json!({
                "email": admin.user.email,
                "password": admin.plaintext_password
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();

    let me: Json = con.get("/api/auth/me").await.unwrap().json().unwrap();
    assert_eq!(me["email"].as_str().unwrap(), admin.user.email);
    let assignments = me["role_assignments"]
        .as_array()
        .expect("role_assignments array");
    assert!(
        assignments
            .iter()
            .any(|r| r["name"].as_str() == Some("super_admin")),
        "admin login should advertise super_admin: {assignments:?}"
    );
}
