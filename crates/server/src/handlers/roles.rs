use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use agent_bastion_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// All valid permissions that can be assigned to a custom role.
pub const ALL_PERMISSIONS: &[&str] = &[
    "ai_gateway:use",
    "mcp_gateway:use",
    "api_keys:read",
    "api_keys:write",
    "team:read",
    "team:write",
    "analytics:read",
    "users:read",
    "users:write",
    "providers:read",
    "providers:write",
    "mcp_servers:read",
    "mcp_servers:write",
    "log_forwarders:read",
    "log_forwarders:write",
    "audit_logs:read",
    "system:settings",
];

// --- Response types ---

#[derive(Debug, Serialize)]
pub struct RoleResponse {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub is_system: bool,
    pub permissions: Vec<String>,
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
    let rows = sqlx::query_as::<_, (Uuid, String, Option<String>, bool, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, name, description, is_system, created_at, updated_at FROM custom_roles ORDER BY is_system DESC, name ASC",
    )
    .fetch_all(&state.db)
    .await?;

    let role_ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();

    let perm_rows = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT custom_role_id, permission FROM custom_role_permissions WHERE custom_role_id = ANY($1)",
    )
    .bind(&role_ids)
    .fetch_all(&state.db)
    .await?;

    let mut perm_map: std::collections::HashMap<Uuid, Vec<String>> =
        std::collections::HashMap::new();
    for (rid, perm) in perm_rows {
        perm_map.entry(rid).or_default().push(perm);
    }

    let items = rows
        .into_iter()
        .map(|(id, name, description, is_system, created_at, updated_at)| RoleResponse {
            id,
            name,
            description,
            is_system,
            permissions: perm_map.remove(&id).unwrap_or_default(),
            created_at: created_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(RolesListResponse { items }))
}

// --- Create custom role ---

#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    pub name: String,
    pub description: Option<String>,
    pub permissions: Vec<String>,
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

    // Validate permissions
    for perm in &payload.permissions {
        if !ALL_PERMISSIONS.contains(&perm.as_str()) {
            return Err(AppError::BadRequest(format!(
                "Invalid permission: {perm}"
            )));
        }
    }

    let mut tx = state.db.begin().await?;

    let row = sqlx::query_as::<_, (Uuid, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>(
        "INSERT INTO custom_roles (name, description, created_by) VALUES ($1, $2, $3) RETURNING id, created_at, updated_at",
    )
    .bind(name)
    .bind(&payload.description)
    .bind(auth_user.claims.sub)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db_err) if db_err.constraint() == Some("custom_roles_name_key") => {
            AppError::BadRequest(format!("Role name '{name}' already exists"))
        }
        _ => AppError::from(e),
    })?;

    for perm in &payload.permissions {
        sqlx::query("INSERT INTO custom_role_permissions (custom_role_id, permission) VALUES ($1, $2)")
            .bind(row.0)
            .bind(perm)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    Ok(Json(RoleResponse {
        id: row.0,
        name: name.to_string(),
        description: payload.description,
        is_system: false,
        permissions: payload.permissions,
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
}

pub async fn update_role(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateRoleRequest>,
) -> Result<Json<RoleResponse>, AppError> {
    // Prevent editing system roles
    let existing = sqlx::query_as::<_, (bool,)>(
        "SELECT is_system FROM custom_roles WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Role not found".into()))?;

    if existing.0 {
        return Err(AppError::BadRequest("Cannot modify system roles".into()));
    }

    if let Some(ref perms) = payload.permissions {
        for perm in perms {
            if !ALL_PERMISSIONS.contains(&perm.as_str()) {
                return Err(AppError::BadRequest(format!(
                    "Invalid permission: {perm}"
                )));
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

    if let Some(ref perms) = payload.permissions {
        sqlx::query("DELETE FROM custom_role_permissions WHERE custom_role_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for perm in perms {
            sqlx::query("INSERT INTO custom_role_permissions (custom_role_id, permission) VALUES ($1, $2)")
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

    // Fetch updated role
    let row = sqlx::query_as::<_, (Uuid, String, Option<String>, bool, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, name, description, is_system, created_at, updated_at FROM custom_roles WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    let perms = sqlx::query_as::<_, (String,)>(
        "SELECT permission FROM custom_role_permissions WHERE custom_role_id = $1",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    Ok(Json(RoleResponse {
        id: row.0,
        name: row.1,
        description: row.2,
        is_system: row.3,
        permissions: perms,
        created_at: row.4.to_rfc3339(),
        updated_at: row.5.to_rfc3339(),
    }))
}

// --- Delete custom role ---

pub async fn delete_role(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let existing = sqlx::query_as::<_, (bool,)>(
        "SELECT is_system FROM custom_roles WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Role not found".into()))?;

    if existing.0 {
        return Err(AppError::BadRequest("Cannot delete system roles".into()));
    }

    sqlx::query("DELETE FROM custom_roles WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    Ok(Json(serde_json::json!({ "deleted": true })))
}

// --- List all valid permissions ---

pub async fn list_permissions(
    _auth_user: AuthUser,
) -> Json<Vec<&'static str>> {
    Json(ALL_PERMISSIONS.to_vec())
}
