//! Setup wizard end-to-end — first admin + first provider + first
//! API key in one POST.
//!
//! Already-initialised platforms are covered by
//! `auth::setup_status_and_initialize_flow`; this file goes wider:
//!
//!   - admin creation with super_admin assignment at global scope
//!   - optional provider seeding (validate URL, store config_json,
//!     show in /api/admin/providers afterwards)
//!   - API key minted, returned in the response, and immediately
//!     usable as `Authorization: Bearer tw-…`
//!   - rate-limit on repeated init attempts (>5/min trips 400)
//!   - subsequent attempt → 403 (advisory lock + DB check)

use serde_json::Value;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn first_everything_initialise_admin_provider_key() {
    let app = TestApp::spawn().await;
    let con = app.console_client();

    // Confirm pristine state.
    let status: Value = con.get("/api/setup/status").await.unwrap().json().unwrap();
    assert_eq!(status["initialized"], json!(false));

    let admin_email = unique_email();
    let admin_password = "FirstAdmin_1234!";
    let resp: Value = con
        .post(
            "/api/setup/initialize",
            json!({
                "admin": {
                    "email": admin_email,
                    "display_name": "First Admin",
                    "password": admin_password
                },
                "site_name": "ThinkWatch (E2E)",
                "provider": {
                    "name": unique_name("first-prov"),
                    "display_name": "First Provider",
                    "provider_type": "openai",
                    "base_url": "https://api.openai.com/v1",
                    "headers": [],
                    "config": {}
                }
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();

    let admin_id = resp["admin_id"].as_str().expect("admin_id in response");
    assert_eq!(resp["admin_email"], admin_email);
    assert!(
        resp["api_key"].is_string(),
        "setup must mint an admin API key: {resp}"
    );
    assert!(
        resp["api_key"].as_str().unwrap().starts_with("tw-"),
        "API key must use the `tw-` prefix"
    );
    assert!(
        resp["provider_id"].is_string(),
        "provider was supplied — provider_id must be returned"
    );

    // DB shape: super_admin assignment at global scope.
    let role_count: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM rbac_role_assignments ra
             JOIN rbac_roles r ON r.id = ra.role_id
            WHERE ra.user_id::text = $1
              AND ra.scope_kind = 'global'
              AND r.name = 'super_admin'"#,
    )
    .bind(admin_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(role_count, 1, "first admin must be super_admin/global");

    // setup.initialized flipped.
    let init: Option<Value> =
        sqlx::query_scalar("SELECT value FROM system_settings WHERE key = 'setup.initialized'")
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(init.as_ref().and_then(|v| v.as_bool()), Some(true));

    // Provider visible in admin list using the new admin's session.
    con.post(
        "/api/auth/login",
        json!({"email": admin_email, "password": admin_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    let providers: Value = con
        .get("/api/admin/providers")
        .await
        .unwrap()
        .json()
        .unwrap();
    let provs = providers
        .as_array()
        .or_else(|| providers.get("data").and_then(|v| v.as_array()))
        .expect("providers list");
    assert!(
        !provs.is_empty(),
        "/api/admin/providers must include the seeded provider"
    );

    // The minted API key must immediately authenticate to the AI
    // gateway. Without a model route the gateway returns 404 — but
    // that's a route lookup, not an auth failure, so it counts as
    // "passed the auth gate".
    let api_key = resp["api_key"].as_str().unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(api_key);
    let gw_resp = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "no-such-model", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    assert_ne!(
        gw_resp.status.as_u16(),
        401,
        "minted API key must pass auth — got 401: {}",
        gw_resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn second_initialize_attempt_is_rejected() {
    // Once initialised, the public endpoint must permanently
    // refuse — this is what protects an already-running deployment
    // from "POST /api/setup/initialize" from any unauthenticated
    // client on the network.
    let app = TestApp::spawn().await;
    let con = app.console_client();
    let body = json!({
        "admin": {
            "email": "first@example.com",
            "display_name": "First",
            "password": "Pass_first_123!"
        }
    });
    con.post("/api/setup/initialize", body.clone())
        .await
        .unwrap()
        .assert_ok();

    let resp = con
        .post(
            "/api/setup/initialize",
            json!({
                "admin": {
                    "email": "second@example.com",
                    "display_name": "Second",
                    "password": "Pass_second_123!"
                }
            }),
        )
        .await
        .unwrap();
    assert!(
        matches!(resp.status.as_u16(), 400 | 403),
        "second init must be refused, got {}",
        resp.status
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn rate_limited_at_six_attempts_per_minute() {
    let app = TestApp::spawn().await;
    let con = app.console_client();

    // 5 attempts with bad input (so each one short-circuits).
    for i in 0..5 {
        let resp = con
            .post(
                "/api/setup/initialize",
                json!({
                    "admin": {
                        "email": format!("malformed-{i}"),  // intentionally bad
                        "display_name": "x",
                        "password": "x"
                    }
                }),
            )
            .await
            .unwrap();
        assert!(
            matches!(resp.status.as_u16(), 400 | 422),
            "attempt {i} should fail validation but pass the rate gate, got {}",
            resp.status
        );
    }

    // 6th must trip the rate limit (`Too many setup attempts`).
    let r = con
        .post(
            "/api/setup/initialize",
            json!({
                "admin": {
                    "email": "rate-limited@example.com",
                    "display_name": "x",
                    "password": "x"
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(r.status.as_u16(), 400, "expected rate-limit 400");
    assert!(
        r.text().to_lowercase().contains("too many"),
        "expected rate-limit message, got {}",
        r.text()
    );
}
