use axum::extract::{Query, State};
use axum::response::Redirect;
use serde::Deserialize;

use think_watch_common::audit::AuditEntry;
use think_watch_common::errors::AppError;
use think_watch_common::models::User;

use crate::app::AppState;

/// GET /api/auth/sso/authorize — redirect to OIDC provider.
pub async fn sso_authorize(State(state): State<AppState>) -> Result<Redirect, AppError> {
    let oidc_guard = state.oidc.read().await;
    let oidc = oidc_guard
        .as_ref()
        .ok_or(AppError::BadRequest("SSO is not configured".into()))?;

    let (auth_url, csrf_token, nonce) = oidc.authorize_url();

    // Store csrf_token and nonce in a short-lived cookie or Redis
    // For simplicity, store in Redis with csrf_token as key
    let nonce_json = serde_json::json!({
        "nonce": nonce.secret(),
        "csrf": csrf_token.secret(),
    });
    fred::interfaces::KeysInterface::set::<(), _, _>(
        &state.redis,
        format!("oidc:state:{}", csrf_token.secret()),
        nonce_json.to_string(),
        Some(fred::types::Expiration::EX(600)), // 10 min TTL
        None,
        false,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

    Ok(Redirect::temporary(&auth_url))
}

#[derive(Deserialize)]
pub struct SsoCallbackParams {
    pub code: String,
    pub state: String,
}

/// GET /api/auth/sso/callback — handle OIDC callback, redirect with tokens in fragment.
pub async fn sso_callback(
    State(state): State<AppState>,
    Query(params): Query<SsoCallbackParams>,
) -> Result<Redirect, AppError> {
    let oidc_guard = state.oidc.read().await;
    let oidc = oidc_guard
        .as_ref()
        .ok_or(AppError::BadRequest("SSO is not configured".into()))?;

    // Retrieve nonce from Redis
    let redis_key = format!("oidc:state:{}", params.state);
    let stored: Option<String> = fred::interfaces::KeysInterface::get(&state.redis, &redis_key)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

    let stored = stored.ok_or(AppError::BadRequest("Invalid or expired SSO state".into()))?;

    // Delete the state from Redis (one-time use)
    let _: () = fred::interfaces::KeysInterface::del(&state.redis, &redis_key)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

    let stored_json: serde_json::Value =
        serde_json::from_str(&stored).map_err(|_| AppError::BadRequest("Invalid state".into()))?;
    let nonce_str = stored_json["nonce"]
        .as_str()
        .ok_or(AppError::BadRequest("Invalid nonce".into()))?;
    let nonce = openidconnect::Nonce::new(nonce_str.to_string());

    // Exchange code for tokens
    let user_info = oidc
        .exchange_code(&params.code, &nonce)
        .await
        .map_err(|e| AppError::BadRequest(format!("SSO authentication failed: {e}")))?;

    // Find or create user
    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE oidc_subject = $1 AND oidc_issuer = $2",
    )
    .bind(&user_info.subject)
    .bind(&user_info.issuer)
    .fetch_optional(&state.db)
    .await?;

    let user = match user {
        Some(u) => u,
        None => {
            let email = user_info.email.as_deref().unwrap_or(&user_info.subject);
            let display_name = user_info.name.as_deref().unwrap_or(email);

            // Create new SSO user (no password)
            let u = sqlx::query_as::<_, User>(
                r#"INSERT INTO users (email, display_name, oidc_subject, oidc_issuer)
                   VALUES ($1, $2, $3, $4) RETURNING *"#,
            )
            .bind(email)
            .bind(display_name)
            .bind(&user_info.subject)
            .bind(&user_info.issuer)
            .fetch_one(&state.db)
            .await?;

            // Assign default developer role
            sqlx::query(
                r#"INSERT INTO user_roles (user_id, role_id, scope)
                   SELECT $1, id, 'global' FROM roles WHERE name = 'developer'"#,
            )
            .bind(u.id)
            .execute(&state.db)
            .await?;

            u
        }
    };

    if !user.is_active {
        return Err(AppError::Forbidden);
    }

    // Fetch roles
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

    // Audit log
    state.audit.log(
        AuditEntry::platform("auth.sso_login")
            .user_id(user.id)
            .resource("auth")
            .detail(serde_json::json!({
                "oidc_issuer": user_info.issuer,
                "oidc_subject": user_info.subject,
            })),
    );

    // Create signing key for HMAC request signing
    let signing_key =
        crate::middleware::verify_signature::create_signing_key(&state.redis, &user.id, None)
            .await
            .map_err(|e| {
                AppError::Internal(anyhow::anyhow!("Failed to create signing key: {e}"))
            })?;

    // Redirect to frontend with tokens in URL fragment (never sent to server in subsequent requests)
    let frontend_url = state
        .config
        .cors_origins
        .first()
        .map(|s| s.as_str())
        .unwrap_or_else(|| {
            tracing::warn!(
                "No CORS_ORIGINS configured for SSO redirect, falling back to console address"
            );
            // No hardcoded localhost — fail clearly if misconfigured
            "/"
        });

    let redirect_url = format!(
        "{}/#access_token={}&refresh_token={}&signing_key={}&expires_in=900",
        frontend_url,
        urlencoding::encode(&access_token),
        urlencoding::encode(&refresh_token),
        urlencoding::encode(&signing_key),
    );

    Ok(Redirect::temporary(&redirect_url))
}
