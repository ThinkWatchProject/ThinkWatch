use axum::Json;
use axum::extract::State;
use serde::Deserialize;

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

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    // Input validation
    if req.password.len() < 8 {
        return Err(AppError::BadRequest(
            "Password must be at least 8 characters".into(),
        ));
    }
    if !req.email.contains('@') || !req.email.contains('.') {
        return Err(AppError::BadRequest("Invalid email format".into()));
    }

    // Rate limiting: per-email, max 10 attempts per minute
    let rate_key = format!("auth_rate:{}", req.email);
    let count: u64 = fred::interfaces::KeysInterface::incr_by(&state.redis, &rate_key, 1)
        .await
        .unwrap_or(1);
    if count == 1 {
        let _: () = fred::interfaces::KeysInterface::expire(&state.redis, &rate_key, 60, None)
            .await
            .unwrap_or(());
    }
    if count > 10 {
        return Err(AppError::BadRequest(
            "Too many login attempts. Please try again later.".into(),
        ));
    }

    // Progressive lockout: after 5 failures, increase lockout exponentially
    let lockout_key = format!("auth_lockout:{}", req.email);
    let lockout_ttl: Option<i64> =
        fred::interfaces::KeysInterface::ttl(&state.redis, &lockout_key).await.unwrap_or(None);
    if lockout_ttl.unwrap_or(-2) > 0 {
        return Err(AppError::BadRequest(
            "Account temporarily locked due to repeated failed attempts. Please try again later."
                .into(),
        ));
    }

    // Constant-time login: always perform Argon2 verify to prevent user enumeration
    let dummy_hash = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    let maybe_user =
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1 AND is_active = true")
            .bind(&req.email)
            .fetch_optional(&state.db)
            .await?;

    let (user, password_hash) = match maybe_user {
        Some(u) => {
            let hash = u
                .password_hash
                .clone()
                .unwrap_or_else(|| dummy_hash.to_string());
            (Some(u), hash)
        }
        None => (None, dummy_hash.to_string()),
    };

    // Always verify (constant time regardless of user existence)
    let password_valid = password::verify_password(&req.password, &password_hash).unwrap_or(false);

    if !password_valid || user.is_none() {
        // Progressive lockout: lock account after repeated failures
        // Lockout duration increases: 5 fails=60s, 8=300s, 10+=900s
        if count >= 10 {
            let _: () = fred::interfaces::KeysInterface::set(
                &state.redis, &lockout_key, "1",
                Some(fred::types::Expiration::EX(900)), None, false,
            ).await.unwrap_or(());
        } else if count >= 8 {
            let _: () = fred::interfaces::KeysInterface::set(
                &state.redis, &lockout_key, "1",
                Some(fred::types::Expiration::EX(300)), None, false,
            ).await.unwrap_or(());
        } else if count >= 5 {
            let _: () = fred::interfaces::KeysInterface::set(
                &state.redis, &lockout_key, "1",
                Some(fred::types::Expiration::EX(60)), None, false,
            ).await.unwrap_or(());
        }

        // Log failed attempt
        state.audit.log(
            AuditEntry::new("auth.login_failed").detail(serde_json::json!({"email": req.email})),
        );
        return Err(AppError::Unauthorized);
    }
    let user = user.unwrap(); // Safe: checked above

    // Clear rate limit and lockout keys on successful login
    let _: i64 = fred::interfaces::KeysInterface::del(&state.redis, &rate_key)
        .await
        .unwrap_or(0);
    let _: i64 = fred::interfaces::KeysInterface::del(&state.redis, &lockout_key)
        .await
        .unwrap_or(0);

    // Fetch user roles
    let roles: Vec<String> = sqlx::query_scalar(
        "SELECT r.name FROM roles r JOIN user_roles ur ON r.id = ur.role_id WHERE ur.user_id = $1",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;

    let access_ttl = state.dynamic_config.jwt_access_ttl_secs().await;
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;

    let access_token =
        state
            .jwt
            .create_access_token_with_ttl(user.id, &user.email, roles.clone(), access_ttl)?;
    let refresh_token =
        state
            .jwt
            .create_refresh_token_with_ttl(user.id, &user.email, roles, refresh_ttl_days)?;

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
        expires_in: access_ttl,
        signing_key,
    }))
}

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
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

    // Check if user already exists
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

    let access_ttl = state.dynamic_config.jwt_access_ttl_secs().await;
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;

    let access_token = state.jwt.create_access_token_with_ttl(
        claims.sub,
        &claims.email,
        claims.roles.clone(),
        access_ttl,
    )?;
    let refresh_token = state.jwt.create_refresh_token_with_ttl(
        claims.sub,
        &claims.email,
        claims.roles,
        refresh_ttl_days,
    )?;

    let signing_key = verify_signature::create_signing_key(&state.redis, &claims.sub)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to create signing key: {e}")))?;

    Ok(Json(LoginResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".into(),
        expires_in: access_ttl,
        signing_key,
    }))
}

pub async fn me(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<UserResponse>, AppError> {
    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
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

pub async fn change_password(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    if req.new_password.len() < 8 {
        return Err(AppError::BadRequest(
            "New password must be at least 8 characters".into(),
        ));
    }

    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(auth_user.claims.sub)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("User not found".into()))?;

    let current_hash = user
        .password_hash
        .as_ref()
        .ok_or(AppError::BadRequest("This account uses SSO login".into()))?;

    if !password::verify_password(&req.old_password, current_hash)? {
        return Err(AppError::Unauthorized);
    }

    let new_hash = password::hash_password(&req.new_password)?;
    sqlx::query("UPDATE users SET password_hash = $1, updated_at = now() WHERE id = $2")
        .bind(&new_hash)
        .bind(user.id)
        .execute(&state.db)
        .await?;

    // Revoke all signing keys for this user (invalidates sessions)
    let signing_key = format!("signing_key:{}", user.id);
    let _: Result<(), _> =
        fred::interfaces::KeysInterface::del::<(), _>(&state.redis, &signing_key).await;

    state.audit.log(
        AuditEntry::new("auth.password_changed")
            .user_id(user.id)
            .resource("auth"),
    );

    Ok(Json(serde_json::json!({"status": "password_changed"})))
}

pub async fn delete_account(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user_id = auth_user.claims.sub;

    // Soft-delete: mark as deleted instead of hard-deleting
    // Records are purged after 30 days by the data retention task
    sqlx::query("UPDATE api_keys SET is_active = false, deleted_at = now(), disabled_reason = 'account_deleted' WHERE user_id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;
    sqlx::query("UPDATE users SET is_active = false, deleted_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;

    // Revoke all sessions
    let _: () =
        fred::interfaces::KeysInterface::del(&state.redis, &format!("signing_key:{user_id}"))
            .await
            .unwrap_or(());

    state
        .audit
        .log(AuditEntry::new("user.account_deleted").user_id(user_id));

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

/// POST /api/auth/revoke-sessions — revoke all sessions for the current user.
pub async fn revoke_sessions(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user_id = auth_user.claims.sub;

    // Delete signing key (invalidates all signed requests)
    let _: () =
        fred::interfaces::KeysInterface::del(&state.redis, &format!("signing_key:{user_id}"))
            .await
            .unwrap_or(());

    state.audit.log(
        AuditEntry::new("auth.sessions_revoked")
            .user_id(user_id)
            .resource("auth"),
    );

    Ok(Json(serde_json::json!({"status": "all_sessions_revoked"})))
}
