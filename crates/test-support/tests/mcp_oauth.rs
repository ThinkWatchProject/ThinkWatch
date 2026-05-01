//! Integration tests for the per-user MCP credential surface
//! (`mcp_oauth.rs` handler) and the resolver-driven tool-call path.
//!
//! Drives the real handlers — login → list / paste / set_default /
//! revoke — and a tool call through the MCP gateway with a real
//! `tw-…` API key, asserting that the upstream wiremock saw the
//! right `Authorization` header (i.e. that `UserTokenResolver`
//! decrypted the credential and the pool injected it per-call).
//!
//! The MCP server row is inserted via `fixtures::create_mcp_server_with`
//! rather than the public `POST /api/mcp/servers` route because the
//! SSRF guard there rejects loopback URLs (and our wiremock lives on
//! 127.0.0.1).
//!
//! The OAuth Authorization Code path is covered by unit tests in
//! `crates/server/src/handlers/mcp_oauth.rs` (PKCE digest against
//! the RFC 7636 test vector, HMAC binding sensitivity, fragment
//! encoder). Driving the full code-exchange roundtrip would need
//! a second wiremock standing in for the upstream's authorize page
//! + token endpoint on top of the upstream MCP itself — out of scope
//! for this PR.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Wiremock fake of an MCP server. Responds to JSON-RPC `tools/list`
/// with one tool (`echo`) and to `tools/call` with a small `result`.
/// Records every request so the test can assert on headers.
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
                    "description": "Echo back the input",
                    "inputSchema": {"type": "object"}
                }]
            }
        })))
        .mount(&server)
        .await;
    server
}

async fn login(con: &TestClient, user: &fixtures::SeededUser) {
    con.post(
        "/api/auth/login",
        json!({
            "email": user.user.email,
            "password": user.plaintext_password,
        }),
    )
    .await
    .unwrap()
    .assert_ok();
}

async fn seed_static_server(app: &TestApp, upstream_uri: &str, prefix: &str) -> Uuid {
    fixtures::create_mcp_server_with(
        &app.db,
        &unique_name(&format!("oauth-{prefix}")),
        prefix,
        &format!("{upstream_uri}/mcp"),
        fixtures::McpServerOpts {
            allow_static_token: true,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn paste_token_then_list_default_revoke_round_trip() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let upstream = mcp_upstream().await;
    let server_id = seed_static_server(&app, &upstream.uri(), "rt").await;

    let con = app.console_client();
    login(&con, &admin).await;

    // Paste a token under the "work" label — should land as default
    // (first credential for this (server, user)).
    con.put(
        &format!("/api/mcp/connections/{server_id}/work/static-token"),
        json!({"token": "pat-work-1"}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Listing should show one default account.
    let conns: Value = con
        .get("/api/mcp/connections")
        .await
        .unwrap()
        .json()
        .unwrap();
    let entry = conns
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["server_id"].as_str() == Some(&server_id.to_string()))
        .expect("connection list missing the registered server");
    assert!(entry["allow_static_token"].as_bool().unwrap_or(false));
    assert_eq!(entry["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(entry["accounts"][0]["account_label"], "work");
    assert_eq!(entry["accounts"][0]["is_default"], true);
    assert_eq!(entry["accounts"][0]["credential_type"], "static_token");

    // Add a second account "personal" — first one stays default.
    con.put(
        &format!("/api/mcp/connections/{server_id}/personal/static-token"),
        json!({"token": "pat-personal-1"}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Promote "personal" to default — partial unique index must let
    // the swap happen atomically without ever seeing two defaults.
    con.put(
        &format!("/api/mcp/connections/{server_id}/personal/default"),
        json!({}),
    )
    .await
    .unwrap()
    .assert_ok();

    let conns: Value = con
        .get("/api/mcp/connections")
        .await
        .unwrap()
        .json()
        .unwrap();
    let accounts = conns
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["server_id"].as_str() == Some(&server_id.to_string()))
        .unwrap()["accounts"]
        .as_array()
        .unwrap()
        .clone();
    let work = accounts
        .iter()
        .find(|a| a["account_label"] == "work")
        .unwrap();
    let personal = accounts
        .iter()
        .find(|a| a["account_label"] == "personal")
        .unwrap();
    assert_eq!(work["is_default"], false);
    assert_eq!(personal["is_default"], true);

    // Revoke "work" — the default ("personal") survives untouched.
    con.delete(&format!("/api/mcp/connections/{server_id}/work"))
        .await
        .unwrap()
        .assert_ok();

    let conns: Value = con
        .get("/api/mcp/connections")
        .await
        .unwrap()
        .json()
        .unwrap();
    let accounts = conns
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["server_id"].as_str() == Some(&server_id.to_string()))
        .unwrap()["accounts"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["account_label"], "personal");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn static_token_round_trips_to_upstream_as_bearer() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let upstream = mcp_upstream().await;
    let server_id = seed_static_server(&app, &upstream.uri(), "stat").await;

    let con = app.console_client();
    login(&con, &admin).await;

    // Trigger the discovery endpoint synchronously so the gateway
    // registry knows about the `echo` tool — without it
    // `find_server_for_tool` won't resolve `stat__echo` and the call
    // bounces with INVALID_PARAMS.
    con.post(&format!("/api/mcp/servers/{server_id}/discover"), json!({}))
        .await
        .unwrap()
        .assert_ok();

    con.put(
        &format!("/api/mcp/connections/{server_id}/work/static-token"),
        json!({"token": "live-secret"}),
    )
    .await
    .unwrap()
    .assert_ok();

    let api_key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "mcp-bearer-test",
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&api_key.plaintext);
    gw.post(
        "/mcp",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "stat__echo",
                "arguments": {"text": "hi"}
            }
        }),
    )
    .await
    .unwrap();

    // The wiremock recorded every inbound request. Two land here: the
    // discover endpoint's `tools/list` probe (anonymous, no
    // Authorization) and our `tools/call` (must carry the freshly-
    // pasted PAT as a Bearer).
    let received = upstream.received_requests().await.unwrap();
    let with_bearer = received
        .iter()
        .find(|r| {
            r.headers.get("Authorization").and_then(|v| v.to_str().ok())
                == Some("Bearer live-secret")
        })
        .expect("upstream never saw a request carrying the user's static token");
    let body: Value = serde_json::from_slice(&with_bearer.body).unwrap();
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], "echo");
}

/// Wiremock fake of an OAuth provider's `/token` and `/userinfo`
/// endpoints. Reuses one MockServer for both routes — that's how
/// real OIDC providers tend to ship them anyway.
async fn oauth_provider() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "live-access-token",
            "refresh_token": "live-refresh-token",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "read",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "preferred_username": "octocat",
            "email": "octocat@example.com",
        })))
        .mount(&server)
        .await;
    server
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn oauth_callback_populates_upstream_subject_via_userinfo() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let upstream_mcp = mcp_upstream().await;
    let provider = oauth_provider().await;

    // Build server with OAuth client config pointing at the wiremock
    // provider, including the userinfo URL the resolver will hit
    // after a successful token exchange.
    let enc_key = think_watch_common::crypto::parse_encryption_key(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let client_secret_encrypted =
        think_watch_common::crypto::encrypt(b"shh-its-a-secret", &enc_key).unwrap();
    let server_id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("oauth-userinfo"),
        "uinfo",
        &format!("{}/mcp", upstream_mcp.uri()),
        fixtures::McpServerOpts {
            allow_static_token: false,
            oauth_issuer: Some(provider.uri()),
            oauth_authorization_endpoint: Some(format!("{}/authorize", provider.uri())),
            oauth_token_endpoint: Some(format!("{}/token", provider.uri())),
            oauth_userinfo_endpoint: Some(format!("{}/userinfo", provider.uri())),
            oauth_client_id: Some("test-client".into()),
            oauth_client_secret_encrypted: Some(client_secret_encrypted),
            oauth_scopes: vec!["read".into()],
        },
    )
    .await
    .unwrap();

    let con = app.console_client();
    login(&con, &admin).await;

    // Kick off authorize. Response carries the URL the user's browser
    // would follow next — we extract the state token from it so we
    // can drive the callback ourselves.
    let auth_resp = con
        .post(
            &format!("/api/mcp/connections/{server_id}/authorize"),
            json!({"account_label": "work"}),
        )
        .await
        .unwrap();
    auth_resp.assert_ok();
    let authorize_url = auth_resp.json::<Value>().unwrap()["authorize_url"]
        .as_str()
        .unwrap()
        .to_string();
    let parsed = url::Url::parse(&authorize_url).unwrap();
    let state_token = parsed
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("authorize_url missing state param");

    // Drive the callback. The wiremock provider will return a token,
    // and the resolver will GET /userinfo with that token.
    let cb = con
        .get(&format!(
            "/api/mcp/oauth/callback?code=fake-code&state={state_token}"
        ))
        .await
        .unwrap();
    // Callback returns 307 redirect to /connections#connected=...
    assert!(
        cb.status.is_redirection(),
        "expected redirect, got {}",
        cb.status
    );

    // Userinfo round-trip should have happened with the access_token.
    let received = provider.received_requests().await.unwrap();
    received
        .iter()
        .find(|r: &&Request| {
            r.url.path() == "/userinfo"
                && r.headers.get("Authorization").and_then(|v| v.to_str().ok())
                    == Some("Bearer live-access-token")
        })
        .expect("userinfo wasn't called with the freshly-issued access_token");

    // The credential row should now carry upstream_subject from the
    // /userinfo response (preferred_username = "octocat").
    let conns: Value = con
        .get("/api/mcp/connections")
        .await
        .unwrap()
        .json()
        .unwrap();
    let entry = conns
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["server_id"].as_str() == Some(&server_id.to_string()))
        .unwrap();
    assert_eq!(entry["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(entry["accounts"][0]["upstream_subject"], "octocat");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn tool_call_without_credential_returns_needs_user_credentials() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let upstream = mcp_upstream().await;
    let server_id = seed_static_server(&app, &upstream.uri(), "needs").await;

    let con = app.console_client();
    login(&con, &admin).await;
    con.post(&format!("/api/mcp/servers/{server_id}/discover"), json!({}))
        .await
        .unwrap()
        .assert_ok();

    // No paste. Tool call must surface the structured -32050 error.
    let api_key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "mcp-needs-test",
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&api_key.plaintext);
    let resp = gw
        .post(
            "/mcp",
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "needs__echo",
                    "arguments": {}
                }
            }),
        )
        .await
        .unwrap();
    let body: Value = resp.json().unwrap();
    let err = body
        .get("error")
        .expect("expected JSON-RPC error for missing credential");
    assert_eq!(err["code"], -32050);
    assert_eq!(err["data"]["kind"], "needs_user_credentials");
    assert_eq!(err["data"]["server_id"], server_id.to_string());
}

// ---------------------------------------------------------------------------
// POST /api/mcp/connections/{server_id}/{account_label}/test
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn test_connection_with_static_token_returns_tools_preview() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let upstream = mcp_upstream().await;
    let server_id = seed_static_server(&app, &upstream.uri(), "tprev").await;

    let con = app.console_client();
    login(&con, &admin).await;

    // Paste a token, then probe.
    con.put(
        &format!("/api/mcp/connections/{server_id}/work/static-token"),
        json!({"token": "live-secret"}),
    )
    .await
    .unwrap()
    .assert_ok();

    let resp = con
        .post(
            &format!("/api/mcp/connections/{server_id}/work/test"),
            json!({}),
        )
        .await
        .unwrap();
    resp.assert_ok();

    let body: Value = resp.json().unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["tools_count"], 1);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");

    // The probe must have carried the user's token — confirms we
    // didn't fall through to the anonymous path.
    let received = upstream.received_requests().await.unwrap();
    let with_bearer = received.iter().find(|r| {
        r.headers.get("Authorization").and_then(|v| v.to_str().ok()) == Some("Bearer live-secret")
    });
    assert!(
        with_bearer.is_some(),
        "upstream never saw the user's bearer token during the probe"
    );

    // Read-only contract: server-level columns must NOT be touched
    // by a user-driven test (admin's `status` view stays clean even
    // if a user's credential is bad).
    let row: (Option<chrono::DateTime<chrono::Utc>>, Option<Value>) = sqlx::query_as(
        "SELECT last_health_check, cached_tools_jsonb FROM mcp_servers WHERE id = $1",
    )
    .bind(server_id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert!(
        row.0.is_none() && row.1.is_none(),
        "test_connection must not write last_health_check / cached_tools_jsonb"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn test_connection_unknown_account_label_returns_404() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let upstream = mcp_upstream().await;
    let server_id = seed_static_server(&app, &upstream.uri(), "tnone").await;

    let con = app.console_client();
    login(&con, &admin).await;

    let resp = con
        .post(
            &format!("/api/mcp/connections/{server_id}/never-pasted/test"),
            json!({}),
        )
        .await
        .unwrap();
    resp.assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn test_connection_returns_failure_when_upstream_rejects_token() {
    // Stand up a custom upstream that 401s on `tools/list` so we can
    // exercise the unhappy path without touching the resolver — the
    // credential decrypts fine, the upstream just doesn't accept it.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&upstream)
        .await;

    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let server_id = seed_static_server(&app, &upstream.uri(), "trej").await;

    let con = app.console_client();
    login(&con, &admin).await;
    con.put(
        &format!("/api/mcp/connections/{server_id}/work/static-token"),
        json!({"token": "stale-token"}),
    )
    .await
    .unwrap()
    .assert_ok();

    let resp = con
        .post(
            &format!("/api/mcp/connections/{server_id}/work/test"),
            json!({}),
        )
        .await
        .unwrap();
    resp.assert_ok();

    let body: Value = resp.json().unwrap();
    assert_eq!(body["success"], false);
    assert!(body.get("tools").is_none() || body["tools"].is_null());
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("401")
            || body["message"]
                .as_str()
                .unwrap_or("")
                .contains("unauthorized"),
        "expected message to surface the 401 — got {body:?}"
    );
}
