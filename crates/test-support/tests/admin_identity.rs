//! The identity admin endpoints end to end: users, roles and teams.
//!
//! Many branches here (scoped listings, role reassignment on delete,
//! the team member cap, PATCH null-vs-absent) were only reached through
//! the UI before; this file pins what each one reads and writes, so
//! moving their SQL around (into `services::*_repository`) is checked
//! rather than assumed.

use serde_json::Value;
use think_watch_test_support::prelude::*;

async fn login_as(app: &TestApp, user: &fixtures::SeededUser) -> TestClient {
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

async fn get(con: &TestClient, path: &str) -> Value {
    let resp = con.get(path).await.unwrap();
    resp.assert_ok();
    resp.json().unwrap()
}

async fn role_id(app: &TestApp, name: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM rbac_roles WHERE name = $1")
        .bind(name)
        .fetch_one(&app.db)
        .await
        .unwrap()
}

async fn create_team(con: &TestClient, name: &str) -> String {
    let resp = con
        .post(
            "/api/admin/teams",
            json!({"name": name, "description": " d "}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let team: Value = resp.json().unwrap();
    team["id"].as_str().expect("team id").to_string()
}

async fn create_role(con: &TestClient, name: &str, actions: &[&str]) -> String {
    let resp = con
        .post(
            "/api/admin/roles",
            json!({
                "name": name,
                "description": "identity test",
                "policy_document": {
                    "Version": "2024-01-01",
                    "Statement": [{"Sid": "T", "Effect": "Allow", "Action": actions, "Resource": "*"}]
                }
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let role: Value = resp.json().unwrap();
    role["id"].as_str().expect("role id").to_string()
}

async fn api_key_state(app: &TestApp, id: Uuid) -> (bool, bool, Option<String>) {
    sqlx::query_as(
        "SELECT is_active, deleted_at IS NOT NULL, disabled_reason FROM api_keys WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&app.db)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn users_are_created_with_roles_and_listed_with_search() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;
    let developer = role_id(&app, "developer").await;
    let viewer = role_id(&app, "viewer").await;
    let team = create_team(&con, &unique_name("ident-team")).await;

    // A tag with LIKE wildcards in it: the search must treat them
    // literally.
    let tag = format!("x_{}%", Uuid::new_v4().simple());
    let email = unique_email();
    let resp = con
        .post(
            "/api/admin/users",
            json!({
                "email": email.to_uppercase(),
                "display_name": format!("Probe {tag}"),
                "role_assignments": [
                    {"role_id": developer},
                    {"role_id": viewer, "scope": format!("team:{team}")},
                ],
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let created: Value = resp.json().unwrap();
    let user_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["email"], email.as_str(), "email is normalized");
    assert!(
        created["generated_password"].is_string(),
        "no password given → one is generated: {created}"
    );
    let assignments = created["role_assignments"].as_array().unwrap();
    assert_eq!(assignments.len(), 2, "{created}");
    assert_eq!(assignments[0]["name"], "developer");
    assert_eq!(assignments[0]["scope"], "global");
    assert_eq!(assignments[0]["is_system"], true);
    assert_eq!(assignments[1]["name"], "viewer");
    assert_eq!(assignments[1]["scope"], format!("team:{team}"));
    let force_change: bool =
        sqlx::query_scalar("SELECT password_change_required FROM users WHERE id = $1::uuid")
            .bind(&user_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert!(force_change);

    // Supplying a password: nothing generated, no forced change.
    let resp = con
        .post(
            "/api/admin/users",
            json!({"email": unique_email(), "display_name": "With pwd", "password": "Supplied_Pwd_1234!"}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let with_pwd: Value = resp.json().unwrap();
    assert!(with_pwd.get("generated_password").is_none(), "{with_pwd}");
    assert_eq!(with_pwd["role_assignments"], json!([]));

    // Duplicate email, unknown role, bad scope.
    con.post(
        "/api/admin/users",
        json!({"email": email, "display_name": "Dup"}),
    )
    .await
    .unwrap()
    .assert_status(409);
    con.post(
        "/api/admin/users",
        json!({"email": unique_email(), "display_name": "R", "role_assignments": [{"role_id": Uuid::new_v4()}]}),
    )
    .await
    .unwrap()
    .assert_status(400);
    con.post(
        "/api/admin/users",
        json!({"email": unique_email(), "display_name": "S", "role_assignments": [{"role_id": developer, "scope": "org:x"}]}),
    )
    .await
    .unwrap()
    .assert_status(400);
    // The failed inserts above rolled back: no stray user row.
    let strays: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE display_name IN ('R', 'S', 'Dup')")
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(strays, 0);

    // Add the new user to the team so the list reports it.
    con.post(
        &format!("/api/admin/teams/{team}/members"),
        json!({"user_id": user_id}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Search matches the literal tag only.
    let list = get(
        &con,
        &format!("/api/admin/users?search={}", urlencode(&tag)),
    )
    .await;
    assert_eq!(list["total"], 1, "{list}");
    let row = &list["data"][0];
    assert_eq!(row["id"], user_id.as_str());
    assert_eq!(row["role_assignments"].as_array().unwrap().len(), 2);
    assert_eq!(row["teams"][0]["id"], team.as_str());
    assert_eq!(row["permissions"], json!([]));
    let none = get(&con, "/api/admin/users?search=x%25nomatch").await;
    assert_eq!(none["total"], 0);
    assert_eq!(none["data"], json!([]));

    // Unfiltered: every live user, newest first, paginated.
    let page = get(&con, "/api/admin/users?per_page=1&page=2").await;
    assert_eq!(page["total"], 3, "admin + two created: {page}");
    assert_eq!(page["page"], 2);
    assert_eq!(page["per_page"], 1);
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    let all = get(&con, "/api/admin/users").await;
    let ids: Vec<&str> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids[2], admin.user.id.to_string(), "oldest last: {all}");

    let supers = get(&con, "/api/admin/users/super-admin-ids").await;
    assert_eq!(supers["ids"], json!([admin.user.id]));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn an_admin_cannot_hand_out_super_admin() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_user_with_role(&app.db, "admin", "global", None)
        .await
        .unwrap();
    let con = login_as(&app, &admin).await;
    let super_admin = role_id(&app, "super_admin").await;
    con.post(
        "/api/admin/users",
        json!({"email": unique_email(), "display_name": "Esc", "role_assignments": [{"role_id": super_admin}]}),
    )
    .await
    .unwrap()
    .assert_status(403);

    let target = fixtures::create_random_user(&app.db).await.unwrap();
    con.patch(
        &format!("/api/admin/users/{}", target.user.id),
        json!({"role_assignments": [{"role_id": super_admin}]}),
    )
    .await
    .unwrap()
    .assert_status(403);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_team_scoped_reader_sees_only_its_teams_and_their_members() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let scoped_team = create_team(&con, &unique_name("scoped")).await;
    let own_team = create_team(&con, &unique_name("own")).await;
    let other_team = create_team(&con, &unique_name("other")).await;
    let role = create_role(
        &con,
        &unique_name("team-reader"),
        &["teams:read", "users:read"],
    )
    .await;

    let reader = fixtures::create_random_user(&app.db).await.unwrap();
    sqlx::query(
        "INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, scope_id, assigned_by) \
         VALUES ($1, $2::uuid, 'team', $3::uuid, $1)",
    )
    .bind(reader.user.id)
    .bind(&role)
    .bind(&scoped_team)
    .execute(&app.db)
    .await
    .unwrap();
    let member = fixtures::create_random_user(&app.db).await.unwrap();
    let outsider = fixtures::create_random_user(&app.db).await.unwrap();
    for (team, user) in [
        (&scoped_team, member.user.id),
        (&own_team, reader.user.id),
        (&other_team, outsider.user.id),
    ] {
        con.post(
            &format!("/api/admin/teams/{team}/members"),
            json!({"user_id": user}),
        )
        .await
        .unwrap()
        .assert_ok();
    }

    let reader_con = login_as(&app, &reader).await;
    let teams = get(&reader_con, "/api/admin/teams").await;
    let mut seen: Vec<&str> = teams
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    seen.sort();
    let mut want = vec![scoped_team.as_str(), own_team.as_str()];
    want.sort();
    assert_eq!(seen, want, "{teams}");

    let users = get(&reader_con, "/api/admin/users").await;
    assert_eq!(
        users["total"], 2,
        "self + the scoped team's member: {users}"
    );
    let ids: Vec<&str> = users["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&reader.user.id.to_string().as_str()));
    assert!(ids.contains(&member.user.id.to_string().as_str()));
    let searched = get(
        &reader_con,
        &format!("/api/admin/users?search={}", urlencode(&member.user.email)),
    )
    .await;
    assert_eq!(searched["total"], 1, "{searched}");
    let hidden = get(
        &reader_con,
        &format!(
            "/api/admin/users?search={}",
            urlencode(&outsider.user.email)
        ),
    )
    .await;
    assert_eq!(hidden["total"], 0, "{hidden}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn updating_a_user_replaces_roles_and_disabling_revokes_keys() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();
    let key = fixtures::create_api_key(&app.db, target.user.id, "k", &["ai_gateway"], None, None)
        .await
        .unwrap();
    let viewer = role_id(&app, "viewer").await;
    let path = format!("/api/admin/users/{}", target.user.id);

    con.patch(&format!("/api/admin/users/{}", Uuid::new_v4()), json!({}))
        .await
        .unwrap()
        .assert_status(404);
    con.patch(&path, json!({"display_name": "  "}))
        .await
        .unwrap()
        .assert_status(400);
    con.patch(
        &format!("/api/admin/users/{}", admin.user.id),
        json!({"is_active": false}),
    )
    .await
    .unwrap()
    .assert_status(400);
    con.patch(
        &format!("/api/admin/users/{}", admin.user.id),
        json!({"role_assignments": []}),
    )
    .await
    .unwrap()
    .assert_status(400);

    let resp = con
        .patch(
            &path,
            json!({"display_name": "  Renamed  ", "role_assignments": [{"role_id": viewer}]}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["status"], "updated");
    let row = get(
        &con,
        &format!("/api/admin/users?search={}", urlencode(&target.user.email)),
    )
    .await;
    let row = &row["data"][0];
    assert_eq!(row["display_name"], "Renamed");
    let roles: Vec<&str> = row["role_assignments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["viewer"], "developer replaced: {row}");
    assert_eq!(row["is_active"], true);
    assert_eq!(api_key_state(&app, key.row.id).await, (true, false, None));

    con.patch(&path, json!({"is_active": false}))
        .await
        .unwrap()
        .assert_ok();
    let row = get(
        &con,
        &format!("/api/admin/users?search={}", urlencode(&target.user.email)),
    )
    .await;
    assert_eq!(row["data"][0]["is_active"], false);
    assert_eq!(
        api_key_state(&app, key.row.id).await,
        (false, true, Some("user_disabled".into()))
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn deleting_a_user_soft_deletes_it_and_its_keys() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;
    let target = fixtures::create_random_user(&app.db).await.unwrap();
    let key = fixtures::create_api_key(&app.db, target.user.id, "k", &["ai_gateway"], None, None)
        .await
        .unwrap();

    con.delete(&format!("/api/admin/users/{}", admin.user.id))
        .await
        .unwrap()
        .assert_status(400);
    con.delete(&format!("/api/admin/users/{}", Uuid::new_v4()))
        .await
        .unwrap()
        .assert_status(404);

    let resp = con
        .delete(&format!("/api/admin/users/{}", target.user.id))
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body["status"], "deleted");
    let (active, deleted): (bool, bool) =
        sqlx::query_as("SELECT is_active, deleted_at IS NOT NULL FROM users WHERE id = $1")
            .bind(target.user.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!((active, deleted), (false, true));
    assert_eq!(
        api_key_state(&app, key.row.id).await,
        (false, true, Some("user_deleted".into()))
    );
    // Gone from the list, and a second delete finds nothing.
    let list = get(&con, "/api/admin/users").await;
    assert_eq!(list["total"], 1, "{list}");
    con.delete(&format!("/api/admin/users/{}", target.user.id))
        .await
        .unwrap()
        .assert_status(404);
    // Reset-password on a deleted user is a 404 too.
    con.post_empty(&format!(
        "/api/admin/users/{}/reset-password",
        target.user.id
    ))
    .await
    .unwrap()
    .assert_status(404);
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_role_is_created_updated_listed_and_its_members_shown() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;
    let name = unique_name("ident-role");
    let resp = con
        .post(
            "/api/admin/roles",
            json!({
                "name": format!("  {name} "),
                "description": "first",
                "policy_document": {"Version": "2024-01-01", "Statement": [{"Effect": "Allow", "Action": ["models:read"], "Resource": "*"}]}
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let role: Value = resp.json().unwrap();
    let id = role["id"].as_str().unwrap().to_string();
    assert_eq!(role["name"], name.as_str());
    assert_eq!(role["is_system"], false);
    assert_eq!(role["user_count"], 0);
    assert_eq!(role["created_by_email"], admin.user.email.as_str());

    // Same name again → 400.
    con.post(
        "/api/admin/roles",
        json!({"name": name, "policy_document": {"Version": "2024-01-01", "Statement": []}}),
    )
    .await
    .unwrap()
    .assert_status(400);

    // Two members: one global, one team-scoped.
    let team = create_team(&con, &unique_name("role-team")).await;
    let a = fixtures::create_user_with_role(&app.db, &name, "global", None)
        .await
        .unwrap();
    let b = fixtures::create_user_with_role(&app.db, &name, "team", Some(team.parse().unwrap()))
        .await
        .unwrap();

    // PATCH: absent description is kept, rename works.
    let renamed = format!("{name}-2");
    let resp = con
        .patch(&format!("/api/admin/roles/{id}"), json!({"name": renamed}))
        .await
        .unwrap();
    resp.assert_ok();
    let patched: Value = resp.json().unwrap();
    assert_eq!(patched["name"], renamed.as_str());
    assert_eq!(patched["description"], "first");
    assert_eq!(patched["user_count"], 2);
    // JSON null clears it.
    let resp = con
        .patch(
            &format!("/api/admin/roles/{id}"),
            json!({"description": null}),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let cleared: Value = resp.json().unwrap();
    assert!(cleared["description"].is_null(), "{cleared}");
    assert_eq!(cleared["name"], renamed.as_str());

    con.patch(&format!("/api/admin/roles/{}", Uuid::new_v4()), json!({}))
        .await
        .unwrap()
        .assert_status(404);
    let developer = role_id(&app, "developer").await;
    con.patch(
        &format!("/api/admin/roles/{developer}"),
        json!({"name": "renamed-dev"}),
    )
    .await
    .unwrap()
    .assert_status(400);
    con.post_empty(&format!("/api/admin/roles/{}/reset", Uuid::new_v4()))
        .await
        .unwrap()
        .assert_status(404);

    // List: system roles first, counts joined in.
    let list = get(&con, "/api/admin/roles").await;
    let items = list["items"].as_array().unwrap();
    let first_custom = items.iter().position(|r| r["is_system"] == false).unwrap();
    assert!(items[..first_custom].iter().all(|r| r["is_system"] == true));
    let ours = items.iter().find(|r| r["id"] == id.as_str()).unwrap();
    assert_eq!(ours["user_count"], 2);
    let supers = items.iter().find(|r| r["name"] == "super_admin").unwrap();
    assert_eq!(supers["user_count"], 1);

    // Members, ordered by email, with encoded scopes.
    let members = get(&con, &format!("/api/admin/roles/{id}/members")).await;
    let members = members["items"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    let mut want = vec![
        (a.user.email.clone(), "global".to_string()),
        (b.user.email.clone(), format!("team:{team}")),
    ];
    want.sort();
    let got: Vec<(String, String)> = members
        .iter()
        .map(|m| {
            (
                m["email"].as_str().unwrap().to_string(),
                m["scope"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(got, want);
    con.get(&format!("/api/admin/roles/{}/members", Uuid::new_v4()))
        .await
        .unwrap()
        .assert_status(404);
    con.get(&format!("/api/admin/roles/{}/history", Uuid::new_v4()))
        .await
        .unwrap()
        .assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn deleting_a_role_reassigns_its_members() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let from_name = unique_name("from");
    let from = create_role(&con, &from_name, &["models:read"]).await;
    let to = create_role(&con, &unique_name("to"), &["models:read"]).await;
    let empty = create_role(&con, &unique_name("empty"), &["models:read"]).await;
    let a = fixtures::create_user_with_role(&app.db, &from_name, "global", None)
        .await
        .unwrap();
    fixtures::create_user_with_role(&app.db, &from_name, "global", None)
        .await
        .unwrap();

    let developer = role_id(&app, "developer").await;
    con.delete(&format!("/api/admin/roles/{developer}"))
        .await
        .unwrap()
        .assert_status(400);
    con.delete(&format!("/api/admin/roles/{}", Uuid::new_v4()))
        .await
        .unwrap()
        .assert_status(404);
    con.delete(&format!("/api/admin/roles/{from}"))
        .await
        .unwrap()
        .assert_status(400);
    con.delete(&format!("/api/admin/roles/{from}?reassign_to={from}"))
        .await
        .unwrap()
        .assert_status(400);
    con.delete(&format!(
        "/api/admin/roles/{from}?reassign_to={}",
        Uuid::new_v4()
    ))
    .await
    .unwrap()
    .assert_status(400);

    let resp = con
        .delete(&format!("/api/admin/roles/{from}?reassign_to={to}"))
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body, json!({"deleted": true, "reassigned": 2}));
    let gone: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rbac_roles WHERE id = $1::uuid)")
            .bind(&from)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert!(!gone);
    let members = get(&con, &format!("/api/admin/roles/{to}/members")).await;
    let emails: Vec<&str> = members["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["email"].as_str().unwrap())
        .collect();
    assert_eq!(emails.len(), 2);
    assert!(emails.contains(&a.user.email.as_str()));

    let resp = con
        .delete(&format!("/api/admin/roles/{empty}"))
        .await
        .unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body, json!({"deleted": true, "reassigned": 0}));
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn role_history_reads_the_audit_log() {
    let app = TestApp::spawn_with_clickhouse().await;
    let con = admin_session(&app).await;
    let id = create_role(&con, &unique_name("hist"), &["models:read"]).await;
    let mut actions = Vec::new();
    for _ in 0..40 {
        let history = get(&con, &format!("/api/admin/roles/{id}/history")).await;
        actions = history["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action"].as_str().unwrap().to_string())
            .collect();
        if !actions.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert_eq!(actions, vec!["role.created".to_string()]);
}

// ---------------------------------------------------------------------------
// Teams
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_team_is_read_updated_and_deleted() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let name = unique_name("ident-team");
    let id = create_team(&con, &name).await;
    let other = unique_name("ident-other");
    create_team(&con, &other).await;
    con.post("/api/admin/teams", json!({"name": name}))
        .await
        .unwrap()
        .assert_status(409);

    let member = fixtures::create_random_user(&app.db).await.unwrap();
    con.post(
        &format!("/api/admin/teams/{id}/members"),
        json!({"user_id": member.user.id}),
    )
    .await
    .unwrap()
    .assert_ok();

    let team = get(&con, &format!("/api/admin/teams/{id}")).await;
    assert_eq!(team["name"], name.as_str());
    assert_eq!(team["description"], "d", "trimmed on create");
    assert_eq!(team["member_count"], 1);
    con.get(&format!("/api/admin/teams/{}", Uuid::new_v4()))
        .await
        .unwrap()
        .assert_status(404);

    let list = get(&con, "/api/admin/teams").await;
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "ordered by name");
    let ours = list
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id.as_str())
        .unwrap();
    assert_eq!(ours["member_count"], 1);

    // PATCH: absent keeps, null clears, whitespace name refused,
    // a taken name conflicts.
    let path = format!("/api/admin/teams/{id}");
    let renamed = format!("{name}-2");
    let resp = con.patch(&path, json!({"name": renamed})).await.unwrap();
    resp.assert_ok();
    let t: Value = resp.json().unwrap();
    assert_eq!(t["name"], renamed.as_str());
    assert_eq!(t["description"], "d");
    let resp = con
        .patch(&path, json!({"description": null}))
        .await
        .unwrap();
    resp.assert_ok();
    let t: Value = resp.json().unwrap();
    assert!(t["description"].is_null(), "{t}");
    assert_eq!(t["name"], renamed.as_str());
    con.patch(&path, json!({"name": "   "}))
        .await
        .unwrap()
        .assert_status(400);
    con.patch(&path, json!({"name": other}))
        .await
        .unwrap()
        .assert_status(409);
    con.patch(
        &format!("/api/admin/teams/{}", Uuid::new_v4()),
        json!({"name": "x"}),
    )
    .await
    .unwrap()
    .assert_status(404);

    let resp = con.delete(&path).await.unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body, json!({"status": "deleted"}));
    con.delete(&path).await.unwrap().assert_status(404);
    let members: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM team_members WHERE team_id = $1::uuid")
            .bind(&id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(members, 0, "memberships cascade");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_membership_is_capped_idempotent_and_hides_deleted_users() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();

    let first = create_team(&con, &unique_name("cap")).await;
    con.post(
        &format!("/api/admin/teams/{first}/members"),
        json!({"user_id": Uuid::new_v4()}),
    )
    .await
    .unwrap()
    .assert_status(404);

    let mut teams = vec![first];
    for _ in 1..10 {
        teams.push(create_team(&con, &unique_name("cap")).await);
    }
    for team in &teams {
        let resp = con
            .post(
                &format!("/api/admin/teams/{team}/members"),
                json!({"user_id": user.user.id}),
            )
            .await
            .unwrap();
        resp.assert_ok();
        let body: Value = resp.json().unwrap();
        assert_eq!(body, json!({"status": "added"}));
    }
    // At the cap: re-adding an existing membership is still fine,
    // an eleventh team is refused.
    con.post(
        &format!("/api/admin/teams/{}/members", teams[0]),
        json!({"user_id": user.user.id}),
    )
    .await
    .unwrap()
    .assert_ok();
    let eleventh = create_team(&con, &unique_name("cap")).await;
    con.post(
        &format!("/api/admin/teams/{eleventh}/members"),
        json!({"user_id": user.user.id}),
    )
    .await
    .unwrap()
    .assert_status(400);

    // A deactivated user can't be added.
    let inactive = fixtures::create_random_user(&app.db).await.unwrap();
    con.patch(
        &format!("/api/admin/users/{}", inactive.user.id),
        json!({"is_active": false}),
    )
    .await
    .unwrap()
    .assert_ok();
    con.post(
        &format!("/api/admin/teams/{eleventh}/members"),
        json!({"user_id": inactive.user.id}),
    )
    .await
    .unwrap()
    .assert_status(404);

    // Roster: in join order, soft-deleted users hidden.
    let second = fixtures::create_random_user(&app.db).await.unwrap();
    con.post(
        &format!("/api/admin/teams/{}/members", teams[0]),
        json!({"user_id": second.user.id}),
    )
    .await
    .unwrap()
    .assert_ok();
    let roster = get(&con, &format!("/api/admin/teams/{}/members", teams[0])).await;
    let ids: Vec<&str> = roster
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["user_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![user.user.id.to_string(), second.user.id.to_string()]
    );
    assert_eq!(roster[0]["email"], user.user.email.as_str());
    con.delete(&format!("/api/admin/users/{}", user.user.id))
        .await
        .unwrap()
        .assert_ok();
    let roster = get(&con, &format!("/api/admin/teams/{}/members", teams[0])).await;
    assert_eq!(roster.as_array().unwrap().len(), 1, "{roster}");

    // Remove: once fine, twice a 404.
    let path = format!("/api/admin/teams/{}/members/{}", teams[0], second.user.id);
    let resp = con.delete(&path).await.unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body, json!({"status": "removed"}));
    con.delete(&path).await.unwrap().assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_member_reads_its_own_team_without_teams_read() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let team = create_team(&con, &unique_name("own")).await;
    let other = create_team(&con, &unique_name("other")).await;
    // Developers hold no teams:read at all.
    let dev = fixtures::create_random_user(&app.db).await.unwrap();
    con.post(
        &format!("/api/admin/teams/{team}/members"),
        json!({"user_id": dev.user.id}),
    )
    .await
    .unwrap()
    .assert_ok();
    let dev_con = login_as(&app, &dev).await;
    let roster = get(&dev_con, &format!("/api/admin/teams/{team}/members")).await;
    assert_eq!(roster[0]["user_id"], dev.user.id.to_string());
    dev_con
        .get(&format!("/api/admin/teams/{other}/members"))
        .await
        .unwrap()
        .assert_status(403);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn team_roles_are_assigned_listed_and_removed() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;
    let team = create_team(&con, &unique_name("roles")).await;
    let custom_name = unique_name("a-custom");
    let custom = create_role(&con, &custom_name, &["models:read"]).await;
    let viewer = role_id(&app, "viewer").await;
    let path = format!("/api/admin/teams/{team}/roles");

    for role in [json!(custom), json!(viewer), json!(viewer)] {
        let resp = con.post(&path, json!({"role_id": role})).await.unwrap();
        resp.assert_ok();
        let body: Value = resp.json().unwrap();
        assert_eq!(body, json!({"status": "assigned"}));
    }
    let roles = get(&con, &path).await;
    let names: Vec<&str> = roles
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["viewer", custom_name.as_str()], "system first");
    assert_eq!(roles[0]["role_id"], viewer.to_string());
    assert_eq!(roles[0]["is_system"], true);
    assert!(roles[0]["assigned_at"].is_string());

    let resp = con.delete(&format!("{path}/{viewer}")).await.unwrap();
    resp.assert_ok();
    let body: Value = resp.json().unwrap();
    assert_eq!(body, json!({"status": "removed"}));
    // Removing again is still a 200.
    con.delete(&format!("{path}/{viewer}"))
        .await
        .unwrap()
        .assert_ok();
    let roles = get(&con, &path).await;
    assert_eq!(roles.as_array().unwrap().len(), 1, "{roles}");
}

/// Percent-encode a query value (the handful of characters the tests
/// put in search terms).
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
