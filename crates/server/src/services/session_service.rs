//! Session service — shared bits of the login / register / refresh /
//! force-logout plumbing that used to live at the top of
//! `handlers::auth`.
//!
//! Three jobs:
//!
//! 1. `issue_auth_session` — loads the user's RBAC, mints a JWT access/
//!    refresh pair, and returns them pre-formatted as `Set-Cookie`
//!    values plus the body fields the login response carries.
//! 2. `invalidate_refresh_tokens` — bumps the per-user "min iat"
//!    epoch in Redis so any refresh token older than the epoch is
//!    rejected by the refresh handler. Called after password changes
//!    and admin force-logout.
//! 3. The temp-password marker (`mark_temporary_password`,
//!    `clear_temporary_password_marker`) — set by admin create/reset,
//!    consumed by login to enforce the 24h TTL on admin-issued temps.
//!
//! Handlers stay in `handlers::auth` and re-export these at their
//! original paths for back-compat while the migration completes.

use axum::Json;
use axum::response::IntoResponse;
use think_watch_common::dto::LoginResponse;
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::verify_signature;

/// Intermediate result from [`issue_auth_session`] containing every
/// piece of data needed to build the final HTTP response: the JWT
/// cookies (pre-formatted as `Set-Cookie` header values) and the
/// fields the JSON body exposes to the frontend.
pub(crate) struct AuthSession {
    pub permissions: Vec<String>,
    pub denied_permissions: Vec<String>,
    pub roles: Vec<String>,
    pub access_ttl: i64,
    /// Pre-formatted Set-Cookie header values (access, refresh).
    cookie_headers: [String; 2],
}

impl AuthSession {
    /// Append the auth cookies to an existing response.
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
    _client_ip: Option<&str>,
) -> Result<AuthSession, AppError> {
    // Load roles/permissions for the login response body (frontend needs them),
    // but they are NOT embedded in the JWT anymore.
    let roles = think_watch_auth::rbac::load_user_role_names(&state.db, user_id).await?;
    let all_perm_keys = crate::handlers::roles::all_permission_keys();
    let permissions =
        think_watch_auth::rbac::compute_user_permissions(&state.db, user_id, &all_perm_keys)
            .await?;
    let denied_permissions =
        think_watch_auth::rbac::compute_denied_permissions(&state.db, user_id, &permissions)
            .await?;

    let access_ttl = state.dynamic_config.jwt_access_ttl_secs().await;
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;

    // JWT tokens only carry identity (sub, email) — permissions are
    // computed at request time from DB (with Redis cache).
    let access_token = state
        .jwt
        .create_access_token_with_ttl(user_id, email, access_ttl)?;
    let refresh_token =
        state
            .jwt
            .create_refresh_token_with_ttl(user_id, email, refresh_ttl_days)?;

    let access_cookie = verify_signature::access_token_cookie(&access_token, access_ttl);
    let refresh_cookie =
        verify_signature::refresh_token_cookie(&refresh_token, refresh_ttl_days * 86400);

    Ok(AuthSession {
        permissions,
        denied_permissions,
        roles,
        access_ttl,
        cookie_headers: [access_cookie, refresh_cookie],
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

/// How long an admin-issued temporary password remains usable.
/// Anchors both the Redis marker TTL at issuance and the legacy
/// grandfather window at login; keeping them identical means a
/// deployed user with `password_change_required=true` is accepted
/// for exactly one window regardless of which side saw them first.
pub(crate) const TEMP_PASSWORD_TTL_SECS: i64 = 86400;

pub(crate) fn temp_password_marker_key(user_id: uuid::Uuid) -> String {
    format!("pw_temp:{user_id}")
}

/// Record that `user_id` has an active admin-issued temporary password.
/// Called from `admin::create_user` (when a password was generated)
/// and `admin::reset_password`. Consumed by the login handler, which
/// rejects logins after this marker expires.
pub(crate) async fn mark_temporary_password(redis: &fred::clients::Client, user_id: uuid::Uuid) {
    let _: Result<(), _> = fred::interfaces::KeysInterface::set(
        redis,
        &temp_password_marker_key(user_id),
        &chrono::Utc::now().timestamp().to_string(),
        Some(fred::types::Expiration::EX(TEMP_PASSWORD_TTL_SECS)),
        None,
        false,
    )
    .await;
}

pub(crate) async fn clear_temporary_password_marker(
    redis: &fred::clients::Client,
    user_id: uuid::Uuid,
) {
    let _: Result<(), _> =
        fred::interfaces::KeysInterface::del::<(), _>(redis, &temp_password_marker_key(user_id))
            .await;
}
