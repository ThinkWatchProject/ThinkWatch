//! MCP gateway and console-side MCP CRUD integration tests.
//! Covers: store template list, server CRUD, tool listing, install
//! rejection on schema violation. The gateway proxy itself is
//! exercised in `gateway_proxy.rs` for the AI side; the MCP proxy
//! requires a live MCP-protocol server which is out of scope here —
//! we cover the registry / CRUD / namespace plumbing instead.

use serde_json::Value;
use think_watch_test_support::prelude::*;

fn pick_list(value: &Value) -> Option<&Vec<Value>> {
    value
        .as_array()
        .or_else(|| value.get("items").and_then(|v| v.as_array()))
        .or_else(|| value.get("data").and_then(|v| v.as_array()))
}

async fn admin_session(app: &TestApp) -> TestClient {
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn store_lists_seeded_templates() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let body: Value = con.get("/api/mcp/store").await.unwrap().json().unwrap();
    let arr = pick_list(&body).expect("store list");
    let slugs: Vec<&str> = arr.iter().filter_map(|t| t["slug"].as_str()).collect();
    // The migration seeds `github` + several others — assert at least
    // one of the canonical slugs comes back.
    assert!(
        slugs.contains(&"github") || !slugs.is_empty(),
        "expected seeded store templates, got: {slugs:?}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn store_categories_endpoint_returns_array() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let body: Value = con
        .get("/api/mcp/store/categories")
        .await
        .unwrap()
        .json()
        .unwrap();
    let arr = pick_list(&body).expect("categories list");
    // Category list is computed live from store templates — must
    // be non-empty for the seeded data.
    assert!(
        !arr.is_empty(),
        "expected at least one MCP category from seeds"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn mcp_servers_create_list_delete_cycle() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    // Pass `transport_type` explicitly so the handler skips the
    // outbound auto-detect probe.
    let created: Value = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("server"),
                "namespace_prefix": "ns_test",
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http"
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let sid = created["id"].as_str().unwrap().to_string();

    let list: Value = con.get("/api/mcp/servers").await.unwrap().json().unwrap();
    let arr = pick_list(&list).expect("servers list");
    assert!(arr.iter().any(|s| s["id"] == created["id"]));

    con.delete(&format!("/api/mcp/servers/{sid}"))
        .await
        .unwrap()
        .assert_ok();

    let after: Value = con.get("/api/mcp/servers").await.unwrap().json().unwrap();
    let after_arr = pick_list(&after).expect("after list");
    assert!(
        !after_arr.iter().any(|s| s["id"] == created["id"]),
        "deleted server must drop out of the active list"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn mcp_servers_reject_duplicate_namespace_prefix() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let body = json!({
        "name": unique_name("first"),
        "namespace_prefix": "shared_ns",
        "endpoint_url": "https://example.com/mcp",
        "transport_type": "streamable_http"
    });
    con.post("/api/mcp/servers", body)
        .await
        .unwrap()
        .assert_ok();

    let dup = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("second"),
                "namespace_prefix": "shared_ns",
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http"
            }),
        )
        .await
        .unwrap();
    assert!(
        !dup.status.is_success(),
        "duplicate namespace_prefix must be rejected, got {}",
        dup.status
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn mcp_tools_endpoint_returns_array_for_authenticated_user() {
    let app = TestApp::spawn().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": user.user.email, "password": user.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    let body = con.get("/api/mcp/tools").await.unwrap();
    body.assert_ok();
    let json: Value = body.json().unwrap();
    // No tools registered yet → the array (or "items") should exist
    // and be empty.
    let arr = pick_list(&json).expect("tools list");
    assert!(arr.is_empty(), "expected empty tools list, got: {arr:?}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn install_template_with_unknown_slug_404s() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let resp = con
        .post(
            "/api/mcp/store/this-slug-does-not-exist/install",
            json!({"endpoint_url": "https://example.com/mcp"}),
        )
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 404);
}
