use axum::{
    extract::{FromRequestParts, State},
    http::{Request, StatusCode, header::AUTHORIZATION, request::Parts},
    middleware::Next,
    response::Response,
};

use think_watch_auth::jwt::Claims;
use think_watch_common::audit::AuditEntry;
use think_watch_common::errors::AppError;

use crate::app::AppState;

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub claims: Claims,
    pub ip: Option<String>,
}

impl AuthUser {
    /// Build an audit entry pre-filled with user_id, user_email, and ip_address.
    pub fn audit(&self, action: impl Into<String>) -> AuditEntry {
        let mut e = AuditEntry::platform(action)
            .user_id(self.claims.sub)
            .user_email(&self.claims.email);
        if let Some(ref ip) = self.ip {
            e = e.ip_address(ip.clone());
        }
        e
    }

    /// Authorization gate: require the JWT to carry the given permission
    /// key (`resource:action`). This is the authoritative authorization
    /// check — every admin handler calls it at the top.
    ///
    /// The permission set was computed at JWT creation as the union of
    /// every role the user holds (see `rbac::compute_user_permissions`).
    /// Returns `AppError::Forbidden` if the permission is not present.
    pub fn require_permission(&self, perm: &str) -> Result<(), AppError> {
        if self.claims.permissions.iter().any(|p| p == perm) {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!(
                "Missing required permission: {perm}"
            )))
        }
    }

    /// Same as `require_permission` but accepts multiple alternatives.
    /// Access is granted if the user has ANY of the listed permissions.
    #[allow(dead_code)]
    pub fn require_any_permission(&self, perms: &[&str]) -> Result<(), AppError> {
        if perms
            .iter()
            .any(|p| self.claims.permissions.iter().any(|c| c == *p))
        {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!(
                "Missing any of required permissions: {}",
                perms.join(", ")
            )))
        }
    }
}

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthUser>()
            .cloned()
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

/// Resolve the client IP for an incoming request, honouring the dynamic
/// `client_ip_source` setting and the `security.trusted_proxies` whitelist.
///
/// When the configured source is `xff` or `x-real-ip` we only trust the
/// header if the direct TCP peer is in the trusted-proxy list — otherwise
/// we fall back to the connection IP. This prevents an attacker from
/// forging an IP via headers to bypass per-IP rate limits.
///
/// Shared between `require_auth` and unauthenticated endpoints (login,
/// register) so all rate limiting uses the same validated IP.
pub async fn extract_client_ip(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    extensions: &axum::http::Extensions,
) -> Option<String> {
    let ip_source = state.dynamic_config.client_ip_source().await;
    let connection_ip = extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string());

    if ip_source != "connection"
        && let Some(trusted_proxies) = state
            .dynamic_config
            .get_string("security.trusted_proxies")
            .await
        && !trusted_proxies.is_empty()
    {
        let conn_ip = connection_ip.as_deref().unwrap_or("");
        let is_trusted = trusted_proxies
            .split(',')
            .map(|s| s.trim())
            .any(|proxy| proxy == conn_ip || proxy == "*");
        if !is_trusted {
            tracing::warn!(
                connection_ip = conn_ip,
                "Request from untrusted proxy, falling back to connection IP"
            );
            return connection_ip;
        }
    }

    match ip_source.as_str() {
        "xff" => {
            let position = state.dynamic_config.client_ip_xff_position().await;
            let depth = state.dynamic_config.client_ip_xff_depth().await.max(1) as usize;
            headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| {
                    let parts: Vec<&str> = v.split(',').map(|s| s.trim()).collect();
                    if parts.is_empty() {
                        return None;
                    }
                    let idx = if position == "right" {
                        parts.len().checked_sub(depth)
                    } else {
                        let i = depth - 1;
                        if i < parts.len() { Some(i) } else { None }
                    };
                    idx.and_then(|i| parts.get(i)).map(|s| s.to_string())
                })
        }
        "x-real-ip" => headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string()),
        // "connection" — use TCP peer address from ConnectInfo
        _ => connection_ip,
    }
}

pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let claims = state
        .jwt
        .verify_token(token)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    if claims.token_type != "access" {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Check JWT blacklist (revoked tokens)
    let token_hash = think_watch_auth::jwt::sha2_hash(token);
    if think_watch_auth::jwt::is_revoked(&state.redis, &token_hash).await {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Extract client IP via the shared helper that performs trusted-proxy
    // validation. Without this, header-based IP sources can be forged.
    let ip = extract_client_ip(&state, request.headers(), request.extensions()).await;

    // Publish user_id into the access-log slot if the access log layer
    // installed one. This is how the HTTP access log gets a user_id
    // attached even though it can't read request extensions after the
    // inner service consumes the request.
    if let Some(slot) = request
        .extensions()
        .get::<crate::middleware::access_log::AccessLogUserSlot>()
    {
        let _ = slot.0.set(claims.sub);
    }

    request.extensions_mut().insert(AuthUser { claims, ip });

    Ok(next.run(request).await)
}
