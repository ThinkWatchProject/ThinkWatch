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
//! `crates/server/src/handlers/mcp_oauth.rs` (PKCE digest against the
//! RFC 7636 test vector, HMAC binding sensitivity, fragment encoder).
//! Driving the full code-exchange roundtrip would need a second
//! wiremock standing in for the upstream's authorize page + token
//! endpoint on top of the upstream MCP itself — out of scope for
//! this PR.

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
            auth_shape: "static".to_string(),
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
    assert_eq!(entry["auth_shape"].as_str(), Some("static"));
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

    // The wiremock recorded every inbound request. Several land here:
    //   * the discover endpoint's `tools/list` probe (anonymous)
    //   * the eager per-user tool-discovery POST that paste_static_token
    //     fires after persisting the credential (`tools/list` with the
    //     new bearer)
    //   * our `tools/call` (must carry the freshly-pasted PAT as a Bearer)
    // We only care about the tools/call here — find that specific
    // request and verify it carries the bearer.
    let received = upstream.received_requests().await.unwrap();
    let tools_call = received
        .iter()
        .find_map(|r| {
            let body: Value = serde_json::from_slice(&r.body).ok()?;
            if body.get("method")?.as_str()? != "tools/call" {
                return None;
            }
            let auth = r.headers.get("Authorization")?.to_str().ok()?;
            Some((body, auth.to_string()))
        })
        .expect("upstream never saw a tools/call");
    assert_eq!(tools_call.1, "Bearer live-secret");
    assert_eq!(tools_call.0["params"]["name"], "echo");
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
    let enc_key = tw_crypto::crypto::parse_encryption_key(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let client_secret_encrypted =
        tw_crypto::crypto::encrypt(b"shh-its-a-secret", &enc_key).unwrap();
    let server_id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("oauth-userinfo"),
        "uinfo",
        &format!("{}/mcp", upstream_mcp.uri()),
        fixtures::McpServerOpts {
            auth_shape: "oauth".to_string(),
            oauth_issuer: Some(provider.uri()),
            oauth_authorization_endpoint: Some(format!("{}/authorize", provider.uri())),
            oauth_token_endpoint: Some(format!("{}/token", provider.uri())),
            oauth_userinfo_endpoint: Some(format!("{}/userinfo", provider.uri())),
            oauth_client_id: Some("test-client".into()),
            oauth_client_secret_encrypted: Some(client_secret_encrypted),
            oauth_scopes: vec!["read".into()],
            ..Default::default()
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

// ---------------------------------------------------------------------------
// API key mcp_account_overrides routing — exact-match contract
// ---------------------------------------------------------------------------

/// When an API key's `mcp_account_overrides` map names a label that
/// no longer exists (e.g. user revoked that credential after the key
/// was minted), the resolver MUST return `NeedsUserCredentials` and
/// NOT silently fall through to the user's `is_default` credential.
/// Falling through would route a tool call meant for "work" to
/// "personal" — wrong account, wrong audit trail, possible data leak.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn override_pointing_at_deleted_credential_does_not_fall_through_to_default() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let upstream = mcp_upstream().await;
    let server_id = seed_static_server(&app, &upstream.uri(), "ovr").await;

    let con = app.console_client();
    login(&con, &admin).await;
    con.post(&format!("/api/mcp/servers/{server_id}/discover"), json!({}))
        .await
        .unwrap()
        .assert_ok();

    // Paste two tokens. "personal" is default (first-paste wins);
    // "work" is the explicit account the API key will route to.
    con.put(
        &format!("/api/mcp/connections/{server_id}/personal/static-token"),
        json!({"token": "pat-personal"}),
    )
    .await
    .unwrap()
    .assert_ok();
    con.put(
        &format!("/api/mcp/connections/{server_id}/work/static-token"),
        json!({"token": "pat-work"}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Mint an API key with an explicit override → "work".
    let api_key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "ovr-key",
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE api_keys SET mcp_account_overrides = $2::jsonb WHERE id = $1")
        .bind(api_key.row.id)
        .bind(serde_json::to_string(&json!({server_id.to_string(): "work"})).unwrap())
        .execute(&app.db)
        .await
        .unwrap();

    let gw = app.gateway_client();
    gw.set_bearer(&api_key.plaintext);

    // Sanity: with the "work" credential present, the call uses it
    // (the upstream sees `Bearer pat-work`).
    let _ = gw
        .post(
            "/mcp",
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "ovr__echo", "arguments": {}}
            }),
        )
        .await
        .unwrap();
    let received_before = upstream.received_requests().await.unwrap();
    assert!(
        received_before.iter().any(|r| {
            r.headers.get("Authorization").and_then(|v| v.to_str().ok()) == Some("Bearer pat-work")
        }),
        "first call should have routed via the 'work' credential"
    );

    // Snapshot the pre-delete count of pat-personal requests. The
    // eager per-user tool discovery hook (oauth_callback /
    // paste_static_token) legitimately POSTs `tools/list` with the
    // freshly-pasted bearer right after the user pastes it, so
    // pat-personal HAS been used against the upstream once already —
    // for tool discovery, not for resolver fall-through. The
    // belt-and-suspenders assertion below checks the *delta* across
    // the failed tools/call, not the lifetime count.
    let baseline_personal_calls = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            r.headers.get("Authorization").and_then(|v| v.to_str().ok())
                == Some("Bearer pat-personal")
        })
        .count();

    // Now delete the "work" row directly — simulates "user revoked
    // that account after the key was minted".
    sqlx::query(
        "DELETE FROM mcp_user_credentials WHERE mcp_server_id = $1
         AND user_id = $2 AND account_label = 'work'",
    )
    .bind(server_id)
    .bind(admin.user.id)
    .execute(&app.db)
    .await
    .unwrap();

    // The personal credential is untouched and still default. If the
    // resolver fell through, the next call would silently use it.
    // The contract says it must NOT.
    let resp = gw
        .post(
            "/mcp",
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "ovr__echo", "arguments": {}}
            }),
        )
        .await
        .unwrap();
    let body: Value = resp.json().unwrap();
    let err = body
        .get("error")
        .expect("expected NeedsUserCredentials, not a silent fall-through");
    assert_eq!(err["code"], -32050, "expected JSON-RPC -32050");
    assert_eq!(err["data"]["kind"], "needs_user_credentials");

    // Belt-and-suspenders: the upstream must not have been called with
    // the *personal* token between the delete and the failed call.
    let post_count = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| {
            r.headers.get("Authorization").and_then(|v| v.to_str().ok())
                == Some("Bearer pat-personal")
        })
        .count();
    assert_eq!(
        post_count, baseline_personal_calls,
        "resolver fell through to the default credential — that's the bug \
         (baseline {baseline_personal_calls} pre-delete uses, observed {post_count})"
    );
}

// ---------------------------------------------------------------------------
// /api/admin/mcp/oauth-probe — full RFC 9728 → 8414 → 7591 chain
// ---------------------------------------------------------------------------

/// Wiremock that plays the role of an MCP-spec-compliant upstream MCP
/// server (issues the WWW-Authenticate challenge) AND its
/// authorization server (publishes RFC 8414 metadata, accepts RFC 7591
/// dynamic client registration). Returning a single MockServer keeps
/// the well-known URL transformations exercising the same origin —
/// the test wants to verify the *chain*, not multi-host routing.
async fn mcp_with_oauth_metadata(want_dcr: bool, public_client: bool) -> MockServer {
    let server = MockServer::start().await;
    let base = server.uri();

    // Step 1: protocol POST returns 401 + WWW-Authenticate hint per
    // RFC 9728 §5.1, exactly the way Feishu / GitHub Copilot do it.
    let www_auth = format!(
        r#"Bearer realm="mcp", resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#
    );
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("www-authenticate", www_auth.as_str())
                .set_body_string(""),
        )
        .mount(&server)
        .await;

    // Step 1.5: protected-resource metadata announces the AS.
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "resource": format!("{base}/mcp"),
            "authorization_servers": [format!("{base}/as")],
            "bearer_methods_supported": ["header"],
        })))
        .mount(&server)
        .await;

    // Step 2: AS metadata at the path-aware well-known URL — same
    // shape Feishu uses (`/.well-known/oauth-authorization-server/<path>`).
    let auth_methods: &[&str] = if public_client {
        &["none"]
    } else {
        &["client_secret_post"]
    };
    let mut meta = json!({
        "issuer": format!("{base}/as"),
        "authorization_endpoint": format!("{base}/as/authorize"),
        "token_endpoint": format!("{base}/as/token"),
        "scopes_supported": ["read", "write"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": auth_methods,
    });
    if want_dcr {
        meta.as_object_mut().unwrap().insert(
            "registration_endpoint".into(),
            json!(format!("{base}/as/register")),
        );
    }
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server/as"))
        .respond_with(ResponseTemplate::new(200).set_body_json(meta))
        .mount(&server)
        .await;

    // Step 3: dynamic client registration (RFC 7591) — when enabled,
    // accept any POST and return a freshly minted client. Public-client
    // mode (`token_endpoint_auth_method: "none"`) returns no secret.
    if want_dcr {
        let registered = if public_client {
            json!({ "client_id": "minted-public-cid" })
        } else {
            json!({
                "client_id": "minted-cid",
                "client_secret": "minted-secret",
            })
        };
        Mock::given(method("POST"))
            .and(path("/as/register"))
            .respond_with(ResponseTemplate::new(201).set_body_json(registered))
            .mount(&server)
            .await;
    }
    server
}

/// SSRF guard for tests: mirrors production semantics (still rejects
/// the cloud metadata service, blank URLs, non-http schemes) but
/// allows the 127.0.0.1 origins our wiremocks bind to. Without this
/// override the probe rejects every wiremock URL before the chain
/// even starts.
fn permissive_validator() -> think_watch_server::app::UrlValidator {
    use std::sync::Arc;
    use think_watch_common::errors::AppError;
    Arc::new(|u: &str| {
        if u.is_empty() {
            return Err(AppError::BadRequest("URL must contain a host".into()));
        }
        if !u.starts_with("http://") && !u.starts_with("https://") {
            return Err(AppError::BadRequest("URL must use http or https".into()));
        }
        // Still defend against the cloud metadata service even in
        // tests — the real-world bug we don't want to mask.
        if u.contains("169.254.169.254") || u.contains("metadata.google.internal") {
            return Err(AppError::BadRequest("URL points to blocked address".into()));
        }
        Ok(())
    })
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn oauth_probe_full_chain_returns_dcr_credentials() {
    // Confidential-client AS that supports DCR — the happy path
    // where the admin pastes one URL and the wizard fills in
    // everything including client_id / client_secret.
    let upstream =
        mcp_with_oauth_metadata(/* want_dcr */ true, /* public_client */ false).await;
    let app = TestApp::try_spawn_with(SpawnOptions {
        url_validator: Some(permissive_validator()),
        ..Default::default()
    })
    .await
    .unwrap();
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    login(&con, &admin).await;

    let resp = con
        .post(
            "/api/admin/mcp/oauth-probe",
            json!({ "endpoint_url": format!("{}/mcp", upstream.uri()) }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();

    assert_eq!(
        body["issuer"].as_str().unwrap(),
        format!("{}/as", upstream.uri()),
        "issuer should come from the AS metadata's `issuer` claim"
    );
    assert_eq!(
        body["authorization_endpoint"].as_str().unwrap(),
        format!("{}/as/authorize", upstream.uri())
    );
    assert_eq!(
        body["token_endpoint"].as_str().unwrap(),
        format!("{}/as/token", upstream.uri())
    );
    assert_eq!(
        body["registration_endpoint"].as_str().unwrap(),
        format!("{}/as/register", upstream.uri())
    );
    assert_eq!(body["client_id"].as_str().unwrap(), "minted-cid");
    assert_eq!(body["client_secret"].as_str().unwrap(), "minted-secret");
    assert!(!body["is_public_client"].as_bool().unwrap());
    assert!(
        body["redirect_uri"]
            .as_str()
            .unwrap()
            .ends_with("/api/mcp/oauth/callback"),
        "redirect_uri should be the console callback so admin can copy-paste it upstream"
    );
    let scopes: Vec<&str> = body["scopes_supported"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(scopes, vec!["read", "write"]);
    let diag: Vec<&str> = body["diagnostic"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        diag.iter()
            .any(|s| s.contains("got resource_metadata hint")),
        "step 1: WWW-Authenticate hint not surfaced in diagnostic ({diag:?})"
    );
    assert!(
        diag.iter()
            .any(|s| s.contains("found authz-server metadata")),
        "step 2: AS metadata fetch not surfaced ({diag:?})"
    );
    assert!(
        diag.iter().any(|s| s.contains("dynamic-registration ok")),
        "step 3: DCR success not surfaced ({diag:?})"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn oauth_probe_public_client_omits_client_secret() {
    // AS that advertises `token_endpoint_auth_methods_supported: ["none"]`
    // (Feishu-style). DCR returns no secret — admin form should hide
    // the Client Secret input. We assert on the wire-level signal
    // (`is_public_client = true`); the UI flip is covered separately.
    let upstream = mcp_with_oauth_metadata(/* want_dcr */ true, /* public_client */ true).await;
    let app = TestApp::try_spawn_with(SpawnOptions {
        url_validator: Some(permissive_validator()),
        ..Default::default()
    })
    .await
    .unwrap();
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    login(&con, &admin).await;

    let resp = con
        .post(
            "/api/admin/mcp/oauth-probe",
            json!({ "endpoint_url": format!("{}/mcp", upstream.uri()) }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();

    assert!(body["is_public_client"].as_bool().unwrap());
    assert_eq!(body["client_id"].as_str().unwrap(), "minted-public-cid");
    assert!(
        body["client_secret"].is_null(),
        "public-client DCR returned a secret — that's surprising and breaks the wizard's hide-secret branch"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn oauth_probe_partial_when_dcr_unavailable() {
    // AS without `registration_endpoint` — Feishu-after-rejecting-our-DCR
    // shape. Endpoints fill in, client_id stays empty so the wizard's
    // partial-state UI ("go register an app upstream and paste back
    // the Client ID") activates.
    let upstream =
        mcp_with_oauth_metadata(/* want_dcr */ false, /* public_client */ false).await;
    let app = TestApp::try_spawn_with(SpawnOptions {
        url_validator: Some(permissive_validator()),
        ..Default::default()
    })
    .await
    .unwrap();
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    login(&con, &admin).await;

    let resp = con
        .post(
            "/api/admin/mcp/oauth-probe",
            json!({ "endpoint_url": format!("{}/mcp", upstream.uri()) }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();

    assert!(body["client_id"].is_null());
    assert!(body["client_secret"].is_null());
    assert!(body["registration_endpoint"].is_null());
    assert!(
        body["authorization_endpoint"].as_str().is_some(),
        "endpoints should still be filled — admin only needs to handle Client ID"
    );
    let diag: Vec<&str> = body["diagnostic"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        diag.iter().any(|s| s.contains("no registration_endpoint")),
        "diagnostic should explain why DCR was skipped ({diag:?})"
    );
}
