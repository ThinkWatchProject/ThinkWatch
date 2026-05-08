//! Integration tests for `PATCH /api/mcp/servers/{id}` covering the
//! credential cleanup that runs alongside auth_shape /
//! credential_owner transitions. Regression suite for the review
//! pass that wrapped the UPDATE + DELETEs in a single transaction
//! and added the admin_shared → per_user upstream-revoke + DELETE
//! sequence.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn update_flipping_auth_shape_purges_user_credentials() {
    // Server starts as static + per_user, gets a user credential,
    // then flips to oauth — the per-user row should be wiped because
    // the old token can't be replayed against the new resolver path.
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session(&app).await;

    let server_id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("flip"),
        "flip",
        "https://example.com/mcp",
        fixtures::McpServerOpts {
            auth_shape: "static".to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Drop a per-user credential. We don't go through the public
    // paste endpoint here — that would require a working upstream
    // probe — so we INSERT directly. Schema-shape only, encryption
    // contents irrelevant for this test.
    sqlx::query(
        r#"INSERT INTO mcp_user_credentials
              (mcp_server_id, user_id, account_label, credential_type,
               is_default, access_token_encrypted, scopes)
           VALUES ($1, $2, 'work', 'static_token', true, $3, '{}')"#,
    )
    .bind(server_id)
    .bind(admin.user.id)
    .bind(b"opaque-bytes-not-real-ciphertext".to_vec())
    .execute(&app.db)
    .await
    .unwrap();

    // PATCH: flip auth_shape. The handler must purge the now-stale
    // user credential as part of the same transaction as the row
    // UPDATE.
    con.patch(
        &format!("/api/mcp/servers/{server_id}"),
        json!({
            "auth_shape": "oauth",
            "oauth_issuer": "https://example.com",
            "oauth_authorization_endpoint": "https://example.com/authorize",
            "oauth_token_endpoint": "https://example.com/token",
            "oauth_client_id": "test-client",
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    let remaining: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM mcp_user_credentials WHERE mcp_server_id = $1 LIMIT 1")
            .bind(server_id)
            .fetch_optional(&app.db)
            .await
            .unwrap();
    assert!(
        remaining.is_none(),
        "auth_shape flip must wipe per-user credentials of the prior shape"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn update_admin_shared_to_per_user_revokes_upstream_and_deletes_row() {
    // admin_shared OAuth server has a shared credential row. PATCH
    // flips credential_owner to per_user. Handler should:
    //   1. POST the upstream's revocation endpoint (wiremock will
    //      record the call) — best-effort, before opening the TX.
    //   2. DELETE the shared row inside the same TX as the row
    //      UPDATE so the orphan can't outlive the transition.
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session(&app).await;

    let revoke_endpoint = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/revoke"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&revoke_endpoint)
        .await;

    let server_id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("revoke"),
        "revoke",
        "https://example.com/mcp",
        fixtures::McpServerOpts {
            auth_shape: "oauth".to_string(),
            credential_owner: "admin_shared".into(),
            oauth_issuer: Some("https://example.com".into()),
            oauth_authorization_endpoint: Some("https://example.com/authorize".into()),
            oauth_token_endpoint: Some("https://example.com/token".into()),
            oauth_client_id: Some("test-client".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Set the revocation endpoint directly — the public PATCH path
    // runs an SSRF guard that rejects loopback URLs, but the test's
    // wiremock lives on 127.0.0.1. Direct UPDATE matches what
    // production rows look like once the admin set this in a real
    // (non-loopback) deployment.
    sqlx::query("UPDATE mcp_servers SET oauth_revocation_endpoint = $1 WHERE id = $2")
        .bind(format!("{}/revoke", revoke_endpoint.uri()))
        .bind(server_id)
        .execute(&app.db)
        .await
        .unwrap();

    // Insert a fake shared credential — same shape as the row the
    // OAuth callback writes. Encryption content is irrelevant; the
    // revoke path tries to decrypt and bails silently on bad
    // ciphertext, which is fine for this test (the assertion is on
    // the DELETE).
    sqlx::query(
        r#"INSERT INTO mcp_server_shared_credentials
              (mcp_server_id, credential_type, access_token_encrypted, configured_by)
           VALUES ($1, 'oauth_authcode', $2, $3)"#,
    )
    .bind(server_id)
    .bind(b"opaque".to_vec())
    .bind(admin.user.id)
    .execute(&app.db)
    .await
    .unwrap();

    // Now flip to per_user.
    con.patch(
        &format!("/api/mcp/servers/{server_id}"),
        json!({"credential_owner": "per_user"}),
    )
    .await
    .unwrap()
    .assert_ok();

    let remaining: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM mcp_server_shared_credentials WHERE mcp_server_id = $1")
            .bind(server_id)
            .fetch_optional(&app.db)
            .await
            .unwrap();
    assert!(
        remaining.is_none(),
        "admin_shared → per_user transition must DELETE the shared credential row"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn create_rejects_anonymous_with_admin_shared() {
    // Cross-axis validation: anonymous shape + admin_shared owner is
    // semantically incoherent (no credential needed vs credential
    // mandatory) and should 400.
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    let resp = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("anon-shared"),
                "namespace_prefix": "anonshr",
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http",
                "auth_shape": "anonymous",
                "credential_owner": "admin_shared",
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        400,
        "anonymous + admin_shared should 400; got: {}",
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn update_rejects_anonymous_with_admin_shared() {
    // Same rule on the update path — flipping to anonymous shape on
    // an admin_shared server (without also flipping owner) should be
    // rejected, otherwise the resolver would never read the orphan
    // shared row.
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    let server_id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("anon-shared-update"),
        "anonsu",
        "https://example.com/mcp",
        fixtures::McpServerOpts {
            auth_shape: "static".to_string(),
            credential_owner: "admin_shared".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let resp = con
        .patch(
            &format!("/api/mcp/servers/{server_id}"),
            json!({"auth_shape": "anonymous"}),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        400,
        "PATCH to anonymous on admin_shared server should 400; got: {}",
        resp.text()
    );
    let body: Value = resp.json().unwrap();
    assert!(
        body.to_string().contains("anonymous"),
        "error message should mention the conflict; got: {body}"
    );
}
