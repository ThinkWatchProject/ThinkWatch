use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use agent_bastion_auth::password;
use agent_bastion_common::dto::UserResponse;
use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::User;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// --- User management ---

pub async fn list_users(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<UserResponse>>, AppError> {
    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY created_at DESC")
        .fetch_all(&state.db)
        .await?;

    let responses: Vec<UserResponse> = users
        .into_iter()
        .map(|u| UserResponse {
            id: u.id,
            email: u.email,
            display_name: u.display_name,
            avatar_url: u.avatar_url,
            is_active: u.is_active,
        })
        .collect();

    Ok(Json(responses))
}

#[derive(Debug, Deserialize)]
pub struct CreateUserByAdminRequest {
    pub email: String,
    pub display_name: String,
    pub password: String,
    pub role: Option<String>,
}

pub async fn create_user(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateUserByAdminRequest>,
) -> Result<Json<UserResponse>, AppError> {
    // Input validation
    if req.password.len() < 8 {
        return Err(AppError::BadRequest(
            "Password must be at least 8 characters".into(),
        ));
    }
    if !req.email.contains('@') || !req.email.contains('.') {
        return Err(AppError::BadRequest("Invalid email format".into()));
    }

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

    let password_hash = password::hash_password(&req.password)?;

    let user = sqlx::query_as::<_, User>(
        r#"INSERT INTO users (email, display_name, password_hash)
           VALUES ($1, $2, $3) RETURNING *"#,
    )
    .bind(&req.email)
    .bind(&req.display_name)
    .bind(&password_hash)
    .fetch_one(&state.db)
    .await?;

    sqlx::query(
        r#"INSERT INTO user_roles (user_id, role_id, scope)
           SELECT $1, id, 'global' FROM roles WHERE name = $2"#,
    )
    .bind(user.id)
    .bind(role_name)
    .execute(&state.db)
    .await?;

    Ok(Json(UserResponse {
        id: user.id,
        email: user.email,
        display_name: user.display_name,
        avatar_url: user.avatar_url,
        is_active: user.is_active,
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
        agent_bastion_common::audit::AuditEntry::new("admin.force_logout")
            .user_id(auth_user.claims.sub)
            .resource(format!("user:{user_id}")),
    );

    Ok(Json(
        serde_json::json!({"status": "user_logged_out", "user_id": user_id}),
    ))
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
    pub quickwit_url: Option<String>,
    pub quickwit_index: String,
}

pub async fn get_audit_settings(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Json<AuditConfigResponse> {
    Json(AuditConfigResponse {
        quickwit_url: state.config.quickwit_url.clone(),
        quickwit_index: state.config.quickwit_index.clone(),
    })
}
