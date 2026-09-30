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
//! A role granted at team scope administers that team and nothing more:
//! it contributes no gateway (model or MCP tool) access. A role attached
//! to the team itself is the members' working role and does count.

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
// Gateway access and team scope
// ---------------------------------------------------------------------------

const GW_MODEL_A: &str = "tm-model-a";
const GW_MODEL_B: &str = "tm-model-b";

async fn two_routed_models(app: &TestApp) -> MockProvider {
    let upstream = MockProvider::openai_chat_ok(GW_MODEL_A).await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("tm-prov"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, GW_MODEL_A)
        .await
        .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, GW_MODEL_B)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    upstream
}

async fn gateway_key(app: &TestApp, user_id: Uuid) -> TestClient {
    let key = fixtures::create_api_key(
        &app.db,
        user_id,
        &unique_name("tm-gw-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    gw
}

async fn grant_at_team_scope(db: &sqlx::PgPool, user_id: Uuid, role: &str, team: Uuid) {
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, scope_id, assigned_by)
           SELECT $1, id, 'team', $2, $1 FROM rbac_roles WHERE name = $3"#,
    )
    .bind(user_id)
    .bind(team)
    .bind(role)
    .execute(db)
    .await
    .unwrap();
}

fn call(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_scoped_role_grants_no_gateway_model_access() {
    // Gateway requests carry no team. A role granted at scope
    // `team:<id>` is for administering that team, so it must not widen
    // the models a user can call — neither for a member of the team nor
    // for anyone else.
    let app = TestApp::spawn().await;
    let _upstream = two_routed_models(&app).await;

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
    let gw = gateway_key(&app, user.user.id).await;

    gw.post("/v1/chat/completions", call(GW_MODEL_A))
        .await
        .unwrap()
        .assert_ok();

    // `developer` (Resource "*") at the scope of a team the user is not
    // a member of, then of one it is a member of.
    let other_team = make_team(&app.db, "tm-gw-other").await;
    grant_at_team_scope(&app.db, user.user.id, "developer", other_team).await;
    let own_team = make_team(&app.db, "tm-gw-own").await;
    add_to_team(&app.db, own_team, user.user.id).await;
    grant_at_team_scope(&app.db, user.user.id, "developer", own_team).await;

    let denied = gw
        .post("/v1/chat/completions", call(GW_MODEL_B))
        .await
        .unwrap();
    assert!(
        !denied.status.is_success(),
        "model-b must stay refused: team-scoped grants carry no gateway access: {}",
        denied.text()
    );
    gw.post("/v1/chat/completions", call(GW_MODEL_A))
        .await
        .unwrap()
        .assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_scoped_grant_alone_gives_no_gateway_access() {
    // A team manager with no global role manages its team from the
    // console, but the grant does not let it call models.
    let app = TestApp::spawn().await;
    let _upstream = two_routed_models(&app).await;
    let team = make_team(&app.db, "tm-gw-only").await;
    let manager = team_manager_of(&app, team).await;
    add_to_team(&app.db, team, manager.user.id).await;
    let gw = gateway_key(&app, manager.user.id).await;

    let resp = gw
        .post("/v1/chat/completions", call(GW_MODEL_A))
        .await
        .unwrap();
    resp.assert_status(403);
    assert!(resp.text().contains("ai_gateway:use"), "{}", resp.text());

    // The console side of the grant is untouched.
    let con = login(&app, &manager).await;
    con.get(&format!("/api/admin/teams/{team}"))
        .await
        .unwrap()
        .assert_ok();
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn role_attached_to_a_team_gives_its_members_gateway_access() {
    // Roles attached to the team itself ("All members automatically
    // inherit the team's roles and permissions") are how a team hands
    // its members their working role. They keep counting.
    let app = TestApp::spawn().await;
    let _upstream = two_routed_models(&app).await;
    let team = make_team(&app.db, "tm-gw-inherit").await;
    sqlx::query(
        "INSERT INTO team_role_assignments (team_id, role_id)
         SELECT $1, id FROM rbac_roles WHERE name = 'developer'",
    )
    .bind(team)
    .execute(&app.db)
    .await
    .unwrap();
    let member = fixtures::create_user(&app.db, &unique_email(), "Member", "MemberPwd_12345!")
        .await
        .unwrap();
    add_to_team(&app.db, team, member.user.id).await;
    let outsider = fixtures::create_user(&app.db, &unique_email(), "Out", "OutPwd_12345!")
        .await
        .unwrap();

    gateway_key(&app, member.user.id)
        .await
        .post("/v1/chat/completions", call(GW_MODEL_B))
        .await
        .unwrap()
        .assert_ok();
    gateway_key(&app, outsider.user.id)
        .await
        .post("/v1/chat/completions", call(GW_MODEL_B))
        .await
        .unwrap()
        .assert_status(403);
}
