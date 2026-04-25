//! Vertical authorization matrix — privilege escalation.
//!
//! For every mutating admin endpoint in the console API, run all
//! five seeded system roles against it and assert the boundary the
//! handlers' `require_permission` + `assert_scope_*` calls intend
//! to enforce. Surfaces silent regressions where a permission gate
//! gets removed or a route migrates between groups.
//!
//! "Allowed" = handler returns 2xx OR a domain error like 400/404
//! (the request *reached* the handler past auth/permission checks).
//! "Denied" = 401 / 403.
//!
//! All tests share one TestApp and one random per-test target row
//! per endpoint — the matrix is one test that aggregates failures
//! so a bad permission gate is easy to spot in the diff rather
//! than buried in a generic "expected 403 got 200".

use serde_json::{Value, json};
use std::str::FromStr;
use think_watch_test_support::prelude::*;
use uuid::Uuid;

const VIEWER: &str = "viewer";
const DEVELOPER: &str = "developer";
const TEAM_MANAGER: &str = "team_manager";
const ADMIN: &str = "admin";
const SUPER_ADMIN: &str = "super_admin";

/// Roles that should always be denied a privileged action. The
/// `team_manager` is included because none of the endpoints below
/// take a team-scoped subject — they require global scope or
/// touch resources outside the manager's team.
const NON_PRIVILEGED: &[&str] = &[VIEWER, DEVELOPER, TEAM_MANAGER];

/// Roles that should always be allowed a privileged action.
const PRIVILEGED: &[&str] = &[ADMIN, SUPER_ADMIN];

#[derive(Clone, Copy)]
enum Verb {
    Post,
    Patch,
    Delete,
}

struct Spec {
    name: &'static str,
    verb: Verb,
    path: &'static str,
    body: Value,
    /// Roles that must succeed (2xx) — defaults to `PRIVILEGED`.
    allow: &'static [&'static str],
    /// Roles that must be denied (401/403) — defaults to `NON_PRIVILEGED`.
    deny: &'static [&'static str],
}

/// Login as a freshly-seeded user holding `role` at the canonical
/// scope. team_manager goes to a fresh, otherwise-empty team so
/// everything in the matrix is "out of their scope" by design.
/// Other roles default to global, matching the seeded role intent.
async fn role_client(app: &TestApp, role: &str) -> TestClient {
    let (scope_kind, scope_id) = if role == TEAM_MANAGER {
        let tid: Uuid = sqlx::query_scalar(
            "INSERT INTO teams (name, description) VALUES ($1, 'matrix-tm-scope') RETURNING id",
        )
        .bind(unique_name("matrix-tm"))
        .fetch_one(&app.db)
        .await
        .unwrap();
        ("team", Some(tid))
    } else {
        ("global", None)
    };
    let user = fixtures::create_user_with_role(&app.db, role, scope_kind, scope_id)
        .await
        .unwrap();
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

async fn run_spec(con: &TestClient, spec: &Spec) -> u16 {
    let resp = match spec.verb {
        Verb::Post => con.post(spec.path, spec.body.clone()).await,
        Verb::Patch => con.patch(spec.path, spec.body.clone()).await,
        Verb::Delete => {
            if spec.body == Value::Null {
                con.delete(spec.path).await
            } else {
                con.delete_with_body(spec.path, spec.body.clone()).await
            }
        }
    }
    .unwrap();
    resp.status.as_u16()
}

/// `code` was a 2xx OR a domain error (400/404/409/422) that means
/// the auth gate let the request through. 401/403 means denied.
fn passed_auth_gate(code: u16) -> bool {
    !matches!(code, 401 | 403)
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn vertical_role_endpoint_matrix() {
    let app = TestApp::spawn().await;

    // Pre-seed targets that some endpoints need in their URL or
    // body. Using stable rows so the matrix can re-target them
    // across roles without colliding.
    let target_user = fixtures::create_user_with_role(&app.db, DEVELOPER, "global", None)
        .await
        .unwrap();
    let target_team_id: Uuid = sqlx::query_scalar(
        "INSERT INTO teams (name, description) VALUES ($1, 'matrix') RETURNING id",
    )
    .bind(unique_name("matrix-team"))
    .fetch_one(&app.db)
    .await
    .unwrap();
    let target_provider = fixtures::create_provider(
        &app.db,
        &unique_name("matrix-prov"),
        "openai",
        "https://api.openai.com/v1",
        None,
    )
    .await
    .unwrap();
    let target_role_id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO rbac_roles (name, description, is_system, policy_document)
           VALUES ($1, 'matrix custom role', false,
                   '{"Version":"2024-01-01","Statement":[{"Sid":"X","Effect":"Allow","Action":"api_keys:read","Resource":"*"}]}'::jsonb)
           RETURNING id"#,
    )
    .bind(unique_name("matrix-role"))
    .fetch_one(&app.db)
    .await
    .unwrap();
    let target_api_key = fixtures::create_api_key(
        &app.db,
        target_user.user.id,
        "matrix-key",
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let target_mcp_server_id = fixtures::create_mcp_server(
        &app.db,
        &unique_name("matrix-mcp"),
        &format!("ns_{}", &Uuid::new_v4().simple().to_string()[..8]),
        "https://example.com/mcp",
    )
    .await
    .unwrap();
    let target_rule_id = fixtures::create_rate_limit_rule(
        &app.db,
        "user",
        target_user.user.id,
        "ai_gateway",
        "requests",
        60,
        100,
    )
    .await
    .unwrap();
    let target_cap_id =
        fixtures::create_budget_cap(&app.db, "user", target_user.user.id, "daily", 5000)
            .await
            .unwrap();

    // The matrix. Each row asserts a permission boundary. Body
    // shapes are minimum-valid so a 422-on-bad-input doesn't get
    // confused with a 403-on-forbidden.
    let specs: Vec<Spec> = vec![
        // --- Users (admin only) -------------------------------------
        Spec {
            name: "POST /api/admin/users",
            verb: Verb::Post,
            path: "/api/admin/users",
            body: json!({
                "email": format!("matrix-{}@example.com", Uuid::new_v4().simple()),
                "display_name": "Matrix",
                "password": "MatrixPwd_123!",
                "is_active": true
            }),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "PATCH /api/admin/users/{id}",
            verb: Verb::Patch,
            path: leak(&format!("/api/admin/users/{}", target_user.user.id)),
            body: json!({"display_name": "renamed-by-matrix"}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "POST /api/admin/users/{id}/force-logout",
            verb: Verb::Post,
            path: leak(&format!(
                "/api/admin/users/{}/force-logout",
                target_user.user.id
            )),
            body: json!({}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "POST /api/admin/users/{id}/reset-password",
            verb: Verb::Post,
            path: leak(&format!(
                "/api/admin/users/{}/reset-password",
                target_user.user.id
            )),
            body: json!({}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // --- Providers (admin / super_admin) ------------------------
        Spec {
            name: "POST /api/admin/providers",
            verb: Verb::Post,
            path: "/api/admin/providers",
            body: json!({
                "name": unique_name("p"),
                "display_name": "matrix",
                "provider_type": "openai",
                "base_url": "https://api.openai.com/v1",
                "config": {}
            }),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "PATCH /api/admin/providers/{id}",
            verb: Verb::Patch,
            path: leak(&format!("/api/admin/providers/{}", target_provider.id)),
            body: json!({"display_name": "matrix-renamed"}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // DELETE last so the row stays around for PATCH above.
        Spec {
            name: "DELETE /api/admin/providers/{id}",
            verb: Verb::Delete,
            path: leak(&format!("/api/admin/providers/{}", target_provider.id)),
            body: Value::Null,
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // --- Teams (admin / super_admin) ----------------------------
        Spec {
            name: "POST /api/admin/teams",
            verb: Verb::Post,
            path: "/api/admin/teams",
            body: json!({"name": unique_name("t"), "description": "matrix"}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "PATCH /api/admin/teams/{id}",
            verb: Verb::Patch,
            path: leak(&format!("/api/admin/teams/{target_team_id}")),
            body: json!({"description": "matrix-touched"}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "POST /api/admin/teams/{id}/members",
            verb: Verb::Post,
            path: leak(&format!("/api/admin/teams/{target_team_id}/members")),
            body: json!({"user_id": target_user.user.id, "role": "member"}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // The DELETE spec runs LAST in this group so the team row
        // it nukes doesn't break the PATCH / members specs above.
        Spec {
            name: "DELETE /api/admin/teams/{id}",
            verb: Verb::Delete,
            path: leak(&format!("/api/admin/teams/{target_team_id}")),
            body: Value::Null,
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // --- MCP servers (admin / super_admin) ----------------------
        Spec {
            name: "POST /api/mcp/servers",
            verb: Verb::Post,
            path: "/api/mcp/servers",
            body: json!({
                "name": unique_name("mcp"),
                "namespace_prefix": format!("ns_{}", &Uuid::new_v4().simple().to_string()[..6]),
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http"
            }),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "DELETE /api/mcp/servers/{id}",
            verb: Verb::Delete,
            path: leak(&format!("/api/mcp/servers/{target_mcp_server_id}")),
            body: Value::Null,
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // --- Roles (admin / super_admin) ----------------------------
        Spec {
            name: "POST /api/admin/roles",
            verb: Verb::Post,
            path: "/api/admin/roles",
            body: json!({
                "name": unique_name("role"),
                "description": "matrix",
                "policy_document": {
                    "Version": "2024-01-01",
                    "Statement": [{"Sid": "X", "Effect": "Allow",
                                   "Action": "api_keys:read", "Resource": "*"}]
                }
            }),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "DELETE /api/admin/roles/{id}",
            verb: Verb::Delete,
            path: leak(&format!("/api/admin/roles/{target_role_id}")),
            body: Value::Null,
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // --- Settings (admin / super_admin) -------------------------
        Spec {
            name: "PATCH /api/admin/settings",
            verb: Verb::Patch,
            path: "/api/admin/settings",
            body: json!({"settings": {"auth.allow_registration": true}}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            // OIDC needs `system:configure_oidc` — only super_admin.
            // The seeded `admin` role does NOT carry that perm, so
            // the matrix expects a 403 there too.
            name: "PATCH /api/admin/settings/oidc",
            verb: Verb::Patch,
            path: "/api/admin/settings/oidc",
            body: json!({"enabled": false}),
            allow: &[SUPER_ADMIN],
            deny: &[VIEWER, DEVELOPER, TEAM_MANAGER, ADMIN],
        },
        // --- Log forwarders (admin / super_admin) -------------------
        Spec {
            name: "POST /api/admin/log-forwarders",
            verb: Verb::Post,
            path: "/api/admin/log-forwarders",
            body: json!({
                "name": unique_name("fwd"),
                "forwarder_type": "webhook",
                "config": {"url": "https://example.com/hook"},
                "log_types": ["audit"],
                "enabled": false
            }),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // --- Limits (admin / super_admin) ---------------------------
        Spec {
            name: "POST /api/admin/limits/user/{id}/rules",
            verb: Verb::Post,
            path: leak(&format!(
                "/api/admin/limits/user/{}/rules",
                target_user.user.id
            )),
            body: json!({
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
                "max_count": 100
            }),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "DELETE /api/admin/limits/user/{uid}/rules/{rule}",
            verb: Verb::Delete,
            path: leak(&format!(
                "/api/admin/limits/user/{}/rules/{target_rule_id}",
                target_user.user.id
            )),
            body: Value::Null,
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        Spec {
            name: "DELETE /api/admin/limits/user/{uid}/budgets/{cap}",
            verb: Verb::Delete,
            path: leak(&format!(
                "/api/admin/limits/user/{}/budgets/{target_cap_id}",
                target_user.user.id
            )),
            body: Value::Null,
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // NOTE: the bulk apply / delete endpoints return HTTP 200
        // with per-row outcomes even when every row was forbidden,
        // so a top-level HTTP-status matrix can't distinguish
        // denial from success. Their per-row scope contract is
        // covered separately in `authz_audit.rs` (cross-team
        // delete) and `limits_bulk.rs` (developer permission
        // boundary).
        // --- API key admin emergency (global only) ------------------
        Spec {
            name: "POST /api/admin/keys/{id}/force-revoke",
            verb: Verb::Post,
            path: leak(&format!(
                "/api/admin/keys/{}/force-revoke",
                target_api_key.row.id
            )),
            body: json!({"reason": "matrix authz check"}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // --- User-limits dashboard (admin / super_admin) ------------
        Spec {
            name: "POST /api/admin/users/{id}/limits/reset",
            verb: Verb::Post,
            path: leak(&format!(
                "/api/admin/users/{}/limits/reset",
                target_user.user.id
            )),
            // `kind` is required by the body extractor — without it
            // we'd 422 before the auth gate even runs and the
            // matrix would (correctly) flag the noise as escalation.
            body: json!({
                "kind": "rule",
                "surface": "ai_gateway",
                "metric": "requests",
                "window_secs": 60,
            }),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // --- Webhook outbox (admin / super_admin) -------------------
        Spec {
            name: "POST /api/admin/webhook-outbox/{id}/retry",
            verb: Verb::Post,
            // `Uuid::nil()` is a non-existent row — handler answers
            // 404 *after* the auth gate, which is exactly what we
            // want to distinguish from a denial.
            path: leak(&format!("/api/admin/webhook-outbox/{}/retry", Uuid::nil())),
            body: json!({}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
        // --- MCP store sync (admin / super_admin) -------------------
        Spec {
            name: "POST /api/admin/mcp-store/sync",
            verb: Verb::Post,
            path: "/api/admin/mcp-store/sync",
            body: json!({}),
            allow: PRIVILEGED,
            deny: NON_PRIVILEGED,
        },
    ];

    // Login once per role and reuse the session — running ~150
    // logins inside the matrix would trip the per-IP rate limit
    // (30 / minute on 127.0.0.1).
    let mut clients: std::collections::HashMap<&str, TestClient> = std::collections::HashMap::new();
    for role in [VIEWER, DEVELOPER, TEAM_MANAGER, ADMIN, SUPER_ADMIN] {
        clients.insert(role, role_client(&app, role).await);
    }

    let mut violations: Vec<String> = Vec::new();

    for spec in &specs {
        for role in spec.deny {
            let code = run_spec(&clients[role], spec).await;
            if passed_auth_gate(code) {
                violations.push(format!(
                    "[VERTICAL ESCALATION] {role} → {} returned {code} (expected 401/403)",
                    spec.name
                ));
            }
        }
        for role in spec.allow {
            let code = run_spec(&clients[role], spec).await;
            if !passed_auth_gate(code) {
                violations.push(format!(
                    "[OVER-DENIED] {role} → {} returned {code} (expected 2xx / 4xx-domain)",
                    spec.name
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{} authz boundary violation(s):\n  - {}",
        violations.len(),
        violations.join("\n  - ")
    );
}

/// Leak a String into a `'static` so it satisfies `Spec::path`.
/// Used for path templates that include a runtime UUID. Safe in
/// tests — the leak is per-process and freed at exit.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

/// Suppress the unused-import warning for `FromStr` if the
/// matrix doesn't end up using it. (Reserved for future specs that
/// parse status-derived UUIDs from the response.)
#[allow(dead_code)]
fn _retain_imports() {
    let _ = Uuid::from_str("00000000-0000-0000-0000-000000000000");
}
