//! Who may use the gateways at all, and which models / MCP tools a
//! role contributes.
//!
//!   - The AI gateway requires a role that grants `ai_gateway:use`, the
//!     MCP gateway one that grants `mcp_gateway:use`. A key whose owner
//!     holds no such role is refused with 403 before anything else runs.
//!   - A role that does not grant gateway use contributes no models and
//!     no tools. It used to count as "unrestricted", so adding the
//!     built-in `viewer` to a user restricted to one model opened every
//!     model to them.
//!   - The per-key allow-list still narrows what the roles grant, and an
//!     empty result means nothing is allowed, not everything.
//!   - `GET /v1/models` and `GET /v1beta/models` list the models the
//!     calling key may use under both, and no others — including a model
//!     the key's allow-list names that a prefix route serves.

use serde_json::Value;
use think_watch_test_support::client::TestResponse;
use think_watch_test_support::prelude::*;
use uuid::Uuid;

const MODEL_A: &str = "authz-model-a";
const MODEL_B: &str = "authz-model-b";

/// Two routed models behind one mock upstream.
async fn two_models(app: &TestApp) -> MockProvider {
    let upstream = MockProvider::openai_chat_ok(MODEL_A).await;
    let provider = fixtures::create_provider(
        &app.db,
        &unique_name("authz-prov"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, MODEL_A)
        .await
        .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, MODEL_B)
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    upstream
}

async fn custom_role(db: &sqlx::PgPool, prefix: &str, statement: Value) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO rbac_roles (name, is_system, policy_document) VALUES ($1, FALSE, $2) RETURNING id",
    )
    .bind(unique_name(prefix))
    .bind(json!({"Version": "2024-01-01", "Statement": [statement]}))
    .fetch_one(db)
    .await
    .unwrap()
}

async fn assign_global(db: &sqlx::PgPool, user_id: Uuid, role_id: Uuid) {
    sqlx::query(
        "INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
         VALUES ($1, $2, 'global', $1)",
    )
    .bind(user_id)
    .bind(role_id)
    .execute(db)
    .await
    .unwrap();
}

/// A role allowed to call `MODEL_A` only.
async fn only_model_a(db: &sqlx::PgPool) -> Uuid {
    custom_role(
        db,
        "only-model-a",
        json!({"Effect": "Allow", "Action": ["ai_gateway:use"], "Resource": [format!("model:{MODEL_A}")]}),
    )
    .await
}

async fn bare_user(app: &TestApp) -> fixtures::SeededUser {
    fixtures::create_user(&app.db, &unique_email(), "Authz", "AuthzPwd_12345!")
        .await
        .unwrap()
}

async fn gateway_for(app: &TestApp, user_id: Uuid, allowed_models: Option<&[&str]>) -> TestClient {
    let key = fixtures::create_api_key(
        &app.db,
        user_id,
        &unique_name("authz-key"),
        &["ai_gateway", "mcp_gateway"],
        allowed_models,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    gw
}

async fn chat(gw: &TestClient, model: &str) -> TestResponse {
    gw.post(
        "/v1/chat/completions",
        json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await
    .unwrap()
}

fn assert_refused(resp: &TestResponse, what: &str) {
    assert!(
        !resp.status.is_success(),
        "{what} must be refused, got {}: {}",
        resp.status,
        resp.text()
    );
}

async fn mcp_initialize(gw: &TestClient) -> TestResponse {
    gw.post(
        "/mcp",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "authz-test", "version": "0"}
            }
        }),
    )
    .await
    .unwrap()
}

async fn policy_scope(app: &TestApp, user: &fixtures::SeededUser) -> Value {
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": user.user.email, "password": user.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    let resp = con.get("/api/keys/policy-scope").await.unwrap();
    resp.assert_ok();
    resp.json().unwrap()
}

// ---------------------------------------------------------------------------
// A role without gateway use contributes nothing
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn viewer_role_does_not_widen_model_access() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    assign_global(&app.db, user.user.id, only_model_a(&app.db).await).await;
    fixtures::assign_role_global(&app.db, user.user.id, "viewer")
        .await
        .unwrap();
    let gw = gateway_for(&app, user.user.id, None).await;

    chat(&gw, MODEL_A).await.assert_ok();
    assert_refused(
        &chat(&gw, MODEL_B).await,
        "model-b under model-a-only + viewer",
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn viewer_role_does_not_widen_mcp_tool_scope() {
    // The effective scope the console reports is the one the gateway
    // enforces (both come from `compute_user_resource_limits`).
    let app = TestApp::spawn().await;
    let user = bare_user(&app).await;
    let tools_only = custom_role(
        &app.db,
        "one-tool",
        json!({"Effect": "Allow", "Action": ["mcp_gateway:use"], "Resource": ["mcp_tool:srv__read"]}),
    )
    .await;
    assign_global(&app.db, user.user.id, tools_only).await;
    fixtures::assign_role_global(&app.db, user.user.id, "viewer")
        .await
        .unwrap();

    let scope = policy_scope(&app, &user).await;
    assert_eq!(scope["allowed_mcp_tools"], json!(["srv__read"]), "{scope}");
    // Neither role grants `ai_gateway:use`: no models at all.
    assert_eq!(scope["allowed_models"], json!([]), "{scope}");
}

// ---------------------------------------------------------------------------
// Gateway use is required
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn user_without_roles_is_refused_at_the_ai_gateway() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    let gw = gateway_for(&app, user.user.id, None).await;

    let resp = chat(&gw, MODEL_A).await;
    resp.assert_status(403);
    assert!(
        resp.text().contains("ai_gateway:use"),
        "the refusal names the missing permission: {}",
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn viewer_only_user_is_refused_at_both_gateways() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    fixtures::assign_role_global(&app.db, user.user.id, "viewer")
        .await
        .unwrap();
    let gw = gateway_for(&app, user.user.id, None).await;

    chat(&gw, MODEL_A).await.assert_status(403);
    let resp = mcp_initialize(&gw).await;
    resp.assert_status(403);
    assert!(
        resp.text().contains("mcp_gateway:use"),
        "the refusal names the missing permission: {}",
        resp.text()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_role_granting_only_one_gateway_opens_only_that_gateway() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    assign_global(&app.db, user.user.id, only_model_a(&app.db).await).await;
    let gw = gateway_for(&app, user.user.id, None).await;

    chat(&gw, MODEL_A).await.assert_ok();
    mcp_initialize(&gw).await.assert_status(403);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_deny_on_gateway_use_wins_over_an_allow() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    fixtures::assign_role_global(&app.db, user.user.id, "developer")
        .await
        .unwrap();
    let deny = custom_role(
        &app.db,
        "deny-ai",
        json!({"Effect": "Deny", "Action": ["ai_gateway:use"], "Resource": ["*"]}),
    )
    .await;
    assign_global(&app.db, user.user.id, deny).await;
    let gw = gateway_for(&app, user.user.id, None).await;

    chat(&gw, MODEL_A).await.assert_status(403);
    // The deny names the AI gateway only.
    mcp_initialize(&gw).await.assert_ok();
}

// ---------------------------------------------------------------------------
// Roles that do grant gateway use keep working
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn built_in_gateway_roles_keep_full_access() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    for role in ["developer", "admin", "super_admin"] {
        let user = bare_user(&app).await;
        fixtures::assign_role_global(&app.db, user.user.id, role)
            .await
            .unwrap();
        let gw = gateway_for(&app, user.user.id, None).await;
        for model in [MODEL_A, MODEL_B] {
            let resp = chat(&gw, model).await;
            assert!(
                resp.status.is_success(),
                "{role} calling {model}: {} {}",
                resp.status,
                resp.text()
            );
        }
        let resp = mcp_initialize(&gw).await;
        assert!(
            resp.status.is_success(),
            "{role} on the MCP gateway: {} {}",
            resp.status,
            resp.text()
        );
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn an_action_wildcard_grants_gateway_use() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    let wildcard = custom_role(
        &app.db,
        "ai-star",
        json!({"Effect": "Allow", "Action": ["ai_gateway:*"], "Resource": ["*"]}),
    )
    .await;
    assign_global(&app.db, user.user.id, wildcard).await;
    let gw = gateway_for(&app, user.user.id, None).await;

    chat(&gw, MODEL_B).await.assert_ok();
}

// ---------------------------------------------------------------------------
// The per-key allow-list
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn key_allow_list_still_narrows_a_developer() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    fixtures::assign_role_global(&app.db, user.user.id, "developer")
        .await
        .unwrap();
    let gw = gateway_for(&app, user.user.id, Some(&[MODEL_A])).await;

    chat(&gw, MODEL_A).await.assert_ok();
    assert_refused(&chat(&gw, MODEL_B).await, "model-b outside the key list");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn key_allow_list_disjoint_from_role_grants_allows_nothing() {
    // The key list is written straight to the table: the console would
    // refuse it, but a key minted before the role was narrowed keeps
    // its old list. The intersection is empty, and empty must mean
    // nothing, not everything.
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    assign_global(&app.db, user.user.id, only_model_a(&app.db).await).await;
    let gw = gateway_for(&app, user.user.id, Some(&[MODEL_B])).await;

    assert_refused(
        &chat(&gw, MODEL_B).await,
        "model-b (not granted by the role)",
    );
    assert_refused(&chat(&gw, MODEL_A).await, "model-a (not on the key list)");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn key_allow_list_within_a_prefix_grant_is_kept() {
    // Role grants are prefixes (`model:authz-model-` covers both
    // models); a key narrowed to one of them keeps that one.
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    let prefix = custom_role(
        &app.db,
        "prefix",
        json!({"Effect": "Allow", "Action": ["ai_gateway:use"], "Resource": ["model:authz-model-"]}),
    )
    .await;
    assign_global(&app.db, user.user.id, prefix).await;
    let gw = gateway_for(&app, user.user.id, Some(&[MODEL_A])).await;

    chat(&gw, MODEL_A).await.assert_ok();
    assert_refused(&chat(&gw, MODEL_B).await, "model-b outside the key list");
}

// ---------------------------------------------------------------------------
// The model listings
// ---------------------------------------------------------------------------

/// The model ids `/v1/models` lists and those `/v1beta/models` lists
/// (without Gemini's `models/` prefix), in that order.
async fn listed_models(gw: &TestClient) -> (Vec<String>, Vec<String>) {
    let openai = gw.get("/v1/models").await.unwrap();
    openai.assert_ok();
    let openai: Value = openai.json().unwrap();
    let gemini = gw.get("/v1beta/models").await.unwrap();
    gemini.assert_ok();
    let gemini: Value = gemini.json().unwrap();
    let ids = openai["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    let names = gemini["models"]
        .as_array()
        .expect("models array")
        .iter()
        .map(|m| {
            let name = m["name"].as_str().unwrap();
            name.strip_prefix("models/").unwrap_or(name).to_string()
        })
        .collect();
    (ids, names)
}

/// Both listings name `expected` (in order) among this test's two models.
async fn assert_lists(gw: &TestClient, expected: &[&str], what: &str) {
    let (openai, gemini) = listed_models(gw).await;
    for (listing, ids) in [("/v1/models", &openai), ("/v1beta/models", &gemini)] {
        let ours: Vec<&str> = ids
            .iter()
            .map(String::as_str)
            .filter(|id| [MODEL_A, MODEL_B].contains(id))
            .collect();
        assert_eq!(ours, expected, "{what}: {listing} listed {ids:?}");
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn model_listings_follow_the_key_allow_list() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;
    let user = bare_user(&app).await;
    fixtures::assign_role_global(&app.db, user.user.id, "developer")
        .await
        .unwrap();

    let unrestricted = gateway_for(&app, user.user.id, None).await;
    assert_lists(&unrestricted, &[MODEL_A, MODEL_B], "an unrestricted key").await;

    let narrowed = gateway_for(&app, user.user.id, Some(&[MODEL_A])).await;
    assert_lists(&narrowed, &[MODEL_A], "a key narrowed to model-a").await;
    // What is listed is what a request is let through for.
    chat(&narrowed, MODEL_A).await.assert_ok();
    assert_refused(&chat(&narrowed, MODEL_B).await, "model-b, not listed");

    // A key entry is a prefix, as at request time.
    let by_prefix = gateway_for(&app, user.user.id, Some(&["authz-model-"])).await;
    assert_lists(&by_prefix, &[MODEL_A, MODEL_B], "a key narrowed by prefix").await;
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn model_listings_follow_the_owners_roles() {
    let app = TestApp::spawn().await;
    let _upstream = two_models(&app).await;

    // A role granting model-a only: model-b is not listed for any key.
    let user = bare_user(&app).await;
    assign_global(&app.db, user.user.id, only_model_a(&app.db).await).await;
    let gw = gateway_for(&app, user.user.id, None).await;
    assert_lists(&gw, &[MODEL_A], "a model-a-only role").await;

    // A key list disjoint from the role grants allows nothing, and lists
    // nothing at all.
    let disjoint = gateway_for(&app, user.user.id, Some(&[MODEL_B])).await;
    let (openai, gemini) = listed_models(&disjoint).await;
    assert!(openai.is_empty(), "/v1/models listed {openai:?}");
    assert!(gemini.is_empty(), "/v1beta/models listed {gemini:?}");

    // A prefix grant narrowed by the key keeps the key's model.
    let prefixed = bare_user(&app).await;
    let prefix = custom_role(
        &app.db,
        "prefix",
        json!({"Effect": "Allow", "Action": ["ai_gateway:use"], "Resource": ["model:authz-model-"]}),
    )
    .await;
    assign_global(&app.db, prefixed.user.id, prefix).await;
    let gw = gateway_for(&app, prefixed.user.id, Some(&[MODEL_B])).await;
    assert_lists(&gw, &[MODEL_B], "a prefix grant narrowed to model-b").await;
}

/// A provider with no routes of its own, which the router serves under its
/// type's model prefixes (`gpt-`, `o1-`, …), behind one mock upstream.
async fn prefix_routed(app: &TestApp) -> MockProvider {
    let upstream = MockProvider::openai_chat_ok("gpt-4o").await;
    fixtures::create_provider(
        &app.db,
        &unique_name("authz-prefix-prov"),
        "openai",
        &upstream.uri(),
        None,
    )
    .await
    .unwrap();
    app.rebuild_gateway_router().await;
    upstream
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn model_listings_name_allowed_models_that_a_prefix_route_serves() {
    let app = TestApp::spawn().await;
    let _upstream = prefix_routed(&app).await;
    let user = bare_user(&app).await;
    fixtures::assign_role_global(&app.db, user.user.id, "developer")
        .await
        .unwrap();

    // No route is named `gpt-4o`; the `gpt-` prefix route serves it, and
    // the key's allow-list names it.
    let narrowed = gateway_for(&app, user.user.id, Some(&["gpt-4o"])).await;
    let (openai, gemini) = listed_models(&narrowed).await;
    assert_eq!(openai, ["gpt-4o"], "/v1/models");
    assert_eq!(gemini, ["gpt-4o"], "/v1beta/models");
    // What is listed is what a request is let through for.
    chat(&narrowed, "gpt-4o").await.assert_ok();

    // An allowed name nothing routes is not listed; a name that is a
    // route and an allow-list entry both is listed once.
    let mixed = gateway_for(
        &app,
        user.user.id,
        Some(&["gpt-", "gpt-4o", "gemini-2.5-pro"]),
    )
    .await;
    let (openai, gemini) = listed_models(&mixed).await;
    assert_eq!(openai, ["gpt-", "gpt-4o"], "/v1/models");
    assert_eq!(gemini, ["gpt-", "gpt-4o"], "/v1beta/models");

    // An unrestricted key lists the routes' own names.
    let unrestricted = gateway_for(&app, user.user.id, None).await;
    let (openai, _) = listed_models(&unrestricted).await;
    assert!(openai.iter().any(|m| m == "gpt-"), "{openai:?}");
    assert!(!openai.iter().any(|m| m == "gpt-4o"), "{openai:?}");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn model_listings_name_role_granted_models_that_a_prefix_route_serves() {
    let app = TestApp::spawn().await;
    let _upstream = prefix_routed(&app).await;
    let user = bare_user(&app).await;
    let gpt_4o = custom_role(
        &app.db,
        "gpt-4o-only",
        json!({"Effect": "Allow", "Action": ["ai_gateway:use"], "Resource": ["model:gpt-4o"]}),
    )
    .await;
    assign_global(&app.db, user.user.id, gpt_4o).await;

    // The key is not narrowed; its owner's role is.
    let gw = gateway_for(&app, user.user.id, None).await;
    let (openai, gemini) = listed_models(&gw).await;
    assert_eq!(openai, ["gpt-4o"], "/v1/models");
    assert_eq!(gemini, ["gpt-4o"], "/v1beta/models");
    chat(&gw, "gpt-4o").await.assert_ok();
}
