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

/// GET /api/auth/sso/callback — handle OIDC callback. Sets the auth
/// httpOnly cookies on the response and redirects to the frontend
/// with only `signing_key` in the URL fragment (the page JS needs
/// it for HMAC computation).
pub async fn sso_callback(
    State(state): State<AppState>,
    Query(params): Query<SsoCallbackParams>,
) -> Result<axum::response::Response, AppError> {
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

            // Assign default developer role via the unified table.
            sqlx::query(
                r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
                   SELECT $1, id, 'global', $1 FROM rbac_roles WHERE name = 'developer'"#,
            )
            .bind(u.id)
            .execute(&state.db)
            .await?;

            u
        }
    };

    if !user.is_active {
        return Err(AppError::Forbidden("Account is deactivated".into()));
    }

    // Union of system + custom role names and permissions — see
    // `rbac::compute_user_permissions` for merge semantics.
    let roles = think_watch_auth::rbac::load_user_role_names(&state.db, user.id).await?;
    let permissions = think_watch_auth::rbac::compute_user_permissions(&state.db, user.id).await?;

    let access_ttl = state.dynamic_config.jwt_access_ttl_secs().await;
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;

    let access_token = state.jwt.create_access_token_with_ttl(
        user.id,
        &user.email,
        roles.clone(),
        permissions.clone(),
        access_ttl,
    )?;
    let refresh_token = state.jwt.create_refresh_token_with_ttl(
        user.id,
        &user.email,
        roles.clone(),
        permissions.clone(),
        refresh_ttl_days,
    )?;

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

    // Build the redirect target. Tokens go via httpOnly cookies
    // (set on this same response below); only the signing_key
    // travels via the URL fragment so the page JS can stash it in
    // sessionStorage for HMAC computation. Fragments aren't sent
    // to the server on subsequent requests, so the signing_key
    // doesn't end up in proxy logs even though it's not httpOnly.
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

    let redirect_url = format!(
        "{}/#sso=ok&signing_key={}&expires_in={}",
        frontend_url,
        urlencoding::encode(&signing_key),
        access_ttl,
    );

    use axum::http::header::{LOCATION, SET_COOKIE};
    let mut response = axum::response::Response::builder()
        .status(axum::http::StatusCode::TEMPORARY_REDIRECT)
        .body(axum::body::Body::empty())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("redirect build failed: {e}")))?;
    let headers = response.headers_mut();
    if let Ok(loc) = redirect_url.parse() {
        headers.insert(LOCATION, loc);
    }
    let signing_cookie =
        crate::middleware::verify_signature::signing_key_cookie(&signing_key, 86400);
    let access_cookie =
        crate::middleware::verify_signature::access_token_cookie(&access_token, access_ttl);
    let refresh_cookie = crate::middleware::verify_signature::refresh_token_cookie(
        &refresh_token,
        refresh_ttl_days * 86400,
    );
    for cookie_str in [&signing_cookie, &access_cookie, &refresh_cookie] {
        if let Ok(v) = cookie_str.parse() {
            headers.append(SET_COOKIE, v);
        }
    }
    Ok(response)
}
