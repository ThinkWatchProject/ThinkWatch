use axum::{
    extract::{FromRequestParts, State},
    http::{Request, StatusCode, header::AUTHORIZATION, request::Parts},
    middleware::Next,
    response::Response,
};

use think_watch_auth::{api_key, jwt::Claims, rbac};
use think_watch_common::audit::AuditEntry;
use think_watch_common::errors::AppError;

use crate::app::AppState;

/// Marker inserted into request extensions when a request is authenticated
/// via a `tw-` API key (console surface) rather than a session JWT.
/// `verify_signature` reads this to skip HMAC checking — HMAC is a
/// session-security mechanism; API keys carry their own credential.
#[derive(Clone)]
pub struct ApiKeyAuthenticated;

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

    // ------------------------------------------------------------------------
    // Scope-aware authorization
    //
    // `require_permission` is a UNION-style check across every role
    // assignment regardless of scope. It's the right gate for:
    //   - middleware "is this user authenticated and even allowed to
    //     hit this route family" checks
    //   - UI button gating in the React layer (the JWT claim is the
    //     same union the frontend reads)
    //
    // It is NOT enough for "can this caller mutate THIS specific
    // subject" decisions. A team-manager scoped to team:engineering
    // has `api_keys:update` in their JWT permissions array, but the
    // server must also verify that the api_key being edited belongs
    // to a user in team:engineering. That second check is what the
    // `assert_scope_for_*` family below does.
    //
    // The implementation queries `rbac_role_assignments JOIN rbac_roles`
    // on every call. That's one indexed-join SQL query per request
    // (~1ms) — cheap, and crucially the lookup is against LIVE data,
    // so revoking a role takes effect on the next request, not the
    // next refresh. The JWT's `role_assignments` field is only used
    // by callers that need the raw assignment list without checking
    // a specific permission (e.g. the /api/auth/me endpoint).
    // ------------------------------------------------------------------------

    /// Assert the caller has `perm` at GLOBAL scope.
    ///
    /// Used for platform-wide resources that no team manager should
    /// ever be able to mutate: providers, mcp_servers, models,
    /// settings, roles, log_forwarders, audit forwarder configs.
    pub async fn assert_scope_global(
        &self,
        pool: &sqlx::PgPool,
        perm: &str,
    ) -> Result<(), AppError> {
        let has: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM rbac_role_assignments ra
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE ra.user_id = $1
                    AND ra.scope_kind = 'global'
                    AND $2 = ANY(r.permissions)
             )",
        )
        .bind(self.claims.sub)
        .bind(perm)
        .fetch_one(pool)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("scope check failed: {e}")))?;
        if has {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!(
                "{perm} requires global scope (this is a platform-wide resource)"
            )))
        }
    }

    /// Assert the caller has `perm` either globally OR scoped to a
    /// team that contains `target_user_id`.
    ///
    /// Used by handlers that mutate a user's own data: api_keys,
    /// limits, role assignments, password resets.
    pub async fn assert_scope_for_user(
        &self,
        pool: &sqlx::PgPool,
        perm: &str,
        target_user_id: uuid::Uuid,
    ) -> Result<(), AppError> {
        // Self-edit is always allowed: a user can manage their own
        // resources without needing a `*:write` perm on themselves.
        // Handlers that DON'T want this (e.g. preventing
        // self-permission-grants) can `require_permission` first and
        // skip the scope check.
        if self.claims.sub == target_user_id {
            return Ok(());
        }
        let has: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM rbac_role_assignments ra
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE ra.user_id = $1
                    AND $2 = ANY(r.permissions)
                    AND (
                        ra.scope_kind = 'global'
                        OR (ra.scope_kind = 'team'
                            AND ra.scope_id IN (
                                SELECT team_id FROM team_members WHERE user_id = $3
                            ))
                    )
             )",
        )
        .bind(self.claims.sub)
        .bind(perm)
        .bind(target_user_id)
        .fetch_one(pool)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("scope check failed: {e}")))?;
        if has {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!(
                "{perm} not granted in any scope covering this user"
            )))
        }
    }

    /// Assert the caller has `perm` either globally OR scoped to
    /// `target_team_id`.
    ///
    /// Used by team CRUD and any handler that operates directly on
    /// a team row (rename, delete, edit budget cap targeting the
    /// team).
    pub async fn assert_scope_for_team(
        &self,
        pool: &sqlx::PgPool,
        perm: &str,
        target_team_id: uuid::Uuid,
    ) -> Result<(), AppError> {
        let has: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM rbac_role_assignments ra
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE ra.user_id = $1
                    AND $2 = ANY(r.permissions)
                    AND (
                        ra.scope_kind = 'global'
                        OR (ra.scope_kind = 'team' AND ra.scope_id = $3)
                    )
             )",
        )
        .bind(self.claims.sub)
        .bind(perm)
        .bind(target_team_id)
        .fetch_one(pool)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("scope check failed: {e}")))?;
        if has {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!(
                "{perm} not granted in any scope covering team {target_team_id}"
            )))
        }
    }

    /// Assert the caller has `perm` covering the api_key whose id
    /// is `target_api_key_id`. Resolves the api_key's owner and
    /// delegates to `assert_scope_for_user`.
    pub async fn assert_scope_for_api_key(
        &self,
        pool: &sqlx::PgPool,
        perm: &str,
        target_api_key_id: uuid::Uuid,
    ) -> Result<(), AppError> {
        let owner: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT user_id FROM api_keys WHERE id = $1")
                .bind(target_api_key_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("api_key lookup failed: {e}")))?;
        let owner = owner.ok_or_else(|| AppError::NotFound("API key not found".into()))?;
        self.assert_scope_for_user(pool, perm, owner).await
    }

    /// Polymorphic scope check for the limits engine. The limits
    /// CRUD endpoints are keyed on `(subject_kind, subject_id)`
    /// where `subject_kind ∈ {user, api_key, team, provider, mcp_server}`.
    /// Provider / mcp_server are global resources and only allow
    /// callers with the perm at global scope.
    pub async fn assert_scope_for_subject(
        &self,
        pool: &sqlx::PgPool,
        perm: &str,
        subject_kind: &str,
        subject_id: uuid::Uuid,
    ) -> Result<(), AppError> {
        match subject_kind {
            "user" => self.assert_scope_for_user(pool, perm, subject_id).await,
            "api_key" => self.assert_scope_for_api_key(pool, perm, subject_id).await,
            "team" => self.assert_scope_for_team(pool, perm, subject_id).await,
            "provider" | "mcp_server" => self.assert_scope_global(pool, perm).await,
            other => Err(AppError::BadRequest(format!(
                "unknown subject_kind '{other}' (expected user / api_key / team / provider / mcp_server)"
            ))),
        }
    }

    /// Returns the set of team ids the caller can act on for `perm`.
    ///
    /// Three return shapes encode the three filter cases:
    ///   - `Ok(None)` — caller has `perm` at GLOBAL scope. The list
    ///     handler should NOT add any team filter (caller sees all).
    ///   - `Ok(Some(empty set))` — caller has the perm but only for
    ///     teams they're not actually scoped to. List should be empty.
    ///   - `Ok(Some(non-empty))` — caller has perm only for these
    ///     specific teams. List handler must filter to subjects in
    ///     those teams.
    ///
    /// Used by list endpoints (GET /api/admin/users, etc.) to scope
    /// the result set without leaking other teams' data.
    pub async fn owned_team_scope_for_perm(
        &self,
        pool: &sqlx::PgPool,
        perm: &str,
    ) -> Result<Option<std::collections::HashSet<uuid::Uuid>>, AppError> {
        let global: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM rbac_role_assignments ra
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE ra.user_id = $1
                    AND ra.scope_kind = 'global'
                    AND $2 = ANY(r.permissions)
             )",
        )
        .bind(self.claims.sub)
        .bind(perm)
        .fetch_one(pool)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("scope lookup failed: {e}")))?;
        if global {
            return Ok(None);
        }
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT DISTINCT ra.scope_id
               FROM rbac_role_assignments ra
               JOIN rbac_roles r ON r.id = ra.role_id
              WHERE ra.user_id = $1
                AND ra.scope_kind = 'team'
                AND ra.scope_id IS NOT NULL
                AND $2 = ANY(r.permissions)",
        )
        .bind(self.claims.sub)
        .bind(perm)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("scope lookup failed: {e}")))?;
        Ok(Some(rows.into_iter().map(|(id,)| id).collect()))
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
    // Resolve the credential. Cookie (httpOnly) is preferred for browser
    // sessions — XSS can never exfiltrate it. Bearer header is the fallback
    // for non-browser clients. If the Bearer value is a `tw-` API key with
    // the `console` surface, we do API-key auth instead of JWT auth.
    let token_from_cookie =
        crate::middleware::verify_signature::extract_cookie(&request, "access_token");

    // Resolve as an owned String so we can move `request` later.
    let bearer: String = match token_from_cookie {
        Some(t) => t,
        None => {
            let auth_header = request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(StatusCode::UNAUTHORIZED)?;
            auth_header
                .strip_prefix("Bearer ")
                .ok_or(StatusCode::UNAUTHORIZED)?
                .to_owned()
        }
    };

    // --- API key path ---
    if bearer.starts_with(api_key::KEY_PREFIX) {
        return auth_via_api_key(&state, &bearer, request, next).await;
    }

    // --- JWT session path ---
    let claims = state
        .jwt
        .verify_token(&bearer)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    if claims.token_type != "access" {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Check JWT blacklist (revoked tokens)
    let token_hash = think_watch_auth::jwt::sha2_hash(&bearer);
    if think_watch_auth::jwt::is_revoked(&state.redis, &token_hash).await {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let ip = extract_client_ip(&state, request.headers(), request.extensions()).await;

    if let Some(slot) = request
        .extensions()
        .get::<crate::middleware::access_log::AccessLogUserSlot>()
    {
        let _ = slot.0.set(claims.sub);
    }

    request.extensions_mut().insert(AuthUser { claims, ip });

    Ok(next.run(request).await)
}

/// Authenticate a `tw-` API key against the `console` surface and build a
/// synthetic `AuthUser` from the key owner's current permissions. Inserts
/// `ApiKeyAuthenticated` so `verify_signature` knows to skip HMAC.
async fn auth_via_api_key(
    state: &AppState,
    token: &str,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let key_hash = api_key::hash_api_key(token);

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

    if !row.surfaces.iter().any(|s| s == "console") {
        return Err(StatusCode::FORBIDDEN);
    }

    if let Some(expires_at) = row.expires_at
        && expires_at < chrono::Utc::now()
    {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Service-account keys (no user_id) cannot use the console surface —
    // all console RBAC checks require a user identity.
    let user_id = row.user_id.ok_or(StatusCode::FORBIDDEN)?;

    // Load current permissions from DB (not a snapshot like JWT claims).
    // This means permission changes take effect immediately for API key users.
    let permissions = rbac::compute_user_permissions(&state.db, user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let role_assignments = rbac::compute_user_role_assignments(&state.db, user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let email = sqlx::query_scalar::<_, String>("SELECT email FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let roles = rbac::load_user_role_names(&state.db, user_id)
        .await
        .unwrap_or_default();

    // Best-effort last_used_at update (same pattern as gateway path).
    let db = state.db.clone();
    let key_id = row.id;
    tokio::spawn(async move {
        let _ = sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE id = $1")
            .bind(key_id)
            .execute(&db)
            .await;
    });

    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: user_id,
        email,
        roles,
        permissions,
        role_assignments,
        exp: now + 86400,
        iat: now,
        token_type: "access".into(),
        aud: String::new(),
        iss: String::new(),
    };

    if let Some(slot) = request
        .extensions()
        .get::<crate::middleware::access_log::AccessLogUserSlot>()
    {
        let _ = slot.0.set(user_id);
    }

    request
        .extensions_mut()
        .insert(AuthUser { claims, ip: None });
    request.extensions_mut().insert(ApiKeyAuthenticated);

    Ok(next.run(request).await)
}
