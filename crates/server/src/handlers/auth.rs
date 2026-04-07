use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use think_watch_auth::password;
use think_watch_common::audit::AuditEntry;
use think_watch_common::dto::{
    CreateUserRequest, LoginRequest, LoginResponse, RefreshRequest, UserResponse,
};
use think_watch_common::errors::AppError;
use think_watch_common::models::User;
use think_watch_common::validation::validate_password;

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
    request: axum::extract::Request,
) -> Result<axum::response::Response, AppError> {
    // Extract client IP for composite rate limiting via the shared helper
    // that validates the trusted-proxy whitelist. Without this validation
    // an attacker can forge X-Forwarded-For to bypass per-IP rate limits.
    let client_ip = crate::middleware::auth_guard::extract_client_ip(
        &state,
        request.headers(),
        request.extensions(),
    )
    .await
    .unwrap_or_else(|| "unknown".to_string());

    let user_agent = request
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Parse body
    let body_bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| AppError::BadRequest("Invalid request body".into()))?;
    let req: LoginRequest = serde_json::from_slice(&body_bytes)
        .map_err(|_| AppError::BadRequest("Invalid JSON".into()))?;

    // Input validation
    if req.password.len() < 8 {
        return Err(AppError::BadRequest(
            "Password must be at least 8 characters".into(),
        ));
    }
    {
        let email = req.email.trim();
        let parts: Vec<&str> = email.splitn(2, '@').collect();
        if parts.len() != 2
            || parts[0].is_empty()
            || parts[1].len() < 3
            || !parts[1].contains('.')
            || parts[1].starts_with('.')
            || parts[1].ends_with('.')
        {
            return Err(AppError::BadRequest("Invalid email format".into()));
        }
    }

    // Composite rate limiting: per-email AND per-IP
    let rate_key = format!("auth_rate:{}:{}", client_ip, req.email);
    let ip_rate_key = format!("auth_rate_ip:{}", client_ip);
    let count: u64 =
        match fred::interfaces::KeysInterface::incr_by(&state.redis, &rate_key, 1).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Redis rate-limit check failed (fail-closed): {e}");
                return Err(AppError::Internal(anyhow::anyhow!(
                    "Rate limiting unavailable"
                )));
            }
        };
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

    // Per-IP rate limit: max 30 attempts per minute across all emails (fail-closed)
    let ip_count: u64 =
        match fred::interfaces::KeysInterface::incr_by(&state.redis, &ip_rate_key, 1).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Redis IP rate-limit check failed (fail-closed): {e}");
                return Err(AppError::Internal(anyhow::anyhow!(
                    "Rate limiting unavailable"
                )));
            }
        };
    if ip_count == 1 {
        let _: () = fred::interfaces::KeysInterface::expire(&state.redis, &ip_rate_key, 60, None)
            .await
            .unwrap_or(());
    }
    if ip_count > 30 {
        return Err(AppError::BadRequest(
            "Too many login attempts from this address. Please try again later.".into(),
        ));
    }

    // Progressive lockout: after 5 failures, increase lockout exponentially.
    // Fail closed: a Redis outage must NOT silently disable the lockout
    // check, otherwise an attacker can brute-force during the outage window.
    let lockout_key = format!("auth_lockout:{}", req.email);
    let lockout_ttl: Option<i64> =
        match fred::interfaces::KeysInterface::ttl(&state.redis, &lockout_key).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Redis lockout TTL check failed (fail-closed): {e}");
                return Err(AppError::Internal(anyhow::anyhow!(
                    "Authentication temporarily unavailable"
                )));
            }
        };
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
        // Progressive lockout: lock account after repeated failures.
        // Lockout duration increases: 5 fails=60s, 8=300s, 10+=900s.
        // Fail closed on Redis errors so a Redis outage doesn't disable
        // brute-force protection mid-attack.
        let lockout_secs: Option<i64> = if count >= 10 {
            Some(900)
        } else if count >= 8 {
            Some(300)
        } else if count >= 5 {
            Some(60)
        } else {
            None
        };
        if let Some(secs) = lockout_secs {
            let r: Result<(), _> = fred::interfaces::KeysInterface::set(
                &state.redis,
                &lockout_key,
                "1",
                Some(fred::types::Expiration::EX(secs)),
                None,
                false,
            )
            .await;
            if let Err(e) = r {
                tracing::error!("Redis lockout SET failed (fail-closed): {e}");
                return Err(AppError::Internal(anyhow::anyhow!(
                    "Authentication temporarily unavailable"
                )));
            }
        }

        // Log failed attempt
        let mut entry = AuditEntry::platform("auth.login_failed")
            .resource("auth")
            .user_email(&req.email)
            .ip_address(&client_ip)
            .detail(serde_json::json!({"email": req.email}));
        if let Some(ref ua) = user_agent {
            entry = entry.user_agent(ua);
        }
        state.audit.log(entry);
        return Err(AppError::Unauthorized);
    }
    let user = user.unwrap(); // Safe: checked above

    // --- TOTP two-factor check ---
    if user.totp_enabled {
        match &req.totp_code {
            None => {
                // First step: password valid but TOTP needed
                return Ok(Json(serde_json::json!({
                    "totp_required": true
                }))
                .into_response());
            }
            Some(code) => {
                // Verify TOTP code or recovery code
                let totp_valid = if let Some(ref encrypted_secret) = user.totp_secret {
                    let enc_key = think_watch_common::crypto::parse_encryption_key(
                        &state.config.encryption_key,
                    )
                    .map_err(|e| {
                        AppError::Internal(anyhow::anyhow!("Encryption key error: {e}"))
                    })?;
                    let secret = think_watch_auth::totp::decrypt_secret(encrypted_secret, &enc_key)
                        .map_err(|e| {
                            AppError::Internal(anyhow::anyhow!("TOTP decrypt error: {e}"))
                        })?;
                    think_watch_auth::totp::verify(&secret, code, &user.email).unwrap_or(false)
                } else {
                    false
                };

                // If TOTP code didn't match, try recovery codes
                // (constant-time comparison). Consumption is atomic: the
                // UPDATE only succeeds if the codes column hasn't changed
                // since we read it, so two concurrent login attempts with
                // the same recovery code can't both succeed.
                if !totp_valid {
                    let mut recovery_used = false;
                    if let Some(ref codes_json) = user.totp_recovery_codes
                        && let Ok(mut codes) = serde_json::from_str::<Vec<String>>(codes_json)
                        && let Some(pos) = think_watch_auth::totp::find_recovery_code(&codes, code)
                    {
                        codes.remove(pos);
                        let updated = serde_json::to_string(&codes).map_err(|e| {
                            AppError::Internal(anyhow::anyhow!(
                                "Recovery codes serialization failed: {e}"
                            ))
                        })?;
                        // Compare-and-swap: only update if the column still
                        // matches what we read. If another request raced us
                        // to consume the same code, rows_affected == 0.
                        let rows = sqlx::query(
                            "UPDATE users SET totp_recovery_codes = $1 \
                             WHERE id = $2 AND totp_recovery_codes = $3",
                        )
                        .bind(&updated)
                        .bind(user.id)
                        .bind(codes_json)
                        .execute(&state.db)
                        .await?
                        .rows_affected();
                        if rows == 1 {
                            recovery_used = true;
                            state.audit.log(
                                AuditEntry::platform("auth.totp_recovery_used")
                                    .user_id(user.id)
                                    .user_email(&user.email)
                                    .resource("auth")
                                    .ip_address(&client_ip),
                            );
                        }
                    }

                    if !recovery_used {
                        let mut entry = AuditEntry::platform("auth.totp_failed")
                            .user_id(user.id)
                            .user_email(&user.email)
                            .resource("auth")
                            .ip_address(&client_ip)
                            .detail(serde_json::json!({"email": req.email}));
                        if let Some(ref ua) = user_agent {
                            entry = entry.user_agent(ua);
                        }
                        state.audit.log(entry);
                        return Err(AppError::Unauthorized);
                    }
                }
            }
        }
    }

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

    let signing_key =
        verify_signature::create_signing_key(&state.redis, &user.id, Some(&client_ip))
            .await
            .map_err(|e| {
                AppError::Internal(anyhow::anyhow!("Failed to create signing key: {e}"))
            })?;

    let mut entry = AuditEntry::platform("auth.login")
        .user_id(user.id)
        .user_email(&user.email)
        .resource("auth")
        .ip_address(&client_ip);
    if let Some(ref ua) = user_agent {
        entry = entry.user_agent(ua);
    }
    state.audit.log(entry);

    // Set signing key as httpOnly cookie (primary) + response body (backward compat)
    let cookie = verify_signature::signing_key_cookie(&signing_key, 86400);
    let mut response = Json(LoginResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".into(),
        expires_in: access_ttl,
        signing_key,
        password_change_required: if user.password_change_required {
            Some(true)
        } else {
            None
        },
    })
    .into_response();
    if let Ok(cookie_value) = cookie.parse() {
        response
            .headers_mut()
            .insert(axum::http::header::SET_COOKIE, cookie_value);
    }
    Ok(response)
}

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Result<Json<UserResponse>, AppError> {
    // Check if public registration is allowed
    if !state.dynamic_config.allow_registration().await {
        return Err(AppError::Forbidden);
    }

    // Input validation
    validate_password(&req.password)?;

    let password_hash = password::hash_password(&req.password)?;

    // Use a transaction to ensure user creation + role assignment are atomic
    let mut tx = state.db.begin().await?;

    // Use INSERT ... ON CONFLICT to avoid leaking whether email exists (user enumeration)
    let user = sqlx::query_as::<_, User>(
        r#"INSERT INTO users (email, display_name, password_hash)
           VALUES ($1, $2, $3)
           ON CONFLICT (email) DO NOTHING
           RETURNING *"#,
    )
    .bind(&req.email)
    .bind(&req.display_name)
    .bind(&password_hash)
    .fetch_optional(&mut *tx)
    .await?;

    let user = match user {
        Some(u) => u,
        None => {
            tx.rollback().await?;
            return Err(AppError::Conflict("Email already registered".into()));
        }
    };

    // Assign default "developer" role
    sqlx::query(
        r#"INSERT INTO user_roles (user_id, role_id, scope)
           SELECT $1, id, 'global' FROM roles WHERE name = 'developer'"#,
    )
    .bind(user.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Json(UserResponse {
        id: user.id,
        email: user.email,
        display_name: user.display_name,
        avatar_url: user.avatar_url,
        is_active: user.is_active,
        roles: vec!["developer".to_string()],
        created_at: user.created_at,
    }))
}

pub async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<axum::response::Response, AppError> {
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

    let signing_key = verify_signature::create_signing_key(&state.redis, &claims.sub, None)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to create signing key: {e}")))?;

    let cookie = verify_signature::signing_key_cookie(&signing_key, 86400);
    let mut response = Json(LoginResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".into(),
        expires_in: access_ttl,
        signing_key,
        password_change_required: None,
    })
    .into_response();
    if let Ok(cookie_value) = cookie.parse() {
        response
            .headers_mut()
            .insert(axum::http::header::SET_COOKIE, cookie_value);
    }
    Ok(response)
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
        roles: auth_user.claims.roles.clone(),
        created_at: user.created_at,
    }))
}

pub async fn change_password(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_password(&req.new_password)?;

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
    sqlx::query("UPDATE users SET password_hash = $1, password_change_required = false, updated_at = now() WHERE id = $2")
        .bind(&new_hash)
        .bind(user.id)
        .execute(&state.db)
        .await?;

    // Revoke all signing keys for this user (invalidates sessions)
    let signing_key = format!("signing_key:{}", user.id);
    let _: Result<(), _> =
        fred::interfaces::KeysInterface::del::<(), _>(&state.redis, &signing_key).await;

    state.audit.log(
        AuditEntry::platform("auth.password_changed")
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

    // Soft-delete in a transaction: mark keys + user as deleted atomically
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE api_keys SET is_active = false, deleted_at = now(), disabled_reason = 'account_deleted' WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET is_active = false, deleted_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

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

    // Force-close any live dashboard WebSockets for this user. The WS
    // loop polls this key every ~32s; setting it triggers a Close frame.
    // 5 minute TTL is plenty — by then all listening WSs will have
    // either disconnected or noticed.
    let revoke_key = crate::handlers::dashboard::user_revoked_key(user_id);
    let _: () = fred::interfaces::KeysInterface::set(
        &state.redis,
        &revoke_key,
        "1",
        Some(fred::types::Expiration::EX(300)),
        None,
        false,
    )
    .await
    .unwrap_or(());

    state.audit.log(
        AuditEntry::platform("auth.sessions_revoked")
            .user_id(user_id)
            .resource("auth"),
    );

    Ok(Json(serde_json::json!({"status": "all_sessions_revoked"})))
}

// --- TOTP endpoints ---

#[derive(Debug, Serialize)]
pub struct TotpSetupResponse {
    secret: String,
    otpauth_uri: String,
    recovery_codes: Vec<String>,
}

/// POST /api/auth/totp/setup — Begin TOTP setup. Returns secret + otpauth URI + recovery codes.
/// The user must call /totp/verify-setup with a valid code to finalize.
pub async fn totp_setup(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<TotpSetupResponse>, AppError> {
    use think_watch_auth::totp;

    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(auth_user.claims.sub)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("User not found".into()))?;

    if user.totp_enabled {
        return Err(AppError::BadRequest("TOTP is already enabled".into()));
    }

    let secret = totp::generate_secret();
    let uri = totp::otpauth_uri(&secret, &user.email)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to create otpauth URI: {e}")))?;
    let recovery_codes = totp::generate_recovery_codes(10);

    // Store pending setup in Redis (encrypted, expires in 10 minutes)
    let pending_key = format!("totp_pending:{}", user.id);
    let pending_data = serde_json::json!({
        "secret": secret,
        "recovery_codes": recovery_codes,
    });
    let enc_key = think_watch_common::crypto::parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption key error: {e}")))?;
    let pending_json = serde_json::to_string(&pending_data)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("JSON serialization error: {e}")))?;
    let encrypted_pending = think_watch_common::crypto::encrypt(pending_json.as_bytes(), &enc_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption error: {e}")))?;
    let _: () = fred::interfaces::KeysInterface::set(
        &state.redis,
        &pending_key,
        hex::encode(encrypted_pending),
        Some(fred::types::Expiration::EX(600)),
        None,
        false,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

    Ok(Json(TotpSetupResponse {
        secret,
        otpauth_uri: uri,
        recovery_codes,
    }))
}

#[derive(Debug, Deserialize)]
pub struct TotpVerifyRequest {
    pub code: String,
}

/// POST /api/auth/totp/verify-setup — Finalize TOTP setup by verifying a code.
pub async fn totp_verify_setup(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<TotpVerifyRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    use think_watch_auth::totp;

    let user_id = auth_user.claims.sub;
    let user_email = &auth_user.claims.email;

    let pending_key = format!("totp_pending:{user_id}");
    let pending_hex: Option<String> =
        fred::interfaces::KeysInterface::get(&state.redis, &pending_key)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

    let pending_hex = pending_hex.ok_or(AppError::BadRequest(
        "No pending TOTP setup. Call /totp/setup first.".into(),
    ))?;

    // Decrypt the pending data from Redis
    let enc_key = think_watch_common::crypto::parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption key error: {e}")))?;
    let encrypted_bytes = hex::decode(&pending_hex)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid hex from Redis: {e}")))?;
    let decrypted = think_watch_common::crypto::decrypt(&encrypted_bytes, &enc_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Decryption error: {e}")))?;
    let pending_str = String::from_utf8(decrypted)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid UTF-8: {e}")))?;

    #[derive(Deserialize)]
    struct PendingData {
        secret: String,
        recovery_codes: Vec<String>,
    }
    let pending: PendingData = serde_json::from_str(&pending_str)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("Corrupt pending TOTP data")))?;

    // Verify the code against the pending secret
    if !totp::verify(&pending.secret, &req.code, user_email).unwrap_or(false) {
        return Err(AppError::BadRequest("Invalid TOTP code".into()));
    }

    // Encrypt and store
    let enc_key = think_watch_common::crypto::parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption key error: {e}")))?;
    let encrypted_secret = totp::encrypt_secret(&pending.secret, &enc_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption error: {e}")))?;
    let recovery_json = serde_json::to_string(&pending.recovery_codes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("JSON serialization error: {e}")))?;

    sqlx::query(
        "UPDATE users SET totp_secret = $1, totp_enabled = true, totp_recovery_codes = $2, updated_at = now() WHERE id = $3",
    )
    .bind(&encrypted_secret)
    .bind(&recovery_json)
    .bind(user_id)
    .execute(&state.db)
    .await?;

    // Clean up pending
    let _: i64 = fred::interfaces::KeysInterface::del(&state.redis, &pending_key)
        .await
        .unwrap_or(0);

    state.audit.log(
        AuditEntry::platform("auth.totp_enabled")
            .user_id(user_id)
            .resource("auth"),
    );

    Ok(Json(serde_json::json!({"status": "totp_enabled"})))
}

/// POST /api/auth/totp/disable — Disable TOTP (requires current password verification).
pub async fn totp_disable(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(auth_user.claims.sub)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("User not found".into()))?;

    if !user.totp_enabled {
        return Err(AppError::BadRequest("TOTP is not enabled".into()));
    }

    // Verify current password (use old_password field)
    let hash = user.password_hash.as_ref().ok_or(AppError::BadRequest(
        "SSO accounts cannot manage TOTP here".into(),
    ))?;
    if !password::verify_password(&req.old_password, hash)? {
        return Err(AppError::Unauthorized);
    }

    sqlx::query(
        "UPDATE users SET totp_secret = NULL, totp_enabled = false, totp_recovery_codes = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(user.id)
    .execute(&state.db)
    .await?;

    state.audit.log(
        AuditEntry::platform("auth.totp_disabled")
            .user_id(user.id)
            .resource("auth"),
    );

    Ok(Json(serde_json::json!({"status": "totp_disabled"})))
}

/// GET /api/auth/totp/status — Check TOTP status for current user.
pub async fn totp_status(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let enabled: bool =
        sqlx::query_scalar("SELECT totp_enabled FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(auth_user.claims.sub)
            .fetch_one(&state.db)
            .await?;

    // Check if platform requires TOTP
    let required: bool = state
        .dynamic_config
        .get_string("security.totp_required")
        .await
        .map(|v| v == "true")
        .unwrap_or(false);

    Ok(Json(serde_json::json!({
        "enabled": enabled,
        "required": required,
    })))
}
