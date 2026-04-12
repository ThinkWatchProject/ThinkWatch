use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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

/// Parse a JSON request body from a raw `axum::extract::Request`,
/// enforcing a maximum byte limit. Shared by login, register, and
/// setup_initialize which all need to extract headers (e.g. client
/// IP, user-agent) *before* consuming the body.
pub(crate) async fn parse_json_body<T: serde::de::DeserializeOwned>(
    request: axum::extract::Request,
    max_bytes: usize,
) -> Result<T, AppError> {
    let body = axum::body::to_bytes(request.into_body(), max_bytes)
        .await
        .map_err(|_| AppError::BadRequest("Invalid request body".into()))?;
    serde_json::from_slice(&body).map_err(|e| AppError::BadRequest(e.to_string()))
}

/// Intermediate result from [`issue_auth_session`] containing every
/// piece of data needed to build the final HTTP response: the JWT
/// cookies (pre-formatted as `Set-Cookie` header values) and the
/// fields the JSON body exposes to the frontend.
pub(crate) struct AuthSession {
    pub signing_key: String,
    pub permissions: Vec<String>,
    pub denied_permissions: Vec<String>,
    pub roles: Vec<String>,
    pub access_ttl: i64,
    /// Pre-formatted Set-Cookie header values (signing, access, refresh).
    cookie_headers: [String; 3],
}

impl AuthSession {
    /// Append the three auth cookies to an existing response.
    pub fn set_cookies(&self, response: &mut axum::response::Response) {
        let headers = response.headers_mut();
        for cookie_str in &self.cookie_headers {
            if let Ok(v) = cookie_str.parse() {
                headers.append(axum::http::header::SET_COOKIE, v);
            }
        }
    }

    /// Build a `LoginResponse`-shaped response with cookies attached.
    pub fn into_login_response(self, password_change_required: bool) -> axum::response::Response {
        let mut response = Json(LoginResponse {
            token_type: "Bearer".into(),
            expires_in: self.access_ttl,
            signing_key: self.signing_key,
            permissions: self.permissions,
            denied_permissions: self.denied_permissions,
            roles: self.roles,
            password_change_required: if password_change_required {
                Some(true)
            } else {
                None
            },
        })
        .into_response();
        let headers = response.headers_mut();
        for cookie_str in &self.cookie_headers {
            if let Ok(v) = cookie_str.parse() {
                headers.append(axum::http::header::SET_COOKIE, v);
            }
        }
        response
    }
}

/// Issue a full auth session: load RBAC, create JWT pair + signing key,
/// and prepare cookie headers. Shared by login, register, refresh, and
/// setup_initialize.
pub(crate) async fn issue_auth_session(
    state: &AppState,
    user_id: uuid::Uuid,
    email: &str,
    client_ip: Option<&str>,
) -> Result<AuthSession, AppError> {
    let roles = think_watch_auth::rbac::load_user_role_names(&state.db, user_id).await?;
    let permissions = think_watch_auth::rbac::compute_user_permissions(&state.db, user_id).await?;
    let denied_permissions =
        think_watch_auth::rbac::compute_denied_permissions(&state.db, user_id, &permissions)
            .await?;
    let role_assignments =
        think_watch_auth::rbac::compute_user_role_assignments(&state.db, user_id).await?;

    let access_ttl = state.dynamic_config.jwt_access_ttl_secs().await;
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;

    let access_token = state.jwt.create_access_token_with_ttl(
        user_id,
        email,
        roles.clone(),
        permissions.clone(),
        denied_permissions.clone(),
        role_assignments.clone(),
        access_ttl,
    )?;
    let refresh_token = state.jwt.create_refresh_token_with_ttl(
        user_id,
        email,
        roles.clone(),
        permissions.clone(),
        denied_permissions.clone(),
        role_assignments,
        refresh_ttl_days,
    )?;

    let signing_key = verify_signature::create_signing_key(&state.redis, &user_id, client_ip)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to create signing key: {e}")))?;

    let signing_cookie = verify_signature::signing_key_cookie(&signing_key, 86400);
    let access_cookie = verify_signature::access_token_cookie(&access_token, access_ttl);
    let refresh_cookie =
        verify_signature::refresh_token_cookie(&refresh_token, refresh_ttl_days * 86400);

    Ok(AuthSession {
        signing_key,
        permissions,
        denied_permissions,
        roles,
        access_ttl,
        cookie_headers: [signing_cookie, access_cookie, refresh_cookie],
    })
}

/// Store the password-change epoch in Redis so the refresh handler
/// rejects refresh tokens issued before this moment. Used by
/// `change_password` and admin `force_logout_user`.
pub(crate) async fn invalidate_refresh_tokens(
    redis: &fred::clients::Client,
    user_id: uuid::Uuid,
    refresh_ttl_days: i64,
) {
    let _: Result<(), _> = fred::interfaces::KeysInterface::set(
        redis,
        &format!("pw_epoch:{user_id}"),
        &chrono::Utc::now().timestamp().to_string(),
        Some(fred::types::Expiration::EX(refresh_ttl_days * 86400)),
        None,
        false,
    )
    .await;
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

#[utoipa::path(
    post,
    path = "/api/auth/login",
    tag = "Auth",
    request_body(
        content_type = "application/json",
        description = "Email, password, and optional TOTP code",
    ),
    responses(
        (status = 200, description = "Login successful — tokens set as httpOnly cookies; body carries signing_key, permissions, roles"),
        (status = 200, description = "TOTP verification required — body contains {\"totp_required\": true}"),
        (status = 400, description = "Invalid credentials or rate-limited"),
        (status = 401, description = "Unauthorized"),
    ),
    security(()),
)]
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
    let req: LoginRequest = parse_json_body(request, 1024 * 1024).await?;

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

    // Soft-deleted users must NOT be able to log in. Every other user
    // lookup in this file already filters `deleted_at IS NULL`; the
    // login path was the lone exception, leaving a 30-day window after
    // soft-delete where the credential still worked.
    let maybe_user = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE email = $1 AND is_active = true AND deleted_at IS NULL",
    )
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

    let mut entry = AuditEntry::platform("auth.login")
        .user_id(user.id)
        .user_email(&user.email)
        .resource("auth")
        .ip_address(&client_ip);
    if let Some(ref ua) = user_agent {
        entry = entry.user_agent(ua);
    }
    state.audit.log(entry);

    let session = issue_auth_session(&state, user.id, &user.email, Some(&client_ip)).await?;
    Ok(session.into_login_response(user.password_change_required))
}

#[utoipa::path(
    post,
    path = "/api/auth/register",
    tag = "Auth",
    request_body(
        content_type = "application/json",
        description = "New user credentials (only when public registration is enabled)",
    ),
    responses(
        (status = 200, description = "User registered and automatically logged in — tokens set as httpOnly cookies"),
        (status = 400, description = "Invalid input"),
        (status = 403, description = "Public registration is disabled"),
        (status = 409, description = "Email already registered"),
    ),
    security(()),
)]
pub async fn register(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, AppError> {
    let client_ip = crate::middleware::auth_guard::extract_client_ip(
        &state,
        request.headers(),
        request.extensions(),
    )
    .await
    .unwrap_or_else(|| "unknown".into());

    let req: CreateUserRequest = parse_json_body(request, 1024 * 1024).await?;

    // Check if public registration is allowed
    if !state.dynamic_config.allow_registration().await {
        return Err(AppError::Forbidden(
            "Public registration is disabled".into(),
        ));
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
            // Return a generic 200 so attackers cannot enumerate which
            // emails are registered. The response carries no signing_key
            // or cookies — the frontend sees an empty signing_key and
            // falls through to the login page.
            tx.rollback().await?;
            return Ok(Json(serde_json::json!({
                "token_type": "Bearer",
                "expires_in": 0,
                "signing_key": "",
                "permissions": [],
                "roles": [],
            }))
            .into_response());
        }
    };

    // Assign default role (configurable via settings; empty = no role)
    if let Some(role_name) = state.dynamic_config.default_role().await {
        sqlx::query(
            r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
               SELECT $1, id, 'global', $1 FROM rbac_roles WHERE name = $2"#,
        )
        .bind(user.id)
        .bind(&role_name)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    state.audit.log(
        AuditEntry::platform("auth.register")
            .user_id(user.id)
            .user_email(&user.email)
            .resource("auth")
            .ip_address(&client_ip),
    );

    // Auto-login: issue tokens + cookies so the user is immediately
    // authenticated — same flow as the login handler.
    let session = issue_auth_session(&state, user.id, &user.email, Some(&client_ip)).await?;
    Ok(session.into_login_response(false))
}

#[utoipa::path(
    post,
    path = "/api/auth/refresh",
    tag = "Auth",
    request_body(
        content_type = "application/json",
        description = "Optional body with refresh_token field; browser flow uses httpOnly cookie instead",
    ),
    responses(
        (status = 200, description = "New access + refresh tokens issued as httpOnly cookies"),
        (status = 401, description = "Invalid or expired refresh token"),
    ),
    security(()),
)]
pub async fn refresh(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, AppError> {
    // Capture client IP before consuming the request — must match
    // what login bound so subsequent signed requests pass IP check.
    let client_ip = crate::middleware::auth_guard::extract_client_ip(
        &state,
        request.headers(),
        request.extensions(),
    )
    .await;

    // Resolve the refresh token, preferring the httpOnly cookie set
    // at login time and falling back to a body field for non-browser
    // clients. The cookie path is the standard browser flow now —
    // the body field is just a courtesy for curl/CI scripts.
    let cookie_token =
        crate::middleware::verify_signature::extract_cookie(&request, "refresh_token");
    let presented_token = if let Some(t) = cookie_token {
        t
    } else {
        // Read body manually so we can reuse the request later if needed.
        let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
            .await
            .map_err(|_| AppError::BadRequest("Failed to read body".into()))?;
        let req: RefreshRequest = if bytes.is_empty() {
            RefreshRequest::default()
        } else {
            serde_json::from_slice(&bytes)
                .map_err(|e| AppError::BadRequest(format!("Invalid refresh body: {e}")))?
        };
        req.refresh_token.ok_or(AppError::Unauthorized)?
    };

    let claims = state
        .jwt
        .verify_token(&presented_token)
        .map_err(|_| AppError::Unauthorized)?;

    if claims.token_type != "refresh" {
        return Err(AppError::BadRequest("Invalid token type".into()));
    }

    // Refresh-token rotation. Without this, a stolen refresh token
    // could be used indefinitely (until natural expiry — 7 days)
    // even after the user changed their password or had a role
    // revoked. See the comment in wave 3 commit for the full
    // model: hash → blacklist with TTL = remaining lifetime →
    // reject on replay.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(presented_token.as_bytes());
    let token_hash = hex::encode(hasher.finalize());
    let blacklist_key = format!("refresh_blacklist:{token_hash}");

    use fred::interfaces::KeysInterface;
    let already_used: Option<String> = state.redis.get(&blacklist_key).await.ok().flatten();
    if already_used.is_some() {
        tracing::warn!(
            user_id = %claims.sub,
            "refresh token replay detected — token already in blacklist"
        );
        metrics::counter!("auth_refresh_replay_total").increment(1);
        return Err(AppError::Unauthorized);
    }

    // Reject refresh tokens issued before the last password change.
    // pw_epoch:{user_id} is set by change_password and has a TTL
    // equal to the refresh token lifetime so it auto-expires.
    let epoch_key = format!("pw_epoch:{}", claims.sub);
    let pw_epoch: Option<String> = state.redis.get(&epoch_key).await.ok().flatten();
    if pw_epoch
        .and_then(|s| s.parse::<i64>().ok())
        .is_some_and(|epoch| claims.iat < epoch)
    {
        tracing::warn!(
            user_id = %claims.sub,
            "refresh token issued before password change — rejecting"
        );
        return Err(AppError::Unauthorized);
    }

    // Set the blacklist entry to expire when the OLD token would
    // have expired naturally — anything past that is a no-op anyway.
    let now = chrono::Utc::now().timestamp();
    let remaining_secs = (claims.exp - now).max(60);
    let _: Result<(), _> = state
        .redis
        .set(
            &blacklist_key,
            "1",
            Some(fred::types::Expiration::EX(remaining_secs)),
            None,
            false,
        )
        .await;

    // Reload roles + permissions + assignments from the DB rather
    // than trusting the refresh token's snapshot. Critical: if an
    // admin revoked a role or changed scope between login and
    // refresh, the re-minted token must reflect the current state.
    let session =
        issue_auth_session(&state, claims.sub, &claims.email, client_ip.as_deref()).await?;
    Ok(session.into_login_response(false))
}

/// POST /api/auth/logout — clear all auth cookies.
///
/// Idempotent: callable without an active session. Sets the three
/// auth cookies to empty with `Max-Age=0`, which causes the browser
/// to evict them immediately. Useful for the "log out from this
/// device" flow and for cleaning up stale sessions on the client
/// side without relying on the page JS to remember to clear
/// localStorage (it can't, since the tokens are httpOnly now).
#[utoipa::path(
    post,
    path = "/api/auth/logout",
    tag = "Auth",
    responses(
        (status = 200, description = "Logged out — auth cookies cleared"),
    ),
    security(()),
)]
pub async fn logout(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> axum::response::Response {
    use axum::http::header::SET_COOKIE;

    // Revoke signing key so stolen keys can't forge HMAC signatures.
    let signing_key = format!("signing_key:{}", auth_user.claims.sub);
    let _: Result<(), _> =
        fred::interfaces::KeysInterface::del::<(), _>(&state.redis, &signing_key).await;

    let mut response = Json(serde_json::json!({"status": "ok"})).into_response();
    let headers = response.headers_mut();
    for cookie_str in verify_signature::clear_auth_cookies() {
        if let Ok(v) = cookie_str.parse() {
            headers.append(SET_COOKIE, v);
        }
    }
    response
}

#[utoipa::path(
    get,
    path = "/api/auth/me",
    tag = "Auth",
    responses(
        (status = 200, description = "Current user profile with live role assignments and permissions"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "User not found"),
    ),
)]
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

    let role_assignments = fetch_user_role_assignments(&state, user.id).await;

    // Re-derive permissions from the DB rather than trusting the
    // JWT claim. The JWT is fresh enough for the request gate
    // (every protected handler reads it) but `/me` is the canonical
    // way the frontend learns "what can this user actually do
    // RIGHT NOW", so we want it to reflect post-login role edits
    // without needing a refresh.
    let permissions = think_watch_auth::rbac::compute_user_permissions(&state.db, user.id)
        .await
        .unwrap_or_default();
    let denied_permissions =
        think_watch_auth::rbac::compute_denied_permissions(&state.db, user.id, &permissions)
            .await
            .unwrap_or_default();

    // Team memberships — used by the frontend permission cache
    // and the team-context badge in the header.
    type TeamRow = (uuid::Uuid, String);
    let team_rows: Vec<TeamRow> = sqlx::query_as(
        "SELECT t.id, t.name FROM team_members tm \
           JOIN teams t ON t.id = tm.team_id \
          WHERE tm.user_id = $1 \
          ORDER BY t.name ASC",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let teams: Vec<think_watch_common::dto::UserTeamSummary> = team_rows
        .into_iter()
        .map(|(id, name)| think_watch_common::dto::UserTeamSummary { id, name })
        .collect();

    Ok(Json(UserResponse {
        id: user.id,
        email: user.email,
        display_name: user.display_name,
        avatar_url: user.avatar_url,
        is_active: user.is_active,
        role_assignments,
        permissions,
        denied_permissions,
        teams,
        created_at: user.created_at,
    }))
}

/// Helper: load every role assignment (system + custom) for a single
/// user. Pure read; never errors — returns an empty Vec on failure so
/// the caller can keep building a response.
async fn fetch_user_role_assignments(
    state: &AppState,
    user_id: uuid::Uuid,
) -> Vec<think_watch_common::dto::RoleAssignment> {
    type Row = (uuid::Uuid, String, bool, String, Option<uuid::Uuid>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT r.id, r.name, r.is_system, ra.scope_kind, ra.scope_id \
           FROM rbac_role_assignments ra \
           JOIN rbac_roles r ON r.id = ra.role_id \
          WHERE ra.user_id = $1 \
          ORDER BY r.is_system DESC, r.name ASC",
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|(role_id, name, is_system, scope_kind, scope_id)| {
            let scope = match (scope_kind.as_str(), scope_id) {
                ("global", _) => "global".to_string(),
                (kind, Some(id)) => format!("{kind}:{id}"),
                (kind, None) => kind.to_string(),
            };
            think_watch_common::dto::RoleAssignment {
                role_id,
                name,
                is_system,
                scope,
            }
        })
        .collect()
}

#[utoipa::path(
    post,
    path = "/api/auth/password",
    tag = "Auth",
    request_body = ChangePasswordRequest,
    responses(
        (status = 200, description = "Password changed — all sessions invalidated"),
        (status = 400, description = "Invalid password or SSO account"),
        (status = 401, description = "Unauthorized or wrong current password"),
    ),
)]
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

    // Store the password-change epoch so the refresh handler can reject
    // refresh tokens issued before this moment (prevents a stolen refresh
    // token from minting new access tokens after a password change).
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;
    invalidate_refresh_tokens(&state.redis, user.id, refresh_ttl_days).await;

    state.audit.log(
        AuditEntry::platform("auth.password_changed")
            .user_id(user.id)
            .resource("auth"),
    );

    Ok(Json(serde_json::json!({"status": "password_changed"})))
}

#[utoipa::path(
    delete,
    path = "/api/auth/account",
    tag = "Auth",
    responses(
        (status = 200, description = "Account soft-deleted and all sessions revoked"),
        (status = 401, description = "Unauthorized"),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/auth/revoke-sessions",
    tag = "Auth",
    responses(
        (status = 200, description = "All sessions (signing keys + WebSocket) revoked"),
        (status = 401, description = "Unauthorized"),
    ),
)]
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

#[derive(Debug, Serialize, ToSchema)]
pub struct TotpSetupResponse {
    secret: String,
    otpauth_uri: String,
    recovery_codes: Vec<String>,
}

/// POST /api/auth/totp/setup — Begin TOTP setup. Returns secret + otpauth URI + recovery codes.
/// The user must call /totp/verify-setup with a valid code to finalize.
#[utoipa::path(
    post,
    path = "/api/auth/totp/setup",
    tag = "Auth",
    responses(
        (status = 200, description = "TOTP setup initiated — scan the otpauth_uri with an authenticator app then call /totp/verify-setup", body = TotpSetupResponse),
        (status = 400, description = "TOTP already enabled"),
        (status = 401, description = "Unauthorized"),
    ),
)]
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

#[derive(Debug, Deserialize, ToSchema)]
pub struct TotpVerifyRequest {
    pub code: String,
}

/// POST /api/auth/totp/verify-setup — Finalize TOTP setup by verifying a code.
#[utoipa::path(
    post,
    path = "/api/auth/totp/verify-setup",
    tag = "Auth",
    request_body = TotpVerifyRequest,
    responses(
        (status = 200, description = "TOTP enabled successfully"),
        (status = 400, description = "Invalid code or no pending setup"),
        (status = 401, description = "Unauthorized"),
    ),
)]
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
#[utoipa::path(
    post,
    path = "/api/auth/totp/disable",
    tag = "Auth",
    request_body = ChangePasswordRequest,
    responses(
        (status = 200, description = "TOTP disabled"),
        (status = 400, description = "TOTP not enabled or SSO account"),
        (status = 401, description = "Unauthorized or wrong password"),
    ),
)]
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
#[utoipa::path(
    get,
    path = "/api/auth/totp/status",
    tag = "Auth",
    responses(
        (status = 200, description = "TOTP status: {\"enabled\": bool, \"required\": bool}"),
        (status = 401, description = "Unauthorized"),
    ),
)]
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
