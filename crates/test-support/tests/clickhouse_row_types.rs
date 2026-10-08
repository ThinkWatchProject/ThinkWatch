//! Every ClickHouse read the console makes, against rows with every
//! column filled in.
//!
//! The clickhouse crate validates each result against the Rust row it
//! lands in: column names and types must match exactly, `i64` against
//! `UInt64` included, and a mismatch fails the query. The check runs on
//! the first row and on each non-NULL value, so a query that returns
//! nothing, or only NULLs, proves nothing. This file seeds all five log
//! tables (and so the rollups their materialised views feed) with
//! complete rows, then calls each endpoint in each of its query shapes:
//! global and team-scoped, every range, `compare`, every `group_by`
//! dimension. Endpoints that hide a failed query behind an empty answer
//! are checked for data, not only for a 200.

use serde_json::Value;
use think_watch_test_support::prelude::*;

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

/// What the seeded rows point at, so the tests can ask for them.
struct Seeded {
    member: fixtures::SeededUser,
    manager: fixtures::SeededUser,
    team: Uuid,
    provider_name: String,
    model_id: String,
    route_id: Uuid,
    role_id: Uuid,
    trace_id: String,
    gateway_log_id: String,
    mcp_log_id: String,
}

async fn seed(app: &TestApp) -> Seeded {
    let db = &app.db;
    let ch = app
        .state
        .clickhouse
        .as_ref()
        .expect("ClickHouse configured");

    // Postgres: a team with one member, a manager scoped to that team,
    // a key, a model route and an MCP server for the rows to name.
    let team: Uuid =
        sqlx::query_scalar("INSERT INTO teams (name, description) VALUES ($1, 'rt') RETURNING id")
            .bind(unique_name("rowtypes-team"))
            .fetch_one(db)
            .await
            .unwrap();
    let member = fixtures::create_random_user(db).await.unwrap();
    sqlx::query("INSERT INTO team_members (user_id, team_id) VALUES ($1, $2)")
        .bind(member.user.id)
        .bind(team)
        .execute(db)
        .await
        .unwrap();
    let manager = fixtures::create_user(db, &unique_email(), "Manager", "MgrPwd_1234567!")
        .await
        .unwrap();
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, scope_id, assigned_by)
           SELECT $1, id, 'team', $2, $1 FROM rbac_roles WHERE name = 'team_manager'"#,
    )
    .bind(manager.user.id)
    .bind(team)
    .execute(db)
    .await
    .unwrap();
    let key = fixtures::create_api_key(
        db,
        member.user.id,
        &unique_name("rowtypes-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE api_keys SET cost_center = 'eng' WHERE id = $1")
        .bind(key.row.id)
        .execute(db)
        .await
        .unwrap();

    let provider_name = unique_name("rowtypes-provider");
    let provider =
        fixtures::create_provider(db, &provider_name, "openai", "http://127.0.0.1:9", None)
            .await
            .unwrap();
    let model_id = unique_name("rowtypes-model");
    fixtures::create_model_and_route(db, provider.id, &model_id)
        .await
        .unwrap();
    let route_id: Uuid =
        sqlx::query_scalar("SELECT id FROM model_routes WHERE model_id = $1 AND provider_id = $2")
            .bind(&model_id)
            .bind(provider.id)
            .fetch_one(db)
            .await
            .unwrap();

    let short = Uuid::new_v4().simple().to_string()[..12].to_string();
    let server_id = fixtures::create_mcp_server(
        db,
        &format!("rowtypes-mcp-{short}"),
        &format!("rt_{short}"),
        "http://127.0.0.1:9/mcp",
    )
    .await
    .unwrap();
    let role_id: Uuid = sqlx::query_scalar("SELECT id FROM rbac_roles WHERE name = 'team_manager'")
        .fetch_one(db)
        .await
        .unwrap();

    // ClickHouse: rows with no NULL anywhere. Recent rows feed the 15-
    // minute and one-hour windows; older ones the 7- and 30-day buckets
    // and the previous windows `compare` reads.
    let trace_id = Uuid::new_v4().to_string();
    let gateway_log_id = Uuid::new_v4().to_string();
    let mcp_log_id = Uuid::new_v4().to_string();
    let user = member.user.id.to_string();
    let email = member.user.email.clone();
    let key_id = key.row.id.to_string();
    let lineage = key.row.lineage_id.to_string();

    for (i, age) in ["2 MINUTE", "3 HOUR", "2 DAY", "9 DAY", "40 DAY"]
        .into_iter()
        .enumerate()
    {
        let id = if i == 0 {
            gateway_log_id.clone()
        } else {
            Uuid::new_v4().to_string()
        };
        ch.query(&format!(
            "INSERT INTO gateway_logs (id, user_id, user_email, api_key_id, api_key_lineage_id, \
               model_id, provider, upstream_model, input_tokens, output_tokens, cost_usd, \
               latency_ms, status_code, ip_address, user_agent, detail, trace_id, session_id, \
               request_body, response_body, request_body_bytes, response_body_bytes, \
               body_capture_status, created_at) VALUES \
             ('{id}', '{user}', '{email}', '{key_id}', '{lineage}', '{model_id}', \
              '{provider_name}', '{model_id}', 120, 80, toDecimal64('0.0123', 10), 345, 200, \
              '10.0.0.1', 'rowtypes', '{{}}', '{trace_id}', 'sess', '{{\"q\":1}}', \
              '{{\"a\":1}}', 7, 7, 'captured', now64(3) - INTERVAL {age})"
        ))
        .execute()
        .await
        .unwrap();
    }
    // An error and a throttled answer for the success / error rates.
    for status in [500, 429] {
        ch.query(&format!(
            "INSERT INTO gateway_logs (id, user_id, user_email, api_key_id, api_key_lineage_id, \
               model_id, provider, upstream_model, input_tokens, output_tokens, cost_usd, \
               latency_ms, status_code, ip_address, user_agent, detail, trace_id, session_id, \
               request_body, response_body, request_body_bytes, response_body_bytes, \
               body_capture_status, created_at) VALUES \
             ('{}', '{user}', '{email}', '{key_id}', '{lineage}', '{model_id}', \
              '{provider_name}', '{model_id}', 1, 1, toDecimal64('0.0001', 10), 50, {status}, \
              '10.0.0.1', 'rowtypes', '{{}}', '{trace_id}', 'sess', '{{}}', '{{}}', 2, 2, \
              'captured', now64(3) - INTERVAL 1 MINUTE)",
            Uuid::new_v4()
        ))
        .execute()
        .await
        .unwrap();
    }
    for (i, age) in ["2 MINUTE", "2 DAY", "40 DAY"].into_iter().enumerate() {
        let id = if i == 0 {
            mcp_log_id.clone()
        } else {
            Uuid::new_v4().to_string()
        };
        ch.query(&format!(
            "INSERT INTO mcp_logs (id, user_id, user_email, server_id, server_name, tool_name, \
               duration_ms, status, error_message, ip_address, detail, tool_arguments, \
               tool_result, arguments_bytes, result_bytes, body_capture_status, trace_id, \
               created_at) VALUES \
             ('{id}', '{user}', '{email}', '{server_id}', 'rowtypes-mcp', 'search', 42, 'ok', \
              'none', '10.0.0.1', '{{}}', '{{\"q\":1}}', '{{\"r\":1}}', 7, 7, 'captured', \
              '{trace_id}', now64(3) - INTERVAL {age})"
        ))
        .execute()
        .await
        .unwrap();
    }
    let role = role_id.to_string();
    for (resource, resource_id) in [
        ("role", role.as_str()),
        ("rate_limit_rule", user.as_str()),
        ("api_key", key_id.as_str()),
    ] {
        ch.query(&format!(
            "INSERT INTO audit_logs (id, user_id, user_email, api_key_id, api_key_lineage_id, \
               action, resource, resource_id, detail, ip_address, user_agent, trace_id, \
               created_at) VALUES \
             ('{}', '{user}', '{email}', '{key_id}', '{lineage}', '{resource}.update', \
              '{resource}', '{resource_id}', '{{\"subject_id\":\"{user}\"}}', '10.0.0.1', \
              'rowtypes', '{trace_id}', now64(3) - INTERVAL 2 MINUTE)",
            Uuid::new_v4()
        ))
        .execute()
        .await
        .unwrap();
    }
    ch.query(&format!(
        "INSERT INTO app_logs (id, level, target, message, fields, span, created_at) VALUES \
         ('{}', 'INFO', 'rowtypes', 'seeded', '{{\"trace_id\":\"{trace_id}\"}}', \
          'request{{trace_id={trace_id}}}', now64(3) - INTERVAL 2 MINUTE)",
        Uuid::new_v4()
    ))
    .execute()
    .await
    .unwrap();
    ch.query(&format!(
        "INSERT INTO access_logs (id, method, path, status_code, latency_ms, port, user_id, \
           user_email, ip_address, user_agent, created_at) VALUES \
         ('{}', 'GET', '/api/rowtypes', 200, 12, 3001, '{user}', '{email}', '10.0.0.1', \
          'rowtypes', now64(3) - INTERVAL 2 MINUTE)",
        Uuid::new_v4()
    ))
    .execute()
    .await
    .unwrap();

    Seeded {
        member,
        manager,
        team,
        provider_name,
        model_id,
        route_id,
        role_id,
        trace_id,
        gateway_log_id,
        mcp_log_id,
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn analytics_and_dashboard_queries_read_their_rows() {
    let app = TestApp::spawn_with_clickhouse().await;
    let admin = admin_session(&app).await;
    let s = seed(&app).await;
    let mgr = login(&app, &s.manager).await;
    let team = s.team;

    for con in [&admin, &mgr] {
        for range in ["24h", "7d", "30d"] {
            for q in [
                format!("/api/analytics/usage/stats?range={range}&compare=true"),
                format!("/api/analytics/costs/stats?range={range}&compare=true"),
                format!("/api/dashboard/stats?range={range}&compare=true"),
                format!("/api/analytics/usage?range={range}"),
            ] {
                get(con, &q).await;
            }
            for group_by in [
                "model",
                "user",
                "cost_center",
                "provider",
                "model,provider,user",
            ] {
                let body = get(
                    con,
                    &format!("/api/analytics/costs?range={range}&group_by={group_by}"),
                )
                .await;
                assert!(
                    !body["items"].as_array().unwrap().is_empty(),
                    "{group_by} {range}: {body}"
                );
            }
        }
        // The AI row and the MCP row both come from ClickHouse, and the
        // AI one through the rollup only for the global view.
        let live = get(con, "/api/dashboard/live").await;
        let providers = live["providers"].as_array().unwrap();
        for name in [s.provider_name.as_str(), "rowtypes-mcp"] {
            assert!(
                providers
                    .iter()
                    .any(|p| p["provider"] == name && p["requests"].as_u64() > Some(0)),
                "{name} health: {live}"
            );
        }
        assert!(
            !live["recent_logs"].as_array().unwrap().is_empty(),
            "recent logs: {live}"
        );
    }
    get(
        &admin,
        &format!("/api/analytics/costs/stats?range=7d&compare=true&team_id={team}"),
    )
    .await;
    get(&admin, "/api/analytics/cost-forecast").await;
    get(&admin, "/api/admin/slo?hours=24").await;
    let csv = admin.get("/api/admin/chargeback.csv").await.unwrap();
    csv.assert_ok();
    assert!(csv.text().contains("eng"), "{}", csv.text());

    let license = get(&admin, "/api/admin/usage-license").await;
    assert!(license.is_object(), "{license}");

    let limits = get(
        &admin,
        &format!("/api/admin/users/{}/limits-dashboard", s.member.user.id),
    )
    .await;
    assert!(
        !limits["usage_7d"].as_array().unwrap().is_empty(),
        "usage series: {limits}"
    );
    assert!(
        !limits["recent_events"].as_array().unwrap().is_empty(),
        "recent limit events: {limits}"
    );

    let history = get(
        &admin,
        &format!(
            "/api/admin/models/{}/route-history?route_id={}&window=3600",
            s.model_id, s.route_id
        ),
    )
    .await;
    let buckets = history["buckets"].as_array().unwrap();
    assert!(!buckets.is_empty(), "route history: {history}");
    assert!(
        buckets.iter().any(|b| b["p50_ms"].as_f64() == Some(345.0)),
        "route history: {history}"
    );

    let servers = get(&admin, "/api/mcp/servers").await;
    assert!(
        servers
            .as_array()
            .unwrap()
            .iter()
            .any(|srv| srv["call_count"].as_i64() > Some(0)),
        "MCP call counts: {servers}"
    );

    for path in ["/health/ready", "/api/health", "/api/admin/settings/audit"] {
        admin.get(path).await.unwrap().assert_ok();
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn log_trace_and_history_queries_read_their_rows() {
    let app = TestApp::spawn_with_clickhouse().await;
    let admin = admin_session(&app).await;
    let s = seed(&app).await;

    for path in [
        "/api/gateway/logs",
        "/api/mcp/logs",
        "/api/audit/logs",
        "/api/admin/app-logs",
        "/api/admin/access-logs",
    ] {
        let body = get(&admin, path).await;
        let items = body["items"]
            .as_array()
            .or_else(|| body["data"].as_array())
            .or_else(|| body.as_array())
            .unwrap_or_else(|| panic!("{path}: {body}"));
        assert!(!items.is_empty(), "{path}: {body}");
    }

    // A range in the `YYYY-MM-DDTHH:MM:SS` form the API accepts. The list
    // queries select `toString(created_at) AS created_at`, which shadows
    // the column: an unqualified time filter compared text with text in
    // the data query and dropped every row of the `from` day, a space
    // sorting before `T`, while the count query still counted them.
    let now = chrono::Utc::now();
    let from = (now - chrono::Duration::hours(1)).format("%Y-%m-%dT%H:%M:%S");
    let to = (now + chrono::Duration::minutes(5)).format("%Y-%m-%dT%H:%M:%S");
    for path in [
        "/api/gateway/logs",
        "/api/mcp/logs",
        "/api/audit/logs",
        "/api/admin/app-logs",
        "/api/admin/access-logs",
    ] {
        let body = get(&admin, &format!("{path}?from={from}&to={to}")).await;
        let items = body["items"]
            .as_array()
            .unwrap_or_else(|| panic!("{path}: {body}"));
        assert!(!items.is_empty(), "{path} from {from}: {body}");
        assert!(body["total"].as_u64() > Some(0), "{path}: {body}");
    }

    let body = get(
        &admin,
        &format!("/api/admin/gateway/logs/{}/body", s.gateway_log_id),
    )
    .await;
    let at = body["created_at"].as_str().unwrap_or_default();
    assert!(chrono::DateTime::parse_from_rfc3339(at).is_ok(), "{body}");
    let body = get(
        &admin,
        &format!("/api/admin/mcp/logs/{}/body", s.mcp_log_id),
    )
    .await;
    assert!(body.is_object(), "{body}");

    let trace = get(&admin, &format!("/api/admin/trace/{}", s.trace_id)).await;
    let events = trace["events"].as_array().unwrap();
    for kind in ["gateway", "mcp", "audit", "app"] {
        assert!(
            events.iter().any(|e| e["kind"] == kind),
            "trace lacks {kind}: {trace}"
        );
    }
    // `%M` in ClickHouse's formatDateTime is the month name, not the
    // minute; the timestamps have to be RFC 3339.
    for e in events {
        let at = e["created_at"].as_str().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(at).is_ok(),
            "not RFC 3339: {at}"
        );
    }

    let history = get(&admin, &format!("/api/admin/roles/{}/history", s.role_id)).await;
    assert!(
        !history["items"].as_array().unwrap().is_empty(),
        "role history: {history}"
    );
}

/// A ClickHouse read that fails is an error where the answer is the
/// ClickHouse data: an empty trace or a zero usage meter would read as
/// "nothing happened", which is how a broken query goes unnoticed. Where
/// ClickHouse only adds to data from Postgres (dashboard tiles, the MCP
/// server list's call counts), the page still loads and the failure is
/// logged.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_failed_clickhouse_read_is_an_error_where_the_answer_is_clickhouse_data() {
    let app = TestApp::spawn_with_clickhouse().await;
    let admin = admin_session(&app).await;
    let ch = app
        .state
        .clickhouse
        .as_ref()
        .expect("ClickHouse configured");
    // Every read of these two tables fails from here on.
    for table in ["gateway_logs", "mcp_logs"] {
        ch.query(&format!("RENAME TABLE {table} TO {table}_gone"))
            .execute()
            .await
            .unwrap();
    }

    for path in [
        "/api/admin/trace/no-such-trace",
        "/api/admin/usage-license",
        "/api/analytics/usage/stats?range=24h&compare=true",
        "/api/analytics/costs/stats?range=24h&compare=true",
    ] {
        let resp = admin.get(path).await.unwrap();
        assert_eq!(resp.status, 500, "{path} answered without ClickHouse");
    }
    for path in [
        "/api/dashboard/stats?range=24h&compare=true",
        "/api/mcp/servers",
    ] {
        admin.get(path).await.unwrap().assert_ok();
    }
}
