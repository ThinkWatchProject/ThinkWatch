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
use wiremock::{Mock, MockServer, ResponseTemplate};

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
