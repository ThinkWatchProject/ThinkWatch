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
    /// User-Agent header captured at middleware time. Populated
    /// alongside `ip` for forensic attribution on audit entries —
    /// account-takeover signatures (TOTP toggle, sessions revoked,
    /// password change) want both the IP and the browser fingerprint.
    pub user_agent: Option<String>,
    /// Flat union of every role's permissions — loaded at request time
    /// from Redis cache (60s TTL) or DB fallback. Never from the JWT.
    pub permissions: Vec<String>,
    /// Permissions explicitly denied by policy documents.
    pub denied_permissions: Vec<String>,
    /// Per-request memo for `owned_team_scope_for_perm`. Some handlers
    /// (e.g. dashboard) call it three times for three different perms;
    /// caching here turns those into one RBAC query each instead of
    /// three round-trips to the same indexed lookup. Wrapped in `Arc`
    /// so clones (axum's per-handler extractor pattern produces them)
    /// share the same map.
    scope_cache: std::sync::Arc<
        tokio::sync::RwLock<
            std::collections::HashMap<String, Option<std::collections::HashSet<uuid::Uuid>>>,
        >,
    >,
}

/// Authenticated-user audit attribution. The trait body lives in
/// `common::audit::AuditActor`; this impl is the single source of
/// truth for what `auth_user.audit("...")` populates. The inherent
/// method below delegates here so they can't diverge — a future
/// change (e.g. adding `session_id`) lands once and reaches every
/// call site automatically.
impl think_watch_common::audit::AuditActor for AuthUser {
    fn audit(&self, action: impl Into<String>) -> AuditEntry {
        #[allow(deprecated)]
        let mut e = AuditEntry::new(action)
            .user_id(self.claims.sub)
            .user_email(&self.claims.email);
        if let Some(ref ip) = self.ip {
            e = e.ip_address(ip.clone());
        }
        if let Some(ref ua) = self.user_agent {
            e = e.user_agent(ua.clone());
        }
        e
    }
}

impl AuthUser {
    /// Inherent shim: `auth_user.audit("...")` resolves without
    /// requiring `use think_watch_common::audit::AuditActor;` at
    /// every call site. Delegates to the trait impl so the two
    /// can't drift — Rust picks the inherent method when both are
    /// visible, but they produce identical output.
    pub fn audit(&self, action: impl Into<String>) -> AuditEntry {
        <Self as think_watch_common::audit::AuditActor>::audit(self, action)
    }

    /// Authorization gate: require the JWT to carry the given permission
    /// key (`resource:action`). This is the authoritative authorization
    /// check — every admin handler calls it at the top.
    ///
    /// The permission set was computed at JWT creation as the union of
    /// every role the user holds (see `rbac::compute_user_permissions`).
    /// Returns `AppError::Forbidden` if the permission is not present.
    pub fn require_permission(&self, perm: &str) -> Result<(), AppError> {
        // Deny always wins over Allow.
        if self.denied_permissions.iter().any(|p| p == perm) {
            return Err(AppError::Forbidden(format!(
                "Permission explicitly denied: {perm}"
            )));
        }
        if self.permissions.iter().any(|p| p == perm) {
            Ok(())
        } else {
            Err(AppError::Forbidden(format!(
                "Missing required permission: {perm}"
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
    // next refresh. Permissions are no longer in the JWT; they are
    // loaded at request time into AuthUser.permissions.
    // ------------------------------------------------------------------------

    /// `require_permission` + `assert_scope_global` in a single call.
    ///
    /// Every admin handler for a global-only resource used to do
    ///
    /// ```ignore
    /// auth_user.require_permission("X")?;
    /// auth_user.assert_scope_global(&state.db, "X").await?;
    /// ```
    ///
    /// where the same permission string was repeated twice and the
    /// scope check could quietly be forgotten when adding a new
    /// handler (which is exactly the regression a recent audit
    /// caught in the log handlers — see `a674934`). This helper
    /// collapses both into one call so missing the scope check
    /// requires actively writing the wrong helper, not just
    /// forgetting to add a line.
    pub async fn require_global_permission(
        &self,
        pool: &sqlx::PgPool,
        perm: &str,
    ) -> Result<(), AppError> {
        self.require_permission(perm)?;
        self.assert_scope_global(pool, perm).await
    }

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
        // Check direct global assignments + team-inherited roles
        // (team-inherited roles act as global scope).
        let has: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM rbac_role_assignments ra
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE ra.user_id = $1
                    AND ra.scope_kind = 'global'
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND EXISTS (
                              SELECT 1
                              FROM jsonb_array_elements_text(
                                  CASE jsonb_typeof(stmt->'Action')
                                      WHEN 'array' THEN stmt->'Action'
                                      ELSE jsonb_build_array(stmt->'Action')
                                  END
                              ) AS act
                              WHERE act = '*' OR act = $2
                                 OR (act LIKE '%*%' AND $2 LIKE replace(act,'*','%'))
                          )
                    )
                 UNION ALL
                 SELECT 1 FROM team_members tm
                   JOIN team_role_assignments tra ON tra.team_id = tm.team_id
                   JOIN rbac_roles r ON r.id = tra.role_id
                  WHERE tm.user_id = $1
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND EXISTS (
                              SELECT 1
                              FROM jsonb_array_elements_text(
                                  CASE jsonb_typeof(stmt->'Action')
                                      WHEN 'array' THEN stmt->'Action'
                                      ELSE jsonb_build_array(stmt->'Action')
                                  END
                              ) AS act
                              WHERE act = '*' OR act = $2
                                 OR (act LIKE '%*%' AND $2 LIKE replace(act,'*','%'))
                          )
                    )
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
        if self.claims.sub == target_user_id {
            return Ok(());
        }
        let has: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM rbac_role_assignments ra
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE ra.user_id = $1
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND EXISTS (
                              SELECT 1
                              FROM jsonb_array_elements_text(
                                  CASE jsonb_typeof(stmt->'Action')
                                      WHEN 'array' THEN stmt->'Action'
                                      ELSE jsonb_build_array(stmt->'Action')
                                  END
                              ) AS act
                              WHERE act = '*' OR act = $2
                                 OR (act LIKE '%*%' AND $2 LIKE replace(act,'*','%'))
                          )
                    )
                    AND (
                        ra.scope_kind = 'global'
                        OR (ra.scope_kind = 'team'
                            AND ra.scope_id IN (
                                SELECT team_id FROM team_members WHERE user_id = $3
                            ))
                    )
                 UNION ALL
                 SELECT 1 FROM team_members tm
                   JOIN team_role_assignments tra ON tra.team_id = tm.team_id
                   JOIN rbac_roles r ON r.id = tra.role_id
                  WHERE tm.user_id = $1
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND EXISTS (
                              SELECT 1
                              FROM jsonb_array_elements_text(
                                  CASE jsonb_typeof(stmt->'Action')
                                      WHEN 'array' THEN stmt->'Action'
                                      ELSE jsonb_build_array(stmt->'Action')
                                  END
                              ) AS act
                              WHERE act = '*' OR act = $2
                                 OR (act LIKE '%*%' AND $2 LIKE replace(act,'*','%'))
                          )
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
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND EXISTS (
                              SELECT 1
                              FROM jsonb_array_elements_text(
                                  CASE jsonb_typeof(stmt->'Action')
                                      WHEN 'array' THEN stmt->'Action'
                                      ELSE jsonb_build_array(stmt->'Action')
                                  END
                              ) AS act
                              WHERE act = '*' OR act = $2
                                 OR (act LIKE '%*%' AND $2 LIKE replace(act,'*','%'))
                          )
                    )
                    AND (
                        ra.scope_kind = 'global'
                        OR (ra.scope_kind = 'team' AND ra.scope_id = $3)
                    )
                 UNION ALL
                 SELECT 1 FROM team_members tm
                   JOIN team_role_assignments tra ON tra.team_id = tm.team_id
                   JOIN rbac_roles r ON r.id = tra.role_id
                  WHERE tm.user_id = $1
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND EXISTS (
                              SELECT 1
                              FROM jsonb_array_elements_text(
                                  CASE jsonb_typeof(stmt->'Action')
                                      WHEN 'array' THEN stmt->'Action'
                                      ELSE jsonb_build_array(stmt->'Action')
                                  END
                              ) AS act
                              WHERE act = '*' OR act = $2
                                 OR (act LIKE '%*%' AND $2 LIKE replace(act,'*','%'))
                          )
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

    /// Polymorphic scope check for the limits engine. The limits
    /// CRUD endpoints are keyed on `(subject_kind, subject_id)`
    /// where `subject_kind ∈ {user, api_key, role}`. All three are
    /// admin-level writes and, for now, require the perm at global
    /// scope — team-scoped grants are not enough to mutate another
    /// team's user or key. A future revision could relax `user` /
    /// `api_key` to allow team_manager-style scoping by looking up
    /// the subject's team membership, but that's not needed today.
    pub async fn assert_scope_for_subject(
        &self,
        pool: &sqlx::PgPool,
        perm: &str,
        subject_kind: &str,
        _subject_id: uuid::Uuid,
    ) -> Result<(), AppError> {
        match subject_kind {
            "role" | "user" | "api_key" => self.assert_scope_global(pool, perm).await,
            other => Err(AppError::BadRequest(format!(
                "unknown subject_kind '{other}' (expected: user, api_key, role)"
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
        // Per-request memo so repeated calls (dashboard hits this 3x
        // for 3 perms) collapse to a single DB round-trip per perm.
        if let Some(cached) = self.scope_cache.read().await.get(perm) {
            return Ok(cached.clone());
        }
        // Team-inherited roles grant global scope, so check both paths.
        let global: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM rbac_role_assignments ra
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE ra.user_id = $1
                    AND ra.scope_kind = 'global'
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND EXISTS (
                              SELECT 1
                              FROM jsonb_array_elements_text(
                                  CASE jsonb_typeof(stmt->'Action')
                                      WHEN 'array' THEN stmt->'Action'
                                      ELSE jsonb_build_array(stmt->'Action')
                                  END
                              ) AS act
                              WHERE act = '*' OR act = $2
                                 OR (act LIKE '%*%' AND $2 LIKE replace(act,'*','%'))
                          )
                    )
                 UNION ALL
                 SELECT 1 FROM team_members tm
                   JOIN team_role_assignments tra ON tra.team_id = tm.team_id
                   JOIN rbac_roles r ON r.id = tra.role_id
                  WHERE tm.user_id = $1
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND EXISTS (
                              SELECT 1
                              FROM jsonb_array_elements_text(
                                  CASE jsonb_typeof(stmt->'Action')
                                      WHEN 'array' THEN stmt->'Action'
                                      ELSE jsonb_build_array(stmt->'Action')
                                  END
                              ) AS act
                              WHERE act = '*' OR act = $2
                                 OR (act LIKE '%*%' AND $2 LIKE replace(act,'*','%'))
                          )
                    )
             )",
        )
        .bind(self.claims.sub)
        .bind(perm)
        .fetch_one(pool)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("scope lookup failed: {e}")))?;
        if global {
            self.scope_cache
                .write()
                .await
                .insert(perm.to_string(), None);
            return Ok(None);
        }
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT DISTINCT ra.scope_id
               FROM rbac_role_assignments ra
               JOIN rbac_roles r ON r.id = ra.role_id
              WHERE ra.user_id = $1
                AND ra.scope_kind = 'team'
                AND ra.scope_id IS NOT NULL
                AND EXISTS (
                    SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                    WHERE stmt->>'Effect' = 'Allow'
                      AND EXISTS (
                          SELECT 1
                          FROM jsonb_array_elements_text(
                              CASE jsonb_typeof(stmt->'Action')
                                  WHEN 'array' THEN stmt->'Action'
                                  ELSE jsonb_build_array(stmt->'Action')
                              END
                          ) AS act
                          WHERE act = '*' OR act = $2
                             OR (act LIKE '%*%' AND $2 LIKE replace(act,'*','%'))
                      )
                )",
        )
        .bind(self.claims.sub)
        .bind(perm)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("scope lookup failed: {e}")))?;
        let scope: std::collections::HashSet<uuid::Uuid> =
            rows.into_iter().map(|(id,)| id).collect();
        self.scope_cache
            .write()
            .await
            .insert(perm.to_string(), Some(scope.clone()));
        Ok(Some(scope))
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
    let connection_ip = extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string());
    resolve_client_ip(
        &state.dynamic_config,
        headers,
        connection_ip,
        state.config.trusted_proxy_secret.as_deref(),
    )
    .await
}

/// Core IP resolution. Same trust contract as [`extract_client_ip`] but
/// takes the already-extracted connection IP directly so it can be
/// called from middleware (like `access_log`) that doesn't have an
/// `AppState` in scope. KEEP THIS THE ONLY READER of `client_ip_source`
/// / `client_ip_xff_*` / `security.trusted_proxies` — every duplicate
/// copy of this logic is one more place a forwarded-IP spoofing bug
/// can hide (caught by audit-IP review when access_log had its own
/// copy that ignored `trusted_proxies` entirely).
pub async fn resolve_client_ip(
    dc: &think_watch_common::dynamic_config::DynamicConfig,
    headers: &axum::http::HeaderMap,
    connection_ip: Option<String>,
    // Shared secret from `AppConfig::trusted_proxy_secret`. `None` when
    // no reverse proxy is configured, in which case only the address
    // whitelist can grant trust.
    proxy_secret: Option<&str>,
) -> Option<String> {
    // The secret is itself the statement "a proxy of ours is in front
    // of you", which makes the TCP peer address definitionally useless
    // — it is the proxy. So a valid secret enables forwarded-IP
    // resolution even under the default `connection` source, and the
    // bundled reverse-proxy deployment is correct with no settings to
    // discover. Without the secret, `connection` means what it says.
    let has_secret = presents_proxy_secret(headers, proxy_secret);
    let ip_source = dc.client_ip_source().await;
    if ip_source == "connection" && !has_secret {
        return connection_ip;
    }

    // Believing a client-supplied header requires proof that it came
    // from infrastructure we control. Without that, any request could
    // forge the IP used for rate limiting, audit logging, PoW
    // difficulty and session binding — so an unproven request falls
    // back to the TCP peer address.
    //
    // Two proofs are accepted. The secret is the one to prefer: a
    // proxy's address is not a stable fact (container IPs change on
    // every recreate; under k8s or a mesh they aren't knowable in
    // advance), while a shared secret survives all of that. The address
    // whitelist stays for deployments that already rely on it.
    let trusted_proxies = dc
        .get_string("security.trusted_proxies")
        .await
        .unwrap_or_default();
    let conn_ip = connection_ip.as_deref().unwrap_or("");
    let whitelisted = trusted_proxies
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|proxy| proxy == conn_ip || proxy == "*");

    if !whitelisted && !has_secret {
        tracing::warn!(
            ip_source = %ip_source,
            connection_ip = conn_ip,
            "Forwarded-IP header ignored: request carries no valid proxy secret and its \
             address is not in security.trusted_proxies"
        );
        return connection_ip;
    }

    // `connection` here means the operator never configured a source
    // but our proxy vouched for the request — read the header it sets.
    let effective_source = if ip_source == "connection" {
        "x-real-ip".to_string()
    } else {
        ip_source
    };
    forwarded_ip(&effective_source, dc, headers)
        .await
        .or(connection_ip)
}

/// Header carrying the shared secret. Named for this project so it
/// can't be confused with a header some other hop already sets.
const PROXY_SECRET_HEADER: &str = "x-thinkwatch-proxy-token";

/// Did this request come through our own reverse proxy?
///
/// The comparison is constant-time: a byte-by-byte early exit would let
/// an attacker who can hit the port directly recover the secret one
/// character at a time from response timing, and that secret is what
/// stands between them and forging every IP the system records.
fn presents_proxy_secret(headers: &axum::http::HeaderMap, secret: Option<&str>) -> bool {
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return false;
    };
    let Some(presented) = headers
        .get(PROXY_SECRET_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    if presented.len() != secret.len() {
        return false;
    }
    presented
        .bytes()
        .zip(secret.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Pull the client address out of the forwarded headers, per the
/// configured source and (for XFF) which hop to read.
async fn forwarded_ip(
    source: &str,
    dc: &think_watch_common::dynamic_config::DynamicConfig,
    headers: &axum::http::HeaderMap,
) -> Option<String> {
    match source {
        "xff" => {
            let position = dc.client_ip_xff_position().await;
            let depth = dc.client_ip_xff_depth().await.max(1) as usize;
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
                    // A malformed header like ", 1.2.3.4" trims to an
                    // empty leading slot — drop it so the audit row
                    // doesn't read as "we had an IP" with no payload.
                    idx.and_then(|i| parts.get(i))
                        .map(|s| s.to_string())
                        .filter(|s| !s.is_empty())
                })
        }
        "x-real-ip" => headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            // An empty-or-whitespace header value passes the to_str
            // check but yields a blank string after trim — same
            // "present but absent" failure mode as the xff branch.
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// Load permissions from Redis cache (`user_perms:{user_id}`, 60s TTL)
/// with DB fallback. Returns `(permissions, denied_permissions)`.
async fn load_user_permissions_cached(
    state: &AppState,
    user_id: uuid::Uuid,
) -> Result<(Vec<String>, Vec<String>), anyhow::Error> {
    use fred::interfaces::KeysInterface;

    let cache_key = format!("user_perms:{user_id}");
    let cached: Option<String> = state.redis.get(&cache_key).await.ok().flatten();

    if let Some(json) = cached
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&json)
    {
        let perms: Vec<String> = val
            .get("p")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let denied: Vec<String> = val
            .get("d")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        return Ok((perms, denied));
    }

    // Cache miss — compute from DB
    let all_perm_keys = crate::handlers::roles::all_permission_keys();
    let perms = rbac::compute_user_permissions(&state.db, user_id, &all_perm_keys).await?;
    let denied = rbac::compute_denied_permissions(&state.db, user_id, &perms).await?;

    // Best-effort cache write (60s TTL)
    let cache_val = serde_json::json!({"p": perms, "d": denied});
    let _: Result<(), _> = state
        .redis
        .set(
            &cache_key,
            cache_val.to_string(),
            Some(fred::types::Expiration::EX(60)),
            None,
            false,
        )
        .await;

    Ok((perms, denied))
}

/// Invalidate the cached permissions for a single user.
pub async fn invalidate_user_perms(redis: &fred::clients::Client, user_id: uuid::Uuid) {
    let _: Result<i64, _> =
        fred::interfaces::KeysInterface::del(redis, &format!("user_perms:{user_id}")).await;
}

/// Invalidate cached permissions for ALL users who hold a given role
/// (directly or via team membership).
pub async fn invalidate_role_perms(
    db: &sqlx::PgPool,
    redis: &fred::clients::Client,
    role_id: uuid::Uuid,
) {
    // Direct assignments
    let direct: Vec<(uuid::Uuid,)> =
        sqlx::query_as("SELECT DISTINCT user_id FROM rbac_role_assignments WHERE role_id = $1")
            .bind(role_id)
            .fetch_all(db)
            .await
            .unwrap_or_default();
    // Team-inherited
    let team: Vec<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT DISTINCT tm.user_id FROM team_role_assignments tra \
         JOIN team_members tm ON tm.team_id = tra.team_id \
         WHERE tra.role_id = $1",
    )
    .bind(role_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    for (uid,) in direct.into_iter().chain(team) {
        invalidate_user_perms(redis, uid).await;
    }
}

/// Invalidate cached permissions for ALL members of a team.
pub async fn invalidate_team_perms(
    db: &sqlx::PgPool,
    redis: &fred::clients::Client,
    team_id: uuid::Uuid,
) {
    let members: Vec<(uuid::Uuid,)> =
        sqlx::query_as("SELECT user_id FROM team_members WHERE team_id = $1")
            .bind(team_id)
            .fetch_all(db)
            .await
            .unwrap_or_default();
    for (uid,) in members {
        invalidate_user_perms(redis, uid).await;
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
    let token_from_cookie = crate::middleware::verify_signature::extract_cookie(
        &request,
        crate::middleware::verify_signature::ACCESS_COOKIE_NAME,
    );

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

    // Check JWT blacklist (revoked tokens). Fail-closed on Redis
    // errors: if we can't verify the blacklist, deny the request
    // rather than silently accepting potentially-revoked tokens.
    let token_hash = think_watch_auth::jwt::sha2_hash(&bearer);
    match think_watch_auth::jwt::is_revoked(&state.redis, &token_hash).await {
        Ok(true) => return Err(StatusCode::UNAUTHORIZED),
        Ok(false) => {}
        Err(e) => {
            tracing::error!(error = %e, "Redis unavailable during JWT blacklist check");
            metrics::counter!("auth_jwt_blacklist_redis_error_total").increment(1);
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    // Reject access tokens issued before a password change / force-logout
    // / user deletion. `pw_epoch:{user_id}` is set by those flows; any
    // access token whose `iat` predates the epoch must be refused.
    // Previously the refresh handler was the only gate, so a deleted or
    // password-rotated user kept working access tokens until the access
    // TTL elapsed (default 15 min). Fail-closed on Redis errors for the
    // same reason as the blacklist check.
    let epoch_key = format!("pw_epoch:{}", claims.sub);
    use fred::interfaces::KeysInterface;
    match state.redis.get::<Option<String>, _>(&epoch_key).await {
        Ok(Some(epoch_str)) => {
            // `<=` not `<` — JWT `iat` is whole-seconds. An access token
            // minted in the same second a password change / force-logout
            // / account-delete fired must also be refused; the refresh
            // handler uses the same comparison for the same reason.
            if let Ok(epoch) = epoch_str.parse::<i64>()
                && claims.iat <= epoch
            {
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::error!(error = %e, "Redis unavailable during pw_epoch check");
            metrics::counter!("auth_pw_epoch_redis_error_total").increment(1);
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    // Defense against Redis flush + stale JWT for a disabled or
    // deleted user. The pw_epoch check above is the fast path —
    // when set, it correctly rejects the token. But pw_epoch lives
    // in Redis; after a flush / restart the key vanishes while
    // existing access JWTs survive their natural TTL (up to 15 min
    // default). A user disabled or soft-deleted before the flush
    // would silently start authenticating again until expiry. One
    // indexed PK lookup per request closes the gap — cost is sub-ms
    // and only on the auth path.
    let user_active: Option<bool> =
        sqlx::query_scalar("SELECT is_active FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(claims.sub)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "DB check for users.is_active failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    if !matches!(user_active, Some(true)) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let ip = extract_client_ip(&state, request.headers(), request.extensions()).await;
    // Mirror the empty-string filter that `extract_client_ip` applies
    // (commit 10ee89d): a `User-Agent:` header with an empty or
    // whitespace-only value passes `to_str()` but would land as
    // `Some("")` in the audit row — same "present but blank" failure
    // mode as the IP path.
    let user_agent = request
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Load permissions from Redis cache (60s TTL) → DB fallback.
    let (permissions, denied_permissions) = load_user_permissions_cached(&state, claims.sub)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if let Some(slot) = request
        .extensions()
        .get::<crate::middleware::access_log::AccessLogUserSlot>()
    {
        let _ = slot
            .0
            .set(crate::middleware::access_log::AccessLogUserInfo {
                user_id: claims.sub,
                user_email: Some(claims.email.clone()),
            });
    }

    request.extensions_mut().insert(AuthUser {
        claims,
        ip,
        user_agent,
        permissions,
        denied_permissions,
        scope_cache: std::sync::Arc::new(
            tokio::sync::RwLock::new(std::collections::HashMap::new()),
        ),
    });

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

    // Same lazy inactivity cutoff as require_api_key — see
    // middleware::api_key_auth for the rationale.
    let global_inactivity_days = state
        .dynamic_config
        .api_keys_inactivity_timeout_days()
        .await;
    // JOIN users so a deleted / disabled user's API key stops working
    // immediately on the console surface, matching the gateway path
    // in `api_key_auth::require_api_key`. Without this, the H8 fix
    // ("clear MCP cache + sessions on user delete") is silently
    // bypassed — the deleted user's API key keeps minting admin
    // requests until the key itself expires.
    let row = sqlx::query_as::<_, think_watch_common::models::ApiKey>(
        r#"SELECT api_keys.* FROM api_keys
           JOIN users ON users.id = api_keys.user_id
           WHERE api_keys.key_hash = $1
             AND api_keys.deleted_at IS NULL
             AND users.is_active = true
             AND users.deleted_at IS NULL
             AND (
                 api_keys.is_active = true
                 OR (api_keys.grace_period_ends_at IS NOT NULL
                     AND api_keys.grace_period_ends_at > now())
             )
             AND (
                 api_keys.last_used_at IS NULL
                 OR api_keys.last_used_at > now() - CASE
                     WHEN COALESCE(api_keys.inactivity_timeout_days, 0) > 0
                         THEN make_interval(days => api_keys.inactivity_timeout_days::int)
                     WHEN $2::bigint > 0
                         THEN make_interval(days => $2::int)
                     ELSE interval '999999 days'
                 END
             )"#,
    )
    .bind(&key_hash)
    .bind(global_inactivity_days)
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

    // Per-key soft rate limit on the console surface. A compromised key
    // shouldn't be able to spam admin endpoints at machine speed — cap
    // at ~2 req/s with the same fixed-window counter used elsewhere. Soft: a Redis hiccup lets traffic through rather than
    // locking admins out mid-investigation.
    const CONSOLE_API_KEY_LIMIT_PER_MIN: u32 = 120;
    let rl_key = format!("rl:console_key:{}", row.id);
    if let Ok(count) = think_watch_common::fixed_window::incr(&state.redis, &rl_key, 60).await
        && count > CONSOLE_API_KEY_LIMIT_PER_MIN as u64
    {
        tracing::warn!(
            api_key_id = %row.id,
            count,
            "console API key exceeded per-minute soft rate limit"
        );
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    // Service-account keys (no user_id) cannot use the console surface —
    // all console RBAC checks require a user identity.
    let user_id = row.user_id.ok_or(StatusCode::FORBIDDEN)?;

    // Load current permissions from DB (not a snapshot like JWT claims).
    // This means permission changes take effect immediately for API key users.
    let all_perm_keys = crate::handlers::roles::all_permission_keys();
    let permissions = rbac::compute_user_permissions(&state.db, user_id, &all_perm_keys)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let denied_permissions = rbac::compute_denied_permissions(&state.db, user_id, &permissions)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let email = sqlx::query_scalar::<_, String>(
        "SELECT email FROM users WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::UNAUTHORIZED)?;

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
        let _ = slot
            .0
            .set(crate::middleware::access_log::AccessLogUserInfo {
                user_id,
                user_email: Some(claims.email.clone()),
            });
    }

    // Populate IP + UA on the API-key path too — `.audit()` is now
    // the canonical audit-emission helper, and the commit message of
    // fac2538 promised "correct forensic attribution by construction"
    // for every caller. Without these the promise breaks silently
    // for API-key-authenticated console actions (admin operations,
    // service-to-service callers), which are exactly the surface
    // forensics needs to cover for programmatic abuse.
    let ip = extract_client_ip(state, request.headers(), request.extensions()).await;
    let user_agent = request
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    request.extensions_mut().insert(AuthUser {
        claims,
        ip,
        user_agent,
        permissions,
        denied_permissions,
        scope_cache: std::sync::Arc::new(
            tokio::sync::RwLock::new(std::collections::HashMap::new()),
        ),
    });
    request.extensions_mut().insert(ApiKeyAuthenticated);

    Ok(next.run(request).await)
}

#[cfg(test)]
mod proxy_trust_tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers_with(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(PROXY_SECRET_HEADER, HeaderValue::from_str(token).unwrap());
        h
    }

    #[test]
    fn the_secret_is_what_grants_trust() {
        let secret = "a3f1c0de";
        assert!(presents_proxy_secret(&headers_with(secret), Some(secret)));
    }

    #[test]
    fn a_client_that_guesses_the_header_name_gains_nothing() {
        // The whole point: knowing where to put the token is not
        // knowing the token.
        assert!(!presents_proxy_secret(
            &headers_with("not-the-secret"),
            Some("a3f1c0de")
        ));
        assert!(!presents_proxy_secret(&HeaderMap::new(), Some("a3f1c0de")));
    }

    #[test]
    fn no_configured_secret_means_no_request_can_claim_proxy_trust() {
        // A deployment with no proxy must not be talked into believing
        // forwarded headers by a request that simply asserts it is one.
        assert!(!presents_proxy_secret(&headers_with("anything"), None));
        assert!(!presents_proxy_secret(&headers_with(""), Some("")));
    }

    #[test]
    fn a_prefix_of_the_secret_is_not_the_secret() {
        // Length is checked before the byte compare; a truncated guess
        // must not pass, and must not reveal how much of it was right.
        assert!(!presents_proxy_secret(
            &headers_with("a3f1"),
            Some("a3f1c0de")
        ));
        assert!(!presents_proxy_secret(
            &headers_with("a3f1c0de00"),
            Some("a3f1c0de")
        ));
    }
}
