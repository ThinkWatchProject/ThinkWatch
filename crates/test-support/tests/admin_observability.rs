//! The observability and limits endpoints end to end: dashboard tiles,
//! live snapshot and layout, health probes, route health, analytics
//! scoping, log forwarders, the webhook outbox, and API-key limit
//! subjects.
//!
//! Several of these were only reached through the UI before; this file
//! pins what each one reads and writes, so moving their SQL around
//! (into `services::*_repository`) is checked rather than assumed.

use chrono::{Duration, Utc};
use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn get(con: &TestClient, path: &str) -> Value {
    let resp = con.get(path).await.unwrap();
    resp.assert_ok();
    resp.json().unwrap()
}

async fn login(app: &TestApp, user: &fixtures::SeededUser) -> TestClient {
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": user.user.email, "password": user.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con
}

async fn create_team(app: &TestApp) -> Uuid {
    sqlx::query_scalar("INSERT INTO teams (name, description) VALUES ($1, 'obs') RETURNING id")
        .bind(unique_name("obs-team"))
        .fetch_one(&app.db)
        .await
        .unwrap()
}

async fn add_member(app: &TestApp, team_id: Uuid, user_id: Uuid) {
    sqlx::query("INSERT INTO team_members (user_id, team_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(team_id)
        .execute(&app.db)
        .await
        .unwrap();
}

/// A user whose only role is `team_manager` scoped to `team_id` — no
/// global developer role, so every permission it has is team-scoped.
async fn create_team_manager(app: &TestApp, team_id: Uuid) -> fixtures::SeededUser {
    let user = fixtures::create_user(&app.db, &unique_email(), "Manager", "MgrPwd_1234567!")
        .await
        .unwrap();
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, scope_id, assigned_by)
           SELECT $1, id, 'team', $2, $1 FROM rbac_roles WHERE name = 'team_manager'"#,
    )
    .bind(user.user.id)
    .bind(team_id)
    .execute(&app.db)
    .await
    .unwrap();
    user
}

async fn api_key(app: &TestApp, user_id: Uuid) -> fixtures::SeededApiKey {
    fixtures::create_api_key(
        &app.db,
        user_id,
        &unique_name("obs-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap()
}

async fn set_last_used(app: &TestApp, key_id: Uuid, at: chrono::DateTime<Utc>) {
    sqlx::query("UPDATE api_keys SET last_used_at = $2 WHERE id = $1")
        .bind(key_id)
        .bind(at)
        .execute(&app.db)
        .await
        .unwrap();
}

async fn create_mcp_server_with_status(app: &TestApp, status: &str) -> String {
    let short = Uuid::new_v4().simple().to_string()[..12].to_string();
    let name = format!("obs-mcp-{short}");
    let id = fixtures::create_mcp_server(
        &app.db,
        &name,
        &format!("obs_{short}"),
        "http://127.0.0.1:9/mcp",
    )
    .await
    .unwrap();
    sqlx::query("UPDATE mcp_servers SET status = $2 WHERE id = $1")
        .bind(id)
        .bind(status)
        .execute(&app.db)
        .await
        .unwrap();
    name
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn dashboard_stats_count_from_postgres_without_clickhouse() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;

    let upstream = MockProvider::openai_chat_ok("obs-model").await;
    fixtures::create_provider(
        &app.db,
        &unique_name("live"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    let off = fixtures::create_provider(
        &app.db,
        &unique_name("off"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE providers SET is_active = false WHERE id = $1")
        .bind(off.id)
        .execute(&app.db)
        .await
        .unwrap();
    create_mcp_server_with_status(&app, "connected").await;
    create_mcp_server_with_status(&app, "disconnected").await;

    // One key used in the current 24h window, one in the window before,
    // one used now but inactive.
    let now = Utc::now();
    let current = api_key(&app, admin.user.id).await;
    set_last_used(&app, current.row.id, now - Duration::hours(1)).await;
    let previous = api_key(&app, admin.user.id).await;
    set_last_used(&app, previous.row.id, now - Duration::hours(30)).await;
    let inactive = api_key(&app, admin.user.id).await;
    set_last_used(&app, inactive.row.id, now - Duration::minutes(5)).await;
    sqlx::query("UPDATE api_keys SET is_active = false WHERE id = $1")
        .bind(inactive.row.id)
        .execute(&app.db)
        .await
        .unwrap();

    let stats = get(&con, "/api/dashboard/stats?range=24h&compare=true").await;
    assert_eq!(stats["range"], "24h", "{stats}");
    assert_eq!(stats["active_providers"], 1, "{stats}");
    assert_eq!(stats["connected_mcp_servers"], 1, "{stats}");
    assert_eq!(stats["active_api_keys"], 1, "{stats}");
    assert_eq!(stats["prev_active_api_keys"], 1, "{stats}");
    assert_eq!(stats["total_requests"], 0, "{stats}");
    assert_eq!(stats["prev_total_requests"], 0, "{stats}");
    assert_eq!(stats["active_keys_buckets"].as_array().unwrap().len(), 24);

    // Without `compare` the previous-window fields are left out; a 7d
    // window takes in the 30h-old key too.
    let week = get(&con, "/api/dashboard/stats?range=7d").await;
    assert_eq!(week["active_api_keys"], 2, "{week}");
    assert!(week.get("prev_active_api_keys").is_none(), "{week}");
    assert_eq!(week["active_keys_buckets"].as_array().unwrap().len(), 7);

    // A team-scoped caller gets the same platform-wide tiles.
    let team = create_team(&app).await;
    let manager = create_team_manager(&app, team).await;
    let scoped = get(&login(&app, &manager).await, "/api/dashboard/stats").await;
    assert_eq!(scoped["active_providers"], 1, "{scoped}");
    assert_eq!(scoped["total_requests"], 0, "{scoped}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn dashboard_live_lists_configured_providers_servers_and_rpm_limit() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;

    let upstream = MockProvider::openai_chat_ok("obs-model").await;
    let live_name = unique_name("live");
    fixtures::create_provider(&app.db, &live_name, "openai", &upstream.uri(), None)
        .await
        .unwrap();
    let off_name = unique_name("off");
    let off = fixtures::create_provider(&app.db, &off_name, "openai", &upstream.uri(), None)
        .await
        .unwrap();
    sqlx::query("UPDATE providers SET deleted_at = now() WHERE id = $1")
        .bind(off.id)
        .execute(&app.db)
        .await
        .unwrap();
    let up = create_mcp_server_with_status(&app, "connected").await;
    let down = create_mcp_server_with_status(&app, "disconnected").await;

    // No per-minute request rule yet → no reference line.
    let live = get(&con, "/api/dashboard/live").await;
    assert!(live["max_rpm_limit"].is_null(), "{live}");

    fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        admin.user.id,
        "ai_gateway",
        "requests",
        60,
        250,
    )
    .await
    .unwrap();
    let disabled = fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        admin.user.id,
        "mcp_gateway",
        "requests",
        60,
        900,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE rate_limit_rules SET enabled = false WHERE id = $1")
        .bind(disabled)
        .execute(&app.db)
        .await
        .unwrap();
    fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        admin.user.id,
        "ai_gateway",
        "requests",
        3600,
        5000,
    )
    .await
    .unwrap();

    let live = get(&con, "/api/dashboard/live").await;
    assert_eq!(live["max_rpm_limit"], 250, "{live}");
    assert_eq!(live["rpm_buckets"].as_array().unwrap().len(), 30);
    let rows = live["providers"].as_array().unwrap();
    let row = |name: &str| rows.iter().find(|r| r["provider"] == name);
    let ai = row(&live_name).unwrap_or_else(|| panic!("no {live_name}: {live}"));
    assert_eq!(ai["kind"], "ai");
    assert!(
        row(&off_name).is_none(),
        "a deleted provider is not listed: {live}"
    );
    let up_row = row(&up).unwrap_or_else(|| panic!("no {up}: {live}"));
    assert_eq!(up_row["kind"], "mcp");
    assert!(up_row["success_rate"].is_null(), "{up_row}");
    let down_row = row(&down).unwrap_or_else(|| panic!("no {down}: {live}"));
    assert_eq!(down_row["success_rate"], 0.0, "{down_row}");

    // A team-scoped caller resolves a user filter first; without
    // ClickHouse the snapshot is the same.
    let team = create_team(&app).await;
    let manager = create_team_manager(&app, team).await;
    let scoped = get(&login(&app, &manager).await, "/api/dashboard/live").await;
    assert_eq!(scoped["max_rpm_limit"], 250, "{scoped}");
    assert_eq!(
        scoped["providers"].as_array().unwrap().len(),
        rows.len(),
        "{scoped}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn dashboard_layout_is_saved_per_user() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let layout = get(&con, "/api/dashboard/layout").await;
    assert_eq!(layout["name"], "default", "{layout}");
    assert!(layout["layout_json"].is_null(), "{layout}");

    con.put(
        "/api/dashboard/layout",
        json!({"name": "ops", "layout_json": {"cards": ["a", "b"]}}),
    )
    .await
    .unwrap()
    .assert_ok();
    let layout = get(&con, "/api/dashboard/layout").await;
    assert_eq!(layout["name"], "ops", "{layout}");
    assert_eq!(layout["layout_json"], json!({"cards": ["a", "b"]}));

    // Saving again replaces the row; an empty name saves as "default".
    con.put(
        "/api/dashboard/layout",
        json!({"name": "", "layout_json": {"cards": ["c"]}}),
    )
    .await
    .unwrap()
    .assert_ok();
    let layout = get(&con, "/api/dashboard/layout").await;
    assert_eq!(layout["name"], "default", "{layout}");
    assert_eq!(layout["layout_json"], json!({"cards": ["c"]}));
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_dashboard_layouts")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(rows, 1);

    // Another user still sees the built-in default.
    let other = admin_session(&app).await;
    let layout = get(&other, "/api/dashboard/layout").await;
    assert!(layout["layout_json"].is_null(), "{layout}");

    let big = "x".repeat(17 * 1024);
    con.put(
        "/api/dashboard/layout",
        json!({"name": "big", "layout_json": {"blob": big}}),
    )
    .await
    .unwrap()
    .assert_status(400);
}

/// Two users call the gateway, one of them in a team; the team's manager
/// sees only that member's traffic on the dashboard and in the cost
/// breakdown, an admin sees both.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_scoped_dashboard_and_cost_breakdowns_with_clickhouse() {
    let app = TestApp::spawn_with_clickhouse().await;
    let admin = admin_session(&app).await;

    let upstream = MockProvider::openai_chat_ok("obs-model").await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("obs"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "obs-model")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let team = create_team(&app).await;
    let member = fixtures::create_random_user(&app.db).await.unwrap();
    add_member(&app, team, member.user.id).await;
    let outsider = fixtures::create_random_user(&app.db).await.unwrap();
    let manager = create_team_manager(&app, team).await;

    let member_key = api_key(&app, member.user.id).await;
    sqlx::query("UPDATE api_keys SET cost_center = 'eng' WHERE id = $1")
        .bind(member_key.row.id)
        .execute(&app.db)
        .await
        .unwrap();
    let outsider_key = api_key(&app, outsider.user.id).await;
    for key in [&member_key, &outsider_key] {
        let gw = app.gateway_client();
        gw.set_bearer(&key.plaintext);
        gw.post(
            "/v1/chat/completions",
            json!({"model": "obs-model", "messages": [{"role": "user", "content": "x"}]}),
        )
        .await
        .unwrap()
        .assert_ok();
    }

    // The audit pipeline writes to ClickHouse asynchronously.
    let mut stats = Value::Null;
    for _ in 0..200 {
        stats = get(&admin, "/api/dashboard/stats").await;
        if stats["total_requests"] == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(stats["total_requests"], 2, "{stats}");
    assert_eq!(stats["active_api_keys"], 2, "{stats}");

    let mgr = login(&app, &manager).await;
    let scoped = get(&mgr, "/api/dashboard/stats").await;
    assert_eq!(scoped["total_requests"], 1, "{scoped}");
    assert_eq!(scoped["active_api_keys"], 1, "{scoped}");

    let rpm_sum = |live: &Value| -> u64 {
        live["rpm_buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .sum()
    };
    let live = get(&admin, "/api/dashboard/live").await;
    assert_eq!(rpm_sum(&live), 2, "{live}");
    let live = get(&mgr, "/api/dashboard/live").await;
    assert_eq!(rpm_sum(&live), 1, "{live}");

    // Costs by user show emails; the manager sees the member only.
    let users_of = |body: &Value| -> Vec<String> {
        let mut v: Vec<String> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["dimensions"]["user"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    let all = get(&admin, "/api/analytics/costs?group_by=user&range=24h").await;
    let mut both = vec![member.user.email.clone(), outsider.user.email.clone()];
    both.sort();
    assert_eq!(users_of(&all), both, "{all}");
    let mine = get(&mgr, "/api/analytics/costs?group_by=user&range=24h").await;
    assert_eq!(users_of(&mine), vec![member.user.email.clone()], "{mine}");
    let by_team = get(
        &admin,
        &format!("/api/analytics/costs?group_by=user&range=24h&team_id={team}"),
    )
    .await;
    assert_eq!(
        users_of(&by_team),
        vec![member.user.email.clone()],
        "{by_team}"
    );

    // Costs by cost center label each key by its tag.
    let cc = get(
        &admin,
        "/api/analytics/costs?group_by=cost_center&range=24h",
    )
    .await;
    let mut labels: Vec<String> = cc["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["dimensions"]["cost_center"].as_str().unwrap().to_string())
        .collect();
    labels.sort();
    assert_eq!(labels, vec!["(untagged)", "eng"], "{cc}");

    // Usage stats go through the same scope resolution.
    let usage = get(&mgr, "/api/analytics/usage/stats?range=24h").await;
    assert!(usage.is_object(), "{usage}");
}

// ---------------------------------------------------------------------------
// Health and route health
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn health_probes_check_postgres_and_providers() {
    let app = TestApp::spawn().await;
    let public = app.console_client();

    let resp = public.get("/health/ready").await.unwrap();
    resp.assert_status(503);
    let body: Value = resp.json().unwrap();
    assert_eq!(body["postgres"], true, "{body}");
    assert_eq!(body["providers"], false, "{body}");

    let upstream = MockProvider::openai_chat_ok("obs-model").await;
    fixtures::create_provider(
        &app.db,
        &unique_name("ready"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    let resp = public.get("/health/ready").await.unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["status"], "ready", "{body}");

    let con = admin_session(&app).await;
    let health = get(&con, "/api/health").await;
    assert_eq!(health["postgres"], true, "{health}");
    assert!(health["pg_latency_ms"].is_i64(), "{health}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn route_health_lists_a_models_routes_heaviest_first() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let model = unique_name("obs-model");

    let upstream = MockProvider::openai_chat_ok(&model).await;
    let light_name = unique_name("light");
    let light = fixtures::create_provider(&app.db, &light_name, "openai", &upstream.uri(), None)
        .await
        .unwrap();
    let heavy_name = unique_name("heavy");
    let heavy = fixtures::create_provider(&app.db, &heavy_name, "openai", &upstream.uri(), None)
        .await
        .unwrap();
    let gone = fixtures::create_provider(
        &app.db,
        &unique_name("gone"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_route(&app.db, light.id, &model, 10)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, heavy.id, &model, 90)
        .await
        .unwrap();
    fixtures::create_model_route(&app.db, gone.id, &model, 50)
        .await
        .unwrap();
    sqlx::query("UPDATE providers SET deleted_at = now() WHERE id = $1")
        .bind(gone.id)
        .execute(&app.db)
        .await
        .unwrap();

    let routes = get(&con, &format!("/api/admin/models/{model}/route-health")).await;
    let routes = routes.as_array().unwrap();
    assert_eq!(routes.len(), 2, "{routes:?}");
    assert_eq!(routes[0]["provider_name"], heavy_name.as_str());
    assert_eq!(routes[0]["weight"], 90);
    assert_eq!(routes[0]["provider_id"], heavy.id.to_string());
    assert_eq!(routes[0]["upstream_model"], model.as_str());
    assert_eq!(routes[0]["enabled"], true);
    assert!(routes[0]["health"].is_object(), "{:?}", routes[0]);
    assert_eq!(routes[1]["provider_name"], light_name.as_str());

    let none = get(&con, "/api/admin/models/no-such-model/route-health").await;
    assert_eq!(none, json!([]));
}

// ---------------------------------------------------------------------------
// Limits on an API key
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn api_key_limits_are_stored_on_the_key_lineage() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;

    let root = api_key(&app, admin.user.id).await;
    let rotated = api_key(&app, admin.user.id).await;
    sqlx::query("UPDATE api_keys SET lineage_id = $2 WHERE id = $1")
        .bind(rotated.row.id)
        .bind(root.row.id)
        .execute(&app.db)
        .await
        .unwrap();

    // Written through the rotated key's id, stored on the lineage root.
    let resp = con
        .post(
            &format!("/api/admin/limits/api_key/{}/rules", rotated.row.id),
            json!({"surface": "ai_gateway", "metric": "requests", "window_secs": 60, "max_count": 42}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let rule: Value = resp.json().unwrap();
    assert_eq!(rule["subject_id"], root.row.id.to_string(), "{rule}");

    let listed = get(
        &con,
        &format!("/api/admin/limits/api_key/{}/rules", root.row.id),
    )
    .await;
    assert_eq!(listed["items"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["items"][0]["max_count"], 42);

    con.get(&format!(
        "/api/admin/limits/api_key/{}/rules",
        Uuid::new_v4()
    ))
    .await
    .unwrap()
    .assert_status(404);
}

// ---------------------------------------------------------------------------
// Log forwarders and the webhook outbox
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_log_forwarder_is_created_edited_tested_and_removed() {
    let app = TestApp::spawn_reaching_loopback().await;
    let con = admin_session(&app).await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    con.post(
        "/api/admin/log-forwarders",
        json!({"name": "bad", "forwarder_type": "webhook", "config": {"url": receiver.uri()},
               "log_types": ["platform"]}),
    )
    .await
    .unwrap()
    .assert_status(400);

    let name = unique_name("fwd");
    let resp = con
        .post(
            "/api/admin/log-forwarders",
            json!({"name": name, "forwarder_type": "webhook",
                   "config": {"url": receiver.uri()}, "enabled": false}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let created: Value = resp.json().unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["enabled"], false, "{created}");
    assert_eq!(created["log_types"], json!(["audit"]), "{created}");

    let list = get(&con, "/api/admin/log-forwarders").await;
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == id.as_str()),
        "{list}"
    );

    // PATCH keeps what it isn't given.
    let resp = con
        .patch(
            &format!("/api/admin/log-forwarders/{id}"),
            json!({"name": "renamed", "log_types": ["audit", "gateway"]}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let updated: Value = resp.json().unwrap();
    assert_eq!(updated["name"], "renamed");
    assert_eq!(updated["log_types"], json!(["audit", "gateway"]));
    assert_eq!(updated["config"]["url"], receiver.uri());
    assert_eq!(updated["enabled"], false);
    con.patch(
        &format!("/api/admin/log-forwarders/{}", Uuid::new_v4()),
        json!({"name": "x"}),
    )
    .await
    .unwrap()
    .assert_status(404);

    // Pause / resume sets the state it is given.
    for enabled in [true, true, false] {
        let resp = con
            .post(
                &format!("/api/admin/log-forwarders/{id}/toggle"),
                json!({"enabled": enabled}),
            )
            .await
            .unwrap();
        resp.assert_ok();
        let body: Value = resp.json().unwrap();
        assert_eq!(body["enabled"], enabled, "{body}");
    }
    con.post(
        &format!("/api/admin/log-forwarders/{}/toggle", Uuid::new_v4()),
        json!({"enabled": true}),
    )
    .await
    .unwrap()
    .assert_status(404);

    sqlx::query(
        "UPDATE log_forwarders SET sent_count = 5, error_count = 2, last_error = 'boom' \
         WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(&app.db)
    .await
    .unwrap();
    let resp = con
        .post_empty(&format!("/api/admin/log-forwarders/{id}/reset-stats"))
        .await
        .unwrap();
    resp.assert_ok();
    let reset: Value = resp.json().unwrap();
    assert_eq!(reset["sent_count"], 0, "{reset}");
    assert_eq!(reset["error_count"], 0, "{reset}");
    assert!(reset["last_error"].is_null(), "{reset}");
    con.post_empty(&format!(
        "/api/admin/log-forwarders/{}/reset-stats",
        Uuid::new_v4()
    ))
    .await
    .unwrap()
    .assert_status(404);

    let resp = con
        .post_empty(&format!("/api/admin/log-forwarders/{id}/test"))
        .await
        .unwrap();
    resp.assert_ok();
    let tested: Value = resp.json().unwrap();
    assert_eq!(tested["success"], true, "{tested}");
    assert!(!receiver.received_requests().await.unwrap().is_empty());
    con.post_empty(&format!(
        "/api/admin/log-forwarders/{}/test",
        Uuid::new_v4()
    ))
    .await
    .unwrap()
    .assert_status(404);

    con.delete(&format!("/api/admin/log-forwarders/{id}"))
        .await
        .unwrap()
        .assert_ok();
    con.delete(&format!("/api/admin/log-forwarders/{id}"))
        .await
        .unwrap()
        .assert_status(404);
    let list = get(&con, "/api/admin/log-forwarders").await;
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == id.as_str()),
        "{list}"
    );
}

async fn install_forwarder(app: &TestApp, name: &str, url: &str) -> Uuid {
    sqlx::query_scalar(
        r#"INSERT INTO log_forwarders (name, forwarder_type, config, log_types, enabled)
           VALUES ($1, 'webhook', $2, ARRAY['audit']::text[], false) RETURNING id"#,
    )
    .bind(name)
    .bind(json!({"url": url}))
    .fetch_one(&app.db)
    .await
    .unwrap()
}

/// An outbox row that is not due for a while, so the background drain
/// leaves it alone.
async fn enqueue(app: &TestApp, forwarder_id: Uuid, due_in_hours: i64) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO webhook_outbox (forwarder_id, payload, next_attempt_at, last_error) \
         VALUES ($1, '{}'::jsonb, now() + make_interval(hours => $2::int), 'HTTP 500') \
         RETURNING id",
    )
    .bind(forwarder_id)
    .bind(due_in_hours as i32)
    .fetch_one(&app.db)
    .await
    .unwrap()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_webhook_outbox_is_listed_counted_retried_and_pruned() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let a_name = unique_name("outbox-a");
    let a = install_forwarder(&app, &a_name, "http://127.0.0.1:9/a").await;
    let b = install_forwarder(&app, &unique_name("outbox-b"), "http://127.0.0.1:9/b").await;
    let a_late = enqueue(&app, a, 48).await;
    let a_soon = enqueue(&app, a, 24).await;
    let b_row = enqueue(&app, b, 36).await;

    let all = get(&con, "/api/admin/webhook-outbox").await;
    assert_eq!(all["total"], 3, "{all}");
    let ids: Vec<&str> = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    let expected = [a_soon.to_string(), b_row.to_string(), a_late.to_string()];
    assert_eq!(ids, expected, "next due first: {all}");
    let first = &all["items"][0];
    assert_eq!(first["forwarder_id"], a.to_string());
    assert_eq!(first["forwarder_name"], a_name.as_str());
    assert_eq!(first["forwarder_url"], "http://127.0.0.1:9/a");
    assert_eq!(first["attempts"], 0);
    assert_eq!(first["last_error"], "HTTP 500");

    let only_a = get(&con, &format!("/api/admin/webhook-outbox?forwarder_id={a}")).await;
    assert_eq!(only_a["total"], 2, "{only_a}");
    assert_eq!(only_a["items"].as_array().unwrap().len(), 2);

    let counts = get(&con, "/api/admin/webhook-outbox/counts").await;
    assert_eq!(
        counts,
        json!([{"forwarder_id": a, "count": 2}, {"forwarder_id": b, "count": 1}])
    );

    con.delete(&format!("/api/admin/webhook-outbox/{a_late}"))
        .await
        .unwrap()
        .assert_ok();
    con.delete(&format!("/api/admin/webhook-outbox/{a_late}"))
        .await
        .unwrap()
        .assert_status(404);
    let only_a = get(&con, &format!("/api/admin/webhook-outbox?forwarder_id={a}")).await;
    assert_eq!(only_a["total"], 1, "{only_a}");

    // Retry makes the row due now; whatever the drain then does with
    // it, it is no longer a day out.
    con.post_empty(&format!("/api/admin/webhook-outbox/{b_row}/retry"))
        .await
        .unwrap()
        .assert_ok();
    let due: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT next_attempt_at FROM webhook_outbox WHERE id = $1")
            .bind(b_row)
            .fetch_optional(&app.db)
            .await
            .unwrap();
    if let Some(due) = due {
        assert!(due < Utc::now() + Duration::hours(1), "{due}");
    }
    con.post_empty(&format!(
        "/api/admin/webhook-outbox/{}/retry",
        Uuid::new_v4()
    ))
    .await
    .unwrap()
    .assert_status(404);
}
