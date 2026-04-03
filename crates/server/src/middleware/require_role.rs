use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};

use agent_bastion_auth::rbac::SystemRole;

use super::auth_guard::AuthUser;
use crate::app::AppState;

/// Middleware that requires the authenticated user to have at least one of the
/// specified roles. Must be applied AFTER `require_auth`.
pub async fn require_admin(
    State(_state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth_user = request
        .extensions()
        .get::<AuthUser>()
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let is_admin = auth_user
        .claims
        .roles
        .iter()
        .any(|r| r == "super_admin" || r == "admin");

    if !is_admin {
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(next.run(request).await)
}

/// Returns a middleware layer that checks granular RBAC permissions.
///
/// Iterates through the user's JWT roles, parses each into a `SystemRole`,
/// and checks if any role grants the required permission.
pub fn require_permission(
    resource: &'static str,
    action: &'static str,
) -> impl Fn(
    Request<axum::body::Body>,
    Next,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Response, StatusCode>> + Send>,
> + Clone
       + Send {
    move |request: Request<axum::body::Body>, next: Next| {
        Box::pin(async move {
            let auth_user = request
                .extensions()
                .get::<AuthUser>()
                .ok_or(StatusCode::UNAUTHORIZED)?;

            let allowed = auth_user.claims.roles.iter().any(|role_str| {
                SystemRole::parse(role_str)
                    .map(|role| role.has_permission(resource, action))
                    .unwrap_or(false)
            });

            if !allowed {
                return Err(StatusCode::FORBIDDEN);
            }

            Ok(next.run(request).await)
        })
    }
}
