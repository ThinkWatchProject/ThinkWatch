//! Integration tests for the new-server registration wizard's
//! pre-creation OAuth admin_shared flow.
//!
//! Covers the moving parts that bypass the existing per-user / on-row
//! admin-shared paths:
//!
//!   1. **Wizard authorize endpoint** stashes OAuth client config in
//!      Redis under a state token, then the callback writes the
//!      resulting tokens to `mcp_wizard:cred:{wizard_session_id}`.
//!   2. **`POST /api/mcp/servers` with `wizard_session_id`** GETDELs
//!      the Redis blob and atomically inserts both the server row
//!      and the matching `mcp_server_shared_credentials` row.
//!   3. **`POST /api/mcp/servers` with `shared_static_token`** is the
//!      no-OAuth path — admin pasted a token in Step 3.
//!   4. **Mutually exclusive guard**: passing both 400s.
//!   5. **`wizard_session_id` without `credential_owner=admin_shared`**
//!      400s — the field only makes sense for shared mode.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mcp_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo",
                    "inputSchema": {"type": "object"}
                }]
            }
        })))
        .mount(&server)
        .await;
    server
}

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
async fn wizard_create_with_shared_static_token_round_trip() {
    // Admin walks Step 1-3 without ever leaving the wizard, pastes a
    // shared PAT in Step 3, hits Save. Single API call lands the row
    // + shared credential in one transaction.
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;
    let upstream = mcp_upstream().await;

    let _ = upstream;
    let resp = con
        .post(
            "/api/mcp/servers",
            // Public endpoint — the SSRF guard (`validate_url`)
            // rejects loopback URLs, so we use the same dummy as
            // mcp.rs::mcp_servers_create_list_delete_cycle. The
            // wizard credential transfer is the actual subject of
            // this test, not the upstream call.
            json!({
                "name": unique_name("wizard-static"),
                "namespace_prefix": "wzst",
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http",
                "auth_shape": "static",
                "credential_owner": "admin_shared",
                "shared_static_token": "shared-paste-bearer",
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        200,
        "create_server should succeed; got: {}",
        resp.text()
    );
    let body: Value = resp.json().unwrap();

    let server_id = body["id"].as_str().unwrap();

    // Shared cred row landed alongside the server.
    let exists: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM mcp_server_shared_credentials WHERE mcp_server_id = $1")
            .bind(uuid::Uuid::parse_str(server_id).unwrap())
            .fetch_optional(&app.db)
            .await
            .unwrap();
    assert_eq!(exists, Some(1), "shared credential row should exist");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn wizard_create_rejects_both_credential_paths() {
    // Both `wizard_session_id` and `shared_static_token` set is a
    // mistake — the API surfaces a 400 instead of silently picking one.
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    let resp = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("wizard-conflict"),
                "namespace_prefix": "wzcf",
                "endpoint_url": "http://example.com/mcp",
                "transport_type": "streamable_http",
                "auth_shape": "static",
                "credential_owner": "admin_shared",
                "wizard_session_id": "fake-session",
                "shared_static_token": "also-a-token",
            }),
        )
        .await
        .unwrap();
    assert_eq!(resp.status, 400, "both fields should 400");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn wizard_credential_fields_require_admin_shared() {
    // `wizard_session_id` is meaningless for per_user — guard against
    // an admin accidentally pasting a wizard session into a per_user
    // create call.
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    let resp = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("wizard-mismatch"),
                "namespace_prefix": "wzmm",
                "endpoint_url": "http://example.com/mcp",
                "transport_type": "streamable_http",
                "credential_owner": "per_user",
                "shared_static_token": "wrong-mode-token",
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status, 400,
        "per_user + shared_static_token should 400"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn wizard_session_id_with_missing_redis_blob_400s() {
    // The Redis TTL is 1h; if the admin walks away too long, the
    // blob's gone and Save must surface a clear "re-run authorize"
    // rather than create a half-baked server with no credential.
    let app = TestApp::spawn().await;
    let (con, _) = admin_session(&app).await;

    let resp = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("wizard-stale"),
                "namespace_prefix": "wzst2",
                "endpoint_url": "http://example.com/mcp",
                "transport_type": "streamable_http",
                "credential_owner": "admin_shared",
                "wizard_session_id": "this-id-is-not-in-redis",
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status, 400,
        "missing wizard credential blob should 400 with 'rerun authorize'"
    );
}

/// Helper: stand up a fake OAuth provider returning a deterministic
/// access_token / refresh_token pair on `/token` and a userinfo blob
/// on `/userinfo`.
async fn wizard_oauth_provider() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "wizard-access-token",
            "refresh_token": "wizard-refresh-token",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "read",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "preferred_username": "wizard-bot",
        })))
        .mount(&server)
        .await;
    server
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn wizard_oauth_admin_shared_full_round_trip() {
    // Full wizard OAuth admin_shared flow:
    //   1. POST /api/admin/mcp/oauth-wizard-authorize → state token
    //      stashed in Redis under OAUTH_STATE_PREFIX, browser-redirect
    //      URL returned.
    //   2. Drive callback /api/mcp/oauth/callback?code=…&state=… →
    //      WizardAdminShared arm exchanges the code at the wiremock
    //      provider, fetches userinfo, writes the credential blob to
    //      `mcp_wizard:cred:{configured_by}:{wizard_session_id}`.
    //   3. GET /api/admin/mcp/wizards/{id}/credential-status → 200,
    //      shape includes credential_type=oauth_authcode and the
    //      upstream_subject from /userinfo.
    //   4. POST /api/mcp/servers with `wizard_session_id` →
    //      claim_wizard_credential GETDELs the blob and the server
    //      row + mcp_server_shared_credentials row land atomically.
    //   5. Status endpoint should now 404 (blob consumed).
    //
    // This pins the round-trip the audit flagged as untested.
    let app = TestApp::spawn().await;
    let (con, _admin) = admin_session(&app).await;
    let provider = wizard_oauth_provider().await;

    let session_id = unique_name("wiz-sess");

    // Phase 1 — authorize.
    let auth_resp = con
        .post(
            "/api/admin/mcp/oauth-wizard-authorize",
            json!({
                "wizard_session_id": session_id,
                "oauth_authorization_endpoint": format!("{}/authorize", provider.uri()),
                "oauth_token_endpoint": format!("{}/token", provider.uri()),
                "oauth_client_id": "test-wizard-client",
                "oauth_client_secret": "shh-its-a-wizard",
                "oauth_scopes": ["read"],
                "oauth_userinfo_endpoint": format!("{}/userinfo", provider.uri()),
            }),
        )
        .await
        .unwrap();
    auth_resp.assert_ok();
    let authorize_url = auth_resp.json::<Value>().unwrap()["authorize_url"]
        .as_str()
        .unwrap()
        .to_string();
    let state_token = url::Url::parse(&authorize_url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("authorize_url missing state param");

    // Phase 2 — drive the callback. Wiremock provider's /token returns
    // wizard-access-token / wizard-refresh-token; /userinfo returns
    // preferred_username=wizard-bot.
    let cb = con
        .get(&format!(
            "/api/mcp/oauth/callback?code=fake-wizard-code&state={state_token}"
        ))
        .await
        .unwrap();
    assert!(
        cb.status.is_redirection(),
        "callback should redirect to /mcp/servers/new#wizard_resume=…, got {}",
        cb.status
    );

    // Phase 3 — credential-status endpoint reflects the staged blob.
    let status: Value = con
        .get(&format!(
            "/api/admin/mcp/wizards/{session_id}/credential-status"
        ))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(status["credential_type"], "oauth_authcode");
    assert_eq!(
        status["upstream_subject"], "wizard-bot",
        "userinfo round-trip should populate upstream_subject"
    );

    // Phase 4 — claim the blob via create_server. The server row
    // should land alongside a shared-credential row in one TX.
    let create_resp = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("wizard-oauth"),
                "namespace_prefix": "wzoa",
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http",
                "auth_shape": "oauth",
                "credential_owner": "admin_shared",
                "wizard_session_id": session_id,
                // Echo the OAuth client config so the new server row
                // can refresh the access_token later (the wizard form
                // collects these in Step 2 alongside the authorize).
                "oauth_authorization_endpoint": format!("{}/authorize", provider.uri()),
                "oauth_token_endpoint": format!("{}/token", provider.uri()),
                "oauth_userinfo_endpoint": format!("{}/userinfo", provider.uri()),
                "oauth_client_id": "test-wizard-client",
                "oauth_client_secret": "shh-its-a-wizard",
                "oauth_scopes": ["read"],
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        create_resp.status,
        200,
        "create_server with wizard_session_id should succeed; got: {}",
        create_resp.text()
    );
    let server_id = create_resp.json::<Value>().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let shared_row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT credential_type, upstream_subject \
           FROM mcp_server_shared_credentials WHERE mcp_server_id = $1",
    )
    .bind(uuid::Uuid::parse_str(&server_id).unwrap())
    .fetch_optional(&app.db)
    .await
    .unwrap();
    let row = shared_row.expect("shared credential row should exist");
    assert_eq!(row.0, "oauth_authcode");
    assert_eq!(row.1.as_deref(), Some("wizard-bot"));

    // Phase 5 — blob is consumed (GETDEL). Status endpoint now 404s.
    let status_after = con
        .get(&format!(
            "/api/admin/mcp/wizards/{session_id}/credential-status"
        ))
        .await
        .unwrap();
    assert_eq!(
        status_after.status, 404,
        "blob must be consumed exactly once after create"
    );
}
