//! API key `allowed_mcp_tools` enforcement on the MCP gateway.
//!
//! `gateway_proxy.rs::gateway_blocks_disallowed_model_on_api_key`
//! covered the `allowed_models` allow-list. This file is the
//! parallel for MCP tools: a key restricted to `["aws_docs__*"]`
//! must see only matching tools in `tools/list` (and only call
//! those via `tools/call`).
//!
//! Hits the public AWS Knowledge MCP endpoint — same offline
//! escape hatch as `mcp_protocol.rs` (`SKIP_NETWORK_TESTS=1`).

use serde_json::Value;
use think_watch_mcp_gateway::access_control::is_tool_allowed;
use think_watch_test_support::prelude::*;

const AWS_DOCS_MCP: &str = "https://knowledge-mcp.global.api.aws";

#[test]
fn pattern_matching_contract() {
    // `is_tool_allowed` is the one-stop pattern matcher that the
    // proxy's `tools/list` filter and `tools/call` gate both use.
    // Pin its semantics so a refactor doesn't silently widen what
    // a `["mysql__*"]` allow-list grants.
    assert!(
        is_tool_allowed(None, "anything"),
        "None list = unrestricted"
    );
    let star = vec!["*".to_string()];
    assert!(is_tool_allowed(Some(&star), "anything"));

    let exact = vec!["github__list_issues".to_string()];
    assert!(is_tool_allowed(Some(&exact), "github__list_issues"));
    assert!(!is_tool_allowed(Some(&exact), "github__create_issue"));
    assert!(!is_tool_allowed(Some(&exact), "stripe__list_issues"));

    let server_wildcard = vec!["github__*".to_string()];
    assert!(is_tool_allowed(
        Some(&server_wildcard),
        "github__list_issues"
    ));
    assert!(is_tool_allowed(
        Some(&server_wildcard),
        "github__create_issue"
    ));
    assert!(!is_tool_allowed(Some(&server_wildcard), "stripe__charge"));

    // Defensive: typo'd pattern with no `*` suffix doesn't act as
    // a prefix match (regression for "github__" matching "github__list_issues").
    let bad = vec!["github__".to_string()];
    assert!(!is_tool_allowed(Some(&bad), "github__list_issues"));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn tools_list_is_filtered_by_api_key_allowed_mcp_tools() {
    if std::env::var("SKIP_NETWORK_TESTS").is_ok() {
        eprintln!("SKIP_NETWORK_TESTS set — skipping live MCP test");
        return;
    }

    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();

    // Register the AWS Knowledge MCP server through the public
    // admin path so tool discovery actually populates the registry.
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con.post(
        "/api/mcp/servers",
        json!({
            "name": unique_name("awsdocs-acl"),
            "namespace_prefix": "aws_docs",
            "endpoint_url": AWS_DOCS_MCP,
            "transport_type": "streamable_http",
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    // Mint an unrestricted admin key first to confirm the
    // in-memory registry has actually loaded the upstream tools
    // (PG row counts aren't enough — the proxy reads its own
    // registry, populated by the background discovery task).
    let admin_key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "admin-probe",
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let probe = app.gateway_client();
    probe.set_bearer(&admin_key.plaintext);
    let target_tool = "aws_docs__aws___list_regions";
    let mut last_names: Vec<String> = Vec::new();
    for _ in 0..120 {
        let body: Value = probe
            .post(
                "/mcp",
                json!({"jsonrpc": "2.0", "id": 0, "method": "tools/list", "params": {}}),
            )
            .await
            .unwrap()
            .json()
            .unwrap();
        last_names = body["result"]["tools"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t["name"].as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if last_names.iter().any(|n| n == target_tool) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        last_names.iter().any(|n| n == target_tool),
        "admin probe never saw {target_tool} — discovery didn't reach the registry. Tools observed: {last_names:?}"
    );

    // Mint an API key restricted to ONE specific tool.
    let key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "narrow-mcp",
        &["mcp_gateway"],
        None,
        Some(chrono::Utc::now() + chrono::Duration::days(1)),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE api_keys SET allowed_mcp_tools = $1 WHERE id = $2")
        .bind(vec![target_tool.to_string()])
        .bind(key.row.id)
        .execute(&app.db)
        .await
        .unwrap();
    let post: Option<Vec<String>> =
        sqlx::query_scalar("SELECT allowed_mcp_tools FROM api_keys WHERE id = $1")
            .bind(key.row.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(
        post,
        Some(vec![target_tool.to_string()]),
        "UPDATE must have persisted the allow-list"
    );

    // tools/list — only the one allowed tool must come back.
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
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools array, got: {body}"))
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&target_tool),
        "the one allowed tool must appear in tools/list, got: {names:?}"
    );
    assert!(
        names.iter().all(|n| *n == target_tool),
        "no other tools should leak through the allowlist, got: {names:?}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn server_wildcard_allows_all_tools_within_namespace() {
    if std::env::var("SKIP_NETWORK_TESTS").is_ok() {
        return;
    }

    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();

    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con.post(
        "/api/mcp/servers",
        json!({
            "name": unique_name("awsdocs-wild"),
            "namespace_prefix": "aws_docs",
            "endpoint_url": AWS_DOCS_MCP,
            "transport_type": "streamable_http",
        }),
    )
    .await
    .unwrap()
    .assert_ok();

    for _ in 0..60 {
        // mcp_tools stores `tool_name` (the upstream name), the
        // gateway namespaces it on the wire as
        // `<namespace_prefix>__<tool_name>`. Just check any row landed.
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_tools")
            .fetch_one(&app.db)
            .await
            .unwrap();
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let key = fixtures::create_api_key(
        &app.db,
        admin.user.id,
        "wild-mcp",
        &["mcp_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    // `aws_docs__*` — any tool in the aws_docs namespace.
    sqlx::query("UPDATE api_keys SET allowed_mcp_tools = $1 WHERE id = $2")
        .bind(vec!["aws_docs__*".to_string()])
        .bind(key.row.id)
        .execute(&app.db)
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
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.iter().all(|n| n.starts_with("aws_docs__")),
        "the wildcard must keep matches scoped to the namespace, got: {names:?}"
    );
    assert!(
        names.len() > 1,
        "wildcard should expose >1 aws_docs tool — only got {}",
        names.len()
    );
}
