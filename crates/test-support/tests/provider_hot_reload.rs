//! Provider hot-reload — admin PATCHes a provider's `base_url`
//! (or `is_active` / `config_json`) and the gateway router picks up
//! the change without a server restart. The test pins that contract
//! so a regression in `rebuild_gateway_router` doesn't silently
//! continue routing to the old URL.

use serde_json::Value;
use think_watch_test_support::prelude::*;

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn patch_provider_base_url_swaps_upstream_for_subsequent_requests() {
    let app = TestApp::spawn().await;

    // Stand up two upstreams. The first 500s (so we can detect when
    // it's still the active one), the second 200s.
    let bad = MockProvider::always_500().await;
    let good = MockProvider::openai_chat_ok("hot-swap").await;
    let bad_uri = bad.uri();
    let good_uri = good.uri();
    Box::leak(Box::new(bad));
    Box::leak(Box::new(good));

    // Seed provider pointing at the bad upstream + matching key.
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let provider =
        fixtures::create_provider(&app.db, &unique_name("hot-prov"), "openai", &bad_uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "hot-swap")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "hot-swap-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);

    // Pre-PATCH — bad upstream is in play, request must fail.
    let r = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "hot-swap", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    assert!(
        !r.status.is_success(),
        "expected non-2xx while bad upstream active, got {}",
        r.status
    );

    // PATCH the provider's base_url through the public admin path.
    // Admin login + signed mutating request — same path the web UI
    // takes.
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    // The admin handler runs `validate_url` which blocks loopback.
    // Bypass it by writing the new URL directly + manually calling
    // rebuild — same end state as the production path. (A future
    // test running against a non-loopback wiremock could hit the
    // PATCH endpoint instead.)
    sqlx::query("UPDATE providers SET base_url = $1 WHERE id = $2")
        .bind(&good_uri)
        .bind(provider.id)
        .execute(&app.db)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    // Same request now hits the good upstream.
    let r = gw
        .post(
            "/v1/chat/completions",
            json!({"model": "hot-swap", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap();
    r.assert_ok();
    let body: Value = r.json().unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hello world");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn deactivating_provider_removes_route_immediately() {
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("deactivate-me").await;
    let upstream_uri = upstream.uri();
    Box::leak(Box::new(upstream));

    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let provider =
        fixtures::create_provider(&app.db, &unique_name("dx"), "openai", &upstream_uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "deactivate-me")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "dx-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);

    // Active → 200.
    gw.post(
        "/v1/chat/completions",
        json!({"model": "deactivate-me", "messages": [{"role": "user", "content": "first"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Flip is_active=false + rebuild — gateway loads only active
    // providers, so the route disappears.
    sqlx::query("UPDATE providers SET is_active = false WHERE id = $1")
        .bind(provider.id)
        .execute(&app.db)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    // Different message body so the response cache (keyed on
    // request hash) doesn't replay the pre-deactivation response.
    let r = gw
        .post(
            "/v1/chat/completions",
            json!({
                "model": "deactivate-me",
                "messages": [{"role": "user", "content": "post-deactivate"}]
            }),
        )
        .await
        .unwrap();
    assert!(
        !r.status.is_success(),
        "deactivated provider should drop out of the router, got {}: {}",
        r.status,
        r.text()
    );
}
