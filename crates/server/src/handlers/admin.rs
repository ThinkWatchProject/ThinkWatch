use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use think_watch_auth::password;
use think_watch_common::dto::{
    PaginatedResponse, PaginationParams, RoleAssignment, RoleAssignmentRequest, UserResponse,
};

/// Parse a scope string into the `(scope_kind, scope_id)` tuple that
/// `rbac_role_assignments` stores. Accepted shapes:
///
///   "global"           → ("global", None)
///   "team:<uuid>"      → ("team",   Some(uuid))
///   "project:<uuid>"   → ("project", Some(uuid))
///
/// Anything else is rejected with a 400.
pub(crate) fn parse_scope(input: &str) -> Result<(String, Option<uuid::Uuid>), AppError> {
    let trimmed = input.trim();
    if trimmed == "global" || trimmed.is_empty() {
        return Ok(("global".into(), None));
    }
    if let Some((kind, rest)) = trimmed.split_once(':') {
        let kind = kind.trim();
        if !matches!(kind, "team" | "project") {
            return Err(AppError::BadRequest(format!(
                "Unknown scope kind '{kind}' (expected 'global', 'team:<uuid>', or 'project:<uuid>')"
            )));
        }
        let id = uuid::Uuid::parse_str(rest.trim())
            .map_err(|_| AppError::BadRequest(format!("Invalid UUID in scope '{input}'")))?;
        return Ok((kind.into(), Some(id)));
    }
    Err(AppError::BadRequest(format!(
        "Invalid scope '{input}' (expected 'global', 'team:<uuid>', or 'project:<uuid>')"
    )))
}
use think_watch_common::dynamic_config::{self, SettingEntry};
use think_watch_common::errors::AppError;
use think_watch_common::models::User;
use think_watch_common::validation::validate_password;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// --- User management ---

pub async fn list_users(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<PaginatedResponse<UserResponse>>, AppError> {
    auth_user.require_permission("users:read")?;

    let per_page = pagination.per_page();
    let offset = pagination.offset();

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE deleted_at IS NULL")
        .fetch_one(&state.db)
        .await?;

    let users = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE deleted_at IS NULL ORDER BY created_at DESC LIMIT $1 OFFSET $2",
    )
    .bind(per_page as i64)
    .bind(offset as i64)
    .fetch_all(&state.db)
    .await?;

    let user_ids: Vec<uuid::Uuid> = users.iter().map(|u| u.id).collect();

    // Single query: every assignment for every user, joined against
    // `rbac_roles` so we can report system + custom uniformly.
    type AssignmentRow = (
        uuid::Uuid,
        uuid::Uuid,
        String,
        bool,
        String,
        Option<uuid::Uuid>,
    );
    let rows: Vec<AssignmentRow> = sqlx::query_as(
        "SELECT ra.user_id, r.id, r.name, r.is_system, ra.scope_kind, ra.scope_id \
           FROM rbac_role_assignments ra \
           JOIN rbac_roles r ON r.id = ra.role_id \
          WHERE ra.user_id = ANY($1) \
          ORDER BY r.is_system DESC, r.name ASC",
    )
    .bind(&user_ids)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut assignments_map: std::collections::HashMap<uuid::Uuid, Vec<RoleAssignment>> =
        std::collections::HashMap::new();
    for (uid, role_id, name, is_system, scope_kind, scope_id) in rows {
        let scope = match (scope_kind.as_str(), scope_id) {
            ("global", _) => "global".to_string(),
            (kind, Some(id)) => format!("{kind}:{id}"),
            (kind, None) => kind.to_string(),
        };
        assignments_map
            .entry(uid)
            .or_default()
            .push(RoleAssignment {
                role_id,
                name,
                is_system,
                scope,
            });
    }

    let responses: Vec<UserResponse> = users
        .into_iter()
        .map(|u| {
            let role_assignments = assignments_map.remove(&u.id).unwrap_or_default();
            UserResponse {
                id: u.id,
                email: u.email,
                display_name: u.display_name,
                avatar_url: u.avatar_url,
                is_active: u.is_active,
                role_assignments,
                created_at: u.created_at,
            }
        })
        .collect();

    Ok(Json(PaginatedResponse {
        data: responses,
        total,
        page: pagination.page.unwrap_or(1).max(1),
        per_page,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateUserByAdminRequest {
    pub email: String,
    pub display_name: String,
    /// If omitted, a random password is generated and the user must change it on first login.
    pub password: Option<String>,
    /// All roles (system + custom) to assign to the new user. If empty
    /// the user has no permissions; callers typically send at least
    /// one entry (e.g. `developer`).
    #[serde(default)]
    pub role_assignments: Vec<RoleAssignmentRequest>,
}

#[derive(Debug, Serialize)]
pub struct CreateUserByAdminResponse {
    #[serde(flatten)]
    pub user: UserResponse,
    /// Only present when password was auto-generated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_password: Option<String>,
}

/// Apply a set of role assignments to a user atomically inside `tx`.
/// Every existing row for `user_id` is deleted first, then the new
/// rows are inserted. Returns the fully-hydrated assignment list for
/// inclusion in the response. The caller is responsible for any
/// escalation checks (super_admin promotion, etc).
async fn write_user_role_assignments(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: uuid::Uuid,
    assignments: &[RoleAssignmentRequest],
    assigned_by: uuid::Uuid,
) -> Result<Vec<RoleAssignment>, AppError> {
    sqlx::query("DELETE FROM rbac_role_assignments WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;

    let mut out: Vec<RoleAssignment> = Vec::with_capacity(assignments.len());
    for a in assignments {
        let raw_scope = a.scope.clone().unwrap_or_else(|| "global".into());
        let (scope_kind, scope_id) = parse_scope(&raw_scope)?;
        // Insert + return role metadata in one round trip so we can
        // build the UserResponse without a second query.
        let row: Option<(String, bool)> = sqlx::query_as(
            "WITH ins AS (\
                INSERT INTO rbac_role_assignments \
                    (user_id, role_id, scope_kind, scope_id, assigned_by) \
                VALUES ($1, $2, $3, $4, $5) \
                ON CONFLICT DO NOTHING \
                RETURNING role_id\
             ) \
             SELECT r.name, r.is_system FROM rbac_roles r \
              WHERE r.id = $2",
        )
        .bind(user_id)
        .bind(a.role_id)
        .bind(&scope_kind)
        .bind(scope_id)
        .bind(assigned_by)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db)
                if db.constraint() == Some("rbac_role_assignments_role_id_fkey") =>
            {
                AppError::BadRequest(format!("Unknown role id: {}", a.role_id))
            }
            _ => AppError::from(e),
        })?;
        let (name, is_system) =
            row.ok_or_else(|| AppError::BadRequest(format!("Unknown role id: {}", a.role_id)))?;
        out.push(RoleAssignment {
            role_id: a.role_id,
            name,
            is_system,
            scope: raw_scope,
        });
    }
    Ok(out)
}

pub async fn create_user(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateUserByAdminRequest>,
) -> Result<Json<CreateUserByAdminResponse>, AppError> {
    auth_user.require_permission("users:create")?;

    if !req.email.contains('@') || !req.email.contains('.') {
        return Err(AppError::BadRequest("Invalid email format".into()));
    }

    let (raw_password, force_change) = match &req.password {
        Some(p) => {
            validate_password(p)?;
            (p.clone(), false)
        }
        None => (password::generate_random_password(), true),
    };

    // Privilege escalation check: only a user who ALREADY has
    // `roles:create` can assign any role here, but granting
    // super_admin/admin additionally requires the caller to hold that
    // role themselves. Without this gate a user with `users:create`
    // could bootstrap themselves a super_admin account.
    let caller_has_super = auth_user.claims.roles.iter().any(|r| r == "super_admin");
    let caller_has_admin = caller_has_super || auth_user.claims.roles.iter().any(|r| r == "admin");
    // Look up requested role names in one query to check privilege.
    let role_ids: Vec<uuid::Uuid> = req.role_assignments.iter().map(|a| a.role_id).collect();
    let requested: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM rbac_roles WHERE id = ANY($1)")
            .bind(&role_ids)
            .fetch_all(&state.db)
            .await?;
    for (name,) in &requested {
        if name == "super_admin" && !caller_has_super {
            return Err(AppError::Forbidden(
                "Only super_admin can assign the super_admin role".into(),
            ));
        }
        if name == "admin" && !caller_has_admin {
            return Err(AppError::Forbidden(
                "Only admin/super_admin can assign the admin role".into(),
            ));
        }
    }

    let exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE email = $1)")
            .bind(&req.email)
            .fetch_one(&state.db)
            .await?;

    if exists {
        return Err(AppError::Conflict("Email already registered".into()));
    }

    let password_hash = password::hash_password(&raw_password)?;

    let mut tx = state.db.begin().await?;

    let user = sqlx::query_as::<_, User>(
        r#"INSERT INTO users (email, display_name, password_hash, password_change_required)
           VALUES ($1, $2, $3, $4) RETURNING *"#,
    )
    .bind(&req.email)
    .bind(&req.display_name)
    .bind(&password_hash)
    .bind(force_change)
    .fetch_one(&mut *tx)
    .await?;

    let role_assignments = write_user_role_assignments(
        &mut tx,
        user.id,
        &req.role_assignments,
        auth_user.claims.sub,
    )
    .await?;

    tx.commit().await?;

    Ok(Json(CreateUserByAdminResponse {
        user: UserResponse {
            id: user.id,
            email: user.email,
            display_name: user.display_name,
            avatar_url: user.avatar_url,
            is_active: user.is_active,
            role_assignments,
            created_at: user.created_at,
        },
        generated_password: if force_change {
            Some(raw_password)
        } else {
            None
        },
    }))
}

/// POST /api/admin/users/{id}/force-logout — admin force-logout a user.
pub async fn force_logout_user(
    auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Path(user_id): axum::extract::Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("sessions:revoke")?;
    // Delete signing key
    let _: () =
        fred::interfaces::KeysInterface::del(&state.redis, &format!("signing_key:{user_id}"))
            .await
            .unwrap_or(());

    state.audit.log(
        auth_user
            .audit("admin.force_logout")
            .resource(format!("user:{user_id}")),
    );

    Ok(Json(
        serde_json::json!({"status": "user_logged_out", "user_id": user_id}),
    ))
}

// --- Update user ---

#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    pub display_name: Option<String>,
    pub is_active: Option<bool>,
    /// When present, replaces **all** role assignments for this user
    /// atomically. Pass an empty array to strip every role. Omit the
    /// field to leave assignments untouched.
    #[serde(default)]
    pub role_assignments: Option<Vec<RoleAssignmentRequest>>,
}

/// PATCH /api/admin/users/{id} — update user display_name, role assignments, or active status.
pub async fn update_user(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<uuid::Uuid>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("users:update")?;

    // Prevent self-deactivation
    if req.is_active == Some(false) && user_id == auth_user.claims.sub {
        return Err(AppError::BadRequest(
            "Cannot deactivate your own account".into(),
        ));
    }

    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    if !exists {
        return Err(AppError::NotFound("User not found".into()));
    }

    // Apply display_name update
    if let Some(ref name) = req.display_name {
        if name.trim().is_empty() {
            return Err(AppError::BadRequest("Display name cannot be empty".into()));
        }
        sqlx::query("UPDATE users SET display_name = $1, updated_at = now() WHERE id = $2")
            .bind(name.trim())
            .bind(user_id)
            .execute(&state.db)
            .await?;
    }

    // Apply is_active toggle
    if let Some(active) = req.is_active {
        sqlx::query("UPDATE users SET is_active = $1, updated_at = now() WHERE id = $2")
            .bind(active)
            .bind(user_id)
            .execute(&state.db)
            .await?;

        // If deactivating, also invalidate signing key
        if !active {
            let _: () = fred::interfaces::KeysInterface::del(
                &state.redis,
                &format!("signing_key:{user_id}"),
            )
            .await
            .unwrap_or(());
        }
    }

    // Apply role assignment replacement. We enforce two escalation
    // guards here:
    //   1. Only a super_admin can grant super_admin.
    //   2. Only admin/super_admin can grant admin.
    //   3. A super_admin cannot remove their own super_admin role
    //      (prevents locking everyone out).
    if let Some(ref assignments) = req.role_assignments {
        let caller_has_super = auth_user.claims.roles.iter().any(|r| r == "super_admin");
        let caller_has_admin =
            caller_has_super || auth_user.claims.roles.iter().any(|r| r == "admin");

        let role_ids: Vec<uuid::Uuid> = assignments.iter().map(|a| a.role_id).collect();
        let requested: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM rbac_roles WHERE id = ANY($1)")
                .bind(&role_ids)
                .fetch_all(&state.db)
                .await?;
        let requested_names: std::collections::HashSet<&String> =
            requested.iter().map(|(n,)| n).collect();

        for name in &requested_names {
            if name.as_str() == "super_admin" && !caller_has_super {
                return Err(AppError::Forbidden(
                    "Only super_admin can assign the super_admin role".into(),
                ));
            }
            if name.as_str() == "admin" && !caller_has_admin {
                return Err(AppError::Forbidden(
                    "Only admin/super_admin can assign the admin role".into(),
                ));
            }
        }

        if user_id == auth_user.claims.sub
            && caller_has_super
            && !requested_names.iter().any(|n| n.as_str() == "super_admin")
        {
            return Err(AppError::BadRequest(
                "Cannot remove your own super_admin role".into(),
            ));
        }

        let mut tx = state.db.begin().await?;
        write_user_role_assignments(&mut tx, user_id, assignments, auth_user.claims.sub).await?;
        tx.commit().await?;
    }

    state.audit.log(
        auth_user
            .audit("admin.update_user")
            .resource(format!("user:{user_id}")),
    );

    Ok(Json(
        serde_json::json!({"status": "updated", "user_id": user_id}),
    ))
}

/// DELETE /api/admin/users/{id} — soft-delete a user.
pub async fn delete_user(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("users:delete")?;
    // Prevent self-deletion
    if user_id == auth_user.claims.sub {
        return Err(AppError::BadRequest(
            "Cannot delete your own account from admin panel".into(),
        ));
    }

    let rows = sqlx::query(
        "UPDATE users SET deleted_at = now(), is_active = false, updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(user_id)
    .execute(&state.db)
    .await?
    .rows_affected();

    if rows == 0 {
        return Err(AppError::NotFound("User not found".into()));
    }

    // Invalidate signing key
    let _: () =
        fred::interfaces::KeysInterface::del(&state.redis, &format!("signing_key:{user_id}"))
            .await
            .unwrap_or(());

    state.audit.log(
        auth_user
            .audit("admin.delete_user")
            .resource(format!("user:{user_id}")),
    );

    Ok(Json(
        serde_json::json!({"status": "deleted", "user_id": user_id}),
    ))
}

/// POST /api/admin/users/{id}/reset-password — admin reset user password.
pub async fn reset_user_password(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("users:update")?;
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;

    if !exists {
        return Err(AppError::NotFound("User not found".into()));
    }

    let new_password = password::generate_random_password();
    let hash = password::hash_password(&new_password)?;

    sqlx::query(
        "UPDATE users SET password_hash = $1, password_change_required = true, updated_at = now() WHERE id = $2",
    )
    .bind(&hash)
    .bind(user_id)
    .execute(&state.db)
    .await?;

    // Invalidate signing key to force re-login
    let _: () =
        fred::interfaces::KeysInterface::del(&state.redis, &format!("signing_key:{user_id}"))
            .await
            .unwrap_or(());

    state.audit.log(
        auth_user
            .audit("admin.reset_password")
            .resource(format!("user:{user_id}")),
    );

    // NOTE: The temporary password is returned here so the admin can securely
    // communicate it to the user. The audit log does NOT record this value
    // (sanitize_detail redacts any field containing "password").
    // The user is forced to change it on first login (password_change_required=true).
    Ok(Json(serde_json::json!({
        "status": "password_reset",
        "temporary_password": new_password,
        "user_id": user_id,
        "password_change_required": true,
    })))
}

// --- System settings ---

#[derive(Debug, Serialize)]
pub struct SystemInfo {
    pub version: String,
    pub uptime: String,
    pub rust_version: String,
    pub server_host: String,
    pub gateway_port: u16,
    pub console_port: u16,
    /// Configured public protocol ("", "http", or "https"). Empty means auto-detect.
    pub public_protocol: String,
    /// Configured public host. Empty means auto-detect from browser.
    pub public_host: String,
    /// Configured public port. 0 means use the gateway listening port.
    pub public_port: i64,
}

fn format_uptime(dur: chrono::TimeDelta) -> String {
    let secs = dur.num_seconds();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

pub async fn get_system_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<SystemInfo>, AppError> {
    auth_user.require_permission("settings:read")?;
    let uptime = chrono::Utc::now() - state.started_at;
    let dc = &state.dynamic_config;
    Ok(Json(SystemInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime: format_uptime(uptime),
        rust_version: env!("RUSTC_VERSION").to_string(),
        server_host: state.config.server_host.clone(),
        gateway_port: state.config.gateway_port,
        console_port: state.config.console_port,
        public_protocol: dc
            .get_string("general.public_protocol")
            .await
            .unwrap_or_default(),
        public_host: dc
            .get_string("general.public_host")
            .await
            .unwrap_or_default(),
        public_port: dc.get_i64("general.public_port").await.unwrap_or(0),
    }))
}

#[derive(Debug, Serialize)]
pub struct OidcConfigResponse {
    pub enabled: bool,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub redirect_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_secret: Option<bool>,
}

pub async fn get_oidc_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<OidcConfigResponse>, AppError> {
    auth_user.require_permission("settings:read")?;
    let dc = &state.dynamic_config;
    let enabled = dc.oidc_enabled().await;
    let issuer_url = dc.oidc_issuer_url().await;
    let client_id = dc.oidc_client_id().await;
    let redirect_url = dc.oidc_redirect_url().await;
    let has_secret = dc
        .oidc_client_secret_encrypted()
        .await
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    Ok(Json(OidcConfigResponse {
        enabled,
        issuer_url,
        client_id: client_id.as_ref().map(|id| {
            if id.len() > 8 {
                format!("{}...{}", &id[..4], &id[id.len() - 4..])
            } else {
                "****".to_string()
            }
        }),
        redirect_url,
        has_secret: Some(has_secret),
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateOidcRequest {
    pub enabled: Option<bool>,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    /// Plaintext client secret — will be encrypted before storage.
    /// Send empty string to keep existing secret unchanged.
    pub client_secret: Option<String>,
    pub redirect_url: Option<String>,
}

/// PATCH /api/admin/settings/oidc — update OIDC/SSO configuration.
/// Client secret is encrypted with AES-256-GCM before storage.
/// Triggers OIDC provider re-discovery if settings change.
pub async fn update_oidc_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateOidcRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("system:configure_oidc")?;
    let dc = &state.dynamic_config;
    let encryption_key =
        think_watch_common::crypto::parse_encryption_key(&state.config.encryption_key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption key error: {e}")))?;
    let user_id = Some(auth_user.claims.sub);

    // Update each field if provided
    if let Some(enabled) = req.enabled {
        dc.upsert(
            "oidc.enabled",
            &serde_json::json!(enabled),
            "oidc",
            Some("SSO enabled"),
            user_id,
        )
        .await
        .map_err(AppError::Internal)?;
    }
    if let Some(ref issuer_url) = req.issuer_url {
        dc.upsert(
            "oidc.issuer_url",
            &serde_json::json!(issuer_url),
            "oidc",
            Some("OIDC issuer URL"),
            user_id,
        )
        .await
        .map_err(AppError::Internal)?;
    }
    if let Some(ref client_id) = req.client_id {
        dc.upsert(
            "oidc.client_id",
            &serde_json::json!(client_id),
            "oidc",
            Some("OIDC client ID"),
            user_id,
        )
        .await
        .map_err(AppError::Internal)?;
    }
    // Encrypt and store client secret (skip if empty = keep existing)
    if let Some(ref client_secret) = req.client_secret
        && !client_secret.is_empty()
    {
        let encrypted =
            think_watch_common::crypto::encrypt(client_secret.as_bytes(), &encryption_key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption failed: {e}")))?;
        let encrypted_hex = hex::encode(encrypted);
        dc.upsert(
            "oidc.client_secret_encrypted",
            &serde_json::json!(encrypted_hex),
            "oidc",
            Some("OIDC client secret (encrypted)"),
            user_id,
        )
        .await
        .map_err(AppError::Internal)?;
    }
    if let Some(ref redirect_url) = req.redirect_url {
        dc.upsert(
            "oidc.redirect_url",
            &serde_json::json!(redirect_url),
            "oidc",
            Some("OIDC redirect URL"),
            user_id,
        )
        .await
        .map_err(AppError::Internal)?;
    }

    // Notify other instances
    think_watch_common::dynamic_config::notify_config_changed(&state.redis).await;

    // Re-discover OIDC provider with updated settings
    let enabled = dc.oidc_enabled().await;
    if enabled {
        let issuer = dc.oidc_issuer_url().await.unwrap_or_default();
        let client_id = dc.oidc_client_id().await.unwrap_or_default();
        let secret_enc = dc.oidc_client_secret_encrypted().await.unwrap_or_default();
        let redirect = dc.oidc_redirect_url().await.unwrap_or_default();

        let client_secret = if secret_enc.is_empty() {
            String::new()
        } else {
            match hex::decode(&secret_enc)
                .map_err(|e| format!("hex decode: {e}"))
                .and_then(|bytes| {
                    think_watch_common::crypto::decrypt(&bytes, &encryption_key)
                        .map_err(|e| format!("decrypt: {e}"))
                })
                .and_then(|plain| String::from_utf8(plain).map_err(|e| format!("utf8: {e}")))
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to decrypt OIDC client secret: {e}");
                    String::new()
                }
            }
        };

        if !issuer.is_empty() && !client_id.is_empty() && !client_secret.is_empty() {
            match think_watch_auth::oidc::OidcManager::discover(
                &issuer,
                &client_id,
                &client_secret,
                &redirect,
            )
            .await
            {
                Ok(mgr) => {
                    tracing::info!("OIDC provider re-discovered after settings update");
                    *state.oidc.write().await = Some(mgr);
                }
                Err(e) => {
                    tracing::error!("OIDC discovery failed after settings update: {e}");
                    return Err(AppError::BadRequest(format!(
                        "OIDC discovery failed: {e}. Settings saved but SSO is not active."
                    )));
                }
            }
        } else {
            *state.oidc.write().await = None;
        }
    } else {
        *state.oidc.write().await = None;
    }

    state
        .audit
        .log(auth_user.audit("settings.oidc_updated").resource("oidc"));

    Ok(Json(
        serde_json::json!({"status": "updated", "sso_active": state.oidc.read().await.is_some()}),
    ))
}

#[derive(Debug, Serialize)]
pub struct AuditConfigResponse {
    pub clickhouse_url: Option<String>,
    pub clickhouse_db: String,
    pub connected: bool,
}

pub async fn get_audit_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<AuditConfigResponse>, AppError> {
    auth_user.require_permission("settings:read")?;
    let connected = if let Some(ref ch) = state.clickhouse {
        ch.query("SELECT 1").fetch_one::<u8>().await.is_ok()
    } else {
        false
    };
    Ok(Json(AuditConfigResponse {
        clickhouse_url: state.config.clickhouse_url.clone(),
        clickhouse_db: state.config.clickhouse_db.clone(),
        connected,
    }))
}

// --- Dynamic settings CRUD ---

/// GET /api/admin/settings — return all settings grouped by category.
pub async fn get_all_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<HashMap<String, Vec<SettingEntry>>>, AppError> {
    auth_user.require_permission("settings:read")?;
    let grouped = state.dynamic_config.get_all_grouped().await;
    Ok(Json(grouped))
}

/// GET /api/admin/settings/{category} — return settings for a specific category.
pub async fn get_settings_by_category(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(category): Path<String>,
) -> Result<Json<Vec<SettingEntry>>, AppError> {
    auth_user.require_permission("settings:read")?;
    let settings = state.dynamic_config.get_by_category(&category).await;
    Ok(Json(settings))
}

#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    pub settings: HashMap<String, serde_json::Value>,
}

/// Map of `data.retention_days_*` setting keys to their ClickHouse table name.
const RETENTION_TABLES: &[(&str, &str)] = &[
    ("data.retention_days_audit", "audit_logs"),
    ("data.retention_days_gateway", "gateway_logs"),
    ("data.retention_days_mcp", "mcp_logs"),
    ("data.retention_days_platform", "platform_logs"),
    ("data.retention_days_access", "access_logs"),
    ("data.retention_days_app", "app_logs"),
];

/// Maximum retention window we will accept, in days. Anything bigger is
/// almost certainly a typo and risks accidentally turning the window off.
const MAX_RETENTION_DAYS: i64 = 36500; // 100 years

/// Whitelist of valid ClickHouse log table identifiers. Used as a
/// belt-and-braces guard so we never inject anything we don't already
/// know about into a `ALTER TABLE ...` statement, even though all call
/// sites today only pass &'static str literals from RETENTION_TABLES.
const VALID_LOG_TABLES: &[&str] = &[
    "audit_logs",
    "gateway_logs",
    "mcp_logs",
    "platform_logs",
    "access_logs",
    "app_logs",
];

/// Issue a single `ALTER TABLE ... MODIFY TTL` against ClickHouse.
/// Validates `table` against an explicit whitelist and clamps `days`
/// into a sane range. Returns `false` if the call was skipped or failed.
async fn apply_single_ttl(ch: &clickhouse::Client, table: &str, days: i64) -> bool {
    if !VALID_LOG_TABLES.contains(&table) {
        tracing::error!(table, "refusing TTL update for unknown table");
        return false;
    }
    if !(1..=MAX_RETENTION_DAYS).contains(&days) {
        tracing::error!(table, days, "refusing TTL update: days out of range");
        return false;
    }
    let sql =
        format!("ALTER TABLE {table} MODIFY TTL toDateTime(created_at) + INTERVAL {days} DAY");
    match ch.query(&sql).execute().await {
        Ok(()) => {
            tracing::info!(table, days, "ClickHouse TTL updated");
            true
        }
        Err(e) => {
            tracing::error!(table, days, "Failed to update ClickHouse TTL: {e}");
            false
        }
    }
}

/// Issue `ALTER TABLE ... MODIFY TTL` for every retention setting included in
/// the update. Failures are logged but not surfaced — the setting is already
/// persisted, and ClickHouse may be temporarily unavailable.
async fn apply_clickhouse_ttls(state: &AppState, settings: &HashMap<String, serde_json::Value>) {
    let Some(ch) = state.clickhouse.as_ref() else {
        return;
    };
    for (key, table) in RETENTION_TABLES {
        let Some(value) = settings.get(*key) else {
            continue;
        };
        let Some(days) = value.as_i64() else { continue };
        if days <= 0 {
            continue;
        }
        apply_single_ttl(ch, table, days).await;
    }
}

/// Apply current persisted retention settings to all ClickHouse log tables.
/// Called once at server startup so settings survive restarts. Silently no-ops
/// if ClickHouse is not configured.
pub async fn reconcile_clickhouse_ttls(state: &AppState) {
    let Some(ch) = state.clickhouse.as_ref() else {
        return;
    };
    let dc = &state.dynamic_config;
    let pairs: [(i64, &str); 6] = [
        (dc.data_retention_days_audit().await, "audit_logs"),
        (dc.data_retention_days_gateway().await, "gateway_logs"),
        (dc.data_retention_days_mcp().await, "mcp_logs"),
        (dc.data_retention_days_platform().await, "platform_logs"),
        (dc.data_retention_days_access().await, "access_logs"),
        (dc.data_retention_days_app().await, "app_logs"),
    ];
    for (days, table) in pairs {
        if days <= 0 {
            continue;
        }
        apply_single_ttl(ch, table, days).await;
    }
}

/// PATCH /api/admin/settings — update one or more settings.
pub async fn update_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("settings:write")?;
    // Validate each setting
    for (key, value) in &req.settings {
        validate_setting(key, value)?;
    }

    state
        .dynamic_config
        .update(&req.settings, Some(auth_user.claims.sub))
        .await
        .map_err(AppError::Internal)?;

    // Hot-reload content filter / PII redactor immediately on this instance
    // (other instances pick it up via the Redis Pub/Sub subscriber).
    if req
        .settings
        .contains_key("security.content_filter_patterns")
    {
        let cf = crate::app::load_content_filter(&state.dynamic_config).await;
        state.content_filter.store(std::sync::Arc::new(cf));
    }
    if req.settings.contains_key("security.pii_redactor_patterns") {
        let pii = crate::app::load_pii_redactor(&state.dynamic_config).await;
        state.pii_redactor.store(std::sync::Arc::new(pii));
    }

    // Apply ClickHouse TTL changes for any retention setting that was updated.
    // ClickHouse runs the cleanup asynchronously in its merge worker, so this
    // returns immediately.
    apply_clickhouse_ttls(&state, &req.settings).await;

    // Notify other instances via Redis Pub/Sub
    dynamic_config::notify_config_changed(&state.redis).await;

    state.audit.log(
        auth_user
            .audit("settings.update")
            .resource("system_settings")
            .detail(serde_json::json!({
                "keys": req.settings.keys().collect::<Vec<_>>(),
            })),
    );

    Ok(Json(
        serde_json::json!({"status": "updated", "count": req.settings.len()}),
    ))
}

// ---------------------------------------------------------------------------
// Content filter — test sandbox & presets
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ContentFilterTestRequest {
    /// User text to test against the supplied rules.
    pub text: String,
    /// Rules to test (the unsaved rules currently in the UI).
    pub rules: Vec<think_watch_gateway::content_filter::DenyRuleConfig>,
}

#[derive(Debug, Serialize)]
pub struct ContentFilterTestMatch {
    pub name: String,
    pub pattern: String,
    pub match_type: String,
    pub action: String,
    pub matched_snippet: String,
}

#[derive(Debug, Serialize)]
pub struct ContentFilterTestResponse {
    pub matches: Vec<ContentFilterTestMatch>,
}

/// POST /api/admin/settings/content-filter/test — try the supplied rules
/// against a sample of user text and return every rule that fires.
pub async fn test_content_filter(
    auth_user: AuthUser,
    Json(req): Json<ContentFilterTestRequest>,
) -> Result<Json<ContentFilterTestResponse>, AppError> {
    auth_user.require_permission("content_filter:read")?;
    use think_watch_gateway::content_filter::ContentFilter;
    let filter = ContentFilter::from_config(&req.rules);
    let matches = filter
        .check_text_all(&req.text)
        .into_iter()
        .map(|m| ContentFilterTestMatch {
            name: m.name,
            pattern: m.pattern,
            match_type: m.match_type.to_string(),
            action: m.action.to_string(),
            matched_snippet: m.matched_snippet,
        })
        .collect();
    Ok(Json(ContentFilterTestResponse { matches }))
}

#[derive(Debug, Serialize)]
pub struct ContentFilterPreset {
    pub id: String,
    pub rules: Vec<think_watch_gateway::content_filter::DenyRuleConfig>,
}

/// GET /api/admin/settings/content-filter/presets — return built-in rule groups
/// (basic / strict / chinese). UI labels are localized on the frontend.
pub async fn list_content_filter_presets(
    auth_user: AuthUser,
) -> Result<Json<Vec<ContentFilterPreset>>, AppError> {
    auth_user.require_permission("content_filter:read")?;
    let groups = think_watch_gateway::content_filter::presets()
        .into_iter()
        .map(|g| ContentFilterPreset {
            id: g.id.to_string(),
            rules: g.rules,
        })
        .collect();
    Ok(Json(groups))
}

// ---------------------------------------------------------------------------
// PII redactor — test sandbox
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PiiRedactorTestRequest {
    pub text: String,
    pub patterns: Vec<think_watch_gateway::pii_redactor::PiiPatternConfig>,
}

#[derive(Debug, Serialize)]
pub struct PiiRedactorTestMatch {
    pub name: String,
    pub original: String,
    pub placeholder: String,
}

#[derive(Debug, Serialize)]
pub struct PiiRedactorTestResponse {
    pub redacted_text: String,
    pub matches: Vec<PiiRedactorTestMatch>,
}

/// POST /api/admin/settings/pii-redactor/test — apply the supplied PII patterns
/// to a text sample and return the redacted version with the substitution map.
pub async fn test_pii_redactor(
    auth_user: AuthUser,
    Json(req): Json<PiiRedactorTestRequest>,
) -> Result<Json<PiiRedactorTestResponse>, AppError> {
    auth_user.require_permission("pii_redactor:read")?;
    use think_watch_gateway::pii_redactor::PiiRedactor;
    use think_watch_gateway::providers::traits::ChatMessage;

    let redactor = PiiRedactor::from_config(&req.patterns);
    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: serde_json::Value::String(req.text.clone()),
    }];
    let (redacted, ctx) = redactor.redact_messages(&messages);

    let redacted_text = redacted
        .first()
        .and_then(|m| m.content.as_str())
        .unwrap_or("")
        .to_string();

    let matches = ctx
        .replacements
        .into_iter()
        .map(|(placeholder, original)| {
            // Extract pattern name from placeholder format "{{NAME_salt_n}}"
            let name = placeholder
                .trim_start_matches("{{")
                .trim_end_matches("}}")
                .split('_')
                .next()
                .unwrap_or("")
                .to_string();
            PiiRedactorTestMatch {
                name,
                original,
                placeholder,
            }
        })
        .collect();

    Ok(Json(PiiRedactorTestResponse {
        redacted_text,
        matches,
    }))
}

/// Validate a setting value based on its key.
fn validate_setting(key: &str, value: &serde_json::Value) -> Result<(), AppError> {
    match key {
        // Integer settings that must be > 0
        "auth.jwt_access_ttl_secs"
        | "auth.jwt_refresh_ttl_days"
        | "gateway.cache_ttl_secs"
        | "gateway.request_timeout_secs"
        | "gateway.body_limit_bytes"
        | "console.request_timeout_secs"
        | "console.body_limit_bytes"
        | "security.signature_nonce_ttl_secs"
        | "security.signature_drift_secs"
        | "audit.batch_size"
        | "audit.flush_interval_secs"
        | "audit.channel_capacity"
        | "api_keys.rotation_grace_period_hours" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if v <= 0 {
                return Err(AppError::BadRequest(format!("{key} must be > 0")));
            }
        }

        // Integer settings that can be 0 (0 = disabled)
        "api_keys.default_expiry_days"
        | "api_keys.inactivity_timeout_days"
        | "api_keys.rotation_period_days"
        | "data.retention_days_usage"
        | "data.retention_days_audit"
        | "data.retention_days_gateway"
        | "data.retention_days_mcp"
        | "data.retention_days_platform"
        | "data.retention_days_access"
        | "data.retention_days_app" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(0..=MAX_RETENTION_DAYS).contains(&v) {
                return Err(AppError::BadRequest(format!(
                    "{key} must be between 0 and {MAX_RETENTION_DAYS}"
                )));
            }
        }

        // Budget thresholds: array of floats in 0.0..1.0
        "budget.alert_thresholds" => {
            let arr = value.as_array().ok_or_else(|| {
                AppError::BadRequest("budget.alert_thresholds must be an array".into())
            })?;
            for item in arr {
                let v = item.as_f64().ok_or_else(|| {
                    AppError::BadRequest("Each threshold must be a number".into())
                })?;
                if !(0.0..=1.0).contains(&v) {
                    return Err(AppError::BadRequest(
                        "Each threshold must be between 0.0 and 1.0".into(),
                    ));
                }
            }
        }

        // Budget webhook URL: null or string
        "budget.webhook_url" => {
            if !value.is_null() && !value.is_string() {
                return Err(AppError::BadRequest(
                    "budget.webhook_url must be a string or null".into(),
                ));
            }
        }

        // Boolean settings
        "setup.initialized" => {
            // Only allow setting to true — prevent resetting initialization
            let v = value
                .as_bool()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a boolean")))?;
            if !v {
                return Err(AppError::BadRequest(
                    "Cannot reset setup.initialized to false".into(),
                ));
            }
        }

        "auth.allow_registration" => {
            if !value.is_boolean() {
                return Err(AppError::BadRequest(format!("{key} must be a boolean")));
            }
        }

        // Client IP resolution
        "security.client_ip_source" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if !["connection", "xff", "x-real-ip"].contains(&s) {
                return Err(AppError::BadRequest(
                    "client_ip_source must be \"connection\", \"xff\", or \"x-real-ip\"".into(),
                ));
            }
        }
        "security.client_ip_xff_position" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if !["left", "right"].contains(&s) {
                return Err(AppError::BadRequest(
                    "client_ip_xff_position must be \"left\" or \"right\"".into(),
                ));
            }
        }
        "security.client_ip_xff_depth" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(1..=20).contains(&v) {
                return Err(AppError::BadRequest(
                    "client_ip_xff_depth must be between 1 and 20".into(),
                ));
            }
        }

        // General — public gateway URL components
        "general.public_protocol" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if !s.is_empty() && s != "http" && s != "https" {
                return Err(AppError::BadRequest(
                    "public_protocol must be \"http\", \"https\", or empty".into(),
                ));
            }
        }
        "general.public_host" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if s.len() > 253 {
                return Err(AppError::BadRequest("public_host too long".into()));
            }
            if s.contains("://") || s.contains('/') {
                return Err(AppError::BadRequest(
                    "public_host must be a hostname only (no scheme or path)".into(),
                ));
            }
        }
        "general.public_port" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(0..=65535).contains(&v) {
                return Err(AppError::BadRequest(
                    "public_port must be between 0 and 65535".into(),
                ));
            }
        }

        // String settings
        "setup.site_name" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if s.is_empty() || s.len() > 100 {
                return Err(AppError::BadRequest(
                    "Site name must be 1-100 characters".into(),
                ));
            }
        }

        // Content filter rules — accept both new (action/match_type/name)
        // and legacy (severity/category) field names for backward compatibility.
        "security.content_filter_patterns" => {
            let arr = value
                .as_array()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a JSON array")))?;
            if arr.len() > 500 {
                return Err(AppError::BadRequest(
                    "Content filter rules: max 500 rules".into(),
                ));
            }
            for (i, item) in arr.iter().enumerate() {
                let pattern = item
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AppError::BadRequest(format!("Rule {i}: missing 'pattern' string"))
                    })?;
                if pattern.len() > 500 {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: pattern max 500 characters"
                    )));
                }
                let match_type =
                    item.get("match_type")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            AppError::BadRequest(format!("Rule {i}: missing 'match_type' field"))
                        })?;
                if !["contains", "regex"].contains(&match_type) {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: match_type must be 'contains' or 'regex'"
                    )));
                }
                if match_type == "regex" && regex::Regex::new(pattern).is_err() {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: invalid regex pattern"
                    )));
                }
                let action = item.get("action").and_then(|v| v.as_str()).ok_or_else(|| {
                    AppError::BadRequest(format!("Rule {i}: missing 'action' field"))
                })?;
                if !["block", "warn", "log"].contains(&action) {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: action must be 'block', 'warn', or 'log'"
                    )));
                }
                if item.get("name").and_then(|v| v.as_str()).is_none() {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: missing 'name' field"
                    )));
                }
            }
        }

        "security.pii_redactor_patterns" => {
            let arr = value
                .as_array()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a JSON array")))?;
            if arr.len() > 100 {
                return Err(AppError::BadRequest(
                    "PII redactor patterns: max 100 rules".into(),
                ));
            }
            for (i, item) in arr.iter().enumerate() {
                let regex_str = item.get("regex").and_then(|v| v.as_str()).ok_or_else(|| {
                    AppError::BadRequest(format!("PII pattern {i}: missing 'regex' string"))
                })?;
                if regex_str.len() > 1000 {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: regex max 1000 characters"
                    )));
                }
                // Validate regex compiles (prevents ReDoS storage of invalid patterns)
                if regex::Regex::new(regex_str).is_err() {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: invalid regex syntax"
                    )));
                }
                if item
                    .get("placeholder_prefix")
                    .and_then(|v| v.as_str())
                    .is_none()
                {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: missing 'placeholder_prefix'"
                    )));
                }
                if item.get("name").and_then(|v| v.as_str()).is_none() {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: missing 'name'"
                    )));
                }
            }
        }

        _ => {
            return Err(AppError::BadRequest(format!("Unknown setting: {key}")));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_positive_integer_settings() {
        assert!(validate_setting("auth.jwt_access_ttl_secs", &json!(900)).is_ok());
        assert!(validate_setting("auth.jwt_access_ttl_secs", &json!(0)).is_err());
        assert!(validate_setting("auth.jwt_access_ttl_secs", &json!(-1)).is_err());
    }

    #[test]
    fn validates_zero_allowed_settings() {
        assert!(validate_setting("api_keys.default_expiry_days", &json!(0)).is_ok());
        assert!(validate_setting("api_keys.default_expiry_days", &json!(30)).is_ok());
        assert!(validate_setting("api_keys.default_expiry_days", &json!(-1)).is_err());
    }

    #[test]
    fn validates_budget_thresholds() {
        assert!(validate_setting("budget.alert_thresholds", &json!([0.5, 0.8])).is_ok());
        assert!(validate_setting("budget.alert_thresholds", &json!([1.5])).is_err());
        assert!(validate_setting("budget.alert_thresholds", &json!("not array")).is_err());
    }

    #[test]
    fn validates_webhook_url() {
        assert!(validate_setting("budget.webhook_url", &json!(null)).is_ok());
        assert!(validate_setting("budget.webhook_url", &json!("https://example.com/hook")).is_ok());
        assert!(validate_setting("budget.webhook_url", &json!(42)).is_err());
    }

    #[test]
    fn validates_site_name() {
        assert!(validate_setting("setup.site_name", &json!("My Site")).is_ok());
        assert!(validate_setting("setup.site_name", &json!("")).is_err());
        let long_name = "x".repeat(101);
        assert!(validate_setting("setup.site_name", &json!(long_name)).is_err());
    }

    #[test]
    fn validates_content_filter_patterns() {
        // Empty array is valid
        assert!(validate_setting("security.content_filter_patterns", &json!([])).is_ok());
        // Valid rule
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "action": "block", "name": "Test", "match_type": "contains"}])
            )
            .is_ok()
        );
        // Regex match_type with valid pattern
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "\\d{4}", "action": "warn", "name": "Test", "match_type": "regex"}])
            )
            .is_ok()
        );
        // Regex match_type with invalid regex → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "[invalid((", "action": "block", "name": "T", "match_type": "regex"}])
            )
            .is_err()
        );
        // Missing match_type → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "action": "block", "name": "T"}])
            )
            .is_err()
        );
        // Missing action → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "name": "T", "match_type": "contains"}])
            )
            .is_err()
        );
        // Missing name → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "action": "block", "match_type": "contains"}])
            )
            .is_err()
        );
        // Not an array → rejected
        assert!(validate_setting("security.content_filter_patterns", &json!("not array")).is_err());
        // Invalid action value → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "action": "invalid", "name": "x", "match_type": "contains"}])
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_settings() {
        assert!(validate_setting("unknown.key", &json!("anything")).is_err());
    }

    #[test]
    fn validates_boolean_settings() {
        assert!(validate_setting("setup.initialized", &json!(true)).is_ok());
        assert!(validate_setting("setup.initialized", &json!(false)).is_err());
        assert!(validate_setting("setup.initialized", &json!("yes")).is_err());
    }
}
