use axum::extract::State;
use axum::Json;

use agent_bastion_auth::password;
use agent_bastion_common::audit::AuditEntry;
use agent_bastion_common::dto::{
    CreateUserRequest, LoginRequest, LoginResponse, RefreshRequest, UserResponse,
};
use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::User;

use crate::middleware::verify_signature;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1 AND is_active = true")
        .bind(&req.email)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::Unauthorized)?;

    let password_hash = user.password_hash.as_ref().ok_or(AppError::BadRequest(
        "This account uses SSO login".into(),
    ))?;

    if !password::verify_password(&req.password, password_hash)? {
        return Err(AppError::Unauthorized);
    }

    // Fetch user roles
    let roles: Vec<String> =
        sqlx::query_scalar("SELECT r.name FROM roles r JOIN user_roles ur ON r.id = ur.role_id WHERE ur.user_id = $1")
            .bind(user.id)
            .fetch_all(&state.db)
            .await?;

    let access_token = state
        .jwt
        .create_access_token(user.id, &user.email, roles.clone())?;
    let refresh_token = state
        .jwt
        .create_refresh_token(user.id, &user.email, roles)?;

    let signing_key = verify_signature::create_signing_key(&state.redis, &user.id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to create signing key: {e}")))?;

    state.audit.log(
        AuditEntry::new("auth.login")
            .user_id(user.id)
            .resource("auth"),
    );

    Ok(Json(LoginResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".into(),
        expires_in: 900,
        signing_key,
    }))
}

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Result<Json<UserResponse>, AppError> {
    // Check if user already exists
    let exists = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM users WHERE email = $1)")
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

    // Assign default "developer" role
    sqlx::query(
        r#"INSERT INTO user_roles (user_id, role_id, scope)
           SELECT $1, id, 'global' FROM roles WHERE name = 'developer'"#,
    )
    .bind(user.id)
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

pub async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    let claims = state
        .jwt
        .verify_token(&req.refresh_token)
        .map_err(|_| AppError::Unauthorized)?;

    if claims.token_type != "refresh" {
        return Err(AppError::BadRequest("Invalid token type".into()));
    }

    let access_token = state
        .jwt
        .create_access_token(claims.sub, &claims.email, claims.roles.clone())?;
    let refresh_token = state
        .jwt
        .create_refresh_token(claims.sub, &claims.email, claims.roles)?;

    let signing_key = verify_signature::create_signing_key(&state.redis, &claims.sub)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to create signing key: {e}")))?;

    Ok(Json(LoginResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".into(),
        expires_in: 900,
        signing_key,
    }))
}

pub async fn me(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<UserResponse>, AppError> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(auth_user.claims.sub)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("User not found".into()))?;

    Ok(Json(UserResponse {
        id: user.id,
        email: user.email,
        display_name: user.display_name,
        avatar_url: user.avatar_url,
        is_active: user.is_active,
    }))
}
