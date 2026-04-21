use axum::extract::{Query, State};
use axum::response::Redirect;
use serde::{Deserialize, Serialize};

use think_watch_common::audit::AuditEntry;
use think_watch_common::errors::AppError;
use think_watch_common::models::User;

use crate::app::AppState;

// Session payload indexed by the OIDC `state` (csrf_token) in Redis.
// Only the nonce is stored; one-time-use is enforced via atomic GETDEL on callback.
#[derive(Serialize, Deserialize)]
struct OidcSessionData {
    nonce: String,
}

/// GET /api/auth/sso/authorize — redirect to OIDC provider.
pub async fn sso_authorize(State(state): State<AppState>) -> Result<Redirect, AppError> {
    let oidc_guard = state.oidc.read().await;
    let oidc = oidc_guard
        .as_ref()
        .ok_or(AppError::BadRequest("SSO is not configured".into()))?;

    let (auth_url, csrf_token, nonce) = oidc.authorize_url();

    let session = OidcSessionData {
        nonce: nonce.secret().clone(),
    };
    let payload = serde_json::to_string(&session)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize oidc session: {e}")))?;
    fred::interfaces::KeysInterface::set::<(), _, _>(
        &state.redis,
        format!("oidc:state:{}", csrf_token.secret()),
        payload,
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

/// GET /api/auth/sso/callback — handle OIDC callback. Sets the auth
/// httpOnly cookies on the response and redirects to the frontend.
/// The client generates an ECDSA key pair locally and registers the
/// public key with the server after the redirect.
pub async fn sso_callback(
    State(state): State<AppState>,
    Query(params): Query<SsoCallbackParams>,
) -> Result<axum::response::Response, AppError> {
    let oidc_guard = state.oidc.read().await;
    let oidc = oidc_guard
        .as_ref()
        .ok_or(AppError::BadRequest("SSO is not configured".into()))?;

    // Atomic retrieve + delete — enforces one-time use of the state and
    // closes the TOCTOU window where a replayed callback could re-fetch the nonce.
    let redis_key = format!("oidc:state:{}", params.state);
    let stored: Option<String> = fred::interfaces::KeysInterface::getdel(&state.redis, &redis_key)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

    let stored = stored.ok_or(AppError::BadRequest("Invalid or expired SSO state".into()))?;

    let session: OidcSessionData =
        serde_json::from_str(&stored).map_err(|_| AppError::BadRequest("Invalid state".into()))?;
    let nonce = openidconnect::Nonce::new(session.nonce);

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

            // Assign default role (configurable via settings; empty = no role)
            if let Some(role_name) = state.dynamic_config.default_role().await {
                sqlx::query(
                    r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
                       SELECT $1, id, 'global', $1 FROM rbac_roles WHERE name = $2"#,
                )
                .bind(u.id)
                .bind(&role_name)
                .execute(&state.db)
                .await?;
            }

            u
        }
    };

    if !user.is_active {
        return Err(AppError::Forbidden("Account is deactivated".into()));
    }

    let access_ttl = state.dynamic_config.jwt_access_ttl_secs().await;
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;

    // JWT tokens only carry identity (sub, email) — permissions are
    // computed at request time from DB (with Redis cache).
    let access_token = state
        .jwt
        .create_access_token_with_ttl(user.id, &user.email, access_ttl)?;
    let refresh_token =
        state
            .jwt
            .create_refresh_token_with_ttl(user.id, &user.email, refresh_ttl_days)?;

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

    // Build the redirect target. Tokens go via httpOnly cookies
    // (set on this same response below). The client generates an
    // ECDSA key pair and registers the public key via
    // POST /api/auth/register-key after the redirect.
    let frontend_url = state
        .config
        .cors_origins
        .first()
        .map(|s| s.as_str())
        .unwrap_or_else(|| {
            tracing::warn!(
                "No CORS_ORIGINS configured for SSO redirect, falling back to console address"
            );
            "/"
        });

    let redirect_url = format!("{}/#sso=ok&expires_in={}", frontend_url, access_ttl,);

    use axum::http::header::{LOCATION, SET_COOKIE};
    let mut response = axum::response::Response::builder()
        .status(axum::http::StatusCode::TEMPORARY_REDIRECT)
        .body(axum::body::Body::empty())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("redirect build failed: {e}")))?;
    let headers = response.headers_mut();
    if let Ok(loc) = redirect_url.parse() {
        headers.insert(LOCATION, loc);
    }
    let access_cookie =
        crate::middleware::verify_signature::access_token_cookie(&access_token, access_ttl);
    let refresh_cookie = crate::middleware::verify_signature::refresh_token_cookie(
        &refresh_token,
        refresh_ttl_days * 86400,
    );
    for cookie_str in [&access_cookie, &refresh_cookie] {
        if let Ok(v) = cookie_str.parse() {
            headers.append(SET_COOKIE, v);
        }
    }
    Ok(response)
}
