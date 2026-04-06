use axum::{
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};

use think_watch_auth::api_key;
use think_watch_gateway::proxy::GatewayRequestIdentity;

use crate::app::AppState;

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

        let gateway_identity = GatewayRequestIdentity {
            user_id: row.user_id.map(|u| u.to_string()),
            api_key_id: Some(row.id.to_string()),
            allowed_models: row.allowed_models.clone(),
            rate_limit_rpm: row.rate_limit_rpm,
            rate_limit_tpm: row.rate_limit_tpm,
        };

        let identity = GatewayIdentity {
            api_key_id: row.id,
            user_id: row.user_id,
            team_id: row.team_id,
            allowed_models: row.allowed_models.clone(),
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

        let gateway_identity = GatewayRequestIdentity {
            user_id: Some(claims.sub.to_string()),
            api_key_id: None,
            allowed_models: None,
            rate_limit_rpm: None,
            rate_limit_tpm: None,
        };

        let identity = GatewayIdentity {
            api_key_id: uuid::Uuid::nil(),
            user_id: Some(claims.sub),
            team_id: None,
            allowed_models: None,
            rate_limit_rpm: None,
            rate_limit_tpm: None,
        };

        request.extensions_mut().insert(identity);
        request.extensions_mut().insert(gateway_identity);
    }

    Ok(next.run(request).await)
}
