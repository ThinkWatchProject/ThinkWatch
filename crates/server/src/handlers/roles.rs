use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_auth::rbac;
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::{AuthUser, invalidate_role_perms};

/// Validate any Constraints blocks inside a policy_document's statements.
fn validate_policy_constraints_in_doc(doc: &serde_json::Value) -> Result<(), AppError> {
    let statements = match doc.get("Statement").and_then(|s| s.as_array()) {
        Some(arr) => arr,
        None => return Ok(()),
    };
    for (i, stmt) in statements.iter().enumerate() {
        if let Some(c) = stmt.get("Constraints")
            && !c.is_null()
        {
            let constraints: think_watch_common::limits::PolicyConstraints =
                serde_json::from_value(c.clone()).map_err(|e| {
                    AppError::BadRequest(format!("Statement[{i}].Constraints: {e}"))
                })?;
            think_watch_common::limits::validate_policy_constraints(&constraints)
                .map_err(|e| AppError::BadRequest(format!("Statement[{i}].{e}")))?;
        }
    }
    Ok(())
}

/// One permission entry in the structured catalog.
///
/// `key` is the canonical `resource:action` string the rest of the system
/// matches against. `resource` and `action` are denormalized for grouping
/// in the UI. `dangerous` flags permissions that should require an extra
/// confirmation when granted (rotating API keys, revoking sessions,
/// disabling PII redaction, etc).
#[derive(Debug, Clone, Copy, Serialize, utoipa::ToSchema)]
pub struct PermissionDef {
    pub key: &'static str,
    pub resource: &'static str,
    pub action: &'static str,
    pub dangerous: bool,
}

/// Helper to make the catalog readable.
const fn p(key: &'static str, resource: &'static str, action: &'static str) -> PermissionDef {
    PermissionDef {
        key,
        resource,
        action,
        dangerous: false,
    }
}
const fn d(key: &'static str, resource: &'static str, action: &'static str) -> PermissionDef {
    PermissionDef {
        key,
        resource,
        action,
        dangerous: true,
    }
}

/// Canonical permission catalog: split by verb, flagging genuinely
/// dangerous ones, grouped by resource for the UI.
pub const PERMISSIONS: &[PermissionDef] = &[
    // --- AI gateway ---
    p("ai_gateway:use", "ai_gateway", "use"),
    // --- MCP gateway ---
    p("mcp_gateway:use", "mcp_gateway", "use"),
    // --- API keys ---
    p("api_keys:read", "api_keys", "read"),
    p("api_keys:create", "api_keys", "create"),
    p("api_keys:update", "api_keys", "update"),
    d("api_keys:rotate", "api_keys", "rotate"),
    d("api_keys:delete", "api_keys", "delete"),
    // --- Teams ---
    // Teams are the unit of "scoped admin": a custom role with
    // `team_members:write` granted in scope `team:<id>` lets the
    // holder add/remove members of that team without touching any
    // other team. The CRUD perms below operate on the team
    // catalog itself (rename, delete) and live at global scope
    // because they're platform-wide bookkeeping.
    p("teams:read", "teams", "read"),
    p("teams:create", "teams", "create"),
    p("teams:update", "teams", "update"),
    d("teams:delete", "teams", "delete"),
    // Membership management is the only perm intended to be
    // granted at team scope. The handler accepts it at global
    // scope too for super_admin convenience.
    d("team_members:write", "team_members", "write"),
    // --- Providers (AI upstream) ---
    p("providers:read", "providers", "read"),
    p("providers:create", "providers", "create"),
    p("providers:update", "providers", "update"),
    d("providers:delete", "providers", "delete"),
    d("providers:rotate_key", "providers", "rotate_key"),
    // --- Models (exposed catalog + routes) ---
    p("models:read", "models", "read"),
    // Write covers create/update/delete. Marked dangerous because
    // tweaking `output_weight` on a heavily-used model can silently
    // invalidate every existing budget cap and rate-limit rule that
    // touches it.
    d("models:write", "models", "write"),
    // --- MCP servers ---
    p("mcp_servers:read", "mcp_servers", "read"),
    p("mcp_servers:create", "mcp_servers", "create"),
    p("mcp_servers:update", "mcp_servers", "update"),
    d("mcp_servers:delete", "mcp_servers", "delete"),
    // --- Users / teams ---
    p("users:read", "users", "read"),
    p("users:create", "users", "create"),
    p("users:update", "users", "update"),
    d("users:delete", "users", "delete"),
    p("team:read", "team", "read"),
    p("team:write", "team", "write"),
    // --- Sessions (revoke other users) ---
    d("sessions:revoke", "sessions", "revoke"),
    // --- Roles & permissions (self-modifying — always dangerous) ---
    p("roles:read", "roles", "read"),
    d("roles:create", "roles", "create"),
    d("roles:update", "roles", "update"),
    d("roles:delete", "roles", "delete"),
    // Editing the system roles is strictly more dangerous than
    // editing custom roles: a typo here can lock every existing
    // user out of the entire system at once. Gated separately so
    // an org can grant `roles:update` (custom roles) without
    // implicitly handing out the ability to neuter `super_admin`.
    d("roles:edit_system", "roles", "edit_system"),
    // --- Analytics & audit ---
    p("analytics:read_own", "analytics", "read_own"),
    p("analytics:read_team", "analytics", "read_team"),
    p("analytics:read_all", "analytics", "read_all"),
    p("audit_logs:read_own", "audit_logs", "read_own"),
    p("audit_logs:read_team", "audit_logs", "read_team"),
    p("audit_logs:read_all", "audit_logs", "read_all"),
    // --- Gateway logs (raw request bodies — sensitive) ---
    p("logs:read_own", "logs", "read_own"),
    p("logs:read_team", "logs", "read_team"),
    d("logs:read_all", "logs", "read_all"),
    p("log_forwarders:read", "log_forwarders", "read"),
    d("log_forwarders:write", "log_forwarders", "write"),
    // --- Webhooks (SSRF surface) ---
    p("webhooks:read", "webhooks", "read"),
    d("webhooks:write", "webhooks", "write"),
    // --- Content filtering / PII redaction ---
    p("content_filter:read", "content_filter", "read"),
    d("content_filter:write", "content_filter", "write"),
    p("pii_redactor:read", "pii_redactor", "read"),
    d("pii_redactor:write", "pii_redactor", "write"),
    // --- Rate limits & budget caps ---
    // Read covers viewing the rules, caps, and current usage on
    // any subject's edit page. Write covers create/update/delete
    // of rules and caps. Both are dangerous because tightening a
    // limit can lock real users out and loosening one can blow
    // through a budget.
    p("rate_limits:read", "rate_limits", "read"),
    d("rate_limits:write", "rate_limits", "write"),
    // --- System settings ---
    p("settings:read", "settings", "read"),
    d("settings:write", "settings", "write"),
    d("system:configure_oidc", "system", "configure_oidc"),
];

pub(super) fn is_known_permission(key: &str) -> bool {
    PERMISSIONS.iter().any(|p| p.key == key)
}

/// Default policy document for each seeded system role.
///
/// This is the **single source of truth** for "what should this
/// system role grant out of the box". The migration uses these to
/// seed `rbac_roles.policy_document` on a fresh database, and the
/// "Reset to defaults" UI in the system-role editor uses them to
/// roll back any in-place edits.
///
/// Kept in lockstep with the seed in `migrations/001_init.sql`
/// — `validate_seeded_roles` runs at startup so a drift between
/// the two would fail-fast.
pub const SYSTEM_ROLE_DEFAULTS: &[(&str, &str)] = &[
    (
        "super_admin",
        r#"{"Version":"2024-01-01","Statement":[{"Sid":"FullAccess","Effect":"Allow","Action":"*","Resource":"*"}]}"#,
    ),
    (
        "admin",
        r#"{"Version":"2024-01-01","Statement":[{"Sid":"AdminAccess","Effect":"Allow","Action":["ai_gateway:use","mcp_gateway:use","api_keys:read","api_keys:create","api_keys:update","api_keys:rotate","api_keys:delete","providers:read","providers:create","providers:update","providers:delete","providers:rotate_key","models:read","models:write","mcp_servers:read","mcp_servers:create","mcp_servers:update","mcp_servers:delete","users:read","users:create","users:update","teams:read","teams:create","teams:update","teams:delete","team_members:write","team:read","team:write","sessions:revoke","roles:read","roles:create","roles:update","roles:delete","analytics:read_all","audit_logs:read_all","logs:read_all","log_forwarders:read","log_forwarders:write","webhooks:read","webhooks:write","content_filter:read","content_filter:write","pii_redactor:read","pii_redactor:write","rate_limits:read","rate_limits:write","settings:read","settings:write"],"Resource":"*"}]}"#,
    ),
    (
        "team_manager",
        r#"{"Version":"2024-01-01","Statement":[{"Sid":"TeamManagement","Effect":"Allow","Action":["ai_gateway:use","mcp_gateway:use","api_keys:read","api_keys:create","api_keys:update","api_keys:rotate","providers:read","models:read","mcp_servers:read","users:read","users:update","team_members:write","team:read","team:write","analytics:read_team","audit_logs:read_team","logs:read_team","rate_limits:read","rate_limits:write"],"Resource":"*"}]}"#,
    ),
    (
        "developer",
        r#"{"Version":"2024-01-01","Statement":[{"Sid":"DeveloperAccess","Effect":"Allow","Action":["ai_gateway:use","mcp_gateway:use","api_keys:read","api_keys:create","api_keys:update","providers:read","models:read","mcp_servers:read","analytics:read_own","audit_logs:read_own","logs:read_own"],"Resource":"*"}]}"#,
    ),
    (
        "viewer",
        r#"{"Version":"2024-01-01","Statement":[{"Sid":"ViewerAccess","Effect":"Allow","Action":["api_keys:read","providers:read","models:read","mcp_servers:read","analytics:read_own","audit_logs:read_own","logs:read_own"],"Resource":"*"}]}"#,
    ),
];

fn system_role_default_policy(name: &str) -> Option<serde_json::Value> {
    SYSTEM_ROLE_DEFAULTS
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, json)| serde_json::from_str(json).ok())
}

/// Startup-time validation: every permission derived from a role's
/// `policy_document` must appear in the static `PERMISSIONS` catalog.
///
/// If this check fails the server refuses to start. Rationale: a seeded
/// role that grants a permission the catalog doesn't know about means
/// either (a) the migration is stale, (b) the catalog was trimmed without
/// updating the seed, or (c) someone wrote to the DB by hand. All three
/// are footguns that silently break authorization, so we want a loud
/// fail-fast.
pub async fn validate_seeded_roles(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let rows: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT name, policy_document FROM rbac_roles")
            .fetch_all(pool)
            .await?;
    let all_perm_keys: Vec<&str> = PERMISSIONS.iter().map(|p| p.key).collect();
    let mut unknown: Vec<String> = Vec::new();
    for (role_name, doc) in &rows {
        let perms = think_watch_common::limits::extract_permissions(doc, &all_perm_keys);
        for perm in &perms {
            if !is_known_permission(perm) {
                unknown.push(format!("{role_name}: {perm}"));
            }
        }
    }
    if !unknown.is_empty() {
        anyhow::bail!(
            "Found {} role permission(s) not in PERMISSION_CATALOG:\n  - {}\n\
             Either update the catalog in crates/server/src/handlers/roles.rs \
             or fix the seed in migrations/001_init.sql.",
            unknown.len(),
            unknown.join("\n  - "),
        );
    }
    tracing::info!(
        role_count = rows.len(),
        "RBAC catalog validation passed: all seeded role permissions are known"
    );
    Ok(())
}

/// All permission keys as a static slice, used when passing the catalog
/// to `compute_user_permissions` in the auth crate.
pub fn all_permission_keys() -> Vec<&'static str> {
    PERMISSIONS.iter().map(|p| p.key).collect()
}

// ============================================================================
// Role + role-assignment handlers.
//
// All reads and writes go through `rbac_roles` and `rbac_role_assignments`.
// ============================================================================

// --- Response types ---

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RoleResponse {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub is_system: bool,
    /// The unified policy document containing permissions, resource
    /// scopes, rate limits, and budgets.
    pub policy_document: serde_json::Value,
    /// Number of users currently assigned to this role.
    pub user_count: i64,
    /// Email of the user who created this role. `None` for system
    /// roles seeded by migrations and for any role whose creator was
    /// later soft-deleted.
    pub created_by_email: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RolesListResponse {
    pub items: Vec<RoleResponse>,
}

/// One row from `rbac_roles` (with creator email LEFT JOINed in)
/// mapped 1:1 by sqlx.
type RoleRow = (
    Uuid,
    String,
    Option<String>,
    bool,
    serde_json::Value,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
);

const ROLE_SELECT: &str = "SELECT r.id, r.name, r.description, r.is_system, \
                                  r.policy_document, \
                                  u.email AS created_by_email, \
                                  r.created_at, r.updated_at \
                           FROM rbac_roles r \
                           LEFT JOIN users u ON u.id = r.created_by";

fn row_to_response(row: RoleRow, user_count: i64) -> RoleResponse {
    RoleResponse {
        id: row.0,
        name: row.1,
        description: row.2,
        is_system: row.3,
        policy_document: row.4,
        user_count,
        created_by_email: row.5,
        created_at: row.6.to_rfc3339(),
        updated_at: row.7.to_rfc3339(),
    }
}

// --- List all roles ---

#[utoipa::path(
    get,
    path = "/api/admin/roles",
    tag = "Roles",
    responses(
        (status = 200, description = "All roles with user counts", body = RolesListResponse),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_roles(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<RolesListResponse>, AppError> {
    auth_user.require_permission("roles:read")?;
    auth_user
        .assert_scope_global(&state.db, "roles:read")
        .await?;
    // System rows first, then alphabetical. Permissions and counts are
    // pulled in two more queries (no N+1) and merged in Rust.
    let rows: Vec<RoleRow> =
        sqlx::query_as(&format!("{ROLE_SELECT} ORDER BY is_system DESC, name ASC"))
            .fetch_all(&state.db)
            .await?;

    let role_ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();

    let counts: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT role_id, COUNT(*)::bigint \
           FROM rbac_role_assignments \
          WHERE role_id = ANY($1) \
          GROUP BY role_id",
    )
    .bind(&role_ids)
    .fetch_all(&state.db)
    .await?;
    let mut count_map: std::collections::HashMap<Uuid, i64> = std::collections::HashMap::new();
    for (rid, c) in counts {
        count_map.insert(rid, c);
    }

    let items = rows
        .into_iter()
        .map(|row| {
            let id = row.0;
            row_to_response(row, count_map.get(&id).copied().unwrap_or(0))
        })
        .collect();

    Ok(Json(RolesListResponse { items }))
}

// --- Create role ---

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateRoleRequest {
    pub name: String,
    pub description: Option<String>,
    pub policy_document: serde_json::Value,
}

#[utoipa::path(
    post,
    path = "/api/admin/roles",
    tag = "Roles",
    request_body = CreateRoleRequest,
    responses(
        (status = 200, description = "Created role", body = RoleResponse),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn create_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(payload): Json<CreateRoleRequest>,
) -> Result<Json<RoleResponse>, AppError> {
    auth_user.require_permission("roles:create")?;
    auth_user
        .assert_scope_global(&state.db, "roles:create")
        .await?;
    let name = payload.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::BadRequest(
            "Role name must be 1-100 characters".into(),
        ));
    }
    rbac::validate_policy_document(&payload.policy_document).map_err(AppError::BadRequest)?;
    validate_policy_constraints_in_doc(&payload.policy_document)?;

    let row: RoleRow = sqlx::query_as(
        "WITH inserted AS ( \
            INSERT INTO rbac_roles (name, description, is_system, policy_document, created_by) \
            VALUES ($1, $2, FALSE, $3, $4) \
            RETURNING * \
         ) \
         SELECT i.id, i.name, i.description, i.is_system, i.policy_document, \
                u.email AS created_by_email, \
                i.created_at, i.updated_at \
         FROM inserted i \
         LEFT JOIN users u ON u.id = i.created_by",
    )
    .bind(name)
    .bind(&payload.description)
    .bind(&payload.policy_document)
    .bind(auth_user.claims.sub)
    .fetch_one(&state.db)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db_err) if db_err.constraint() == Some("rbac_roles_name_key") => {
            AppError::BadRequest(format!("Role name '{name}' already exists"))
        }
        _ => AppError::from(e),
    })?;

    let response = row_to_response(row, 0);

    state.audit.log(
        auth_user
            .audit("role.created")
            .resource("role")
            .resource_id(response.id.to_string())
            .detail(serde_json::json!({ "name": name })),
    );

    Ok(Json(response))
}

// --- Update role ---

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateRoleRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub policy_document: Option<serde_json::Value>,
}

#[utoipa::path(
    patch,
    path = "/api/admin/roles/{id}",
    tag = "Roles",
    params(
        ("id" = uuid::Uuid, Path, description = "Role ID"),
    ),
    request_body = UpdateRoleRequest,
    responses(
        (status = 200, description = "Updated role", body = RoleResponse),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Role not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn update_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateRoleRequest>,
) -> Result<Json<RoleResponse>, AppError> {
    auth_user.require_permission("roles:update")?;
    auth_user
        .assert_scope_global(&state.db, "roles:update")
        .await?;
    let existing =
        sqlx::query_as::<_, (bool, String)>("SELECT is_system, name FROM rbac_roles WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound("Role not found".into()))?;
    let is_system = existing.0;

    // System role gating:
    //   - Editing the policy_document or description of a system role
    //     requires the additional `roles:edit_system` permission, which
    //     is strictly more dangerous than `roles:update`.
    //   - The `name` and `is_system` columns are immovable on system rows
    //     because the rest of the app key off the literal string
    //     ("super_admin", "developer", ...). The UI hides the name field
    //     for system rows; the SQL clause below enforces it on the wire.
    if is_system {
        auth_user.require_permission("roles:edit_system")?;
        auth_user
            .assert_scope_global(&state.db, "roles:edit_system")
            .await?;
        if payload.name.is_some() {
            return Err(AppError::BadRequest("Cannot rename system roles".into()));
        }
    }

    if let Some(ref doc) = payload.policy_document {
        rbac::validate_policy_document(doc).map_err(AppError::BadRequest)?;
        validate_policy_constraints_in_doc(doc)?;
    }

    sqlx::query(
        "UPDATE rbac_roles SET \
            name             = COALESCE($2, name), \
            description      = COALESCE($3, description), \
            policy_document  = COALESCE($4, policy_document), \
            updated_at       = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(payload.name.as_deref().map(str::trim))
    .bind(payload.description.as_deref())
    .bind(payload.policy_document.as_ref())
    .execute(&state.db)
    .await?;

    let row: RoleRow = sqlx::query_as(&format!("{ROLE_SELECT} WHERE id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;

    let user_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM rbac_role_assignments WHERE role_id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);

    invalidate_role_perms(&state.db, &state.redis, id).await;

    state.audit.log(
        auth_user
            .audit("role.updated")
            .resource("role")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": existing.1 })),
    );

    Ok(Json(row_to_response(row, user_count)))
}

// --- Reset system role to defaults ---
//
// Re-applies the catalog-default permission set for a system role
// (whatever `SYSTEM_ROLE_DEFAULTS` says) and clears any
// allowed_models / allowed_mcp_tools / policy_document overrides
// that have accumulated. Useful after an in-place edit goes wrong.
//
// Custom roles have no canonical defaults, so this endpoint is
// system-only and rejects everything else with 400.

#[utoipa::path(
    post,
    path = "/api/admin/roles/{id}/reset",
    tag = "Roles",
    params(
        ("id" = uuid::Uuid, Path, description = "Role ID"),
    ),
    responses(
        (status = 200, description = "Role reset to defaults", body = RoleResponse),
        (status = 400, description = "Not a system role"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Role not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn reset_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RoleResponse>, AppError> {
    auth_user.require_permission("roles:edit_system")?;
    auth_user
        .assert_scope_global(&state.db, "roles:edit_system")
        .await?;

    let existing =
        sqlx::query_as::<_, (bool, String)>("SELECT is_system, name FROM rbac_roles WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound("Role not found".into()))?;
    if !existing.0 {
        return Err(AppError::BadRequest(
            "Reset is only available for system roles".into(),
        ));
    }

    let default_doc = system_role_default_policy(&existing.1).ok_or_else(|| {
        AppError::BadRequest(format!(
            "No defaults registered for system role '{}'",
            existing.1
        ))
    })?;

    sqlx::query(
        "UPDATE rbac_roles SET \
            policy_document  = $2, \
            updated_at       = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(&default_doc)
    .execute(&state.db)
    .await?;

    let row: RoleRow = sqlx::query_as(&format!("{ROLE_SELECT} WHERE id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    let user_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM rbac_role_assignments WHERE role_id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);

    invalidate_role_perms(&state.db, &state.redis, id).await;

    state.audit.log(
        auth_user
            .audit("role.reset")
            .resource("role")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": existing.1 })),
    );

    Ok(Json(row_to_response(row, user_count)))
}

// --- Delete role (with reassign-on-members) ---

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct DeleteRoleQuery {
    pub reassign_to: Option<Uuid>,
}

#[utoipa::path(
    delete,
    path = "/api/admin/roles/{id}",
    tag = "Roles",
    params(
        ("id" = uuid::Uuid, Path, description = "Role ID"),
        ("reassign_to" = Option<uuid::Uuid>, Query, description = "Role ID to migrate existing members to"),
    ),
    responses(
        (status = 200, description = "Role deleted"),
        (status = 400, description = "System role or members still assigned without reassignment target"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Role not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn delete_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<DeleteRoleQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("roles:delete")?;
    auth_user
        .assert_scope_global(&state.db, "roles:delete")
        .await?;
    let existing =
        sqlx::query_as::<_, (bool, String)>("SELECT is_system, name FROM rbac_roles WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound("Role not found".into()))?;
    if existing.0 {
        return Err(AppError::BadRequest("Cannot delete system roles".into()));
    }
    let role_name = existing.1;

    let assigned: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM rbac_role_assignments WHERE role_id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);

    let mut tx = state.db.begin().await?;

    if assigned > 0 {
        match query.reassign_to {
            Some(target_id) if target_id == id => {
                return Err(AppError::BadRequest(
                    "reassign_to must be a different role".into(),
                ));
            }
            Some(target_id) => {
                let target_exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rbac_roles WHERE id = $1)")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                if !target_exists {
                    return Err(AppError::BadRequest("reassign_to role not found".into()));
                }
                // Migrate every (user, scope) pair to the new role.
                sqlx::query(
                    "INSERT INTO rbac_role_assignments \
                         (user_id, role_id, scope_kind, scope_id, assigned_by) \
                     SELECT user_id, $2, scope_kind, scope_id, $3 \
                       FROM rbac_role_assignments WHERE role_id = $1 \
                     ON CONFLICT DO NOTHING",
                )
                .bind(id)
                .bind(target_id)
                .bind(auth_user.claims.sub)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM rbac_role_assignments WHERE role_id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            None => {
                return Err(AppError::BadRequest(format!(
                    "Role still has {assigned} member(s). Pass reassign_to=<other_role_id> to migrate them."
                )));
            }
        }
    }

    sqlx::query("DELETE FROM rbac_roles WHERE id = $1 AND is_system = FALSE")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    state.audit.log(
        auth_user
            .audit("role.deleted")
            .resource("role")
            .resource_id(id.to_string())
            .detail(serde_json::json!({
                "name": role_name,
                "reassign_to": query.reassign_to,
                "members_migrated": assigned,
            })),
    );

    Ok(Json(serde_json::json!({
        "deleted": true,
        "reassigned": assigned,
    })))
}

// --- List members of a role ---

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RoleMember {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    /// Encoded scope string matching `RoleAssignment.scope` shape:
    /// `"global"` for unscoped, `"team:<uuid>"` for team-scoped. Kept
    /// as a single string (not a kind/id pair) because every other
    /// scope-bearing DTO in this codebase uses the same encoding —
    /// admins comparing assignments across endpoints expect one shape.
    pub scope: String,
    pub assigned_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RoleMembersResponse {
    pub items: Vec<RoleMember>,
}

#[utoipa::path(
    get,
    path = "/api/admin/roles/{id}/members",
    tag = "Roles",
    params(
        ("id" = uuid::Uuid, Path, description = "Role ID"),
    ),
    responses(
        (status = 200, description = "Users assigned to this role", body = RoleMembersResponse),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Role not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_role_members(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RoleMembersResponse>, AppError> {
    auth_user.require_permission("roles:read")?;
    auth_user
        .assert_scope_global(&state.db, "roles:read")
        .await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rbac_roles WHERE id = $1)")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    if !exists {
        return Err(AppError::NotFound("Role not found".into()));
    }

    type Row = (
        Uuid,
        String,
        Option<String>,
        String,
        Option<Uuid>,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT u.id, u.email, u.display_name, ra.scope_kind, ra.scope_id, ra.assigned_at \
           FROM rbac_role_assignments ra \
           JOIN users u ON u.id = ra.user_id \
          WHERE ra.role_id = $1 \
          ORDER BY u.email ASC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;

    let items = rows
        .into_iter()
        .map(
            |(user_id, email, display_name, scope_kind, scope_id, assigned_at)| {
                let scope = match (scope_kind.as_str(), scope_id) {
                    ("global", _) => "global".to_string(),
                    (kind, Some(id)) => format!("{kind}:{id}"),
                    (kind, None) => kind.to_string(),
                };
                RoleMember {
                    user_id,
                    email,
                    display_name,
                    scope,
                    assigned_at: assigned_at.to_rfc3339(),
                }
            },
        )
        .collect();

    Ok(Json(RoleMembersResponse { items }))
}

// --- List all valid permissions ---
//
// Returns the structured catalog so the frontend can group by resource
// and surface dangerous permissions with extra confirmation.
#[utoipa::path(
    get,
    path = "/api/admin/roles/permissions",
    tag = "Roles",
    responses(
        (status = 200, description = "Full permission catalog"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_permissions(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<PermissionDef>>, AppError> {
    auth_user.require_permission("roles:read")?;
    auth_user
        .assert_scope_global(&state.db, "roles:read")
        .await?;
    Ok(Json(PERMISSIONS.to_vec()))
}

// --- Role audit history ---
//
// Reads the platform_logs table (where role mutations are recorded
// via `auth_user.audit("role.*")`) for entries scoped to one role.
// Gated by `roles:read` rather than `logs:read_all` so anyone who
// can view the role can also see who has touched it — there is no
// extra information here beyond what the role itself already shows.

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RoleHistoryEntry {
    pub id: String,
    pub action: String,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub ip_address: Option<String>,
    pub detail: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RoleHistoryResponse {
    pub items: Vec<RoleHistoryEntry>,
}

#[derive(Debug, Serialize, Deserialize, clickhouse::Row)]
struct RoleHistoryRow {
    id: String,
    action: String,
    user_id: Option<String>,
    user_email: Option<String>,
    ip_address: Option<String>,
    detail: Option<String>,
    created_at: String,
}

#[utoipa::path(
    get,
    path = "/api/admin/roles/{id}/history",
    tag = "Roles",
    params(
        ("id" = uuid::Uuid, Path, description = "Role ID"),
    ),
    responses(
        (status = 200, description = "Role audit history (last 100 entries)", body = RoleHistoryResponse),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Role not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_role_history(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RoleHistoryResponse>, AppError> {
    auth_user.require_permission("roles:read")?;
    auth_user
        .assert_scope_global(&state.db, "roles:read")
        .await?;

    // 404 if the role doesn't exist — same shape as list_role_members.
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM rbac_roles WHERE id = $1)")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    if !exists {
        return Err(AppError::NotFound("Role not found".into()));
    }

    let Some(ch) = state.clickhouse.as_ref() else {
        // ClickHouse not configured — history is recorded in
        // platform_logs only, so without it there is nothing to
        // return. Empty list, not an error: the rest of the page
        // should still render fine.
        return Ok(Json(RoleHistoryResponse { items: vec![] }));
    };

    // Bound the result set so a long-lived role can't load
    // unbounded history into the dialog. 100 rows == ~1 month of
    // typical activity for a single role.
    let sql = "SELECT id, action, user_id, user_email, ip_address, detail, \
                      toString(created_at) AS created_at \
               FROM platform_logs \
              WHERE resource = 'role' AND resource_id = ? \
              ORDER BY created_at DESC \
              LIMIT 100";
    let rows: Vec<RoleHistoryRow> = ch
        .query(sql)
        .bind(id.to_string().as_str())
        .fetch_all()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    let items = rows
        .into_iter()
        .map(|r| RoleHistoryEntry {
            id: r.id,
            action: r.action,
            user_id: r.user_id,
            user_email: r.user_email,
            ip_address: r.ip_address,
            detail: r.detail.and_then(|s| serde_json::from_str(&s).ok()),
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(RoleHistoryResponse { items }))
}
