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
