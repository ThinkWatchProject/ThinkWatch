use axum::{
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};

use think_watch_auth::api_key;
use think_watch_auth::rbac;
use think_watch_gateway::proxy::GatewayRequestIdentity;

use crate::app::AppState;

/// Intersect a per-API-key allow-list with a per-role allow-list.
///
/// Both inputs are nullable, where `None` means "unrestricted":
///   - key=None, role=None    → None (unrestricted)
///   - key=Some, role=None    → key (role doesn't tighten)
///   - key=None, role=Some    → role (key doesn't tighten)
///   - key=Some, role=Some    → set intersection
///
/// Intersection (not union) is the right merge here because the
/// per-key list is a tightening of what the user as a whole can do
/// — an admin who restricts a developer's API key to gpt-4o-mini
/// shouldn't have that overridden by the role's broader list.
fn intersect_allowlists(
    key_list: Option<Vec<String>>,
    role_list: Option<Vec<String>>,
) -> Option<Vec<String>> {
    match (key_list, role_list) {
        (None, None) => None,
        (Some(k), None) => Some(k),
        (None, Some(r)) => Some(r),
        (Some(k), Some(r)) => {
            let role_set: std::collections::HashSet<&String> = r.iter().collect();
            Some(k.into_iter().filter(|m| role_set.contains(m)).collect())
        }
    }
}

/// Authenticated identity for gateway requests (via `tw-` API key).
/// Inserted into request extensions for use by downstream middleware/handlers.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GatewayIdentity {
    pub api_key_id: uuid::Uuid,
    pub user_id: Option<uuid::Uuid>,
    pub team_id: Option<uuid::Uuid>,
    pub allowed_models: Option<Vec<String>>,
    pub rate_limit_rpm: Option<i32>,
    pub rate_limit_tpm: Option<i32>,
}

/// Middleware that authenticates requests via `tw-` prefixed API keys
/// OR falls back to JWT Bearer tokens (for admin/testing convenience).
pub async fn require_api_key_or_jwt(
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

    // Check if this is a ThinkWatch API key
    if token.starts_with(api_key::KEY_PREFIX) {
        let key_hash = api_key::hash_api_key(token);

        // Query active keys OR keys within their rotation grace period
        let row = sqlx::query_as::<_, think_watch_common::models::ApiKey>(
            r#"SELECT * FROM api_keys
               WHERE key_hash = $1
                 AND deleted_at IS NULL
                 AND (
                     is_active = true
                     OR (grace_period_ends_at IS NOT NULL AND grace_period_ends_at > now())
                 )"#,
        )
        .bind(&key_hash)
        .fetch_optional(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

        // Check expiration
        if let Some(expires_at) = row.expires_at
            && expires_at < chrono::Utc::now()
        {
            return Err(StatusCode::UNAUTHORIZED);
        }

        // Update last_used_at (best-effort, don't block on failure)
        let db = state.db.clone();
        let key_id = row.id;
        tokio::spawn(async move {
            if let Err(e) = sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE id = $1")
                .bind(key_id)
                .execute(&db)
                .await
            {
                tracing::warn!("Failed to update api_key last_used_at: {e}");
            }
        });

        // Compute the user's role-derived constraints and intersect
        // with the API-key allow-list. The role union is loaded once
        // per request — fast enough at our scale, but a future
        // optimization could cache it on the user_id for ~5s.
        let role_limits = if let Some(uid) = row.user_id {
            rbac::compute_user_resource_limits(&state.db, uid)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        } else {
            // Service-account API keys (no user_id) inherit only
            // the per-key constraints, since there's no user to
            // resolve roles against.
            rbac::UserResourceLimits {
                allowed_models: None,
                allowed_mcp_servers: None,
            }
        };
        let merged_models =
            intersect_allowlists(row.allowed_models.clone(), role_limits.allowed_models);

        let gateway_identity = GatewayRequestIdentity {
            user_id: row.user_id.map(|u| u.to_string()),
            api_key_id: Some(row.id.to_string()),
            allowed_models: merged_models.clone(),
            rate_limit_rpm: row.rate_limit_rpm,
            rate_limit_tpm: row.rate_limit_tpm,
        };

        let identity = GatewayIdentity {
            api_key_id: row.id,
            user_id: row.user_id,
            team_id: row.team_id,
            allowed_models: merged_models,
            rate_limit_rpm: row.rate_limit_rpm,
            rate_limit_tpm: row.rate_limit_tpm,
        };

        request.extensions_mut().insert(identity);
        request.extensions_mut().insert(gateway_identity);
    } else {
        // Try JWT fallback
        let claims = state
            .jwt
            .verify_token(token)
            .map_err(|_| StatusCode::UNAUTHORIZED)?;

        if claims.token_type != "access" {
            return Err(StatusCode::UNAUTHORIZED);
        }

        // JWT-authenticated gateway calls (rare; mostly admin/test
        // tooling) inherit the user's role-derived allow-list as-is.
        // No per-key list to intersect with, so the role union IS
        // the effective allow-list.
        let role_limits = rbac::compute_user_resource_limits(&state.db, claims.sub)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let gateway_identity = GatewayRequestIdentity {
            user_id: Some(claims.sub.to_string()),
            api_key_id: None,
            allowed_models: role_limits.allowed_models.clone(),
            rate_limit_rpm: None,
            rate_limit_tpm: None,
        };

        let identity = GatewayIdentity {
            api_key_id: uuid::Uuid::nil(),
            user_id: Some(claims.sub),
            team_id: None,
            allowed_models: role_limits.allowed_models,
            rate_limit_rpm: None,
            rate_limit_tpm: None,
        };

        request.extensions_mut().insert(identity);
        request.extensions_mut().insert(gateway_identity);
    }

    Ok(next.run(request).await)
}
