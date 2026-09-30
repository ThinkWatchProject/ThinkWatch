//! What the seeded `team_manager` role can do when it is granted at
//! team scope — the shape its description says it is meant for.
//!
//!   - read the team it manages: the team list, the team, its roster
//!     and its roles (`teams:read`);
//!   - manage rate limits and budget caps for the other members of that
//!     team and their API keys (`rate_limits:read` / `rate_limits:write`),
//!     but not for itself, not for anyone outside the team, and not by
//!     pairing an in-scope subject with someone else's row id.
//!
//! The last test pins how gateway permissions from a team-scoped role
//! behave today: scope is not consulted on the gateway path.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use uuid::Uuid;

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

async fn make_team(db: &sqlx::PgPool, prefix: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO teams (name, description) VALUES ($1, 'rbac') RETURNING id")
        .bind(unique_name(prefix))
        .fetch_one(db)
        .await
        .unwrap()
}

async fn add_to_team(db: &sqlx::PgPool, team_id: Uuid, user_id: Uuid) {
    sqlx::query("INSERT INTO team_members (team_id, user_id) VALUES ($1, $2)")
        .bind(team_id)
        .bind(user_id)
        .execute(db)
        .await
        .unwrap();
}

async fn team_manager_of(app: &TestApp, team_id: Uuid) -> fixtures::SeededUser {
    fixtures::create_user_with_role(&app.db, "team_manager", "team", Some(team_id))
        .await
        .unwrap()
}

fn rule_body(max_count: i64) -> Value {
    json!({
        "surface": "ai_gateway",
        "metric": "requests",
        "window_secs": 60,
        "max_count": max_count,
    })
}

async fn rule_count(db: &sqlx::PgPool, subject_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM rate_limit_rules WHERE subject_id = $1")
        .bind(subject_id)
        .fetch_one(db)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// teams:read
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_manager_can_read_the_team_it_manages() {
    let app = TestApp::spawn().await;
    let team_a = make_team(&app.db, "tm-read-a").await;
    let team_b = make_team(&app.db, "tm-read-b").await;
    let member = fixtures::create_random_user(&app.db).await.unwrap();
    add_to_team(&app.db, team_a, member.user.id).await;
    // The manager is NOT a member of team A: every read below has to
    // come from the team-scoped grant, not the members' baseline right.
    let manager = team_manager_of(&app, team_a).await;
    let con = login(&app, &manager).await;

    let resp = con.get("/api/admin/teams").await.unwrap();
    resp.assert_ok();
    let teams: Vec<Value> = resp.json().unwrap();
    let ids: Vec<&str> = teams.iter().filter_map(|t| t["id"].as_str()).collect();
    assert_eq!(ids, vec![team_a.to_string()], "team list: {teams:?}");

    con.get(&format!("/api/admin/teams/{team_a}"))
        .await
        .unwrap()
        .assert_ok();

    let resp = con
        .get(&format!("/api/admin/teams/{team_a}/members"))
        .await
        .unwrap();
    resp.assert_ok();
    let members: Vec<Value> = resp.json().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["user_id"], json!(member.user.id));

    con.get(&format!("/api/admin/teams/{team_a}/roles"))
        .await
        .unwrap()
        .assert_ok();

    // Another team stays out of reach.
    for path in [
        format!("/api/admin/teams/{team_b}"),
        format!("/api/admin/teams/{team_b}/members"),
        format!("/api/admin/teams/{team_b}/roles"),
    ] {
        con.get(&path).await.unwrap().assert_status(403);
    }
}

// ---------------------------------------------------------------------------
// rate_limits:read / rate_limits:write
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_manager_can_manage_limits_for_members_of_its_team() {
    let app = TestApp::spawn().await;
    let team = make_team(&app.db, "tm-limits").await;
    let member = fixtures::create_random_user(&app.db).await.unwrap();
    add_to_team(&app.db, team, member.user.id).await;
    let key = fixtures::create_api_key(
        &app.db,
        member.user.id,
        &unique_name("tm-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let manager = team_manager_of(&app, team).await;
    let con = login(&app, &manager).await;

    // User subject: write, read, delete.
    let resp = con
        .post(
            &format!("/api/admin/limits/user/{}/rules", member.user.id),
            rule_body(10),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let rule: Value = resp.json().unwrap();
    let rule_id = rule["id"].as_str().unwrap().to_string();
    let resp = con
        .get(&format!("/api/admin/limits/user/{}/rules", member.user.id))
        .await
        .unwrap();
    resp.assert_ok();
    let listed: Value = resp.json().unwrap();
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    con.post(
        &format!("/api/admin/limits/user/{}/budgets", member.user.id),
        json!({"period": "daily", "limit_tokens": 1000}),
    )
    .await
    .unwrap()
    .assert_ok();
    con.delete(&format!(
        "/api/admin/limits/user/{}/rules/{rule_id}",
        member.user.id
    ))
    .await
    .unwrap()
    .assert_ok();
    assert_eq!(rule_count(&app.db, member.user.id).await, 0);

    // API key subject (persisted against the key's lineage).
    con.post(
        &format!("/api/admin/limits/api_key/{}/rules", key.row.id),
        rule_body(5),
    )
    .await
    .unwrap()
    .assert_ok();
    assert_eq!(rule_count(&app.db, key.row.lineage_id).await, 1);

    // Bulk apply across the team.
    let resp = con
        .post(
            "/api/admin/limits/bulk/rules",
            json!({
                "targets": [{"kind": "user", "id": member.user.id}],
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 3600,
                "max_count": 100,
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["success_count"], json!(1), "{body}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_scoped_limits_grant_does_not_cover_the_manager_itself() {
    // A team manager is usually a member of the team it manages. The
    // team-scoped grant must not let it lift its own limits.
    let app = TestApp::spawn().await;
    let team = make_team(&app.db, "tm-self").await;
    let manager = team_manager_of(&app, team).await;
    add_to_team(&app.db, team, manager.user.id).await;
    let own_key = fixtures::create_api_key(
        &app.db,
        manager.user.id,
        &unique_name("tm-own-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let con = login(&app, &manager).await;

    con.post(
        &format!("/api/admin/limits/user/{}/rules", manager.user.id),
        rule_body(1_000_000),
    )
    .await
    .unwrap()
    .assert_status(403);
    con.post(
        &format!("/api/admin/limits/api_key/{}/rules", own_key.row.id),
        rule_body(1_000_000),
    )
    .await
    .unwrap()
    .assert_status(403);
    let resp = con
        .post(
            "/api/admin/limits/bulk/rules",
            json!({
                "targets": [{"kind": "user", "id": manager.user.id}],
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 1_000_000,
            }),
        )
        .await
        .unwrap();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["success_count"], json!(0), "{body}");
    assert_eq!(rule_count(&app.db, manager.user.id).await, 0);
    assert_eq!(rule_count(&app.db, own_key.row.lineage_id).await, 0);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn limits_delete_is_bound_to_the_subject_in_the_path() {
    // Scope is checked on the subject in the URL; the row id must
    // belong to that subject, or a manager could pair its own member's
    // id with an outsider's rule id.
    let app = TestApp::spawn().await;
    let team = make_team(&app.db, "tm-bound").await;
    let member = fixtures::create_random_user(&app.db).await.unwrap();
    add_to_team(&app.db, team, member.user.id).await;
    let outsider = fixtures::create_random_user(&app.db).await.unwrap();
    let outsider_rule = fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        outsider.user.id,
        "ai_gateway",
        "requests",
        60,
        100,
    )
    .await
    .unwrap();
    let outsider_cap =
        fixtures::create_budget_cap(&app.db, "user", outsider.user.id, "daily", 1000)
            .await
            .unwrap();
    let manager = team_manager_of(&app, team).await;
    let con = login(&app, &manager).await;

    con.delete(&format!(
        "/api/admin/limits/user/{}/rules/{outsider_rule}",
        member.user.id
    ))
    .await
    .unwrap()
    .assert_status(404);
    con.delete(&format!(
        "/api/admin/limits/user/{}/budgets/{outsider_cap}",
        member.user.id
    ))
    .await
    .unwrap()
    .assert_status(404);
    assert_eq!(rule_count(&app.db, outsider.user.id).await, 1);
    let caps: i64 = sqlx::query_scalar("SELECT count(*) FROM budget_caps WHERE id = $1")
        .bind(outsider_cap)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(caps, 1);
}

// ---------------------------------------------------------------------------
// Gateway permissions from a team-scoped role (current behaviour)
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_scoped_role_widens_gateway_model_access_platform_wide() {
    // Pins today's behaviour: gateway requests carry no team, and the
    // model allow-list is the union of every role the user holds,
    // whatever its scope. A role granted at scope `team:<id>` therefore
    // widens model access for every request the user makes — the user
    // does not even have to be a member of that team.
    let app = TestApp::spawn().await;
    let upstream = MockProvider::openai_chat_ok("tm-model-a").await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("tm-prov"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "tm-model-a")
        .await
        .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "tm-model-b")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;

    let only_a: Uuid = sqlx::query_scalar(
        r#"INSERT INTO rbac_roles (name, is_system, policy_document)
           VALUES ($1, FALSE, '{"Version":"2024-01-01","Statement":[{"Effect":"Allow",
             "Action":["ai_gateway:use"],"Resource":["model:tm-model-a"]}]}')
           RETURNING id"#,
    )
    .bind(unique_name("only-model-a"))
    .fetch_one(&app.db)
    .await
    .unwrap();
    let user = fixtures::create_user(&app.db, &unique_email(), "Scoped", "ScopedPwd_12345!")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
         VALUES ($1, $2, 'global', $1)",
    )
    .bind(user.user.id)
    .bind(only_a)
    .execute(&app.db)
    .await
    .unwrap();
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("tm-gw-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    let call = |model: &'static str| json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});

    gw.post("/v1/chat/completions", call("tm-model-a"))
        .await
        .unwrap()
        .assert_ok();
    let denied = gw
        .post("/v1/chat/completions", call("tm-model-b"))
        .await
        .unwrap();
    assert!(
        !denied.status.is_success(),
        "model-b must be refused under the global model-a-only role: {}",
        denied.text()
    );

    // Grant `developer` (Resource "*") scoped to a team the user is
    // not a member of.
    let team = make_team(&app.db, "tm-gw").await;
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, scope_id, assigned_by)
           SELECT $1, id, 'team', $2, $1 FROM rbac_roles WHERE name = 'developer'"#,
    )
    .bind(user.user.id)
    .bind(team)
    .execute(&app.db)
    .await
    .unwrap();

    gw.post("/v1/chat/completions", call("tm-model-b"))
        .await
        .unwrap()
        .assert_ok();
}
