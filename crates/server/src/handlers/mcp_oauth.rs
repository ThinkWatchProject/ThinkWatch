//! Per-user MCP credential management — OAuth Authorization Code flow
//! and static-token vault. The proxy hot path consumes these via
//! [`think_watch_mcp_gateway::user_token::UserTokenResolver`]; this
//! module owns the lifecycle (authorize / callback / revoke / paste).
//!
//! The OAuth flow mirrors [`super::sso`]:
//!   1. `POST .../authorize` mints a `(state, code_verifier)` pair,
//!      HMAC-binds them with the encryption key, persists the binding
//!      blob in Redis under `mcp_oauth:state:{state}` (TTL 600s), and
//!      returns the upstream `authorize_url` for the browser to follow.
//!   2. `GET /api/mcp/oauth/callback` is unauthenticated — the user
//!      lands here from the upstream OAuth provider. The handler does
//!      a GETDEL on the state, re-derives the HMAC, and only then
//!      believes the (user_id, server_id, account_label) it pulls out
//!      of the blob.
//!   3. The token endpoint exchange happens with the stored
//!      `code_verifier` (PKCE) and the server's encrypted client
//!      credentials. Tokens are AES-GCM encrypted before they touch the
//!      DB.

mod discovery;
mod shared;
mod wizard;

pub use discovery::{
    DiscoverRequest, DiscoverResponse, ProbeRequest, ProbeResponse, oauth_discover, oauth_probe,
};
pub use shared::{
    SharedCredentialStatus, SharedStaticTokenRequest, best_effort_revoke_shared_upstream,
    paste_shared_static_token, revoke_shared_credential, shared_credential_status,
    start_shared_authorize,
};
pub use wizard::{
    PoppedWizardCredential, WizardAuthorizeRequest, WizardCredentialStatus,
    claim_wizard_credential, discard_wizard_credential, start_wizard_authorize,
    wizard_credential_status,
};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use data_encoding::BASE64URL_NOPAD;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use think_watch_auth::oauth::client::TokenEndpointResponse;
use think_watch_auth::oauth::pkce::{pkce_challenge, random_token, state_binding};
use think_watch_auth::oauth::subject::{extract_subject_from_json, subject_from_jwt};
use think_watch_common::audit::AuditActor;
use think_watch_common::crypto::{self, parse_encryption_key};
use think_watch_common::errors::AppError;
use think_watch_common::models::McpServer;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;
use crate::services::mcp_credential_repository as credential_repo;
use crate::services::mcp_server_repository as server_repo;

pub(super) const OAUTH_STATE_PREFIX: &str = "mcp_oauth:state:";
pub(super) const OAUTH_STATE_TTL_SECS: i64 = 600;

// ---------------------------------------------------------------------------
// State blob persisted in Redis between authorize and callback
// ---------------------------------------------------------------------------

/// Where an OAuth callback should land its tokens. Three target
/// shapes the callback dispatches on:
///
///   * **PerUser**: end user authorized from /connections — write
///     to `mcp_user_credentials` keyed on (server, user, label).
///   * **AdminShared**: admin re-authorized an existing
///     admin_shared server — write to
///     `mcp_server_shared_credentials` keyed on server_id.
///   * **WizardAdminShared**: admin is in the new-server wizard,
///     authorizing the shared credential **before the server row
///     exists**. Bakes the OAuth client config into the state blob
///     (no server row to look up), and stashes the resulting tokens
///     in Redis under `mcp_wizard:cred:{wizard_session_id}` so the
///     wizard's `Save` step can transfer them to
///     `mcp_server_shared_credentials` atomically with row insert.
///     Avoids a "pending" server row that could orphan if the
///     wizard is abandoned.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub(super) enum OauthStateTarget {
    PerUser {
        server_id: Uuid,
        user_id: Uuid,
        account_label: String,
    },
    AdminShared {
        server_id: Uuid,
        /// Audit pointer — which admin started the authorize flow.
        configured_by: Uuid,
    },
    WizardAdminShared {
        wizard_session_id: String,
        configured_by: Uuid,
        /// OAuth client config baked in here because there is no
        /// `mcp_servers` row to look it up from yet.
        oauth_token_endpoint: String,
        oauth_client_id: String,
        /// Pre-encrypted with the server's encryption key. Same
        /// shape as `mcp_servers.oauth_client_secret_encrypted`.
        oauth_client_secret_encrypted: Option<Vec<u8>>,
        oauth_scopes: Vec<String>,
        oauth_userinfo_endpoint: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct McpOauthState {
    pub(super) target: OauthStateTarget,
    /// PKCE code_verifier — sent to the upstream token endpoint to
    /// prove the same client that started the flow is finishing it.
    pub(super) code_verifier: String,
    /// Captured at authorize time so the token-endpoint exchange
    /// presents the exact same value the upstream saw at /authorize.
    pub(super) redirect_uri: String,
    /// HMAC-SHA256(encryption_key, state || ":" || code_verifier).
    /// Catches Redis tampering — only a server holding the encryption
    /// key can forge a matching pair.
    pub(super) binding: String,
}

/// Fully-qualified base URL the OAuth provider should redirect back
/// to. The first CORS origin is the canonical console URL — same
/// pattern the SSO callback uses.
pub(super) fn callback_base_url(state: &AppState) -> Result<String, AppError> {
    state
        .config
        .cors_origins
        .first()
        .map(|s| s.trim_end_matches('/').to_string())
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "CORS_ORIGINS must include the console URL for MCP OAuth callbacks"
            ))
        })
}

pub(super) fn callback_redirect_uri(state: &AppState) -> Result<String, AppError> {
    Ok(format!(
        "{}/api/mcp/oauth/callback",
        callback_base_url(state)?
    ))
}

// ---------------------------------------------------------------------------
// GET /api/mcp/connections — list current user's connections
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ConnectionAccount {
    pub account_label: String,
    pub credential_type: String,
    pub is_default: bool,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub upstream_subject: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ServerConnections {
    pub server_id: Uuid,
    pub server_name: String,
    /// Optional human-friendly label admin set on this server (e.g.
    /// "Linear (Acme prod)" when two installs of the same template
    /// would otherwise look identical to users). Frontend renders
    /// this when set, falling back to `server_name`.
    pub display_label: Option<String>,
    pub namespace_prefix: String,
    /// Single-valued auth shape — drives which UI the connections
    /// dialog renders (OAuth button vs PAT input). `'anonymous'`
    /// servers don't surface here at all (filtered server-side).
    pub auth_shape: String,
    pub static_token_help_url: Option<String>,
    /// Header name + value template the upstream credential is sent
    /// under. Surfaced so the connections dialog can show a
    /// "submitted as `Authorization: Bearer ghp_…`" preview, which
    /// answers the most common user confusion ("where does this token
    /// go?").
    pub auth_header_name: String,
    pub auth_value_template: String,
    pub accounts: Vec<ConnectionAccount>,
}

pub async fn list_connections(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<ServerConnections>>, AppError> {
    auth_user.require_permission("mcp:connect")?;

    let servers = server_repo::list_by_name(&state.db).await?;
    let rows = credential_repo::list_user_accounts(&state.db, auth_user.claims.sub).await?;

    let mut out = Vec::with_capacity(servers.len());
    for s in servers {
        // Admin-shared servers manage their credential through the
        // admin UI; surfacing them here would only show a card with
        // no actionable buttons. Skip outright.
        if s.credential_owner == "admin_shared" {
            continue;
        }
        // /connections only lists servers that *need* user-level
        // credentials. Anonymous servers work without setup so a
        // card with no actions would just be confusing.
        if s.auth_shape == "anonymous" {
            continue;
        }
        let mut accounts = Vec::new();
        for r in rows.iter().filter(|r| r.mcp_server_id == s.id) {
            accounts.push(ConnectionAccount {
                account_label: r.account_label.clone(),
                credential_type: r.credential_type.clone(),
                is_default: r.is_default,
                scopes: r.scopes.clone(),
                expires_at: r.expires_at,
                upstream_subject: r.upstream_subject.clone(),
                created_at: r.created_at,
                updated_at: r.updated_at,
            });
        }
        out.push(ServerConnections {
            server_id: s.id,
            server_name: s.name,
            display_label: s.display_label,
            namespace_prefix: s.namespace_prefix,
            auth_shape: s.auth_shape,
            static_token_help_url: s.static_token_help_url,
            auth_header_name: s.auth_header_name,
            auth_value_template: s.auth_value_template,
            accounts,
        });
    }

    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// POST /api/mcp/connections/{server_id}/authorize — start OAuth flow
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AuthorizeRequest {
    /// User's free-form label for the account being connected
    /// (e.g. "work", "personal"). Must be unique within
    /// (server, user); reusing an existing label re-authorizes that
    /// account at callback time.
    pub account_label: String,
}

#[derive(Debug, Serialize)]
pub struct AuthorizeResponse {
    pub authorize_url: String,
}

pub async fn start_authorize(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(server_id): Path<Uuid>,
    Json(req): Json<AuthorizeRequest>,
) -> Result<Json<AuthorizeResponse>, AppError> {
    auth_user.require_permission("mcp:connect")?;

    // Authorize triggers a Redis state-binding write and a redirect
    // to an upstream OAuth provider. Rate-limit per user so a stolen
    // session token can't fill `mcp_oauth:state:*` or hammer the
    // upstream's authorize endpoint as a stepping stone.
    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp_oauth_authorize",
    )
    .await?;

    if req.account_label.trim().is_empty() || req.account_label.len() > 64 {
        return Err(AppError::BadRequest(
            "account_label must be 1–64 characters".into(),
        ));
    }

    let server = load_server(&state, server_id).await?;
    if server.credential_owner == "admin_shared" {
        return Err(AppError::BadRequest(
            "This server uses an admin-supplied shared credential — \
             the per-user authorize flow is disabled. Ask an administrator \
             to configure the shared credential instead."
                .into(),
        ));
    }
    if server.auth_shape != "oauth" {
        // The admin set this server to a non-OAuth shape — the user
        // should be pasting a token in /connections, not running the
        // authorize flow. Surface the actual shape so the UI knows
        // what to render.
        return Err(AppError::BadRequest(format!(
            "This server's auth shape is '{}', not 'oauth'. \
             OAuth authorize is only valid for OAuth-shape servers.",
            server.auth_shape
        )));
    }
    let auth_endpoint = server
        .oauth_authorization_endpoint
        .as_deref()
        .ok_or_else(|| {
            AppError::BadRequest(
                "This server has no OAuth authorization endpoint configured".into(),
            )
        })?;
    let token_endpoint_present = server.oauth_token_endpoint.is_some();
    let client_id = server
        .oauth_client_id
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("OAuth client_id not configured".into()))?;
    if !token_endpoint_present {
        return Err(AppError::BadRequest(
            "OAuth token endpoint not configured".into(),
        ));
    }

    let state_token = random_token();
    let code_verifier = random_token();
    let code_challenge = pkce_challenge(&code_verifier);
    let redirect_uri = callback_redirect_uri(&state)?;

    let enc_key = parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let binding = state_binding(&enc_key, &state_token, &code_verifier);

    let blob = McpOauthState {
        target: OauthStateTarget::PerUser {
            server_id,
            user_id: auth_user.claims.sub,
            account_label: req.account_label.trim().to_string(),
        },
        code_verifier: code_verifier.clone(),
        redirect_uri: redirect_uri.clone(),
        binding,
    };
    let payload = serde_json::to_string(&blob)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize state: {e}")))?;
    fred::interfaces::KeysInterface::set::<(), _, _>(
        &state.redis,
        format!("{OAUTH_STATE_PREFIX}{state_token}"),
        payload,
        Some(fred::types::Expiration::EX(OAUTH_STATE_TTL_SECS)),
        None,
        false,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

    // Build the authorize URL. URL-encoding via `url::Url` keeps us
    // honest about reserved characters (the scopes string in
    // particular often contains spaces).
    let mut url = url::Url::parse(auth_endpoint)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid authorization_endpoint: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", client_id);
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("state", &state_token);
        q.append_pair("code_challenge", &code_challenge);
        q.append_pair("code_challenge_method", "S256");
        if !server.oauth_scopes.is_empty() {
            q.append_pair("scope", &server.oauth_scopes.join(" "));
        }
    }

    Ok(Json(AuthorizeResponse {
        authorize_url: url.to_string(),
    }))
}

/// Bounded retry around the OAuth-callback storage step. The upstream
/// OAuth code has been exchanged for tokens by the time we reach this
/// point — a transient PG blip would otherwise drop those tokens on
/// the floor and force the user back through the full re-authorize
/// flow even though the upstream side already succeeded. 2 attempts
/// with a 100 ms backoff catches the typical failover/restart case;
/// persistent failures still surface so the operator sees the metric.
async fn retry_pg_storage<F, Fut, T>(label: &'static str, mut op: F) -> Result<T, AppError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, AppError>>,
{
    const ATTEMPTS: u32 = 2;
    let mut last_err: Option<AppError> = None;
    for attempt in 0..ATTEMPTS {
        match op().await {
            Ok(v) => {
                if attempt > 0 {
                    metrics::counter!("oauth_storage_retried_total", "label" => label).increment(1);
                }
                return Ok(v);
            }
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
    metrics::counter!("oauth_storage_failed_total", "label" => label).increment(1);
    tracing::error!(
        label,
        "OAuth callback could not persist credential after {ATTEMPTS} attempts; \
         user must re-authorize (upstream OAuth code is single-use, tokens are lost)"
    );
    Err(last_err.expect("ATTEMPTS >= 1 so last_err is always Some by here"))
}

// ---------------------------------------------------------------------------
// GET /api/mcp/oauth/callback — upstream redirects here with code+state
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

pub async fn oauth_callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackParams>,
) -> Result<Response, AppError> {
    // Upstream signalled a user-side failure (deny / scope error).
    // Bounce to /connections with the error in the URL fragment so the
    // page can show it without us having to render HTML here.
    if let Some(err) = params.error {
        let base = callback_base_url(&state)?;
        let detail = params
            .error_description
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        let url = format!(
            "{}/connections#error={}{}",
            base,
            urlencode_fragment(&err),
            urlencode_fragment(&detail)
        );
        return Ok(Redirect::temporary(&url).into_response());
    }

    let code = params
        .code
        .ok_or_else(|| AppError::BadRequest("OAuth callback missing `code`".into()))?;
    let state_token = params
        .state
        .ok_or_else(|| AppError::BadRequest("OAuth callback missing `state`".into()))?;

    // Atomic retrieve + delete enforces single-use.
    let redis_key = format!("{OAUTH_STATE_PREFIX}{state_token}");
    let stored: Option<String> = fred::interfaces::KeysInterface::getdel(&state.redis, &redis_key)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;
    let stored = stored.ok_or_else(|| {
        AppError::BadRequest(
            "Invalid or expired OAuth state — please retry from /connections".into(),
        )
    })?;

    let blob: McpOauthState = serde_json::from_str(&stored)
        .map_err(|_| AppError::BadRequest("Corrupt OAuth state blob".into()))?;

    // Re-derive the binding and constant-time-compare.
    let enc_key = parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let expected = state_binding(&enc_key, &state_token, &blob.code_verifier);
    if !bool::from(expected.as_bytes().ct_eq(blob.binding.as_bytes())) {
        // Don't log the state token — it's the bearer credential
        // for this callback. Single-use via GETDEL above so replay
        // is impossible, but if logs reach a less-trusted
        // destination (forwarder, downstream SIEM) the value would
        // leak unnecessarily.
        tracing::warn!("MCP OAuth state binding mismatch");
        return Err(AppError::BadRequest(
            "OAuth session binding failed; please retry".into(),
        ));
    }

    // Resolve OAuth client config — comes from the server row for
    // existing-server flows, or from the state blob itself for the
    // wizard flow (where no server row exists yet).
    let exchange = build_exchange_context(&state, &blob.target, &enc_key).await?;

    // Token-exchange + subject resolution + encryption — all
    // independent of where the credential will land.
    let token = oauth_token_exchange(
        state.http_client.load().as_ref(),
        &exchange,
        &code,
        &blob.redirect_uri,
        &blob.code_verifier,
    )
    .await?;
    let access_encrypted = crypto::encrypt(token.access_token.as_bytes(), &enc_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt access_token: {e}")))?;
    let refresh_encrypted = match token.refresh_token.as_deref() {
        Some(rt) if !rt.is_empty() => Some(
            crypto::encrypt(rt.as_bytes(), &enc_key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt refresh_token: {e}")))?,
        ),
        _ => None,
    };
    let expires_at = token
        .expires_in
        // Use try_from so a malicious upstream returning u64::MAX
        // doesn't silently wrap into a negative i64 and produce an
        // "expired in 1969" timestamp; cap at "no expiry" instead.
        .and_then(|s| i64::try_from(s).ok())
        .map(|s| Utc::now() + chrono::Duration::seconds(s));
    let scopes: Vec<String> = token
        .scope
        .as_deref()
        .map(|s| s.split_whitespace().map(String::from).collect::<Vec<_>>())
        .unwrap_or_else(|| exchange.default_scopes.clone());
    let upstream_subject = resolve_upstream_subject(
        state.http_client.load().as_ref(),
        &token.access_token,
        exchange.userinfo_endpoint.as_deref(),
    )
    .await;

    // Dispatch storage based on target.
    //
    // The Redis state was GETDEL'd above (line 451) — by this point
    // the upstream OAuth code is also consumed, so any storage
    // failure here lands the user in a dead-end ("authorization
    // succeeded upstream, but we lost the tokens"). Bounded retry on
    // the PG insert catches the common transient case (PG failover
    // blip, network jitter). A persistent failure still surfaces the
    // error, but the operator can scrape the warn log to manually
    // re-create the credential — the user, on the other hand, must
    // re-authorize because the upstream code is single-use.
    match &blob.target {
        OauthStateTarget::PerUser {
            server_id,
            user_id,
            account_label,
        } => {
            let server = load_server(&state, *server_id).await?;
            retry_pg_storage("per_user_credential", || {
                credential_repo::upsert_user_credential(
                    &state.db,
                    *server_id,
                    *user_id,
                    account_label,
                    "oauth_authcode",
                    &access_encrypted,
                    refresh_encrypted.as_deref(),
                    expires_at,
                    &scopes,
                    upstream_subject.as_deref(),
                )
            })
            .await?;
            think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
                .invalidate_user_lane(server_id, user_id)
                .await;
            spawn_user_tool_discovery(
                state.db.clone(),
                (**state.http_client.load()).clone(),
                server.endpoint_url.clone(),
                server.name.clone(),
                *server_id,
                *user_id,
                server.auth_header_name.clone(),
                server.auth_value_template.clone(),
                token.access_token.clone(),
            );
            // OAuth callback handler signature doesn't expose headers,
            // so IP/UA are None for now — sets up the actor scaffolding
            // without behavior change; a follow-up can plumb headers
            // through if forensics on the callback path becomes
            // relevant. The `user_id` resolved from the state cookie
            // is the load-bearing attribution here.
            let actor = think_watch_common::audit::OAuthCallbackActor {
                user_id: *user_id,
                ip: None,
                user_agent: None,
            };
            state.audit.log(
                actor
                    .audit("mcp.connection.authorized")
                    .resource("mcp_server")
                    .resource_id(server_id.to_string())
                    .detail(serde_json::json!({
                        "account_label": account_label,
                        "scopes": scopes,
                    })),
            );
            let base = callback_base_url(&state)?;
            let url = format!(
                "{}/connections#connected={}/{}",
                base,
                server_id,
                urlencode_fragment(account_label),
            );
            Ok(Redirect::temporary(&url).into_response())
        }
        OauthStateTarget::AdminShared {
            server_id,
            configured_by,
        } => {
            let server = load_server(&state, *server_id).await?;
            retry_pg_storage("admin_shared_credential", || {
                credential_repo::upsert_shared_credential(
                    &state.db,
                    *server_id,
                    "oauth_authcode",
                    &access_encrypted,
                    refresh_encrypted.as_deref(),
                    expires_at,
                    &scopes,
                    upstream_subject.as_deref(),
                    *configured_by,
                )
            })
            .await?;
            think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
                .invalidate_server_lane(server_id)
                .await;
            shared::spawn_shared_tool_discovery(
                state.db.clone(),
                (**state.http_client.load()).clone(),
                server.clone(),
                token.access_token.clone(),
            );
            let actor = think_watch_common::audit::OAuthCallbackActor {
                user_id: *configured_by,
                ip: None,
                user_agent: None,
            };
            state.audit.log(
                actor
                    .audit("mcp.shared_credential.authorized")
                    .resource("mcp_server")
                    .resource_id(server_id.to_string())
                    .detail(serde_json::json!({ "scopes": scopes })),
            );
            let base = callback_base_url(&state)?;
            let url = format!(
                "{}/admin/mcp/servers/{}#shared_connected=1",
                base, server_id
            );
            Ok(Redirect::temporary(&url).into_response())
        }
        OauthStateTarget::WizardAdminShared {
            wizard_session_id,
            configured_by,
            ..
        } => {
            // Stash the encrypted tokens in Redis under the wizard's
            // session ID — the wizard's "Save" step will GETDEL this
            // and transfer it into mcp_server_shared_credentials in
            // the same TX as the server-row insert. No server row is
            // created here, so abandoning the wizard leaves nothing
            // behind that needs cleanup beyond Redis TTL.
            let payload = serde_json::json!({
                "credential_type": "oauth_authcode",
                "access_token_encrypted": BASE64URL_NOPAD.encode(&access_encrypted),
                "refresh_token_encrypted": refresh_encrypted
                    .as_ref()
                    .map(|b| BASE64URL_NOPAD.encode(b)),
                "expires_at": expires_at,
                "scopes": scopes,
                "upstream_subject": upstream_subject,
                "configured_by": configured_by,
            });
            fred::interfaces::KeysInterface::set::<(), _, _>(
                &state.redis,
                wizard_credential_redis_key(*configured_by, wizard_session_id),
                payload.to_string(),
                Some(fred::types::Expiration::EX(WIZARD_CREDENTIAL_TTL_SECS)),
                None,
                false,
            )
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

            let actor = think_watch_common::audit::OAuthCallbackActor {
                user_id: *configured_by,
                ip: None,
                user_agent: None,
            };
            state.audit.log(
                actor
                    .audit("mcp.wizard.shared_credential_authorized")
                    .resource("mcp_wizard")
                    .resource_id(wizard_session_id.clone())
                    .detail(serde_json::json!({ "scopes": scopes })),
            );

            let base = callback_base_url(&state)?;
            let url = format!(
                "{}/mcp/servers/new#wizard_resume={}",
                base,
                urlencode_fragment(wizard_session_id),
            );
            Ok(Redirect::temporary(&url).into_response())
        }
    }
}

// ---------------------------------------------------------------------------
// Wizard pending-credential storage in Redis
// ---------------------------------------------------------------------------

/// TTL on `mcp_wizard:cred:*` blobs. Long enough to cover a thoughtful
/// admin filling out the rest of the wizard after returning from the
/// upstream OAuth dance, short enough that abandoned tokens age out
/// without any cleanup logic.
pub(super) const WIZARD_CREDENTIAL_TTL_SECS: i64 = 3_600;

/// Redis key under which the OAuth callback parks a wizard's pending
/// shared credential. Also used by `claim_wizard_credential` to
/// GETDEL the payload at server-create time.
///
/// Keyed by (configured_by, wizard_session_id) so the blob is bound
/// to the user who initiated the OAuth dance. Even if a sibling
/// user somehow learns the session_id (UUID — guessing is hard, but
/// defense in depth), they can't claim against a different user's
/// blob — `claim_wizard_credential` checks the requesting JWT's
/// `sub` against the configured_by half of the key.
pub(super) fn wizard_credential_redis_key(configured_by: Uuid, wizard_session_id: &str) -> String {
    format!("mcp_wizard:cred:{configured_by}:{wizard_session_id}")
}

/// Resolved OAuth client config + tail-state metadata used by the
/// callback to run a token exchange. Bridges the two cases:
///   1. The OAuth flow was started against an existing server row —
///      config comes from the row.
///   2. The flow was started from the new-server wizard — config is
///      baked into the state blob (no row exists yet).
pub(super) struct ExchangeContext {
    pub(super) token_endpoint: String,
    pub(super) client_id: String,
    /// Decrypted plaintext. `None` for public clients.
    pub(super) client_secret: Option<String>,
    pub(super) default_scopes: Vec<String>,
    pub(super) userinfo_endpoint: Option<String>,
}

pub(super) async fn build_exchange_context(
    state: &AppState,
    target: &OauthStateTarget,
    enc_key: &[u8; 32],
) -> Result<ExchangeContext, AppError> {
    match target {
        OauthStateTarget::PerUser { server_id, .. }
        | OauthStateTarget::AdminShared { server_id, .. } => {
            let server = load_server(state, *server_id).await?;
            let token_endpoint = server.oauth_token_endpoint.ok_or_else(|| {
                AppError::BadRequest("OAuth token endpoint not configured".into())
            })?;
            let client_id = server
                .oauth_client_id
                .ok_or_else(|| AppError::BadRequest("OAuth client_id not configured".into()))?;
            let client_secret =
                decrypt_optional_secret(server.oauth_client_secret_encrypted.as_deref(), enc_key)?;
            Ok(ExchangeContext {
                token_endpoint,
                client_id,
                client_secret,
                default_scopes: server.oauth_scopes,
                userinfo_endpoint: server.oauth_userinfo_endpoint,
            })
        }
        OauthStateTarget::WizardAdminShared {
            oauth_token_endpoint,
            oauth_client_id,
            oauth_client_secret_encrypted,
            oauth_scopes,
            oauth_userinfo_endpoint,
            ..
        } => {
            let client_secret =
                decrypt_optional_secret(oauth_client_secret_encrypted.as_deref(), enc_key)?;
            Ok(ExchangeContext {
                token_endpoint: oauth_token_endpoint.clone(),
                client_id: oauth_client_id.clone(),
                client_secret,
                default_scopes: oauth_scopes.clone(),
                userinfo_endpoint: oauth_userinfo_endpoint.clone(),
            })
        }
    }
}

pub(super) fn decrypt_optional_secret(
    encrypted: Option<&[u8]>,
    enc_key: &[u8; 32],
) -> Result<Option<String>, AppError> {
    match encrypted {
        Some(bytes) => {
            let plain = crypto::decrypt(bytes, enc_key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt client_secret: {e}")))?;
            let s = String::from_utf8(plain)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("client_secret not utf8: {e}")))?;
            Ok(Some(s))
        }
        None => Ok(None),
    }
}

/// Thin wrapper around [`think_watch_auth::oauth::client::exchange_authorization_code`]
/// that unpacks our handler-side `ExchangeContext` and maps the
/// structured `TokenExchangeError` back to the HTTP-rendering
/// `AppError` shape, attaching the operator-facing hint to the
/// "upstream rejected" case.
pub(super) async fn oauth_token_exchange(
    http: &reqwest::Client,
    cfg: &ExchangeContext,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenEndpointResponse, AppError> {
    use think_watch_auth::oauth::client::{TokenExchangeError, exchange_authorization_code};
    exchange_authorization_code(
        http,
        &cfg.token_endpoint,
        &cfg.client_id,
        cfg.client_secret.as_deref(),
        code,
        redirect_uri,
        code_verifier,
    )
    .await
    .map_err(|e| match e {
        TokenExchangeError::EncodeForm(err) => {
            AppError::Internal(anyhow::anyhow!("encode token form: {err}"))
        }
        TokenExchangeError::Unreachable(err) => {
            AppError::BadRequest(format!("Token endpoint unreachable: {err}"))
        }
        TokenExchangeError::UpstreamRejected { detail } => AppError::BadRequest(format!(
            "Upstream rejected the OAuth exchange: {detail}. \
             The MCP server's OAuth client_id/secret may be misconfigured — \
             ask an administrator to verify them at /mcp/servers."
        )),
        TokenExchangeError::InvalidResponse(msg) => {
            // Don't surface the upstream body — on malformed-but-
            // token-bearing OAuth responses (rare misbehaving
            // servers) it would leak `access_token` / `refresh_token`
            // / vendor `_debug_*` fields. `msg` is the serde error,
            // which carries position info but no payload.
            AppError::BadRequest(format!("Token response not JSON: {msg}"))
        }
    })
}

// ---------------------------------------------------------------------------
// DELETE /api/mcp/connections/{server_id}/{account_label}
// ---------------------------------------------------------------------------

pub async fn revoke_connection(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((server_id, account_label)): Path<(Uuid, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("mcp:connect")?;

    // Revoke triggers an outbound POST to the upstream revocation
    // endpoint plus a DB DELETE. Per-user cap so a stolen session
    // can't script connect/revoke loops to hammer the upstream (or,
    // if the admin somehow bypassed `validate_url`, an internal one).
    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp_revoke",
    )
    .await?;

    // Best-effort revoke at the upstream — only when we actually have
    // an access_token AND the server advertises a revocation endpoint.
    let row = credential_repo::find_user_token(
        &state.db,
        server_id,
        auth_user.claims.sub,
        &account_label,
    )
    .await?;
    let Some((credential_type, access_encrypted)) = row else {
        return Err(AppError::NotFound("Connection not found".into()));
    };

    if credential_type == "oauth_authcode" {
        let server = load_server(&state, server_id).await?;
        if let Some(revocation_endpoint) = server.oauth_revocation_endpoint.as_deref() {
            let enc_key = parse_encryption_key(&state.config.encryption_key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
            if let Ok(token_bytes) = crypto::decrypt(&access_encrypted, &enc_key)
                && let Ok(token) = String::from_utf8(token_bytes)
            {
                let form = vec![("token", token.as_str())];
                let body = serde_urlencoded::to_string(&form).unwrap_or_default();
                let http = state.http_client.load();
                let _ = http
                    .post(revocation_endpoint)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body)
                    .send()
                    .await; // ignore — best-effort
            }
        }
    }

    // Delete + (if needed) promote-another-default inside one
    // transaction. The partial unique index `uq_mcp_user_credentials_default`
    // guarantees there's AT MOST one default per (server, user), but
    // doesn't enforce AT LEAST one — so deleting the user's default
    // credential while they still hold other rows would leave the
    // gateway unable to resolve a default and return 401 on the next
    // call, even though the user clearly still has a usable connection.
    // Promote the most recently created remaining row as a graceful
    // fallback so the user keeps working without manually re-marking.
    credential_repo::delete_user_credential(
        &state.db,
        server_id,
        auth_user.claims.sub,
        &account_label,
    )
    .await?;

    // Cached responses pinned to this credential are now serving an
    // identity that no longer has access. Wipe the user's lane for
    // this server so post-revoke reads can't tunnel back to the
    // pre-revoke epoch.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_user_lane(&server_id, &auth_user.claims.sub)
        .await;

    state.audit.log(
        auth_user
            .audit("mcp.connection.revoked")
            .resource("mcp_server")
            .resource_id(server_id.to_string())
            .detail(serde_json::json!({ "account_label": account_label })),
    );

    Ok(Json(serde_json::json!({"status": "revoked"})))
}

// ---------------------------------------------------------------------------
// PUT /api/mcp/connections/{server_id}/{account_label}/default
// ---------------------------------------------------------------------------

pub async fn set_default_connection(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((server_id, account_label)): Path<(Uuid, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("mcp:connect")?;

    // Default-switching is a security-relevant routing change — flips
    // which upstream identity proxies a user's no-override calls. Cap
    // per user so a stolen session can't churn the partial-unique
    // index in a loop.
    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp_set_default",
    )
    .await?;

    let found = credential_repo::set_default_user_credential(
        &state.db,
        server_id,
        auth_user.claims.sub,
        &account_label,
    )
    .await?;
    if !found {
        return Err(AppError::NotFound("Connection not found".into()));
    }

    // Switching default flips which credential the resolver picks
    // when no API-key override is set. The no-override lane (`_`)
    // is now serving responses pinned to the *old* default's
    // upstream identity — wipe so post-switch reads see the new
    // default's data.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_user_lane(&server_id, &auth_user.claims.sub)
        .await;

    state.audit.log(
        auth_user
            .audit("mcp.connection.default_set")
            .resource("mcp_server")
            .resource_id(server_id.to_string())
            .detail(serde_json::json!({ "account_label": account_label })),
    );

    Ok(Json(serde_json::json!({"status": "ok"})))
}

// ---------------------------------------------------------------------------
// PUT /api/mcp/connections/{server_id}/{account_label}/static-token
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PasteTokenRequest {
    pub token: String,
}

pub async fn paste_static_token(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((server_id, account_label)): Path<(Uuid, String)>,
    Json(req): Json<PasteTokenRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("mcp:connect")?;

    // No outbound HTTP, but each call writes the credential vault
    // (encrypt + DB upsert + cache wipe). Same 5/min cap as the other
    // connection-mutation endpoints for consistency.
    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp_paste_token",
    )
    .await?;

    if account_label.trim().is_empty() || account_label.len() > 64 {
        return Err(AppError::BadRequest(
            "account_label must be 1–64 characters".into(),
        ));
    }
    if req.token.is_empty() {
        return Err(AppError::BadRequest("token is required".into()));
    }

    let server = load_server(&state, server_id).await?;
    if server.credential_owner == "admin_shared" {
        return Err(AppError::BadRequest(
            "This server uses an admin-supplied shared credential — no per-user \
             token is needed."
                .into(),
        ));
    }
    if server.auth_shape != "static" {
        return Err(AppError::BadRequest(
            "This server's auth shape isn't 'static' — pasted tokens can't be used here.".into(),
        ));
    }

    let enc_key = parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let access_encrypted = crypto::encrypt(req.token.as_bytes(), &enc_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt token: {e}")))?;

    credential_repo::upsert_user_credential(
        &state.db,
        server_id,
        auth_user.claims.sub,
        account_label.trim(),
        "static_token",
        &access_encrypted,
        None,
        None,
        &[],
        None,
    )
    .await?;

    // Replacing a static token in place flips the upstream identity
    // for this user — wipe any cached responses pinned to the old one.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_user_lane(&server_id, &auth_user.claims.sub)
        .await;

    // Per-user tool discovery — same rationale as the OAuth callback
    // path: token works now, populate `mcp_user_tools` so the user's
    // first MCP call sees a fresh tool list immediately.
    spawn_user_tool_discovery(
        state.db.clone(),
        (**state.http_client.load()).clone(),
        server.endpoint_url.clone(),
        server.name.clone(),
        server_id,
        auth_user.claims.sub,
        server.auth_header_name.clone(),
        server.auth_value_template.clone(),
        req.token.trim().to_string(),
    );

    state.audit.log(
        auth_user
            .audit("mcp.connection.authorized")
            .resource("mcp_server")
            .resource_id(server_id.to_string())
            .detail(serde_json::json!({
                "account_label": account_label,
                "credential_type": "static_token",
            })),
    );

    Ok(Json(serde_json::json!({"status": "ok"})))
}

/// Fire-and-forget helper — spawn `discover_user_tools` so callers in
/// the credential-write hot path (oauth_callback / paste_static_token)
/// don't block on the upstream `tools/list` round-trip. Errors are
/// logged at warn level and discarded; the next gateway request from
/// this user will lazy-discover via the proxy fallback if this attempt
/// missed.
///
/// Substitutes `{{token}}` in `auth_value_template` with the freshly
/// minted token before handing the resolved header to
/// `discover_user_tools`. Without this, per-user servers configured
/// with a custom header (e.g. `X-API-Key: {{token}}`) would receive
/// `Authorization: Bearer ...` instead and 401 — leaving
/// `mcp_user_tools` empty for that user until the proxy's lazy
/// fallback re-discovers on the first real call.
#[allow(clippy::too_many_arguments)]
fn spawn_user_tool_discovery(
    db: sqlx::PgPool,
    http: reqwest::Client,
    endpoint_url: String,
    server_name: String,
    server_id: Uuid,
    user_id: Uuid,
    auth_header_name: String,
    auth_value_template: String,
    token: String,
) {
    let auth_header_value = auth_value_template.replace("{{token}}", &token);
    tokio::spawn(async move {
        match crate::mcp_runtime::discover_user_tools(
            &db,
            &http,
            &endpoint_url,
            server_id,
            user_id,
            &auth_header_name,
            &auth_header_value,
        )
        .await
        {
            Ok(n) => tracing::info!(
                mcp_server = %server_name,
                user_id = %user_id,
                tools = n,
                "Per-user MCP tool discovery succeeded"
            ),
            Err(e) => tracing::warn!(
                mcp_server = %server_name,
                user_id = %user_id,
                error = %e,
                "Per-user MCP tool discovery failed (will retry on first proxy call)"
            ),
        }
    });
}

// ---------------------------------------------------------------------------
// POST /api/mcp/connections/{server_id}/{account_label}/test
// ---------------------------------------------------------------------------
//
// User-driven probe that exercises the *caller's* credential — the
// admin-side `/api/mcp/servers/{id}/discover` is anonymous and cannot
// validate OAuth-gated upstreams. This endpoint is read-only: it never
// updates `mcp_servers.status`, `cached_tools_jsonb`, or `last_error`
// so that one user's bad token can't pollute the admin view.

pub async fn test_connection(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path((server_id, account_label)): Path<(Uuid, String)>,
) -> Result<Json<super::mcp_servers::TestMcpServerResponse>, AppError> {
    use super::mcp_servers::TestMcpServerResponse;
    use think_watch_mcp_gateway::user_token::{ResolverCaller, ResolverError};

    auth_user.require_permission("mcp:connect")?;
    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp_user_test",
    )
    .await?;

    let server = load_server(&state, server_id).await?;

    // Confirm the credential exists before probing — saves a misleading
    // `NeedsUserCredentials` result for an account_label the user
    // never created (typo in the URL, stale UI cache, etc.).
    let exists = credential_repo::user_account_exists(
        &state.db,
        server_id,
        auth_user.claims.sub,
        &account_label,
    )
    .await?;
    if !exists {
        return Err(AppError::NotFound("Connection not found".into()));
    }

    let oauth_cfg = crate::mcp_runtime::build_oauth_cfg(&server, &state.config.encryption_key);

    // Pin the resolver to this exact account_label via the same override
    // map the gateway hot-path uses. Avoids exposing `fetch_row` and
    // keeps a single code path for credential selection.
    let caller = ResolverCaller {
        user_id: auth_user.claims.sub,
        mcp_account_overrides: serde_json::json!({
            server_id.to_string(): account_label,
        }),
    };

    let server_auth_cfg = think_watch_mcp_gateway::user_token::ServerAuthCfg {
        // /connections is per-user only — this endpoint is dead code
        // for admin_shared servers (the UI hides them), so always
        // resolve as PerUser regardless of the row's actual
        // credential_owner. Keeps "test my own connection" semantics
        // unambiguous.
        credential_owner: think_watch_mcp_gateway::user_token::CredentialOwner::PerUser,
        auth_shape: think_watch_mcp_gateway::user_token::AuthShape::parse(&server.auth_shape),
        oauth_cfg,
        auth_header_name: server.auth_header_name.clone(),
        auth_value_template: server.auth_value_template.clone(),
    };

    let header = match state
        .user_token_resolver
        .resolve(server_id, &server_auth_cfg, &caller)
        .await
    {
        Ok(opt) => opt,
        Err(ResolverError::NeedsUserCredentials { .. }) => {
            return Ok(Json(TestMcpServerResponse {
                success: false,
                requires_auth: false,
                message: "Credential is missing — re-authorize this account.".into(),
                latency_ms: 0,
                tools_count: None,
                tools: None,
            }));
        }
        Err(ResolverError::RefreshFailed { kind, message, .. }) => {
            // Tailor the next-step hint to the actual failure shape.
            // Transient = retry; Permanent = the row is gone, user
            // must re-authorize.
            let hint = match kind {
                think_watch_mcp_gateway::user_token::RefreshFailureKind::Transient => {
                    "The upstream OAuth provider is temporarily unavailable. Retry in a few seconds."
                }
                think_watch_mcp_gateway::user_token::RefreshFailureKind::Permanent => {
                    "Re-authorize this account at /connections."
                }
            };
            return Ok(Json(TestMcpServerResponse {
                success: false,
                requires_auth: false,
                message: format!("OAuth refresh failed: {message}. {hint}"),
                latency_ms: 0,
                tools_count: None,
                tools: None,
            }));
        }
        Err(e) => return Err(AppError::Internal(anyhow::anyhow!(e))),
    };

    let mut headers_map = std::collections::HashMap::new();
    if let Some(injection) = header {
        headers_map.insert(injection.header_name, injection.header_value);
    }
    let http = state.http_client.load();
    let outcome = super::mcp_servers::probe_mcp_endpoint(
        &http,
        &server.endpoint_url,
        if headers_map.is_empty() {
            None
        } else {
            Some(&headers_map)
        },
    )
    .await;

    state.audit.log(
        auth_user
            .audit("mcp.connection.tested")
            .resource("mcp_server")
            .resource_id(server_id.to_string())
            .detail(serde_json::json!({
                "account_label": account_label,
                "success": outcome.success,
                "tools_count": outcome.tools.len(),
            })),
    );

    let (tools_count, tools) = if outcome.success {
        (Some(outcome.tools.len()), Some(outcome.tools))
    } else {
        (None, None)
    };
    // This caller probes with the user's *real* credential, so a 401/403
    // means the user's token was rejected — that's a genuine failure,
    // not a "needs auth" soft-success. Pass `outcome.success` straight
    // through; surface `requires_auth` purely for telemetry.
    Ok(Json(TestMcpServerResponse {
        success: outcome.success,
        requires_auth: outcome.requires_auth,
        message: outcome.message,
        latency_ms: outcome.latency_ms,
        tools_count,
        tools,
    }))
}

// ---------------------------------------------------------------------------
// upstream_subject resolution (Tier 1 JWT → Tier 2 userinfo → fallback NULL)
// ---------------------------------------------------------------------------

/// Best-effort upstream-identity resolution. Never errors — failures
/// fall through to `None` and the column stays NULL. The pure
/// JWT/JSON walkers live in [`think_watch_auth::oauth::subject`]; this
/// function is just the HTTP glue (JWT first, fallback to userinfo
/// endpoint).
pub(crate) async fn resolve_upstream_subject(
    http: &reqwest::Client,
    access_token: &str,
    userinfo_endpoint: Option<&str>,
) -> Option<String> {
    if let Some(s) = subject_from_jwt(access_token) {
        return Some(s);
    }
    let endpoint = userinfo_endpoint?;
    let body = match http
        .get(endpoint)
        .bearer_auth(access_token)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.ok()?,
        Ok(r) => {
            tracing::debug!(
                endpoint = %endpoint,
                status = %r.status(),
                "userinfo endpoint returned non-success — leaving upstream_subject NULL"
            );
            return None;
        }
        Err(e) => {
            tracing::debug!(
                endpoint = %endpoint,
                error = %e,
                "userinfo fetch failed — leaving upstream_subject NULL"
            );
            return None;
        }
    };
    extract_subject_from_json(&body)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(super) async fn load_server(state: &AppState, server_id: Uuid) -> Result<McpServer, AppError> {
    server_repo::find_without_counts(&state.db, server_id)
        .await?
        .ok_or_else(|| AppError::NotFound("MCP server not found".into()))
}

// ---------------------------------------------------------------------------

/// Minimal URL-fragment encoder. Only protects the characters that
/// would corrupt the `#k=v` fragment shape (`/`, `#`, `&`, `=`, ` `).
/// Real URL crates assume reserved-character semantics in fragments
/// that don't apply here — we want a literal account_label round-tripped
/// to the SPA, not a parsed URL parameter.
pub(super) fn urlencode_fragment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '#' | '&' | '=' | ' ' | '/' | '%' => format!("%{:02X}", c as u8),
            _ => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests for `pkce_challenge`, `random_token`, `state_binding`,
    // and `parse_token_endpoint_error` live in `think_watch_auth::oauth::pkce`
    // now — pure crypto helpers tested in the crate that owns them.

    #[test]
    fn fragment_encoder_escapes_only_dangerous_chars() {
        assert_eq!(urlencode_fragment("hello"), "hello");
        assert_eq!(urlencode_fragment("a b"), "a%20b");
        assert_eq!(urlencode_fragment("a#b"), "a%23b");
        assert_eq!(urlencode_fragment("a/b"), "a%2Fb");
    }
}
