//! Protocol-level MCP gateway test against a live, public MCP
//! server.
//!
//! Hits the **AWS Knowledge MCP** endpoint
//! (`https://knowledge-mcp.global.api.aws`) which is hosted by AWS,
//! requires no auth, and ships in the store seed at slug `aws-docs`.
//!
//! Because this depends on the public internet:
//!   - It is `#[ignore]`d like every other integration test, so
//!     `make test-it` runs it but `cargo test --workspace` skips.
//!   - When run offline the create-server probe / tool discovery
//!     will fail and the test will surface a clear "no tools
//!     discovered" panic — no false positives.
//!   - Set `SKIP_NETWORK_TESTS=1` to skip even when running with
//!     `--ignored`.

use serde_json::Value;
use think_watch_test_support::prelude::*;

const AWS_DOCS_MCP: &str = "https://knowledge-mcp.global.api.aws";

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn aws_docs_tools_list_round_trips_through_the_proxy() {
    if std::env::var("SKIP_NETWORK_TESTS").is_ok() {
        eprintln!("SKIP_NETWORK_TESTS set — skipping live MCP protocol test");
        return;
    }

    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();

    // 1. Login as admin so we can register the upstream via the
    //    public POST /api/mcp/servers handler — same path the web
    //    UI uses, including in-memory registry registration and
    //    background tool discovery.
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    // 2. Create the MCP server. Pass `transport_type` so the
    //    handler skips the auto-detect probe — initial discovery
    //    will exercise the network round-trip on its own.
    let resp: Value = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("aws-docs"),
                "namespace_prefix": "aws_docs",
                "endpoint_url": AWS_DOCS_MCP,
                "transport_type": "streamable_http"
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let server_id = resp["id"].as_str().unwrap().to_string();

    // 3. Wait for the background tool discovery to land rows. The
    //    AWS docs server publishes 6 tools — anything > 0 proves
    //    the discovery path works end-to-end. Time-box at ~30 s
    //    so a slow network doesn't hang CI forever.
    let mut tools_seen = 0i64;
    for _ in 0..60 {
        tools_seen =
            sqlx::query_scalar("SELECT count(*) FROM mcp_tools WHERE server_id::text = $1")
                .bind(&server_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        if tools_seen > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        tools_seen > 0,
        "background tool discovery against {AWS_DOCS_MCP} produced no rows in mcp_tools \
         (network unreachable? server changed shape?)"
    );

    // 4. Mint a `tw-` API key with mcp_gateway surface for the
    //    same admin and call POST /mcp tools/list through the
    //    public gateway port.
    let key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "aws-docs-key",
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);

    let body: Value = gw
        .post(
            "/mcp",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let tools = body["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("expected JSON-RPC tools array, got: {body}"));
    assert!(
        !tools.is_empty(),
        "tools/list should return at least one entry, got: {body}"
    );

    // The gateway prepends our namespace_prefix to upstream names
    // (`aws_docs__aws___list_regions`). Loose-assert on prefix
    // since the AWS server can grow new tools.
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.iter().any(|n| n.starts_with("aws_docs__")),
        "expected at least one tool to carry the `aws_docs__` namespace prefix, got: {names:?}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn aws_docs_initialize_round_trip() {
    if std::env::var("SKIP_NETWORK_TESTS").is_ok() {
        return;
    }

    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();

    // No upstream server needed for `initialize` — the proxy
    // handles it locally and announces the gateway's protocol
    // version + capabilities. Also exercises the session-creation
    // path: first request without `Mcp-Session-Id` mints a session.
    let key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "init-key",
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);

    let resp = gw
        .post(
            "/mcp",
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "integration-test", "version": "0.0.1"}
                }
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();

    // Server must echo the session id in `Mcp-Session-Id` and a
    // valid initialize result.
    let session_header = resp
        .headers
        .get("mcp-session-id")
        .or_else(|| resp.headers.get("Mcp-Session-Id"));
    assert!(session_header.is_some(), "expected Mcp-Session-Id header");

    let body: Value = resp.json().unwrap();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let result = &body["result"];
    assert!(
        result["protocolVersion"].is_string(),
        "initialize must return protocolVersion: {body}"
    );
    assert!(
        result["capabilities"]["tools"].is_object(),
        "initialize must advertise tools capability: {body}"
    );
}
