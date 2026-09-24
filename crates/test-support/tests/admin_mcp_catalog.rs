//! The MCP admin and connection endpoints end to end: servers, the
//! store, the tool catalog, shared and per-user credentials.
//!
//! The rest of the MCP suite covers the proxy and the credential
//! transitions; this file pins what the remaining endpoints read and
//! write — lookups, 404s, 409s, background error reporting, the registry
//! sync — so moving their SQL around (into `services::mcp_*_repository`)
//! is checked rather than assumed.

use std::time::Duration;

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An MCP upstream whose `tools/list` returns one tool (`echo`).
async fn mcp_ok() -> MockServer {
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

/// An MCP upstream that answers every request with `status`.
async fn mcp_status(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

async fn get(con: &TestClient, path: &str) -> Value {
    let resp = con.get(path).await.unwrap();
    resp.assert_ok();
    resp.json().unwrap()
}

async fn create_server(con: &TestClient, body: Value) -> Value {
    let resp = con.post("/api/mcp/servers", body).await.unwrap();
    resp.assert_ok();
    resp.json().unwrap()
}

fn prefix() -> String {
    format!("p_{}", &Uuid::new_v4().simple().to_string()[..12])
}

/// Poll `GET /api/mcp/servers` until the server's row satisfies `done`
/// (background discovery writes it after the request returns).
async fn wait_for_server(con: &TestClient, id: &str, done: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    for _ in 0..100 {
        let list = get(con, "/api/mcp/servers").await;
        if let Some(row) = list.as_array().unwrap().iter().find(|s| s["id"] == id) {
            if done(row) {
                return row.clone();
            }
            last = row.clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("server {id} never reached the expected state: {last}");
}

async fn insert_tool(app: &TestApp, server_id: Uuid, name: &str, desc: &str, active: bool) {
    sqlx::query(
        "INSERT INTO mcp_tools (server_id, tool_name, description, input_schema, is_active)
         VALUES ($1, $2, $3, '{}'::jsonb, $4)",
    )
    .bind(server_id)
    .bind(name)
    .bind(desc)
    .bind(active)
    .execute(&app.db)
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Servers
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_server_is_created_read_edited_and_deleted() {
    let app = TestApp::spawn_reaching_loopback().await;
    let con = admin_session(&app).await;
    let upstream = mcp_status(500).await;

    let name = unique_name("srv");
    let pfx = prefix();
    let created = create_server(
        &con,
        json!({
            "name": name,
            "namespace_prefix": pfx,
            "display_label": "  Shown  ",
            "description": "first",
            "endpoint_url": format!("{}/mcp", upstream.uri()),
            "transport_type": "streamable_http",
            "oauth_scopes": ["a", "b"],
            "custom_headers": {"X-Team": "{{user_id}}"},
            "cache_ttl_secs": 30,
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["name"], name.as_str());
    assert_eq!(created["namespace_prefix"], pfx.as_str());
    assert_eq!(created["display_label"], "Shown", "{created}");
    assert_eq!(created["auth_shape"], "anonymous");
    assert_eq!(created["credential_owner"], "per_user");
    assert_eq!(created["auth_header_name"], "Authorization");
    assert_eq!(created["auth_value_template"], "Bearer {{token}}");
    assert_eq!(created["oauth_scopes"], json!(["a", "b"]));
    assert_eq!(
        created["config_json"],
        json!({"custom_headers": {"X-Team": "{{user_id}}"}, "cache_ttl_secs": 30})
    );

    // The failed first discovery lands on the row.
    let row = wait_for_server(&con, &id, |s| s["last_error"].is_string()).await;
    assert!(
        row["last_error"].as_str().unwrap().contains("HTTP 500"),
        "{row}"
    );

    let got = get(&con, &format!("/api/mcp/servers/{id}")).await;
    assert_eq!(got["description"], "first");
    assert_eq!(got["display_label"], "Shown");
    con.get(&format!("/api/mcp/servers/{}", Uuid::new_v4()))
        .await
        .unwrap()
        .assert_status(404);

    // PATCH: JSON null clears, absent keeps, a value replaces.
    let new_pfx = prefix();
    let resp = con
        .patch(
            &format!("/api/mcp/servers/{id}"),
            json!({
                "display_label": null,
                "description": "second",
                "namespace_prefix": new_pfx,
                "custom_headers": {"X-Other": "1"},
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let patched: Value = resp.json().unwrap();
    assert!(patched["display_label"].is_null(), "{patched}");
    assert_eq!(patched["description"], "second");
    assert_eq!(patched["namespace_prefix"], new_pfx.as_str());
    assert_eq!(patched["name"], name.as_str());
    assert_eq!(patched["oauth_scopes"], json!(["a", "b"]));
    assert_eq!(
        patched["config_json"],
        json!({"custom_headers": {"X-Other": "1"}, "cache_ttl_secs": 30})
    );
    let got = get(&con, &format!("/api/mcp/servers/{id}")).await;
    assert_eq!(got["description"], "second");
    assert!(got["display_label"].is_null());

    con.patch(
        &format!("/api/mcp/servers/{}", Uuid::new_v4()),
        json!({"description": "x"}),
    )
    .await
    .unwrap()
    .assert_status(404);

    con.delete(&format!("/api/mcp/servers/{id}"))
        .await
        .unwrap()
        .assert_ok();
    con.get(&format!("/api/mcp/servers/{id}"))
        .await
        .unwrap()
        .assert_status(404);
    con.delete(&format!("/api/mcp/servers/{id}"))
        .await
        .unwrap()
        .assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_taken_name_or_prefix_is_a_409() {
    let app = TestApp::spawn_reaching_loopback().await;
    let con = admin_session(&app).await;
    let upstream = mcp_status(500).await;
    let endpoint = format!("{}/mcp", upstream.uri());

    let (name_a, pfx_a) = (unique_name("a"), prefix());
    create_server(
        &con,
        json!({"name": name_a, "namespace_prefix": pfx_a, "endpoint_url": endpoint,
               "transport_type": "streamable_http"}),
    )
    .await;
    let b = create_server(
        &con,
        json!({"name": unique_name("b"), "namespace_prefix": prefix(), "endpoint_url": endpoint,
               "transport_type": "streamable_http"}),
    )
    .await;
    let b_id = b["id"].as_str().unwrap();

    let resp = con
        .post(
            "/api/mcp/servers",
            json!({"name": name_a, "namespace_prefix": prefix(), "endpoint_url": endpoint,
                   "transport_type": "streamable_http"}),
        )
        .await
        .unwrap();
    resp.assert_status(409);
    assert!(
        resp.text().contains("server name already in use"),
        "{}",
        resp.text()
    );

    let resp = con
        .post(
            "/api/mcp/servers",
            json!({"name": unique_name("c"), "namespace_prefix": pfx_a, "endpoint_url": endpoint,
                   "transport_type": "streamable_http"}),
        )
        .await
        .unwrap();
    resp.assert_status(409);
    assert!(
        resp.text().contains("namespace_prefix already in use"),
        "{}",
        resp.text()
    );

    let resp = con
        .patch(&format!("/api/mcp/servers/{b_id}"), json!({"name": name_a}))
        .await
        .unwrap();
    resp.assert_status(409);
    assert!(
        resp.text().contains("server name already in use"),
        "{}",
        resp.text()
    );

    let resp = con
        .patch(
            &format!("/api/mcp/servers/{b_id}"),
            json!({"namespace_prefix": pfx_a}),
        )
        .await
        .unwrap();
    resp.assert_status(409);
    assert!(
        resp.text().contains("namespace_prefix already in use"),
        "{}",
        resp.text()
    );

    // Unknown template on the install path.
    con.post(
        "/api/mcp/servers",
        json!({"name": unique_name("t"), "namespace_prefix": prefix(), "endpoint_url": endpoint,
               "transport_type": "streamable_http", "template_slug": unique_name("nope")}),
    )
    .await
    .unwrap()
    .assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_server_list_counts_active_tools_only() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let id = fixtures::create_mcp_server(
        &app.db,
        &unique_name("count"),
        &prefix(),
        "https://example.com/mcp",
    )
    .await
    .unwrap();
    insert_tool(&app, id, "one", "", true).await;
    insert_tool(&app, id, "two", "", true).await;
    insert_tool(&app, id, "gone", "", false).await;
    let bare = fixtures::create_mcp_server(
        &app.db,
        &unique_name("bare"),
        &prefix(),
        "https://example.com/mcp",
    )
    .await
    .unwrap();

    let list = get(&con, "/api/mcp/servers").await;
    let rows = list.as_array().unwrap();
    let row = |id: Uuid| {
        rows.iter()
            .find(|s| s["id"] == id.to_string())
            .unwrap_or_else(|| panic!("{id} missing: {list}"))
    };
    assert_eq!(row(id)["tools_count"], 2);
    assert_eq!(row(bare)["tools_count"], 0);
    // Newest first.
    let pos = |id: Uuid| rows.iter().position(|s| s["id"] == id.to_string()).unwrap();
    assert!(pos(bare) < pos(id), "{list}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_shared_static_token_at_create_lands_with_the_server() {
    let app = TestApp::spawn_reaching_loopback().await;
    let (con, admin) = admin_session_with_user(&app).await;
    let upstream = mcp_ok().await;

    let created = create_server(
        &con,
        json!({
            "name": unique_name("shared"),
            "namespace_prefix": prefix(),
            "endpoint_url": format!("{}/mcp", upstream.uri()),
            "transport_type": "streamable_http",
            "auth_shape": "static",
            "credential_owner": "admin_shared",
            "shared_static_token": "tok-at-create",
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let status = get(
        &con,
        &format!("/api/admin/mcp/servers/{id}/shared-credential"),
    )
    .await;
    assert_eq!(status["configured"], true, "{status}");
    assert_eq!(status["credential_type"], "static_token");
    assert_eq!(status["configured_by"], admin.user.id.to_string());

    // Discovery ran with the shared bearer and found the tool.
    wait_for_server(&con, id, |s| s["tools_count"] == 1).await;
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn discover_on_an_unknown_server_is_a_404() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    con.post(
        &format!("/api/mcp/servers/{}/discover", Uuid::new_v4()),
        json!({}),
    )
    .await
    .unwrap()
    .assert_status(404);
}

// ---------------------------------------------------------------------------
// Shared credentials
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_shared_credential_is_pasted_reported_and_revoked() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;
    let upstream = mcp_ok().await;
    let id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("sc"),
        &prefix(),
        &format!("{}/mcp", upstream.uri()),
        fixtures::McpServerOpts {
            auth_shape: "static".into(),
            credential_owner: "admin_shared".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let base = format!("/api/admin/mcp/servers/{id}/shared-credential");

    let status = get(&con, &base).await;
    assert_eq!(
        status,
        json!({"configured": false, "credential_type": null, "expires_at": null,
               "upstream_subject": null, "configured_by": null, "updated_at": null})
    );
    con.delete(&base).await.unwrap().assert_status(404);

    // A stale error is cleared once discovery with the new token works.
    sqlx::query("UPDATE mcp_servers SET last_error = 'stale' WHERE id = $1")
        .bind(id)
        .execute(&app.db)
        .await
        .unwrap();
    con.put(&format!("{base}/static-token"), json!({"token": "one"}))
        .await
        .unwrap()
        .assert_ok();
    let row = wait_for_server(&con, &id.to_string(), |s| s["last_error"].is_null()).await;
    assert_eq!(row["tools_count"], 1, "{row}");

    let status = get(&con, &base).await;
    assert_eq!(status["configured"], true);
    assert_eq!(status["credential_type"], "static_token");
    assert_eq!(status["configured_by"], admin.user.id.to_string());
    assert!(status["updated_at"].is_string());
    assert!(status["expires_at"].is_null());

    // Pasting again replaces the one row.
    con.put(&format!("{base}/static-token"), json!({"token": "two"}))
        .await
        .unwrap()
        .assert_ok();
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mcp_server_shared_credentials WHERE mcp_server_id = $1",
    )
    .bind(id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(rows, 1);

    let resp = con.delete(&base).await.unwrap();
    resp.assert_ok();
    assert_eq!(resp.json::<Value>().unwrap(), json!({"status": "revoked"}));
    assert_eq!(get(&con, &base).await["configured"], false);
    con.delete(&base).await.unwrap().assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_rejected_shared_token_is_reported_on_the_server() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let upstream = mcp_status(401).await;
    let id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("rej"),
        &prefix(),
        &format!("{}/mcp", upstream.uri()),
        fixtures::McpServerOpts {
            auth_shape: "static".into(),
            credential_owner: "admin_shared".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    con.put(
        &format!("/api/admin/mcp/servers/{id}/shared-credential/static-token"),
        json!({"token": "bad"}),
    )
    .await
    .unwrap()
    .assert_ok();
    let row = wait_for_server(&con, &id.to_string(), |s| s["last_error"].is_string()).await;
    assert_eq!(
        row["last_error"],
        "Shared credential rejected by upstream — verify token / scopes"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn shared_authorize_needs_an_admin_shared_oauth_server() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let per_user = fixtures::create_mcp_server(
        &app.db,
        &unique_name("pu"),
        &prefix(),
        "https://example.com/mcp",
    )
    .await
    .unwrap();
    let static_shared = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("ss"),
        &prefix(),
        "https://example.com/mcp",
        fixtures::McpServerOpts {
            auth_shape: "static".into(),
            credential_owner: "admin_shared".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let oauth_shared = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("os"),
        &prefix(),
        "https://example.com/mcp",
        fixtures::McpServerOpts {
            auth_shape: "oauth".into(),
            credential_owner: "admin_shared".into(),
            oauth_authorization_endpoint: Some("https://auth.example.com/authorize".into()),
            oauth_token_endpoint: Some("https://auth.example.com/token".into()),
            oauth_client_id: Some("cid".into()),
            oauth_scopes: vec!["read".into()],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let authorize = |id: Uuid| format!("/api/admin/mcp/servers/{id}/shared-credential/authorize");

    con.post(&authorize(Uuid::new_v4()), json!({}))
        .await
        .unwrap()
        .assert_status(404);
    con.post(&authorize(per_user), json!({}))
        .await
        .unwrap()
        .assert_status(400);
    con.post(&authorize(static_shared), json!({}))
        .await
        .unwrap()
        .assert_status(400);
    con.put(
        &format!("/api/admin/mcp/servers/{per_user}/shared-credential/static-token"),
        json!({"token": "x"}),
    )
    .await
    .unwrap()
    .assert_status(400);

    let resp = con.post(&authorize(oauth_shared), json!({})).await.unwrap();
    resp.assert_ok();
    let url = resp.json::<Value>().unwrap()["authorize_url"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        url.starts_with("https://auth.example.com/authorize?"),
        "{url}"
    );
    assert!(
        url.contains("client_id=cid") && url.contains("scope=read"),
        "{url}"
    );
}

// ---------------------------------------------------------------------------
// Per-user connections
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn accounts_are_listed_switched_and_revoked() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let upstream = mcp_ok().await;
    let id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("conn"),
        &prefix(),
        &format!("{}/mcp", upstream.uri()),
        fixtures::McpServerOpts {
            auth_shape: "static".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for label in ["first", "second", "third"] {
        con.put(
            &format!("/api/mcp/connections/{id}/{label}/static-token"),
            json!({"token": format!("tok-{label}")}),
        )
        .await
        .unwrap()
        .assert_ok();
    }

    let accounts = |list: &Value| -> Vec<(String, bool)> {
        let entry = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["server_id"] == id.to_string())
            .unwrap_or_else(|| panic!("server missing: {list}"));
        entry["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["account_label"].as_str().unwrap().to_string(),
                    a["is_default"].as_bool().unwrap(),
                )
            })
            .collect()
    };
    // Default first, then by label.
    assert_eq!(
        accounts(&get(&con, "/api/mcp/connections").await),
        vec![
            ("first".to_string(), true),
            ("second".to_string(), false),
            ("third".to_string(), false)
        ]
    );

    con.put(
        &format!("/api/mcp/connections/{id}/second/default"),
        json!({}),
    )
    .await
    .unwrap()
    .assert_ok();
    assert_eq!(
        accounts(&get(&con, "/api/mcp/connections").await),
        vec![
            ("second".to_string(), true),
            ("first".to_string(), false),
            ("third".to_string(), false)
        ]
    );

    // Revoking a non-default account leaves the default alone.
    con.delete(&format!("/api/mcp/connections/{id}/first"))
        .await
        .unwrap()
        .assert_ok();
    assert_eq!(
        accounts(&get(&con, "/api/mcp/connections").await),
        vec![("second".to_string(), true), ("third".to_string(), false)]
    );

    con.delete(&format!("/api/mcp/connections/{id}/first"))
        .await
        .unwrap()
        .assert_status(404);
    con.put(
        &format!("/api/mcp/connections/{id}/missing/default"),
        json!({}),
    )
    .await
    .unwrap()
    .assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn revoking_the_default_account_promotes_the_newest_one() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let upstream = mcp_ok().await;
    let id = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("promote"),
        &prefix(),
        &format!("{}/mcp", upstream.uri()),
        fixtures::McpServerOpts {
            auth_shape: "static".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for label in ["first", "second", "third"] {
        con.put(
            &format!("/api/mcp/connections/{id}/{label}/static-token"),
            json!({"token": format!("tok-{label}")}),
        )
        .await
        .unwrap()
        .assert_ok();
        // created_at decides who is promoted; keep them apart.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let defaults = || async {
        let rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT account_label, is_default FROM mcp_user_credentials
             WHERE mcp_server_id = $1 ORDER BY account_label",
        )
        .bind(id)
        .fetch_all(&app.db)
        .await
        .unwrap();
        rows
    };
    assert_eq!(
        defaults().await,
        vec![
            ("first".to_string(), true),
            ("second".to_string(), false),
            ("third".to_string(), false)
        ]
    );

    con.delete(&format!("/api/mcp/connections/{id}/first"))
        .await
        .unwrap()
        .assert_ok();
    assert_eq!(
        defaults().await,
        vec![("second".to_string(), false), ("third".to_string(), true)]
    );

    // The last account goes too; nothing is left to promote.
    con.delete(&format!("/api/mcp/connections/{id}/third"))
        .await
        .unwrap()
        .assert_ok();
    assert_eq!(defaults().await, vec![("second".to_string(), true)]);
    con.delete(&format!("/api/mcp/connections/{id}/second"))
        .await
        .unwrap()
        .assert_ok();
    assert_eq!(defaults().await, vec![]);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn connections_list_only_per_user_servers_that_need_a_credential() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let listed = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("listed"),
        &prefix(),
        "https://example.com/mcp",
        fixtures::McpServerOpts {
            auth_shape: "static".into(),
            auth_header_name: "X-API-Key".into(),
            auth_value_template: "{{token}}".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE mcp_servers SET display_label = 'Label' WHERE id = $1")
        .bind(listed)
        .execute(&app.db)
        .await
        .unwrap();
    let anonymous = fixtures::create_mcp_server(
        &app.db,
        &unique_name("anon"),
        &prefix(),
        "https://example.com/mcp",
    )
    .await
    .unwrap();
    let shared = fixtures::create_mcp_server_with(
        &app.db,
        &unique_name("shared"),
        &prefix(),
        "https://example.com/mcp",
        fixtures::McpServerOpts {
            auth_shape: "static".into(),
            credential_owner: "admin_shared".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let list = get(&con, "/api/mcp/connections").await;
    let rows = list.as_array().unwrap();
    let ids: Vec<&str> = rows
        .iter()
        .map(|r| r["server_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&listed.to_string().as_str()), "{list}");
    assert!(!ids.contains(&anonymous.to_string().as_str()), "{list}");
    assert!(!ids.contains(&shared.to_string().as_str()), "{list}");
    let row = rows
        .iter()
        .find(|r| r["server_id"] == listed.to_string())
        .unwrap();
    assert_eq!(row["display_label"], "Label");
    assert_eq!(row["auth_shape"], "static");
    assert_eq!(row["auth_header_name"], "X-API-Key");
    assert_eq!(row["auth_value_template"], "{{token}}");
    assert_eq!(row["accounts"], json!([]));

    // Per-user endpoints on a server that doesn't exist.
    let ghost = Uuid::new_v4();
    con.put(
        &format!("/api/mcp/connections/{ghost}/x/static-token"),
        json!({"token": "t"}),
    )
    .await
    .unwrap()
    .assert_status(404);
    con.post(
        &format!("/api/mcp/connections/{ghost}/authorize"),
        json!({"account_label": "x"}),
    )
    .await
    .unwrap()
    .assert_status(404);
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_template_is_read_by_slug() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let slug = unique_name("tpl");
    sqlx::query(
        "INSERT INTO mcp_store_templates (slug, name, category, endpoint_template, deploy_type)
         VALUES ($1, 'Tpl', 'dev', 'https://example.com/mcp', 'hosted')",
    )
    .bind(&slug)
    .execute(&app.db)
    .await
    .unwrap();

    let got = get(&con, &format!("/api/mcp/store/{slug}")).await;
    assert_eq!(got["slug"], slug.as_str());
    assert_eq!(got["name"], "Tpl");
    assert_eq!(got["endpoint_template"], "https://example.com/mcp");
    let resp = con
        .get(&format!("/api/mcp/store/{}", unique_name("nope")))
        .await
        .unwrap();
    resp.assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_registry_sync_upserts_prunes_and_keeps_installed_templates() {
    let app = TestApp::spawn_reaching_loopback().await;
    let (con, admin) = admin_session_with_user(&app).await;

    // Something installed survives a registry that no longer lists it.
    let installed_slug = unique_name("kept");
    let installed_id: Uuid = sqlx::query_scalar(
        "INSERT INTO mcp_store_templates (slug, name, deploy_type) VALUES ($1, 'Kept', 'hosted')
         RETURNING id",
    )
    .bind(&installed_slug)
    .fetch_one(&app.db)
    .await
    .unwrap();
    let server = fixtures::create_mcp_server(
        &app.db,
        &unique_name("inst"),
        &prefix(),
        "https://example.com/mcp",
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mcp_store_installs (template_id, server_id, installed_by) VALUES ($1, $2, $3)",
    )
    .bind(installed_id)
    .bind(server)
    .bind(admin.user.id)
    .execute(&app.db)
    .await
    .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_store_templates")
        .fetch_one(&app.db)
        .await
        .unwrap();

    let registry = MockServer::start().await;
    let registry_body = |description: &str| {
        json!({
            "version": 1,
            "templates": [
                {
                    "slug": "sync-oauth",
                    "name": "Sync OAuth",
                    "description": {"en": description, "zh": "说明"},
                    "category": "dev",
                    "tags": ["x", "y"],
                    "endpoint_template": "https://oauth.example.com/mcp",
                    "oauth_issuer": "https://oauth.example.com",
                    "oauth_default_scopes": ["repo"],
                    "featured": true
                },
                {
                    "slug": "sync-static",
                    "name": "Sync Static",
                    "static_token_help_url": "https://example.com/token",
                    "auth_header_name": "X-API-Key",
                    "auth_value_template": "{{token}}",
                    "auth_instructions": "paste it",
                    "deploy_type": "manual"
                },
                {
                    "slug": "sync-bad",
                    "name": "Bad",
                    "auth_value_template": "Bearer {{nope}}"
                }
            ]
        })
    };
    Mock::given(method("GET"))
        .and(path("/registry.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(registry_body("first")))
        .up_to_n_times(1)
        .mount(&registry)
        .await;
    Mock::given(method("GET"))
        .and(path("/registry.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(registry_body("second")))
        .mount(&registry)
        .await;
    let url = format!("{}/registry.json", registry.uri());

    let resp = con
        .post("/api/admin/mcp-store/sync", json!({"registry_url": url}))
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["status"], "synced");
    assert_eq!(body["count"], 2, "the invalid template is skipped: {body}");
    // Every other seeded template went; the installed one stayed.
    assert_eq!(body["removed"], before - 1, "{body}");

    let slugs: Vec<String> =
        sqlx::query_scalar("SELECT slug FROM mcp_store_templates ORDER BY slug")
            .fetch_all(&app.db)
            .await
            .unwrap();
    let mut want = vec![
        installed_slug.clone(),
        "sync-oauth".to_string(),
        "sync-static".to_string(),
    ];
    want.sort();
    assert_eq!(slugs, want);

    let oauth = get(&con, "/api/mcp/store/sync-oauth").await;
    assert_eq!(oauth["description"], "first\n---\n说明");
    assert_eq!(oauth["auth_shape"], "oauth");
    assert_eq!(oauth["tags"], json!(["x", "y"]));
    assert_eq!(oauth["oauth_default_scopes"], json!(["repo"]));
    assert_eq!(oauth["auth_header_name"], "Authorization");
    assert_eq!(oauth["auth_value_template"], "Bearer {{token}}");
    assert_eq!(oauth["deploy_type"], "hosted");
    assert_eq!(oauth["featured"], true);
    let stat = get(&con, "/api/mcp/store/sync-static").await;
    assert_eq!(stat["auth_shape"], "static");
    assert_eq!(stat["auth_header_name"], "X-API-Key");
    assert_eq!(stat["auth_value_template"], "{{token}}");
    assert_eq!(stat["auth_instructions"], "paste it");
    assert_eq!(stat["deploy_type"], "manual");
    assert_eq!(stat["tags"], json!([]));
    assert_eq!(stat["featured"], false);

    // A second sync updates in place and removes nothing.
    let body: Value = con
        .post("/api/admin/mcp-store/sync", json!({"registry_url": url}))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(body["count"], 2);
    assert_eq!(body["removed"], 0);
    let oauth = get(&con, "/api/mcp/store/sync-oauth").await;
    assert_eq!(oauth["description"], "second\n---\n说明");

    let cats = get(&con, "/api/mcp/store/categories").await;
    assert_eq!(cats, json!([{"category": "dev", "count": 1}]));
}

// ---------------------------------------------------------------------------
// Tool catalog
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_tool_catalog_filters_pages_and_includes_the_callers_own_tools() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;
    let other = fixtures::create_admin_user(&app.db).await.unwrap();

    let pfx_a = prefix();
    let a = fixtures::create_mcp_server(&app.db, "cat-a", &pfx_a, "https://example.com/mcp")
        .await
        .unwrap();
    let b = fixtures::create_mcp_server(&app.db, "cat-b", &prefix(), "https://example.com/mcp")
        .await
        .unwrap();
    insert_tool(&app, a, "alpha", "first tool", true).await;
    insert_tool(&app, a, "beta", "finds needles", true).await;
    insert_tool(&app, a, "hidden", "", false).await;
    insert_tool(&app, b, "gamma", "", true).await;
    for (user, tool) in [(admin.user.id, "mine"), (other.user.id, "theirs")] {
        sqlx::query(
            "INSERT INTO mcp_user_tools (mcp_server_id, user_id, tool_name, description, input_schema)
             VALUES ($1, $2, $3, 'personal', '{}'::jsonb)",
        )
        .bind(b)
        .bind(user)
        .bind(tool)
        .execute(&app.db)
        .await
        .unwrap();
    }

    let names = |v: &Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };

    let all = get(&con, "/api/mcp/tools").await;
    assert_eq!(all["total"], 3, "{all}");
    assert_eq!(names(&all), ["alpha", "beta", "gamma"]);
    let alpha = &all["items"][0];
    assert_eq!(alpha["server_name"], "cat-a");
    assert_eq!(alpha["server_id"], a.to_string());
    assert_eq!(alpha["namespaced_name"], format!("{pfx_a}__alpha"));
    assert_eq!(alpha["description"], "first tool");
    assert_eq!(alpha["input_schema"], json!({}));

    let mine = get(&con, "/api/mcp/tools?include_user_tools=true").await;
    assert_eq!(mine["total"], 4);
    assert_eq!(names(&mine), ["alpha", "beta", "gamma", "mine"]);

    let by_server = get(&con, &format!("/api/mcp/tools?server_id={b}")).await;
    assert_eq!(names(&by_server), ["gamma"]);

    // Search matches the name, the namespaced name and the description.
    assert_eq!(names(&get(&con, "/api/mcp/tools?q=ALP").await), ["alpha"]);
    assert_eq!(names(&get(&con, "/api/mcp/tools?q=needle").await), ["beta"]);
    let by_prefix = get(&con, &format!("/api/mcp/tools?q={pfx_a}__b")).await;
    assert_eq!(names(&by_prefix), ["beta"]);

    let page = get(&con, "/api/mcp/tools?page=2&page_size=2").await;
    assert_eq!(page["total"], 3);
    assert_eq!(names(&page), ["gamma"]);
}
