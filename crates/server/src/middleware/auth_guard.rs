use axum::{
    extract::{FromRequestParts, State},
    http::{Request, StatusCode, header::AUTHORIZATION, request::Parts},
    middleware::Next,
    response::Response,
};

use agent_bastion_auth::jwt::Claims;
use agent_bastion_common::audit::AuditEntry;

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
    let token_hash = agent_bastion_auth::jwt::sha2_hash(token);
    if agent_bastion_auth::jwt::is_revoked(&state.redis, &token_hash).await {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Extract client IP based on dynamic config.
    // When using header-based IP sources (xff, x-real-ip), validate that
    // the direct connection comes from a trusted proxy if configured.
    let ip_source = state.dynamic_config.client_ip_source().await;
    let connection_ip = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string());

    if ip_source != "connection" {
        // Check trusted proxy whitelist (comma-separated IPs/CIDRs in dynamic config)
        if let Some(trusted_proxies) = state
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
                // Fall back to connection IP instead of trusting the header
                request.extensions_mut().insert(AuthUser {
                    claims,
                    ip: connection_ip,
                });
                return Ok(next.run(request).await);
            }
        }
    }

    let ip = match ip_source.as_str() {
        "xff" => {
            let position = state.dynamic_config.client_ip_xff_position().await;
            let depth = state.dynamic_config.client_ip_xff_depth().await.max(1) as usize;
            request
                .headers()
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
        "x-real-ip" => request
            .headers()
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string()),
        // "connection" — use TCP peer address from ConnectInfo
        _ => request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip().to_string()),
    };

    request.extensions_mut().insert(AuthUser { claims, ip });

    Ok(next.run(request).await)
}
