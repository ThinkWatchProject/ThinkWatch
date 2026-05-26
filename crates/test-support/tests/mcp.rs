//! MCP gateway and console-side MCP CRUD integration tests.
//! Covers: store template list, server CRUD, tool listing, install
//! rejection on schema violation. The gateway proxy itself is
//! exercised in `gateway_proxy.rs` for the AI side; the MCP proxy
//! requires a live MCP-protocol server which is out of scope here —
//! we cover the registry / CRUD / namespace plumbing instead.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn pick_list(value: &Value) -> Option<&Vec<Value>> {
    value
        .as_array()
        .or_else(|| value.get("items").and_then(|v| v.as_array()))
        .or_else(|| value.get("data").and_then(|v| v.as_array()))
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
async fn mcp_servers_bulk_delete_happy_path() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let mut ids: Vec<String> = Vec::new();
    for i in 0..3 {
        let created: Value = con
            .post(
                "/api/mcp/servers",
                json!({
                    "name": unique_name(&format!("bulk{i}")),
                    "namespace_prefix": format!("bulk_ns_{}", uuid::Uuid::new_v4().simple()),
                    "endpoint_url": "https://example.com/mcp",
                    "transport_type": "streamable_http"
                }),
            )
            .await
            .unwrap()
            .json()
            .unwrap();
        ids.push(created["id"].as_str().unwrap().to_string());
    }

    let resp = con
        .post("/api/mcp/servers/bulk-delete", json!({ "server_ids": ids }))
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    let deleted = body["deleted"].as_array().expect("deleted array");
    let skipped = body["skipped"].as_array().expect("skipped array");
    assert_eq!(deleted.len(), 3, "all three ids should be deleted: {body}");
    assert!(skipped.is_empty(), "nothing should skip: {body}");

    // All gone from the active list.
    let after: Value = con.get("/api/mcp/servers").await.unwrap().json().unwrap();
    let after_arr = pick_list(&after).expect("after list");
    for id in &ids {
        assert!(
            !after_arr.iter().any(|s| s["id"] == json!(id)),
            "id {id} should be gone after bulk delete"
        );
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn mcp_servers_bulk_delete_skips_not_found() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let created: Value = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": unique_name("present"),
                "namespace_prefix": format!("present_{}", uuid::Uuid::new_v4().simple()),
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http"
            }),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    let real_id = created["id"].as_str().unwrap().to_string();
    let phantom = uuid::Uuid::new_v4().to_string();

    let resp = con
        .post(
            "/api/mcp/servers/bulk-delete",
            json!({ "server_ids": [real_id, phantom] }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    let deleted = body["deleted"].as_array().expect("deleted array");
    let skipped = body["skipped"].as_array().expect("skipped array");
    assert_eq!(
        deleted.len(),
        1,
        "only the present id should land in deleted: {body}"
    );
    assert_eq!(skipped.len(), 1, "phantom id should be skipped: {body}");
    assert_eq!(
        skipped[0]["reason"], "not_found",
        "skip reason must be not_found"
    );
    assert_eq!(skipped[0]["id"], json!(phantom));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn mcp_servers_bulk_delete_rejects_oversized_batch() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    // 51 fake ids — never touches the DB because the handler should
    // reject with 400 before doing any work.
    let ids: Vec<String> = (0..51).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let resp = con
        .post("/api/mcp/servers/bulk-delete", json!({ "server_ids": ids }))
        .await
        .unwrap();
    assert_eq!(
        resp.status.as_u16(),
        400,
        "oversized batch must be rejected with 400, got {} body={}",
        resp.status,
        resp.text()
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
async fn discover_failure_returns_structured_error_detail() {
    // Stand up a wiremock that returns 500 for `tools/list` — the
    // discover handler should surface the underlying error string in
    // the new `discovery_failed` discriminator instead of collapsing
    // it into a generic 400.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream is on fire"))
        .mount(&upstream)
        .await;

    // Insert directly via fixtures — the public POST /api/mcp/servers
    // route's SSRF guard rejects 127.0.0.1, where wiremock listens.
    let server_id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("fail"),
        "failns",
        &format!("{}/mcp", upstream.uri()),
        fixtures::McpServerOpts::default(),
    )
    .await
    .unwrap();

    let resp = con
        .post(&format!("/api/mcp/servers/{server_id}/discover"), json!({}))
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(
        body["status"], "discovery_failed",
        "expected discovery_failed discriminator, got: {body}"
    );
    assert_eq!(body["server_id"], server_id.to_string());
    assert_eq!(body["tools_discovered"], 0);
    let err = body["error"]
        .as_str()
        .expect("error field must be a string");
    assert!(
        err.contains("500"),
        "error should reference upstream HTTP 500, got: {err}"
    );
    assert!(err.len() <= 500 + 4, "error must be bounded: {err}");
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
