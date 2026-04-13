use axum::{
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::Response,
};

use think_watch_auth::api_key;
use think_watch_auth::rbac;
use think_watch_gateway::proxy::GatewayRequestIdentity;
use think_watch_mcp_gateway::transport::streamable_http::McpRequestIdentity;

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
    /// Role NAMES the underlying user holds. Used by the MCP gateway's
    /// access controller, which gates per-tool access on role names.
    /// Empty for service-account keys (no associated user).
    pub user_roles: Vec<String>,
}

/// Future returned by the middleware closure. Boxed because the
/// generated impl trait isn't nameable; pulled out into a type
/// alias to keep clippy::type_complexity happy.
type AuthFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, StatusCode>> + Send>>;

/// Build a middleware that authenticates requests via `tw-` prefixed
/// API keys and additionally requires the key's `surfaces` array to
/// contain `surface`.
///
/// One layer per gateway: the AI router mounts `require_api_key("ai_gateway")`,
/// the MCP router mounts `require_api_key("mcp_gateway")`. Same key
/// shape and same lookup logic; only the surface check differs.
///
/// JWT fallback was removed in this commit — the MCP gateway used to
/// auth via JWT and the AI gateway accepted JWT as a developer
/// convenience, but both surfaces now require an API key. Internal
/// admin tooling can still call the console API with JWT; the
/// gateway data path is API-key-only.
pub fn require_api_key(
    surface: &'static str,
) -> impl Fn(State<AppState>, Request, Next) -> AuthFuture + Clone {
    move |State(state): State<AppState>, mut request: Request, next: Next| {
        Box::pin(async move {
            let auth_header = request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(StatusCode::UNAUTHORIZED)?;

            let token = auth_header
                .strip_prefix("Bearer ")
                .ok_or(StatusCode::UNAUTHORIZED)?;

            // Reject anything that doesn't look like a `tw-` key. The
            // separate JWT fallback path is gone — gateway data
            // requests must use a real API key.
            if !token.starts_with(api_key::KEY_PREFIX) {
                return Err(StatusCode::UNAUTHORIZED);
            }

            let key_hash = api_key::hash_api_key(token);

            // Active keys OR keys still inside their rotation grace period.
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

            // Surface gate. Even a valid key gets rejected if its
            // `surfaces` list doesn't include the gateway being called.
            // Forbidden (not Unauthorized) because the credential is
            // valid — it's just not allowed to call this surface.
            if !row.surfaces.iter().any(|s| s == surface) {
                tracing::warn!(
                    api_key_id = %row.id,
                    surface,
                    surfaces = ?row.surfaces,
                    "API key not allowed for this gateway surface"
                );
                return Err(StatusCode::FORBIDDEN);
            }

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
                if let Err(e) =
                    sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE id = $1")
                        .bind(key_id)
                        .execute(&db)
                        .await
                {
                    tracing::warn!("Failed to update api_key last_used_at: {e}");
                }
            });

            // Compute the user's role-derived constraints and intersect
            // with the API-key allow-list. The role union is loaded once
            // per request — fast enough at our scale.
            //
            // We also pull the role NAMES so the MCP access controller
            // can gate per-tool access without re-querying the DB.
            let (role_limits, user_roles, role_ids) = if let Some(uid) = row.user_id {
                let limits = rbac::compute_user_resource_limits(&state.db, uid)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                let names = rbac::load_user_role_names(&state.db, uid)
                    .await
                    .unwrap_or_default();
                let ids = rbac::load_user_role_ids(&state.db, uid)
                    .await
                    .unwrap_or_default();
                (limits, names, ids)
            } else {
                // Service-account API keys (no user_id) inherit only
                // the per-key constraints, since there's no user to
                // resolve roles against. They get an empty role list,
                // which means the MCP access controller will deny
                // anything that requires a role match.
                (
                    rbac::UserResourceLimits {
                        allowed_models: None,
                        allowed_mcp_servers: None,
                    },
                    Vec::new(),
                    Vec::new(),
                )
            };
            let merged_models =
                intersect_allowlists(row.allowed_models.clone(), role_limits.allowed_models);

            let gateway_identity = GatewayRequestIdentity {
                user_id: row.user_id.map(|u| u.to_string()),
                api_key_id: Some(row.id.to_string()),
                allowed_models: merged_models.clone(),
                role_ids: role_ids.iter().map(|id| id.to_string()).collect(),
            };

            let identity = GatewayIdentity {
                api_key_id: row.id,
                user_id: row.user_id,
                team_id: row.team_id,
                allowed_models: merged_models,
                user_roles,
            };

            // The MCP transport handlers expect their own typed
            // extension and require a user_id (sessions are keyed
            // by user). Service-account keys without a user_id
            // can't talk to MCP — return 401 here rather than
            // letting the handler 500 on a missing extension.
            if surface == "mcp_gateway" {
                let Some(uid) = row.user_id else {
                    tracing::warn!(
                        api_key_id = %row.id,
                        "MCP gateway requires a user-bound API key (service-account keys are not supported)"
                    );
                    return Err(StatusCode::UNAUTHORIZED);
                };
                let user_email: String =
                    sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
                        .bind(uid)
                        .fetch_one(&state.db)
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                let mcp_identity = McpRequestIdentity {
                    user_id: uid,
                    user_email,
                    user_roles: identity.user_roles.clone(),
                    role_ids: role_ids.clone(),
                };
                request.extensions_mut().insert(mcp_identity);
            }

            request.extensions_mut().insert(identity);
            request.extensions_mut().insert(gateway_identity);

            Ok(next.run(request).await)
        })
    }
}
