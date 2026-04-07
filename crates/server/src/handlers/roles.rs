use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_auth::rbac;
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// One permission entry in the structured catalog.
///
/// `key` is the canonical `resource:action` string the rest of the system
/// matches against. `resource` and `action` are denormalized for grouping
/// in the UI. `dangerous` flags permissions that should require an extra
/// confirmation when granted (rotating API keys, revoking sessions,
/// disabling PII redaction, etc).
#[derive(Debug, Clone, Copy, Serialize)]
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

/// Canonical permission catalog. The previous catalog had 17 entries with
/// coarse `*:write` / `*:manage` lumps that hid important distinctions
/// (rotate API key vs delete provider, disable PII redaction vs change
/// CORS origins). This is the audit-driven replacement: split by verb,
/// flag the genuinely dangerous ones, group by resource for the UI.
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
    // --- Providers (AI upstream) ---
    p("providers:read", "providers", "read"),
    p("providers:create", "providers", "create"),
    p("providers:update", "providers", "update"),
    d("providers:delete", "providers", "delete"),
    d("providers:rotate_key", "providers", "rotate_key"),
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
    // --- System settings ---
    p("settings:read", "settings", "read"),
    d("settings:write", "settings", "write"),
    d("system:configure_oidc", "system", "configure_oidc"),
];

fn is_known_permission(key: &str) -> bool {
    PERMISSIONS.iter().any(|p| p.key == key)
}

// --- Response types ---

#[derive(Debug, Serialize)]
pub struct RoleResponse {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub is_system: bool,
    pub permissions: Vec<String>,
    /// Allowed model IDs. `null` = unrestricted (all models).
    pub allowed_models: Option<Vec<String>>,
    /// Allowed MCP server UUIDs. `null` = unrestricted (all servers).
    pub allowed_mcp_servers: Option<Vec<Uuid>>,
    /// AWS IAM-style policy document JSON. `null` = use legacy permissions.
    pub policy_document: Option<serde_json::Value>,
    /// Number of users currently assigned to this role. Used by the
    /// admin UI to surface impact when changing or deleting a role.
    pub user_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct RolesListResponse {
    pub items: Vec<RoleResponse>,
}

// --- List all roles ---

pub async fn list_roles(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<RolesListResponse>, AppError> {
    // Roles + their permissions + user counts in three queries (instead
    // of N+1 per role). System rows are ordered first so the table shows
    // the seeded templates above any custom roles.
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Option<String>,
            bool,
            Option<Vec<String>>,
            Option<Vec<Uuid>>,
            Option<serde_json::Value>,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        "SELECT id, name, description, is_system, allowed_models, allowed_mcp_servers, \
                policy_document, created_at, updated_at \
         FROM custom_roles \
         ORDER BY is_system DESC, name ASC",
    )
    .fetch_all(&state.db)
    .await?;

    let role_ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();

    let perm_rows = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT custom_role_id, permission FROM custom_role_permissions \
         WHERE custom_role_id = ANY($1)",
    )
    .bind(&role_ids)
    .fetch_all(&state.db)
    .await?;

    let mut perm_map: std::collections::HashMap<Uuid, Vec<String>> =
        std::collections::HashMap::new();
    for (rid, perm) in perm_rows {
        perm_map.entry(rid).or_default().push(perm);
    }

    // User count per role: surfaces "X users will lose this access" in
    // the UI when an admin tries to delete or downgrade a role.
    //
    // Counts are pulled from BOTH membership tables and merged by role
    // name, because legacy system-role assignments live in the
    // `user_roles` -> `roles` (by-name) tables while custom-role
    // assignments live in `user_custom_roles`. Until we unify the two
    // membership tables in a future migration, summing them by name is
    // the most accurate "who has this role today" view.
    let count_rows = sqlx::query_as::<_, (Uuid, i64)>(
        "SELECT cr.id, COALESCE(legacy.cnt, 0) + COALESCE(modern.cnt, 0) AS total \
           FROM custom_roles cr \
           LEFT JOIN ( \
             SELECT r.name AS name, COUNT(*)::bigint AS cnt \
               FROM user_roles ur \
               JOIN roles r ON r.id = ur.role_id \
              GROUP BY r.name \
           ) AS legacy ON legacy.name = cr.name \
           LEFT JOIN ( \
             SELECT custom_role_id, COUNT(*)::bigint AS cnt \
               FROM user_custom_roles \
              GROUP BY custom_role_id \
           ) AS modern ON modern.custom_role_id = cr.id \
          WHERE cr.id = ANY($1)",
    )
    .bind(&role_ids)
    .fetch_all(&state.db)
    .await?;

    let mut count_map: std::collections::HashMap<Uuid, i64> = std::collections::HashMap::new();
    for (rid, c) in count_rows {
        count_map.insert(rid, c);
    }

    let items = rows
        .into_iter()
        .map(
            |(
                id,
                name,
                description,
                is_system,
                allowed_models,
                allowed_mcp_servers,
                policy_document,
                created_at,
                updated_at,
            )| {
                let mut permissions = perm_map.remove(&id).unwrap_or_default();
                permissions.sort();
                RoleResponse {
                    id,
                    name,
                    description,
                    is_system,
                    permissions,
                    allowed_models,
                    allowed_mcp_servers,
                    policy_document,
                    user_count: count_map.get(&id).copied().unwrap_or(0),
                    created_at: created_at.to_rfc3339(),
                    updated_at: updated_at.to_rfc3339(),
                }
            },
        )
        .collect();

    Ok(Json(RolesListResponse { items }))
}

// --- Create custom role ---

#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    pub name: String,
    pub description: Option<String>,
    pub permissions: Vec<String>,
    /// Restrict to specific model IDs. `null` or absent = unrestricted.
    pub allowed_models: Option<Vec<String>>,
    /// Restrict to specific MCP server UUIDs. `null` or absent = unrestricted.
    pub allowed_mcp_servers: Option<Vec<Uuid>>,
    /// AWS IAM-style policy document. When provided, takes precedence over permissions.
    pub policy_document: Option<serde_json::Value>,
}

pub async fn create_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(payload): Json<CreateRoleRequest>,
) -> Result<Json<RoleResponse>, AppError> {
    let name = payload.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::BadRequest(
            "Role name must be 1-100 characters".into(),
        ));
    }

    // Validate policy document if provided
    if let Some(ref doc) = payload.policy_document {
        rbac::validate_policy_document(doc).map_err(AppError::BadRequest)?;
    }

    // Validate permissions
    for perm in &payload.permissions {
        if !is_known_permission(perm) {
            return Err(AppError::BadRequest(format!("Invalid permission: {perm}")));
        }
    }

    let mut tx = state.db.begin().await?;

    let row = sqlx::query_as::<_, (Uuid, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>(
        "INSERT INTO custom_roles (name, description, created_by, allowed_models, allowed_mcp_servers, policy_document) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id, created_at, updated_at",
    )
    .bind(name)
    .bind(&payload.description)
    .bind(auth_user.claims.sub)
    .bind(&payload.allowed_models)
    .bind(&payload.allowed_mcp_servers)
    .bind(&payload.policy_document)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db_err) if db_err.constraint() == Some("custom_roles_name_key") => {
            AppError::BadRequest(format!("Role name '{name}' already exists"))
        }
        sqlx::Error::Database(db_err) if db_err.constraint() == Some("custom_roles_created_by_fkey") => {
            AppError::BadRequest("Current user not found — cannot create role".into())
        }
        _ => {
            tracing::error!("Failed to create custom role: {e:?}");
            AppError::from(e)
        }
    })?;

    for perm in &payload.permissions {
        sqlx::query(
            "INSERT INTO custom_role_permissions (custom_role_id, permission) VALUES ($1, $2)",
        )
        .bind(row.0)
        .bind(perm)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    state.audit.log(
        auth_user
            .audit("role.created")
            .resource("role")
            .resource_id(row.0.to_string())
            .detail(serde_json::json!({ "name": name })),
    );

    Ok(Json(RoleResponse {
        id: row.0,
        name: name.to_string(),
        description: payload.description,
        is_system: false,
        permissions: payload.permissions,
        allowed_models: payload.allowed_models,
        allowed_mcp_servers: payload.allowed_mcp_servers,
        policy_document: payload.policy_document,
        user_count: 0,
        created_at: row.1.to_rfc3339(),
        updated_at: row.2.to_rfc3339(),
    }))
}

// --- Update custom role ---

#[derive(Debug, Deserialize)]
pub struct UpdateRoleRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub permissions: Option<Vec<String>>,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_mcp_servers: Option<Vec<Uuid>>,
    pub policy_document: Option<serde_json::Value>,
}

pub async fn update_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateRoleRequest>,
) -> Result<Json<RoleResponse>, AppError> {
    // Prevent editing system roles
    let existing = sqlx::query_as::<_, (bool, String)>(
        "SELECT is_system, name FROM custom_roles WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Role not found".into()))?;

    if existing.0 {
        return Err(AppError::BadRequest("Cannot modify system roles".into()));
    }

    // Validate policy document if provided
    if let Some(ref doc) = payload.policy_document {
        rbac::validate_policy_document(doc).map_err(AppError::BadRequest)?;
    }

    if let Some(ref perms) = payload.permissions {
        for perm in perms {
            if !is_known_permission(perm) {
                return Err(AppError::BadRequest(format!("Invalid permission: {perm}")));
            }
        }
    }

    let mut tx = state.db.begin().await?;

    if let Some(ref name) = payload.name {
        let name = name.trim();
        if name.is_empty() || name.len() > 100 {
            return Err(AppError::BadRequest(
                "Role name must be 1-100 characters".into(),
            ));
        }
        sqlx::query("UPDATE custom_roles SET name = $1, updated_at = now() WHERE id = $2")
            .bind(name)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }

    if let Some(ref desc) = payload.description {
        sqlx::query("UPDATE custom_roles SET description = $1, updated_at = now() WHERE id = $2")
            .bind(desc)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }

    // Update allowed_models (explicit null clears restriction)
    if payload.allowed_models.is_some() {
        sqlx::query(
            "UPDATE custom_roles SET allowed_models = $1, updated_at = now() WHERE id = $2",
        )
        .bind(&payload.allowed_models)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }

    // Update allowed_mcp_servers
    if payload.allowed_mcp_servers.is_some() {
        sqlx::query(
            "UPDATE custom_roles SET allowed_mcp_servers = $1, updated_at = now() WHERE id = $2",
        )
        .bind(&payload.allowed_mcp_servers)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }

    // Update policy_document
    if payload.policy_document.is_some() {
        sqlx::query(
            "UPDATE custom_roles SET policy_document = $1, updated_at = now() WHERE id = $2",
        )
        .bind(&payload.policy_document)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }

    if let Some(ref perms) = payload.permissions {
        sqlx::query("DELETE FROM custom_role_permissions WHERE custom_role_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for perm in perms {
            sqlx::query(
                "INSERT INTO custom_role_permissions (custom_role_id, permission) VALUES ($1, $2)",
            )
            .bind(id)
            .bind(perm)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE custom_roles SET updated_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    state.audit.log(
        auth_user
            .audit("role.updated")
            .resource("role")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": existing.1 })),
    );

    // Fetch updated role
    let row = sqlx::query_as::<_, (Uuid, String, Option<String>, bool, Option<Vec<String>>, Option<Vec<Uuid>>, Option<serde_json::Value>, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, name, description, is_system, allowed_models, allowed_mcp_servers, policy_document, created_at, updated_at FROM custom_roles WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    let perms: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT permission FROM custom_role_permissions WHERE custom_role_id = $1",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    // Same dual-table count as list_roles (see comment there).
    let user_count: i64 = sqlx::query_scalar(
        "SELECT COALESCE(legacy.cnt, 0) + COALESCE(modern.cnt, 0) \
           FROM custom_roles cr \
           LEFT JOIN ( \
             SELECT r.name AS name, COUNT(*)::bigint AS cnt \
               FROM user_roles ur JOIN roles r ON r.id = ur.role_id \
              GROUP BY r.name \
           ) legacy ON legacy.name = cr.name \
           LEFT JOIN ( \
             SELECT custom_role_id, COUNT(*)::bigint AS cnt \
               FROM user_custom_roles GROUP BY custom_role_id \
           ) modern ON modern.custom_role_id = cr.id \
          WHERE cr.id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    Ok(Json(RoleResponse {
        id: row.0,
        name: row.1,
        description: row.2,
        is_system: row.3,
        permissions: perms,
        allowed_models: row.4,
        allowed_mcp_servers: row.5,
        policy_document: row.6,
        user_count,
        created_at: row.7.to_rfc3339(),
        updated_at: row.8.to_rfc3339(),
    }))
}

// --- Delete custom role ---

#[derive(Debug, Deserialize)]
pub struct DeleteRoleQuery {
    /// If set, all users currently assigned to the role being deleted
    /// will be reassigned to this role atomically. Otherwise the delete
    /// is rejected when the role still has members (so the admin
    /// doesn't accidentally orphan users).
    pub reassign_to: Option<Uuid>,
}

pub async fn delete_role(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<DeleteRoleQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let existing = sqlx::query_as::<_, (bool, String)>(
        "SELECT is_system, name FROM custom_roles WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Role not found".into()))?;

    if existing.0 {
        return Err(AppError::BadRequest("Cannot delete system roles".into()));
    }

    let role_name = existing.1;

    // Don't orphan users. If members exist, the caller must either pass
    // ?reassign_to=<role_id> or remove the assignments first.
    let assigned: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM user_custom_roles WHERE custom_role_id = $1",
    )
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
                // Verify the target exists and isn't the same row.
                let target_exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM custom_roles WHERE id = $1)")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                if !target_exists {
                    return Err(AppError::BadRequest("reassign_to role not found".into()));
                }
                // Swap each membership over. Use ON CONFLICT DO NOTHING
                // because the user might already have the target role.
                sqlx::query(
                    "INSERT INTO user_custom_roles (user_id, custom_role_id) \
                     SELECT user_id, $2 FROM user_custom_roles WHERE custom_role_id = $1 \
                     ON CONFLICT DO NOTHING",
                )
                .bind(id)
                .bind(target_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM user_custom_roles WHERE custom_role_id = $1")
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

    sqlx::query("DELETE FROM custom_roles WHERE id = $1")
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

// --- List all valid permissions ---
//
// Returns the structured catalog so the frontend can group by resource
// and surface dangerous permissions with extra confirmation. The legacy
// `Vec<String>` shape is preserved by serializing each entry as an
// object — frontends that only read `key` keep working.

pub async fn list_permissions(_auth_user: AuthUser) -> Json<Vec<PermissionDef>> {
    Json(PERMISSIONS.to_vec())
}
