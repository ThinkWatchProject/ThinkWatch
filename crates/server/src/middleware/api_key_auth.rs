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

use think_watch_common::limits::SurfaceConstraints;

use crate::app::AppState;

/// Intersect a per-API-key allow-list with a per-role allow-list.
///
/// Both inputs are nullable, where `None` means "unrestricted":
///   - key=None, role=None    → None (unrestricted)
///   - key=Some, role=None    → key (role doesn't tighten)
///   - key=None, role=Some    → role (key doesn't tighten)
///   - key=Some, role=Some    → the entries of either list that the
///     other list covers
///
/// Entries are patterns, not literals (a model entry is a prefix, an MCP
/// entry may be `<server>__*`), so the intersection keeps an entry of
/// one side when some entry of the other side covers it: a key narrowed
/// to `gpt-4o-mini` under a role granting `gpt-4o` keeps `gpt-4o-mini`.
/// An empty result allows nothing.
///
/// Intersection (not union) is the right merge here because the
/// per-key list is a tightening of what the user as a whole can do
/// — an admin who restricts a developer's API key to gpt-4o-mini
/// shouldn't have that overridden by the role's broader list.
fn intersect_allowlists(
    key_list: Option<Vec<String>>,
    role_list: Option<Vec<String>>,
    covers: fn(&str, &str) -> bool,
) -> Option<Vec<String>> {
    match (key_list, role_list) {
        (None, None) => None,
        (Some(k), None) => Some(k),
        (None, Some(r)) => Some(r),
        (Some(k), Some(r)) => {
            let mut out = std::collections::BTreeSet::new();
            for (side, other) in [(&k, &r), (&r, &k)] {
                for entry in side {
                    if other.iter().any(|g| covers(g, entry)) {
                        out.insert(entry.clone());
                    }
                }
            }
            Some(out.into_iter().collect())
        }
    }
}

/// Model entries match by prefix (`is_access_allowed` in the gateway
/// lifecycle): `general` covers every model that `specific` covers.
fn model_entry_covers(general: &str, specific: &str) -> bool {
    specific.starts_with(general)
}

/// MCP tool patterns (`*`, `<server>__*`, `<server>__<tool>`):
/// `general` covers every tool that `specific` matches.
fn mcp_entry_covers(general: &str, specific: &str) -> bool {
    use think_watch_mcp_gateway::access_control::is_tool_allowed;
    if general == "*" || general == specific {
        return true;
    }
    if specific == "*" || specific.ends_with("__*") {
        return false;
    }
    is_tool_allowed(Some(&[general.to_string()]), specific)
}

/// The format an AI-gateway caller reads errors in, from the path it
/// called.
fn client_dialect(path: &str) -> tw_dialect::ir::Dialect {
    use tw_dialect::ir::Dialect;
    if path == "/v1/messages" {
        Dialect::Anthropic
    } else if path.starts_with("/v1beta/") || path.starts_with("/v1/models/") {
        Dialect::Gemini
    } else if path == "/v1/responses" {
        Dialect::Responses
    } else {
        Dialect::Chat
    }
}

/// 403 for a key whose owner holds no role granting `surface`'s
/// `*_gateway:use`. The body is in the shape the caller's SDK reads:
/// the protocol's error object on the AI gateway (every one of them
/// carries `error.message`), a JSON-RPC error on the MCP gateway.
fn gateway_use_refused(surface: &str, path: &str) -> Response {
    use axum::response::IntoResponse;

    let permission = format!("{surface}:use");
    let message =
        format!("Access denied: no role held by the owner of this API key grants {permission}.");
    let (content_type, body) = if surface == "mcp_gateway" {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32001, "message": message},
        });
        ("application/json", body.to_string().into_bytes())
    } else {
        (
            "application/json",
            tw_dialect::convert::error_body(
                client_dialect(path),
                StatusCode::FORBIDDEN.as_u16(),
                &message,
            ),
        )
    };
    (
        StatusCode::FORBIDDEN,
        [(axum::http::header::CONTENT_TYPE, content_type)],
        body,
    )
        .into_response()
}

/// The limits a request is held to: its owner's (role limits with the
/// user's overrides) and the calling key's own, counted apart.
async fn load_request_limits(
    db: &sqlx::PgPool,
    user_id: uuid::Uuid,
    lineage_id: uuid::Uuid,
) -> Result<(SurfaceConstraints, SurfaceConstraints), sqlx::Error> {
    let user = rbac::compute_user_surface_constraints(db, user_id).await?;
    let key = rbac::compute_key_surface_constraints(db, lineage_id).await?;
    Ok((user, key))
}

/// The label a request refused for want of its limits carries — the
/// limits could not be loaded, so they could not be checked.
const LIMITS_UNAVAILABLE: &str = "limits_unavailable";

/// What a request goes on with when loading its limits failed, decided
/// by `security.rate_limit_fail_closed` — the setting the rate limiter
/// and the budget gate follow when Redis fails. Failing closed, `None`:
/// the request is refused ([`limits_unavailable`]). Failing open, no
/// limits for this request, and a warning that says why.
fn limits_or_refusal(
    loaded: Result<(SurfaceConstraints, SurfaceConstraints), sqlx::Error>,
    fail_closed: bool,
    api_key_id: uuid::Uuid,
) -> Option<(SurfaceConstraints, SurfaceConstraints)> {
    let error = match loaded {
        Ok(limits) => return Some(limits),
        Err(e) => e,
    };
    if fail_closed {
        metrics::counter!("gateway_limits_load_fail_closed_total").increment(1);
        tracing::error!(
            %api_key_id,
            error = %error,
            "loading the request's limits failed; failing closed per security.rate_limit_fail_closed"
        );
        None
    } else {
        metrics::counter!("gateway_limits_load_fail_open_total").increment(1);
        tracing::warn!(
            %api_key_id,
            error = %error,
            "loading the request's limits failed; failing open, the request runs without limits"
        );
        Some(Default::default())
    }
}

/// The refusal for a request whose limits could not be loaded while
/// failing closed: the same answer the gateways give when the rate
/// limiter itself is unreachable — 429 with `Retry-After`, labelled
/// [`LIMITS_UNAVAILABLE`], in the caller's format.
fn limits_unavailable(surface: &str, path: &str) -> Response {
    use axum::response::IntoResponse;
    let refusal = think_watch_gateway::error::GatewayError::limiter_unavailable(LIMITS_UNAVAILABLE);
    if surface == "mcp_gateway" {
        let retry_after = refusal.retry_after_secs().unwrap_or(30);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {
                "code": think_watch_mcp_gateway::proxy::INVALID_REQUEST,
                "message": refusal.to_string(),
            },
        });
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/json".to_string(),
                ),
                (axum::http::header::RETRY_AFTER, retry_after.to_string()),
            ],
            body.to_string(),
        )
            .into_response();
    }
    think_watch_gateway::proxy::GatewayErrorResponse::from(refusal)
        .in_dialect(client_dialect(path))
        .into_response()
}

/// The key a client presented, wherever its SDK puts it.
///
/// `Authorization: Bearer` (OpenAI's SDKs, Claude Code with
/// `ANTHROPIC_AUTH_TOKEN`), `x-api-key` (Anthropic's SDKs),
/// `x-goog-api-key` or `?key=` (Gemini's). Headers first: a key in the
/// query ends up in access logs and browser history, and is accepted only
/// because Gemini's REST form sends it there. The query never reaches an
/// upstream — requests go out with the paths and queries the gateway
/// builds.
fn presented_key<'a>(
    headers: &'a axum::http::HeaderMap,
    query: Option<&'a str>,
) -> Option<&'a str> {
    let header = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
    };
    header("x-api-key")
        .or_else(|| header("x-goog-api-key"))
        .or_else(|| {
            header(AUTHORIZATION.as_str())
                .and_then(|v| v.strip_prefix("Bearer "))
                .filter(|v| !v.is_empty())
        })
        .or_else(|| {
            query?
                .split('&')
                .find_map(|kv| kv.strip_prefix("key="))
                .filter(|v| !v.is_empty())
        })
}

/// Future returned by the middleware closure. Boxed because the
/// generated impl trait isn't nameable; pulled out into a type
/// alias to keep clippy::type_complexity happy.
type AuthFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, StatusCode>> + Send>>;

/// When the key stops authenticating: its expiry, or the end of its
/// rotation grace period (set on a key that a rotation replaced),
/// whichever comes first.
fn stops_authenticating_at(
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    grace_period_ends_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    match (expires_at, grace_period_ends_at) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// What the request does with the key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyUse {
    /// Calls the gateway: a model, a model listing, an MCP tool.
    Call,
    /// Reads about the key itself (`GET /v1/usage`).
    Read,
}

/// Build a middleware that authenticates requests via `tw-` prefixed
/// API keys and additionally requires the key's `surfaces` array to
/// contain `surface`.
///
/// One layer per gateway: the AI router mounts `require_api_key("ai_gateway")`,
/// the MCP router mounts `require_api_key("mcp_gateway")`. Same key
/// shape and same lookup logic; only the surface check differs.
///
/// The gateway data path is API-key-only. The console API accepts
/// JWT for interactive admin users.
pub fn require_api_key(
    surface: &'static str,
) -> impl Fn(State<AppState>, Request, Next) -> AuthFuture + Clone {
    authenticate(surface, KeyUse::Call)
}

/// [`require_api_key`] for reads about the calling key itself
/// (`GET /v1/usage`): the same lookup, checks and refusals, but reading
/// about a key is not a use of it. `last_used_at`, which the inactivity
/// cutoff reads, stays as it was — a client polling its usage does not
/// keep an otherwise idle key alive — and a client that leaves early
/// leaves no `gateway_logs` row.
pub fn require_api_key_to_read(
    surface: &'static str,
) -> impl Fn(State<AppState>, Request, Next) -> AuthFuture + Clone {
    authenticate(surface, KeyUse::Read)
}

fn authenticate(
    surface: &'static str,
    key_use: KeyUse,
) -> impl Fn(State<AppState>, Request, Next) -> AuthFuture + Clone {
    move |State(state): State<AppState>, mut request: Request, next: Next| {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let token = presented_key(request.headers(), request.uri().query())
                .ok_or(StatusCode::UNAUTHORIZED)?
                .to_string();
            let token = token.as_str();

            // Reject anything that doesn't look like a `tw-` key. The
            // separate JWT fallback path is gone — gateway data
            // requests must use a real API key.
            if !token.starts_with(api_key::KEY_PREFIX) {
                return Err(StatusCode::UNAUTHORIZED);
            }

            let key_hash = api_key::hash_api_key(token);

            // Active keys OR keys still inside their rotation grace period,
            // with inactivity cutoff applied lazily at auth time so a
            // compromised-but-idle key cannot be used during the up-to-10-
            // minute window between lifecycle-sweeper ticks. Per-key
            // `inactivity_timeout_days` overrides the global setting when
            // non-zero; otherwise the global applies.
            let global_inactivity_days = state
                .dynamic_config
                .api_keys_inactivity_timeout_days()
                .await;
            // Defense-in-depth `users` join: even if `update_user` /
            // `delete_user` ever miss the api_keys cascade, a key for
            // a disabled or deleted user is rejected here. The
            // console-facing `auth_via_api_key` path (auth_guard.rs)
            // has the matching join — the two paths MUST stay in sync,
            // otherwise the wrong half-fix lets a deleted user's API
            // key keep working on whichever surface drifted.
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

            // From here on a client that leaves before the handler has a
            // response still leaves a gateway_logs row (the MCP surface
            // records its own). Every return below produces a response
            // or an auth refusal, so the guard is disarmed after all of
            // them; only a dropped future leaves it armed.
            let cancel = (surface == "ai_gateway" && key_use == KeyUse::Call).then(|| {
                think_watch_gateway::proxy::EarlyCancel::arm(
                    state.audit.clone(),
                    GatewayRequestIdentity {
                        user_id: row.user_id.map(|u| u.to_string()),
                        api_key_id: Some(row.id.to_string()),
                        api_key_lineage_id: Some(row.lineage_id.to_string()),
                        ..Default::default()
                    },
                    started,
                )
            });
            let result: Result<Response, StatusCode> = async {
                // Update last_used_at (best-effort, don't block on failure)
                if key_use == KeyUse::Call {
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
                }

                // Compute the user's role-derived constraints and intersect
                // with the API-key allow-list. The role union is loaded once
                // per request — fast enough at our scale.
                //
                // We also pull the role NAMES so the MCP access controller
                // can gate per-tool access without re-querying the DB, and
                // the aggregated `surface_constraints` JSON so the gateway
                // hot path has rate limits + budgets without further lookups.
                let (role_limits, user_roles, loaded_limits) = if let Some(uid) = row.user_id {
                    let limits = rbac::compute_user_resource_limits(&state.db, uid)
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    let names = rbac::load_user_role_names(&state.db, uid)
                        .await
                        .unwrap_or_default();
                    // The user's limits and the key's own, kept apart:
                    // the gateway counts each on its own counters and
                    // checks both. A failure to load them is decided
                    // below, once the request is known to be allowed.
                    let loaded = load_request_limits(&state.db, uid, row.lineage_id).await;
                    (limits, names, loaded)
                } else {
                    // A key without an owner (its user row was removed
                    // and `user_id` set NULL) has no roles to grant
                    // gateway use. The users JOIN above already rejects
                    // such keys; refuse here as well rather than treat
                    // "no roles" as "no restrictions".
                    (
                        rbac::UserResourceLimits::none(),
                        Vec::new(),
                        Ok(Default::default()),
                    )
                };

                // Gateway use itself. A key is only as good as its owner's
                // roles: without one that grants this surface's
                // `*_gateway:use`, nothing behind the gateway is reachable.
                let surface_granted = match surface {
                    "ai_gateway" => role_limits.ai_gateway,
                    "mcp_gateway" => role_limits.mcp_gateway,
                    _ => false,
                };
                if !surface_granted {
                    tracing::warn!(
                        api_key_id = %row.id,
                        user_id = ?row.user_id,
                        surface,
                        "API key owner holds no role granting gateway use"
                    );
                    return Ok(gateway_use_refused(surface, request.uri().path()));
                }

                // Limits that could not be loaded are not "no limits":
                // the request is refused or let through without them as
                // `security.rate_limit_fail_closed` says.
                let fail_closed = match &loaded_limits {
                    Ok(_) => false,
                    Err(_) => state.dynamic_config.rate_limit_fail_closed().await,
                };
                let Some((surface_constraints, key_constraints)) =
                    limits_or_refusal(loaded_limits, fail_closed, row.id)
                else {
                    let user_id = row.user_id.map(|u| u.to_string());
                    let api_key_id = row.id.to_string();
                    let lineage_id = row.lineage_id.to_string();
                    state.audit.log(
                        think_watch_common::audit::AuditActor::audit(
                            &think_watch_common::audit::GatewayActor {
                                user_id: user_id.as_deref(),
                                user_email: None,
                                api_key_id: Some(&api_key_id),
                                api_key_lineage_id: Some(&lineage_id),
                                ip: None,
                                session_id: None,
                            },
                            LIMITS_UNAVAILABLE,
                        )
                        .detail(serde_json::json!({ "surface": surface })),
                    );
                    return Ok(limits_unavailable(surface, request.uri().path()));
                };

                let merged_models = intersect_allowlists(
                    row.allowed_models.clone(),
                    role_limits.allowed_models,
                    model_entry_covers,
                );
                let merged_mcp_tools = intersect_allowlists(
                    row.allowed_mcp_tools.clone(),
                    role_limits.allowed_mcp_tools,
                    mcp_entry_covers,
                );

                // Load email for template header resolution ({{user_email}})
                let user_email: Option<String> = if let Some(uid) = row.user_id {
                    sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
                        .bind(uid)
                        .fetch_optional(&state.db)
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };

                // Resolve client IP once, share across both identities so
                // gateway_logs and mcp_logs see the same value the rest
                // of the auth stack uses (honours client_ip_source +
                // trusted_proxies via auth_guard::extract_client_ip).
                let client_ip = crate::middleware::auth_guard::extract_client_ip(
                    &state,
                    request.headers(),
                    request.extensions(),
                )
                .await;

                let gateway_identity = GatewayRequestIdentity {
                    user_id: row.user_id.map(|u| u.to_string()),
                    user_email,
                    api_key_id: Some(row.id.to_string()),
                    api_key_lineage_id: Some(row.lineage_id.to_string()),
                    allowed_models: merged_models.clone(),
                    surface_constraints: surface_constraints.clone(),
                    key_constraints: key_constraints.clone(),
                    ip_address: client_ip.clone(),
                    key_expires_at: stops_authenticating_at(
                        row.expires_at,
                        row.grace_period_ends_at,
                    ),
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
                    // Reuse the email already loaded for `gateway_identity`
                    // above — same user_id, same row. The MCP branch used
                    // to issue a SECOND `SELECT email` query against PG on
                    // every request which is pure waste; the user-state
                    // gate at the JOIN above guarantees the user still
                    // exists, so an absent email here means the user was
                    // hard-deleted between the JOIN and this point (rare)
                    // and we should 401 rather than serve the request.
                    let Some(user_email) = gateway_identity.user_email.clone() else {
                        return Err(StatusCode::UNAUTHORIZED);
                    };
                    let mcp_identity = McpRequestIdentity {
                        user_id: uid,
                        user_email,
                        user_roles,
                        surface_constraints: surface_constraints.clone(),
                        api_key_lineage_id: row.lineage_id,
                        key_constraints,
                        allowed_mcp_tools: merged_mcp_tools.clone(),
                        mcp_account_overrides: row.mcp_account_overrides.clone(),
                        ip_address: client_ip.clone(),
                    };
                    request.extensions_mut().insert(mcp_identity);
                }

                if let Some(c) = &cancel {
                    c.identity(&gateway_identity);
                    request.extensions_mut().insert(c.slot());
                }
                request.extensions_mut().insert(gateway_identity);
                Ok(next.run(request).await)
            }
            .await;
            if let Some(c) = cancel {
                c.disarm();
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    fn v(items: &[&str]) -> Option<Vec<String>> {
        Some(items.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn allowlist_intersection_is_pattern_aware() {
        let m = model_entry_covers;
        assert_eq!(intersect_allowlists(None, None, m), None);
        assert_eq!(intersect_allowlists(v(&["a"]), None, m), v(&["a"]));
        assert_eq!(intersect_allowlists(None, v(&["a"]), m), v(&["a"]));
        // A narrower key entry under a prefix grant is kept, and so is a
        // narrower role entry under a broader key entry.
        assert_eq!(
            intersect_allowlists(v(&["gpt-4o-mini"]), v(&["gpt-4o"]), m),
            v(&["gpt-4o-mini"])
        );
        assert_eq!(
            intersect_allowlists(v(&["gpt-"]), v(&["gpt-4o", "claude"]), m),
            v(&["gpt-4o"])
        );
        // Disjoint lists allow nothing.
        assert_eq!(intersect_allowlists(v(&["b"]), v(&["a"]), m), v(&[]));

        let t = mcp_entry_covers;
        assert_eq!(
            intersect_allowlists(v(&["github__list"]), v(&["github__*"]), t),
            v(&["github__list"])
        );
        assert_eq!(
            intersect_allowlists(v(&["*"]), v(&["github__*", "slack__send"]), t),
            v(&["github__*", "slack__send"])
        );
        assert_eq!(
            intersect_allowlists(v(&["github__*"]), v(&["slack__*"]), t),
            v(&[])
        );
    }

    /// Limits that failed to load are not "no limits": failing closed
    /// refuses the request, failing open runs it without them; limits
    /// that loaded are used as they are either way.
    #[test]
    fn a_failed_limits_load_follows_the_fail_closed_setting() {
        let key = uuid::Uuid::new_v4();
        let failed = || Err(sqlx::Error::PoolTimedOut);
        assert_eq!(limits_or_refusal(failed(), true, key), None);
        assert_eq!(
            limits_or_refusal(failed(), false, key),
            Some((SurfaceConstraints::default(), SurfaceConstraints::default()))
        );

        let user = SurfaceConstraints {
            ai_gateway: Some(think_watch_common::limits::SurfaceBlock::default()),
            mcp_gateway: None,
        };
        let loaded = || Ok((user.clone(), SurfaceConstraints::default()));
        for fail_closed in [true, false] {
            assert_eq!(
                limits_or_refusal(loaded(), fail_closed, key),
                Some((user.clone(), SurfaceConstraints::default()))
            );
        }
    }

    /// The refusal is the limiter-unavailable answer: 429, a
    /// `Retry-After`, the label, in the caller's format.
    #[tokio::test]
    async fn the_refusal_is_a_429_in_the_callers_format() {
        async fn read(resp: Response) -> (u16, Option<String>, serde_json::Value) {
            let status = resp.status().as_u16();
            let retry = resp
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .map(|v| v.to_str().unwrap().to_string());
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, retry, serde_json::from_slice(&bytes).unwrap())
        }

        let (status, retry, body) =
            read(limits_unavailable("ai_gateway", "/v1/chat/completions")).await;
        assert_eq!((status, retry.as_deref()), (429, Some("30")));
        assert_eq!(body["error"]["message"], "Rate limited: limits_unavailable");

        let (status, _, body) = read(limits_unavailable("ai_gateway", "/v1/messages")).await;
        assert_eq!(status, 429);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["message"], "Rate limited: limits_unavailable");

        let (status, retry, body) = read(limits_unavailable("mcp_gateway", "/mcp")).await;
        assert_eq!((status, retry.as_deref()), (429, Some("30")));
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["error"]["message"], "Rate limited: limits_unavailable");
    }

    #[test]
    fn a_key_stops_at_its_expiry_or_its_grace_end_whichever_is_first() {
        use chrono::TimeZone;
        let d = |day| {
            Some(
                chrono::Utc
                    .with_ymd_and_hms(2026, 10, day, 0, 0, 0)
                    .unwrap(),
            )
        };
        assert_eq!(stops_authenticating_at(None, None), None);
        assert_eq!(stops_authenticating_at(d(20), None), d(20));
        assert_eq!(stops_authenticating_at(None, d(12)), d(12));
        assert_eq!(stops_authenticating_at(d(20), d(12)), d(12));
        assert_eq!(stops_authenticating_at(d(11), d(12)), d(11));
    }

    #[test]
    fn a_key_is_read_where_each_sdk_puts_it() {
        assert_eq!(
            presented_key(&h(&[("authorization", "Bearer tw-1")]), None),
            Some("tw-1")
        );
        assert_eq!(
            presented_key(&h(&[("x-api-key", "tw-2")]), None),
            Some("tw-2")
        );
        assert_eq!(
            presented_key(&h(&[("x-goog-api-key", "tw-3")]), None),
            Some("tw-3")
        );
        assert_eq!(
            presented_key(&HeaderMap::new(), Some("alt=sse&key=tw-4")),
            Some("tw-4")
        );
    }

    #[test]
    fn the_query_is_the_last_resort_and_empty_values_are_no_key() {
        assert_eq!(
            presented_key(&h(&[("x-api-key", "tw-h")]), Some("key=tw-q")),
            Some("tw-h")
        );
        assert_eq!(
            presented_key(
                &h(&[("x-api-key", ""), ("authorization", "Bearer tw-b")]),
                None
            ),
            Some("tw-b")
        );
        assert_eq!(presented_key(&h(&[("authorization", "tw-1")]), None), None);
        assert_eq!(
            presented_key(&HeaderMap::new(), Some("key=&monkey=1")),
            None
        );
    }
}
