use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use agent_bastion_auth::password;
use agent_bastion_common::dto::{PaginatedResponse, PaginationParams, UserResponse};
use agent_bastion_common::dynamic_config::{self, SettingEntry};
use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::User;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// --- User management ---

pub async fn list_users(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<PaginatedResponse<UserResponse>>, AppError> {
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

    // Fetch roles for all users in one query
    let role_rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT ur.user_id, r.name FROM user_roles ur JOIN roles r ON r.id = ur.role_id WHERE ur.user_id = ANY($1)",
    )
    .bind(&user_ids)
    .fetch_all(&state.db)
    .await?;

    let mut roles_map: std::collections::HashMap<uuid::Uuid, Vec<String>> =
        std::collections::HashMap::new();
    for (uid, rname) in role_rows {
        roles_map.entry(uid).or_default().push(rname);
    }

    let responses: Vec<UserResponse> = users
        .into_iter()
        .map(|u| {
            let roles = roles_map.remove(&u.id).unwrap_or_default();
            UserResponse {
                id: u.id,
                email: u.email,
                display_name: u.display_name,
                avatar_url: u.avatar_url,
                is_active: u.is_active,
                roles,
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
    pub role: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateUserByAdminResponse {
    #[serde(flatten)]
    pub user: UserResponse,
    /// Only present when password was auto-generated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_password: Option<String>,
}

pub async fn create_user(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateUserByAdminRequest>,
) -> Result<Json<CreateUserByAdminResponse>, AppError> {
    if !req.email.contains('@') || !req.email.contains('.') {
        return Err(AppError::BadRequest("Invalid email format".into()));
    }

    let (raw_password, force_change) = match &req.password {
        Some(p) => {
            if p.len() < 8 {
                return Err(AppError::BadRequest(
                    "Password must be at least 8 characters".into(),
                ));
            }
            (p.clone(), false)
        }
        None => (password::generate_random_password(), true),
    };

    // Role escalation prevention
    let role_name = req.role.as_deref().unwrap_or("developer");
    let allowed_roles = ["developer", "viewer", "team_manager"];
    if !allowed_roles.contains(&role_name) {
        return Err(AppError::BadRequest(
            "Cannot assign admin or super_admin role via API".into(),
        ));
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

    sqlx::query(
        r#"INSERT INTO user_roles (user_id, role_id, scope)
           SELECT $1, id, 'global' FROM roles WHERE name = $2"#,
    )
    .bind(user.id)
    .bind(role_name)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Json(CreateUserByAdminResponse {
        user: UserResponse {
            id: user.id,
            email: user.email,
            display_name: user.display_name,
            avatar_url: user.avatar_url,
            is_active: user.is_active,
            roles: vec![role_name.to_string()],
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
    pub role: Option<String>,
    pub is_active: Option<bool>,
}

/// PATCH /api/admin/users/{id} — update user display_name, role, or active status.
pub async fn update_user(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<uuid::Uuid>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
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

    // Apply role change
    if let Some(ref role_name) = req.role {
        // Check the target role exists
        let role_id: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT id FROM roles WHERE name = $1")
                .bind(role_name)
                .fetch_optional(&state.db)
                .await?;
        let role_id = role_id.ok_or(AppError::BadRequest(format!("Unknown role: {role_name}")))?;

        // Prevent self role-downgrade from super_admin
        if user_id == auth_user.claims.sub {
            let is_super: bool = auth_user.claims.roles.contains(&"super_admin".to_string());
            if is_super && role_name != "super_admin" {
                return Err(AppError::BadRequest(
                    "Cannot downgrade your own super_admin role".into(),
                ));
            }
        }

        // Replace all existing roles with the new one (atomic)
        let mut tx = state.db.begin().await?;
        sqlx::query("DELETE FROM user_roles WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO user_roles (user_id, role_id, scope) VALUES ($1, $2, 'global')")
            .bind(user_id)
            .bind(role_id)
            .execute(&mut *tx)
            .await?;
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
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Json<SystemInfo> {
    let uptime = chrono::Utc::now() - state.started_at;
    Json(SystemInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime: format_uptime(uptime),
        rust_version: env!("RUSTC_VERSION").to_string(),
        server_host: state.config.server_host.clone(),
        gateway_port: state.config.gateway_port,
        console_port: state.config.console_port,
    })
}

#[derive(Debug, Serialize)]
pub struct OidcConfigResponse {
    pub enabled: bool,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub redirect_url: Option<String>,
}

pub async fn get_oidc_settings(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Json<OidcConfigResponse> {
    Json(OidcConfigResponse {
        enabled: state.config.oidc_enabled(),
        issuer_url: state.config.oidc_issuer_url.clone(),
        client_id: state.config.oidc_client_id.as_ref().map(|id| {
            if id.len() > 8 {
                format!("{}...{}", &id[..4], &id[id.len() - 4..])
            } else {
                "****".to_string()
            }
        }),
        redirect_url: state.config.oidc_redirect_url.clone(),
    })
}

#[derive(Debug, Serialize)]
pub struct AuditConfigResponse {
    pub clickhouse_url: Option<String>,
    pub clickhouse_db: String,
}

pub async fn get_audit_settings(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Json<AuditConfigResponse> {
    Json(AuditConfigResponse {
        clickhouse_url: state.config.clickhouse_url.clone(),
        clickhouse_db: state.config.clickhouse_db.clone(),
    })
}

// --- Dynamic settings CRUD ---

/// GET /api/admin/settings — return all settings grouped by category.
pub async fn get_all_settings(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<HashMap<String, Vec<SettingEntry>>>, AppError> {
    let grouped = state.dynamic_config.get_all_grouped().await;
    Ok(Json(grouped))
}

/// GET /api/admin/settings/{category} — return settings for a specific category.
pub async fn get_settings_by_category(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Path(category): Path<String>,
) -> Result<Json<Vec<SettingEntry>>, AppError> {
    let settings = state.dynamic_config.get_by_category(&category).await;
    Ok(Json(settings))
}

#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    pub settings: HashMap<String, serde_json::Value>,
}

/// PATCH /api/admin/settings — update one or more settings.
pub async fn update_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Validate each setting
    for (key, value) in &req.settings {
        validate_setting(key, value)?;
    }

    state
        .dynamic_config
        .update(&req.settings, Some(auth_user.claims.sub))
        .await
        .map_err(AppError::Internal)?;

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
        | "data.retention_days_audit" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if v < 0 {
                return Err(AppError::BadRequest(format!("{key} must be >= 0")));
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

        // JSON array settings (patterns) — with size limits and regex validation
        "security.content_filter_patterns" => {
            let arr = value
                .as_array()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a JSON array")))?;
            if arr.len() > 500 {
                return Err(AppError::BadRequest(
                    "Content filter patterns: max 500 rules".into(),
                ));
            }
            for (i, item) in arr.iter().enumerate() {
                let pattern = item
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AppError::BadRequest(format!("Pattern {i}: missing 'pattern' string"))
                    })?;
                if pattern.len() > 500 {
                    return Err(AppError::BadRequest(format!(
                        "Pattern {i}: max 500 characters"
                    )));
                }
                let severity = item
                    .get("severity")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AppError::BadRequest(format!("Pattern {i}: missing 'severity' string"))
                    })?;
                if !["critical", "high", "medium", "low"].contains(&severity) {
                    return Err(AppError::BadRequest(format!(
                        "Pattern {i}: severity must be critical/high/medium/low"
                    )));
                }
                if item.get("category").and_then(|v| v.as_str()).is_none() {
                    return Err(AppError::BadRequest(format!(
                        "Pattern {i}: missing 'category' string"
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
        // Valid pattern with all required fields
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "severity": "high", "category": "custom"}])
            )
            .is_ok()
        );
        // Missing severity → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test"}])
            )
            .is_err()
        );
        // Not an array → rejected
        assert!(validate_setting("security.content_filter_patterns", &json!("not array")).is_err());
        // Invalid severity value → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "severity": "invalid", "category": "x"}])
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
