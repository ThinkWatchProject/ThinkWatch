//! The access endpoints end to end: API keys, the account endpoints
//! under `/api/auth`, the default-role setting, and SSO sign-in against
//! a mock identity provider.
//!
//! Login, refresh, password change, TOTP set-up / disable / recovery,
//! account deletion, first-boot setup and key rotation have their own
//! files; this one covers what they don't reach, so moving the access
//! handlers' SQL around (into `services::*_repository`) is checked
//! rather than assumed.

use serde_json::Value;
use think_watch_test_support::prelude::*;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

async fn create_key(con: &TestClient, body: Value) -> Value {
    let resp = con.post("/api/keys", body).await.unwrap();
    resp.assert_ok();
    resp.json().unwrap()
}

async fn get(con: &TestClient, path: &str) -> Value {
    let resp = con.get(path).await.unwrap();
    resp.assert_ok();
    resp.json().unwrap()
}

async fn patch(con: &TestClient, path: &str, body: Value) -> Value {
    let resp = con.patch(path, body).await.unwrap();
    resp.assert_ok();
    resp.json().unwrap()
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .expect("array")
        .iter()
        .map(|k| k["id"].as_str().unwrap().to_string())
        .collect()
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_key_is_created_read_and_patched() {
    let app = TestApp::spawn().await;
    let (con, admin) = admin_session_with_user(&app).await;

    let name = unique_name("access-key");
    let created = create_key(
        &con,
        json!({
            "name": name,
            "surfaces": ["mcp_gateway", "ai_gateway", "ai_gateway"],
            "allowed_models": ["gpt-4o"],
            "expires_in_days": 30,
            "cost_center": "  team-a  ",
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["name"], name.as_str());
    let plaintext = created["key"].as_str().unwrap();
    assert!(
        plaintext.starts_with(created["key_prefix"].as_str().unwrap()),
        "{created}"
    );

    let key = get(&con, &format!("/api/keys/{id}")).await;
    assert_eq!(key["user_id"], admin.user.id.to_string());
    assert_eq!(key["surfaces"], json!(["ai_gateway", "mcp_gateway"]));
    assert_eq!(key["allowed_models"], json!(["gpt-4o"]));
    assert!(key["allowed_mcp_tools"].is_null(), "{key}");
    assert_eq!(key["cost_center"], "team-a");
    assert_eq!(key["mcp_account_overrides"], json!({}));
    assert!(key["expires_at"].is_string(), "{key}");
    assert!(key["is_active"].as_bool().unwrap());
    assert!(key.get("key_hash").is_none(), "{key}");
    assert!(key.get("lineage_id").is_none(), "{key}");
    // A new key is the root of its own lineage.
    let (lineage_id,): (Uuid,) = sqlx::query_as("SELECT lineage_id FROM api_keys WHERE id = $1")
        .bind(Uuid::parse_str(&id).unwrap())
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(lineage_id.to_string(), id);

    // Set and clear in one PATCH: null clears a list, "" clears the
    // cost center, 0 clears the expiry.
    let patched = patch(
        &con,
        &format!("/api/keys/{id}"),
        json!({
            "allowed_models": null,
            "allowed_mcp_tools": ["github__list_issues"],
            "surfaces": ["console"],
            "expires_in_days": 0,
            "rotation_period_days": 45,
            "inactivity_timeout_days": 10,
            "cost_center": "",
        }),
    )
    .await;
    assert!(patched["allowed_models"].is_null(), "{patched}");
    assert_eq!(patched["allowed_mcp_tools"], json!(["github__list_issues"]));
    assert_eq!(patched["surfaces"], json!(["console"]));
    assert!(patched["expires_at"].is_null(), "{patched}");
    assert_eq!(patched["rotation_period_days"], 45);
    assert_eq!(patched["inactivity_timeout_days"], 10);
    assert!(patched["cost_center"].is_null(), "{patched}");

    // Absent fields are left alone.
    let untouched = patch(&con, &format!("/api/keys/{id}"), json!({})).await;
    assert_eq!(
        untouched["allowed_mcp_tools"],
        json!(["github__list_issues"])
    );
    assert_eq!(untouched["surfaces"], json!(["console"]));
    assert_eq!(untouched["rotation_period_days"], 45);
    assert_eq!(untouched["inactivity_timeout_days"], 10);
    assert!(untouched["expires_at"].is_null(), "{untouched}");

    let later = patch(
        &con,
        &format!("/api/keys/{id}"),
        json!({"expires_in_days": 5, "cost_center": "team-b", "mcp_account_overrides": {}}),
    )
    .await;
    assert!(later["expires_at"].is_string(), "{later}");
    assert_eq!(later["cost_center"], "team-b");
    assert_eq!(later["mcp_account_overrides"], json!({}));

    // Refused input.
    for body in [
        json!({"expires_in_days": -1}),
        json!({"rotation_period_days": -1}),
        json!({"surfaces": []}),
        json!({"surfaces": ["nope"]}),
        json!({"cost_center": "x".repeat(65)}),
        json!({"mcp_account_overrides": ["not", "an", "object"]}),
        json!({"mcp_account_overrides": {"not-a-uuid": "work"}}),
        json!({"mcp_account_overrides": {Uuid::new_v4().to_string(): "work"}}),
        json!({"mcp_account_overrides": {Uuid::new_v4().to_string(): 7}}),
    ] {
        con.patch(&format!("/api/keys/{id}"), body.clone())
            .await
            .unwrap()
            .assert_status(400);
    }
    for body in [
        json!({"name": "k", "surfaces": []}),
        json!({"name": "k", "surfaces": ["nope"]}),
        json!({"name": "k", "surfaces": ["ai_gateway"], "expires_in_days": -1}),
        json!({"name": "k", "surfaces": ["ai_gateway"], "mcp_account_overrides": {Uuid::new_v4().to_string(): "work"}}),
    ] {
        con.post("/api/keys", body)
            .await
            .unwrap()
            .assert_status(400);
    }

    let missing = format!("/api/keys/{}", Uuid::new_v4());
    con.get(&missing).await.unwrap().assert_status(404);
    con.patch(&missing, json!({}))
        .await
        .unwrap()
        .assert_status(404);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_key_is_created_with_the_default_expiry_setting() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    app.set_setting("api_keys.default_expiry_days", json!(0))
        .await;
    let never = create_key(&con, json!({"name": "never", "surfaces": ["ai_gateway"]})).await;
    let never = get(
        &con,
        &format!("/api/keys/{}", never["id"].as_str().unwrap()),
    )
    .await;
    assert!(never["expires_at"].is_null(), "{never}");

    app.set_setting("api_keys.default_expiry_days", json!(10))
        .await;
    app.set_setting("api_keys.rotation_period_days", json!(20))
        .await;
    let expiring = create_key(
        &con,
        json!({"name": "expiring", "surfaces": ["ai_gateway"]}),
    )
    .await;
    let expiring = get(
        &con,
        &format!("/api/keys/{}", expiring["id"].as_str().unwrap()),
    )
    .await;
    assert!(expiring["expires_at"].is_string(), "{expiring}");
    assert_eq!(expiring["rotation_period_days"], 20);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn the_key_list_pages_revokes_and_archives() {
    let app = TestApp::spawn().await;
    let (admin, _) = admin_session_with_user(&app).await;
    let dev_user = fixtures::create_random_user(&app.db).await.unwrap();
    let dev = login(&app, &dev_user).await;

    let dev_key = create_key(&dev, json!({"name": "dev", "surfaces": ["ai_gateway"]})).await;
    let dev_key = dev_key["id"].as_str().unwrap().to_string();
    let admin_key = create_key(&admin, json!({"name": "admin", "surfaces": ["ai_gateway"]})).await;
    let admin_key = admin_key["id"].as_str().unwrap().to_string();

    // A developer sees their own keys; the admin tier sees everyone's,
    // newest first, a page at a time.
    let mine = get(&dev, "/api/keys").await;
    assert_eq!(mine["total"], 1);
    assert_eq!(ids(&mine["data"]), vec![dev_key.clone()]);
    let page1 = get(&admin, "/api/keys?per_page=1").await;
    assert_eq!(page1["total"], 2);
    assert_eq!(page1["page"], 1);
    assert_eq!(page1["per_page"], 1);
    assert_eq!(ids(&page1["data"]), vec![admin_key.clone()]);
    let page2 = get(&admin, "/api/keys?per_page=1&page=2").await;
    assert_eq!(ids(&page2["data"]), vec![dev_key.clone()]);

    // Somebody else's key is out of a developer's reach.
    dev.get(&format!("/api/keys/{admin_key}"))
        .await
        .unwrap()
        .assert_status(403);

    // Revoke (developers lack `api_keys:delete`; the admin tier may
    // revoke anyone's key): once, then it's gone.
    dev.delete(&format!("/api/keys/{dev_key}"))
        .await
        .unwrap()
        .assert_status(403);
    let resp: Value = admin
        .delete(&format!("/api/keys/{dev_key}"))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(resp["status"], "revoked");
    admin
        .delete(&format!("/api/keys/{dev_key}"))
        .await
        .unwrap()
        .assert_status(404);

    // Force-revoke needs a reason, and records it.
    admin
        .post(
            &format!("/api/admin/keys/{admin_key}/force-revoke"),
            json!({"reason": "  "}),
        )
        .await
        .unwrap()
        .assert_status(400);
    let resp: Value = admin
        .post(
            &format!("/api/admin/keys/{admin_key}/force-revoke"),
            json!({"reason": "suspected leak"}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(resp["status"], "force_revoked");
    assert_eq!(resp["reason"], "suspected leak");
    admin
        .post(
            &format!("/api/admin/keys/{admin_key}/force-revoke"),
            json!({"reason": "again"}),
        )
        .await
        .unwrap()
        .assert_status(404);

    // A key that went with its deleted account is not "revoked".
    let leaver = fixtures::create_random_user(&app.db).await.unwrap();
    fixtures::create_api_key(
        &app.db,
        leaver.user.id,
        "leaver",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let leaver_con = login(&app, &leaver).await;
    leaver_con
        .delete("/api/auth/account")
        .await
        .unwrap()
        .assert_ok();

    let live = get(&admin, "/api/keys").await;
    assert_eq!(live["total"], 0, "{live}");
    let archived = get(&admin, "/api/keys?archived=true").await;
    assert_eq!(archived["total"], 2, "{archived}");
    let mut reasons: Vec<String> = archived["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["disabled_reason"].as_str().unwrap().to_string())
        .collect();
    reasons.sort();
    assert_eq!(reasons, vec!["force_revoked:suspected leak", "revoked"]);
    let dev_archived = get(&dev, "/api/keys?archived=true").await;
    assert_eq!(dev_archived["total"], 1);
    assert_eq!(ids(&dev_archived["data"]), vec![dev_key]);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn expiring_keys_cost_centers_and_policy_scope() {
    let app = TestApp::spawn().await;
    let admin = admin_session(&app).await;
    let dev_user = fixtures::create_random_user(&app.db).await.unwrap();
    let dev = login(&app, &dev_user).await;

    let key = |name: &str, days: i32, cost_center: &str| {
        json!({
            "name": name,
            "surfaces": ["ai_gateway"],
            "expires_in_days": days,
            "cost_center": cost_center,
        })
    };
    let in3 = create_key(&admin, key("in3", 3, "zeta")).await;
    let in20 = create_key(&admin, key("in20", 20, "alpha")).await;
    let dev_in2 = create_key(&dev, key("dev-in2", 2, "alpha")).await;
    let gone = create_key(&admin, key("gone", 1, "beta")).await;
    admin
        .delete(&format!("/api/keys/{}", gone["id"].as_str().unwrap()))
        .await
        .unwrap()
        .assert_ok();
    let id = |v: &Value| v["id"].as_str().unwrap().to_string();

    // Soonest first; revoked keys never show.
    let week = get(&admin, "/api/keys/expiring").await;
    assert_eq!(ids(&week), vec![id(&dev_in2), id(&in3)]);
    let month = get(&admin, "/api/keys/expiring?days=30").await;
    assert_eq!(ids(&month), vec![id(&dev_in2), id(&in3), id(&in20)]);
    let none = get(&admin, "/api/keys/expiring?days=-5").await;
    assert_eq!(ids(&none), Vec::<String>::new());
    let dev_month = get(&dev, "/api/keys/expiring?days=30").await;
    assert_eq!(ids(&dev_month), vec![id(&dev_in2)]);

    let centers = get(&admin, "/api/keys/cost-centers").await;
    assert_eq!(centers, json!(["alpha", "zeta"]));

    let scope = get(&dev, "/api/keys/policy-scope").await;
    assert!(scope.get("allowed_models").is_some(), "{scope}");
    assert!(scope.get("allowed_mcp_tools").is_some(), "{scope}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn registration_assigns_the_default_role_and_me_lists_roles_and_teams() {
    let app = TestApp::spawn().await;
    let admin = admin_session(&app).await;

    // Seeded empty: no role until an admin picks one through the API.
    assert_eq!(app.state.dynamic_config.default_role().await, None);
    // The default role must name a role that exists.
    admin
        .patch(
            "/api/admin/settings",
            json!({"settings": {"auth.default_role": "no-such-role"}}),
        )
        .await
        .unwrap()
        .assert_status(400);
    admin
        .patch(
            "/api/admin/settings",
            json!({"settings": {"auth.default_role": "viewer"}}),
        )
        .await
        .unwrap()
        .assert_ok();
    assert_eq!(
        app.state.dynamic_config.default_role().await.as_deref(),
        Some("viewer")
    );
    app.set_setting("auth.allow_registration", json!(true))
        .await;

    let email = unique_email();
    let con = app.console_client();
    con.post(
        "/api/auth/register",
        json!({"email": email, "display_name": "Newcomer", "password": "Test_password_12345!"}),
    )
    .await
    .unwrap()
    .assert_ok();
    assert!(con.cookie("__Host-access_token").is_some());

    // Registering the same address again answers the same way, without
    // a session.
    let again = app.console_client();
    let resp: Value = again
        .post(
            "/api/auth/register",
            json!({"email": email, "display_name": "Twin", "password": "Test_password_12345!"}),
        )
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(resp["expires_in"], 0);
    assert!(again.cookie("__Host-access_token").is_none());

    let (user_id,): (Uuid,) = sqlx::query_as("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_one(&app.db)
        .await
        .unwrap();
    fixtures::assign_role_global(&app.db, user_id, "developer")
        .await
        .unwrap();
    for team in ["b-team", "a-team"] {
        let team_name = unique_name(team);
        sqlx::query(
            "WITH t AS (INSERT INTO teams (name) VALUES ($1) RETURNING id) \
             INSERT INTO team_members (user_id, team_id) SELECT $2, id FROM t",
        )
        .bind(&team_name)
        .bind(user_id)
        .execute(&app.db)
        .await
        .unwrap();
    }

    let me = get(&con, "/api/auth/me").await;
    assert_eq!(me["id"], user_id.to_string());
    assert_eq!(me["email"], email.as_str());
    assert_eq!(me["display_name"], "Newcomer");
    let roles: Vec<(String, String)> = me["role_assignments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            assert!(r["is_system"].as_bool().unwrap(), "{r}");
            (
                r["name"].as_str().unwrap().to_string(),
                r["scope"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        roles,
        vec![
            ("developer".to_string(), "global".to_string()),
            ("viewer".to_string(), "global".to_string()),
        ]
    );
    let teams: Vec<&str> = me["teams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(teams.len(), 2, "{me}");
    assert!(teams[0].starts_with("a-team-") && teams[1].starts_with("b-team-"));
    assert!(!me["permissions"].as_array().unwrap().is_empty(), "{me}");

    let status = get(&con, "/api/auth/totp/status").await;
    assert_eq!(status, json!({"enabled": false, "required": false}));
}

// --- SSO against a mock identity provider ---

/// Test-only RSA key the mock identity provider signs ID tokens with.
const IDP_KEY_PEM: &str = "\
-----BEGIN RSA PRIVATE KEY-----\n\
MIIEpAIBAAKCAQEAvDyl7CDoXwG8SqSFodeEVK3aGjpg7cmsweIZIblHOLA/Ftd5\n\
D1XEOaAt7AuXjYQINz4zy3Nvcd3DCx42mCw4tdeGobZOSpGI7z5dq3rJFV+pjVGh\n\
J5o7nLO3hipNbaiZKMrCFwybh/pSF9jtf6aP1nzMKEH9kTabxbnHyiEZuNJW07oH\n\
jQhi7JdrM9+l+dfxWpnHSdyZ+6DBIG9jV9NB5fT8yJ+oGUuakUm38+7TwE1rR19L\n\
x2HW7r41s09t2Qkzjo0E7McSF1nwJM+Ek0VS4eD3zqdoz1aHLUdWavzQZJ6lyCkN\n\
A/FAdmQ912oMdlIqvfOp/2RiuVQ+QH/UJY8S5QIDAQABAoIBACUkZGr0zVUdzwz9\n\
bJ7UGzjoOv5s4X5aCnwRRHs6h1qgsDouFyWW+0qRmC4Y1XUnhcV8wRSWePmDU/6I\n\
HialpyT+W4LiKY2eLOJkMHBrIG1WvGp1nnJlhPi1H3PaOf/2wg3iAC0zICdTFcq9\n\
05MaBwzAADq7VrDGETORJmJ0aJJmm+APvLRgu/3CnAT5ja/RPpcCcRgA9Wt1oy0Q\n\
SaF5Kb15cHfWjtix+FmFCpLKLQPOI2dBf64PFRsglUMstL0pqJ7noihzVLpdS6Pb\n\
+a2cndgL4bH2FpgJTaO6z3whqmVjamZfksRpAU9s8f3/puCISidYz9qCgNC0BvyS\n\
XJjWBJECgYEA5ijbrld40JIIzgZ7zW0W4tv1MKY9EsC23IGl7ZesxY5Rsgmsa0Ka\n\
lhNMRugd58NqG/XnAIBWZRLQM40qYBHyARW+cJxh7+3onvOHYwNkcrdQ/kcBbvme\n\
aSFRWeGn0QLo/4pGGpCssWoO+OvYKHUbH4qbPHdEFKOMwjsmKvHQX30CgYEA0V7d\n\
+hqea/fb6gvBDhosMqDe4y8Ry7zqu1IOVBxoG3cAhnoJn7zPn0rCLbYXgCp5/Nj9\n\
VqjQpUHCgevTE+W/ac/lq73e3oXaUX8M7baD/aPkNF5Y09AeabVw1ITLrYL9zWdU\n\
zfclwsIhWq2Dt/5DE4/bD9FtD1r7P7Vigba/LYkCgYBr1/E3e50MfaDKiJcx5k+2\n\
9MGqjfpH8yy7nbQV49/8oXb+KTI0//xXHau7/b8lfZcWit42ievxaCNORHL6mO4A\n\
PCQDuALb3WoGMK3bYxeJ+QNmYfb1/NiRAh+QMf/kG6z5L90xTWDdsIhbcobSTizr\n\
VpLufiPUV934lKaJsMymMQKBgQDByxKh7kOW4jwO/cQ6/mTMk/TayfWp5HpM2p3i\n\
osyGJ3c4AfuofEadRcBIOVS1UBvLuzl7HhTJ8f1M7nBY6X5sPX9zoPKKe9DhQD1C\n\
Rn8TpcCT7IRBwlB0Pfpq62PvfeDYX/2yC0JLbA8ddKAIDXQexjfZA1r0LJ2EkarV\n\
L8bzKQKBgQCbH3O8Dd1XQJ13SqDiWt7PJ37PalawTerYlFnw7jWOSooAzjuWw/La\n\
/jOy0BPlmkjxjAXAP1jY5Kq/UdkeXnlvQVne4F8oRKL2PD8iqYSNWAAyO4v+zHdX\n\
WtuDNx5B6NeOBk3E4n28oYStdHw20B+mHCxdr/wQ2iosqQfqEugKmg==\n\
-----END RSA PRIVATE KEY-----\n";
/// Its modulus, base64url, for the JWKS.
const IDP_KEY_N: &str = "vDyl7CDoXwG8SqSFodeEVK3aGjpg7cmsweIZIblHOLA_Ftd5D1XEOaAt7AuXjYQINz4zy3Nvcd3DCx42mCw4tdeGobZOSpGI7z5dq3rJFV-pjVGhJ5o7nLO3hipNbaiZKMrCFwybh_pSF9jtf6aP1nzMKEH9kTabxbnHyiEZuNJW07oHjQhi7JdrM9-l-dfxWpnHSdyZ-6DBIG9jV9NB5fT8yJ-oGUuakUm38-7TwE1rR19Lx2HW7r41s09t2Qkzjo0E7McSF1nwJM-Ek0VS4eD3zqdoz1aHLUdWavzQZJ6lyCkNA_FAdmQ912oMdlIqvfOp_2RiuVQ-QH_UJY8S5Q";
const IDP_KID: &str = "access-test";
const CLIENT_ID: &str = "tw-access-client";

/// Serve discovery and the JWKS; `/token` is mounted per login.
async fn mock_idp() -> MockServer {
    let idp = MockServer::start().await;
    let issuer = idp.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
        })))
        .mount(&idp)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256", "kid": IDP_KID,
                "n": IDP_KEY_N, "e": "AQAB",
            }]
        })))
        .mount(&idp)
        .await;
    idp
}

/// Draft the mock provider in the wizard and activate it.
async fn activate_sso(app: &TestApp, admin: &TestClient, idp: &MockServer) {
    admin
        .patch(
            "/api/admin/settings/oidc/draft",
            json!({
                "issuer_url": idp.uri(),
                "client_id": CLIENT_ID,
                "client_secret": "access-client-secret-123",
                "redirect_url": "http://localhost:3001/api/auth/sso/callback",
            }),
        )
        .await
        .unwrap()
        .assert_ok();
    let passed = json!({"passed": true, "at": chrono::Utc::now().timestamp()});
    let _: () = fred::interfaces::KeysInterface::set(
        &app.state.redis,
        "oidc:test:result",
        passed.to_string(),
        Some(fred::types::Expiration::EX(1800)),
        None,
        false,
    )
    .await
    .unwrap();
    admin
        .post("/api/admin/settings/oidc/activate", json!({}))
        .await
        .unwrap()
        .assert_ok();
}

fn query_param(url: &str, name: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .unwrap_or_else(|| panic!("{name} in {url}"))
        .1
        .into_owned()
}

/// Sign in through `/api/auth/sso/authorize` → the provider → the
/// callback, as the identity `claims` describes. Returns the client and
/// the callback's response.
async fn sso_login(app: &TestApp, idp: &MockServer, claims: Value) -> (TestClient, u16) {
    let con = app.console_client();
    let resp = con.get("/api/auth/sso/authorize").await.unwrap();
    resp.assert_status(307);
    let location = resp.headers["location"].to_str().unwrap().to_string();
    let nonce = query_param(&location, "nonce");
    let state = query_param(&location, "state");

    let now = chrono::Utc::now().timestamp();
    let mut claims = claims;
    let obj = claims.as_object_mut().unwrap();
    obj.insert("iss".into(), json!(idp.uri()));
    obj.insert("aud".into(), json!(CLIENT_ID));
    obj.insert("iat".into(), json!(now));
    obj.insert("exp".into(), json!(now + 300));
    obj.insert("nonce".into(), json!(nonce));
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(IDP_KID.into());
    let id_token = jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(IDP_KEY_PEM.as_bytes()).unwrap(),
    )
    .unwrap();

    let code = Uuid::new_v4().simple().to_string();
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains(code.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "idp-access-token",
            "token_type": "Bearer",
            "expires_in": 300,
            "id_token": id_token,
        })))
        .mount(idp)
        .await;

    let resp = con
        .get(&format!("/api/auth/sso/callback?code={code}&state={state}"))
        .await
        .unwrap();
    (con, resp.status.as_u16())
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn sso_provisions_a_user_then_signs_them_in_again() {
    let app = TestApp::spawn_reaching_loopback().await;
    let admin = admin_session(&app).await;
    let idp = mock_idp().await;
    activate_sso(&app, &admin, &idp).await;

    // Activation promotes the draft and drops it.
    let oidc = get(&admin, "/api/admin/settings/oidc").await;
    assert!(oidc["draft"].is_null(), "{oidc}");
    assert_eq!(oidc["active"]["enabled"], true);
    assert_eq!(oidc["active"]["configured"], true);

    app.set_setting("auth.default_role", json!("viewer")).await;

    let subject = unique_name("sub");
    let identity = json!({"sub": subject, "email": "Person.One@Example.com", "name": "Person One"});
    let (con, status) = sso_login(&app, &idp, identity.clone()).await;
    assert_eq!(status, 307);
    assert!(con.cookie("__Host-access_token").is_some());
    let me = get(&con, "/api/auth/me").await;
    assert_eq!(me["email"], "person.one@example.com");
    assert_eq!(me["display_name"], "Person One");
    assert_eq!(me["oidc_subject"], subject.as_str());
    let roles: Vec<&str> = me["role_assignments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["viewer"]);

    // The same identity signs in to the same row.
    let (_, status) = sso_login(&app, &idp, identity.clone()).await;
    assert_eq!(status, 307);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE oidc_subject = $1")
        .bind(&subject)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(rows, 1);

    // An identity with no email gets a stable placeholder address.
    let anonymous = unique_name("sub");
    let (con, status) = sso_login(&app, &idp, json!({"sub": anonymous})).await;
    assert_eq!(status, 307);
    let me = get(&con, "/api/auth/me").await;
    let email = me["email"].as_str().unwrap();
    assert!(
        email.starts_with("sso-") && email.ends_with("@oidc.invalid"),
        "{email}"
    );
    assert_eq!(me["display_name"], email);

    // Deactivated, then deleted: refused.
    sqlx::query("UPDATE users SET is_active = false WHERE oidc_subject = $1")
        .bind(&subject)
        .execute(&app.db)
        .await
        .unwrap();
    let (con, status) = sso_login(&app, &idp, identity.clone()).await;
    assert_eq!(status, 403);
    assert!(con.cookie("__Host-access_token").is_none());
    sqlx::query("UPDATE users SET deleted_at = now() WHERE oidc_subject = $1")
        .bind(&subject)
        .execute(&app.db)
        .await
        .unwrap();
    let (_, status) = sso_login(&app, &idp, identity).await;
    assert_eq!(status, 403);
}

/// `security.totp_required` is a JSON boolean. It used to be read as a
/// string and compared with "true", which never matched, so a platform
/// that required TOTP told every user it did not.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn requiring_totp_is_reported_to_users() {
    let app = TestApp::spawn().await;
    let admin = admin_session(&app).await;

    let status = get(&admin, "/api/auth/totp/status").await;
    assert_eq!(status["required"], false, "{status}");

    // A string is refused: it would read as "not required".
    admin
        .patch(
            "/api/admin/settings",
            json!({"settings": {"security.totp_required": "true"}}),
        )
        .await
        .unwrap()
        .assert_status(400);
    admin
        .patch(
            "/api/admin/settings",
            json!({"settings": {"security.totp_required": true}}),
        )
        .await
        .unwrap()
        .assert_ok();

    let status = get(&admin, "/api/auth/totp/status").await;
    assert_eq!(status["required"], true, "{status}");
}

/// With `security.totp_required` on, an SSO sign-in of a user who has
/// not enrolled gets a session held at enrollment, exactly like a
/// password sign-in (`totp_required.rs`), and enrolling releases it.
#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn an_unenrolled_sso_user_is_held_at_totp_enrollment() {
    let app = TestApp::spawn_reaching_loopback().await;
    let admin = admin_session(&app).await;
    let idp = mock_idp().await;
    activate_sso(&app, &admin, &idp).await;
    app.set_setting("auth.default_role", json!("developer"))
        .await;
    app.set_setting("security.totp_required", json!(true)).await;

    let identity = json!({"sub": unique_name("sub"), "email": unique_email()});
    let (con, status) = sso_login(&app, &idp, identity).await;
    assert_eq!(status, 307);

    let me = get(&con, "/api/auth/me").await;
    assert_eq!(me["totp_enrollment_required"], true, "{me}");
    let resp = con.get("/api/keys").await.unwrap();
    resp.assert_status(403);
    let body: Value = resp.json().unwrap();
    assert_eq!(body["error"]["type"], "totp_enrollment_required", "{body}");

    let setup: Value = con
        .post_empty("/api/auth/totp/setup")
        .await
        .unwrap()
        .json()
        .unwrap();
    let email = me["email"].as_str().unwrap();
    let code =
        think_watch_auth::totp::current_code(setup["secret"].as_str().unwrap(), email).unwrap();
    con.post("/api/auth/totp/verify-setup", json!({"code": code}))
        .await
        .unwrap()
        .assert_ok();
    con.get("/api/keys").await.unwrap().assert_ok();
}
