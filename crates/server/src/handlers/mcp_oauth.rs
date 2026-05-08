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

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use data_encoding::BASE64URL_NOPAD;
use hmac::{Hmac, Mac, digest::KeyInit};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use think_watch_common::audit::AuditEntry;
use think_watch_common::crypto::{self, parse_encryption_key};
use think_watch_common::errors::AppError;
use think_watch_common::models::McpServer;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

const OAUTH_STATE_PREFIX: &str = "mcp_oauth:state:";
const OAUTH_STATE_TTL_SECS: i64 = 600;

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
enum OauthStateTarget {
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
struct McpOauthState {
    target: OauthStateTarget,
    /// PKCE code_verifier — sent to the upstream token endpoint to
    /// prove the same client that started the flow is finishing it.
    code_verifier: String,
    /// Captured at authorize time so the token-endpoint exchange
    /// presents the exact same value the upstream saw at /authorize.
    redirect_uri: String,
    /// HMAC-SHA256(encryption_key, state || ":" || code_verifier).
    /// Catches Redis tampering — only a server holding the encryption
    /// key can forge a matching pair.
    binding: String,
}

/// Look at the upstream token-endpoint response body for an OAuth 2.0
/// error envelope (`{"error": "...", "error_description": "..."}`).
/// Returns a one-line user-facing summary if the body is shaped like
/// an error, `None` otherwise.
///
/// Per RFC 6749 §5.2 the error response is JSON with at least an
/// `error` field. We tolerate non-spec upstreams that omit
/// `error_description` and just fall back to `error` alone.
fn parse_token_endpoint_error(body: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ErrorEnvelope {
        error: String,
        #[serde(default)]
        error_description: Option<String>,
    }
    let env: ErrorEnvelope = serde_json::from_str(body).ok()?;
    Some(match env.error_description {
        Some(d) if !d.is_empty() => format!("{} ({})", d, env.error),
        _ => env.error,
    })
}

fn state_binding(enc_key: &[u8; 32], state: &str, verifier: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(enc_key).expect("HMAC-SHA256 accepts any key length");
    mac.update(state.as_bytes());
    mac.update(b":");
    mac.update(verifier.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 32 random bytes → URL-safe base64 with no padding. Used for both
/// the state token and the PKCE code_verifier (RFC 7636 mandates
/// `[A-Z][a-z][0-9]-._~`, 43–128 chars; base64url-no-pad of 32 bytes
/// gives 43 URL-safe characters).
fn random_token() -> Result<String, AppError> {
    let bytes: [u8; 32] = rand::rng().random();
    Ok(BASE64URL_NOPAD.encode(&bytes))
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    BASE64URL_NOPAD.encode(&digest)
}

/// Fully-qualified base URL the OAuth provider should redirect back
/// to. The first CORS origin is the canonical console URL — same
/// pattern the SSO callback uses.
fn callback_base_url(state: &AppState) -> Result<String, AppError> {
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

fn callback_redirect_uri(state: &AppState) -> Result<String, AppError> {
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

    let servers = sqlx::query_as::<_, McpServer>(
        r#"SELECT s.*, 0::bigint AS tools_count, 0::bigint AS call_count
             FROM mcp_servers s
            ORDER BY s.name"#,
    )
    .fetch_all(&state.db)
    .await?;

    #[derive(sqlx::FromRow)]
    struct AccountRow {
        mcp_server_id: Uuid,
        account_label: String,
        credential_type: String,
        is_default: bool,
        scopes: Vec<String>,
        expires_at: Option<DateTime<Utc>>,
        upstream_subject: Option<String>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    }
    let rows = sqlx::query_as::<_, AccountRow>(
        r#"SELECT mcp_server_id, account_label, credential_type, is_default,
                  scopes, expires_at, upstream_subject, created_at, updated_at
             FROM mcp_user_credentials
            WHERE user_id = $1
            ORDER BY mcp_server_id, is_default DESC, account_label"#,
    )
    .bind(auth_user.claims.sub)
    .fetch_all(&state.db)
    .await?;

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

    let state_token = random_token()?;
    let code_verifier = random_token()?;
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
        tracing::warn!("MCP OAuth state binding mismatch for state {state_token}");
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
        .map(|s| Utc::now() + chrono::Duration::seconds(s as i64));
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
    match &blob.target {
        OauthStateTarget::PerUser {
            server_id,
            user_id,
            account_label,
        } => {
            let server = load_server(&state, *server_id).await?;
            upsert_credential(
                &state,
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
                token.access_token.clone(),
            );
            state.audit.log(
                AuditEntry::new("mcp.connection.authorized")
                    .user_id(*user_id)
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
            upsert_shared_credential(
                &state,
                *server_id,
                "oauth_authcode",
                &access_encrypted,
                refresh_encrypted.as_deref(),
                expires_at,
                &scopes,
                upstream_subject.as_deref(),
                *configured_by,
            )
            .await?;
            think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
                .invalidate_server_lane(server_id)
                .await;
            spawn_shared_tool_discovery(
                state.db.clone(),
                (**state.http_client.load()).clone(),
                server.clone(),
                token.access_token.clone(),
            );
            state.audit.log(
                AuditEntry::new("mcp.shared_credential.authorized")
                    .user_id(*configured_by)
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
                wizard_credential_redis_key(wizard_session_id),
                payload.to_string(),
                Some(fred::types::Expiration::EX(WIZARD_CREDENTIAL_TTL_SECS)),
                None,
                false,
            )
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;

            state.audit.log(
                AuditEntry::new("mcp.wizard.shared_credential_authorized")
                    .user_id(*configured_by)
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
const WIZARD_CREDENTIAL_TTL_SECS: i64 = 3_600;

/// Redis key under which the OAuth callback parks a wizard's pending
/// shared credential. Also used by `finalize_wizard_credential` to
/// GETDEL the payload at server-create time.
fn wizard_credential_redis_key(wizard_session_id: &str) -> String {
    format!("mcp_wizard:cred:{wizard_session_id}")
}

/// Resolved OAuth client config + tail-state metadata used by the
/// callback to run a token exchange. Bridges the two cases:
///   1. The OAuth flow was started against an existing server row —
///      config comes from the row.
///   2. The flow was started from the new-server wizard — config is
///      baked into the state blob (no row exists yet).
struct ExchangeContext {
    token_endpoint: String,
    client_id: String,
    /// Decrypted plaintext. `None` for public clients.
    client_secret: Option<String>,
    default_scopes: Vec<String>,
    userinfo_endpoint: Option<String>,
}

async fn build_exchange_context(
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

fn decrypt_optional_secret(
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

/// POST to the upstream token endpoint with PKCE. Extracted from the
/// callback so the per-user, admin-shared, and wizard paths all share
/// one error envelope and one transport.
async fn oauth_token_exchange(
    http: &reqwest::Client,
    cfg: &ExchangeContext,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenEndpointResponse, AppError> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", cfg.client_id.as_str()),
        ("code_verifier", code_verifier),
    ];
    if let Some(secret) = cfg.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let body = serde_urlencoded::to_string(&form)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encode token form: {e}")))?;

    let resp = http
        .post(&cfg.token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Token endpoint unreachable: {e}")))?;

    let status = resp.status();
    let resp_text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail =
            parse_token_endpoint_error(&resp_text).unwrap_or_else(|| format!("HTTP {status}"));
        return Err(AppError::BadRequest(format!(
            "Upstream rejected the OAuth exchange: {detail}. \
             The MCP server's OAuth client_id/secret may be misconfigured — \
             ask an administrator to verify them at /mcp/servers."
        )));
    }
    if let Some(detail) = parse_token_endpoint_error(&resp_text) {
        return Err(AppError::BadRequest(format!(
            "Upstream rejected the OAuth exchange: {detail}. \
             The MCP server's OAuth client_id/secret may be misconfigured — \
             ask an administrator to verify them at /mcp/servers."
        )));
    }
    serde_json::from_str(&resp_text)
        .map_err(|e| AppError::BadRequest(format!("Token response not JSON: {e}: {resp_text}")))
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
    let row: Option<(String, Vec<u8>)> = sqlx::query_as(
        r#"SELECT credential_type, access_token_encrypted
             FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
    )
    .bind(server_id)
    .bind(auth_user.claims.sub)
    .bind(&account_label)
    .fetch_optional(&state.db)
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

    sqlx::query(
        r#"DELETE FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
    )
    .bind(server_id)
    .bind(auth_user.claims.sub)
    .bind(&account_label)
    .execute(&state.db)
    .await?;

    // Cached responses pinned to this credential are now serving an
    // identity that no longer has access. Wipe the user's lane for
    // this server so post-revoke reads can't tunnel back to the
    // pre-revoke epoch.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_user_lane(&server_id, &auth_user.claims.sub)
        .await;

    state.audit.log(
        AuditEntry::new("mcp.connection.revoked")
            .user_id(auth_user.claims.sub)
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

    let mut tx = state.db.begin().await?;
    let exists: Option<i32> = sqlx::query_scalar(
        r#"SELECT 1 FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
    )
    .bind(server_id)
    .bind(auth_user.claims.sub)
    .bind(&account_label)
    .fetch_optional(&mut *tx)
    .await?;
    if exists.is_none() {
        return Err(AppError::NotFound("Connection not found".into()));
    }

    // Two-step toggle so the partial unique index never sees two
    // is_default rows at once: clear the old default first, then mark
    // the new one inside the same transaction.
    sqlx::query(
        r#"UPDATE mcp_user_credentials SET is_default = false, updated_at = now()
            WHERE mcp_server_id = $1 AND user_id = $2 AND is_default"#,
    )
    .bind(server_id)
    .bind(auth_user.claims.sub)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        r#"UPDATE mcp_user_credentials SET is_default = true, updated_at = now()
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
    )
    .bind(server_id)
    .bind(auth_user.claims.sub)
    .bind(&account_label)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // Switching default flips which credential the resolver picks
    // when no API-key override is set. The no-override lane (`_`)
    // is now serving responses pinned to the *old* default's
    // upstream identity — wipe so post-switch reads see the new
    // default's data.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_user_lane(&server_id, &auth_user.claims.sub)
        .await;

    state.audit.log(
        AuditEntry::new("mcp.connection.default_set")
            .user_id(auth_user.claims.sub)
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

    upsert_credential(
        &state,
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
        req.token.trim().to_string(),
    );

    state.audit.log(
        AuditEntry::new("mcp.connection.authorized")
            .user_id(auth_user.claims.sub)
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
fn spawn_user_tool_discovery(
    db: sqlx::PgPool,
    http: reqwest::Client,
    endpoint_url: String,
    server_name: String,
    server_id: Uuid,
    user_id: Uuid,
    bearer_token: String,
) {
    tokio::spawn(async move {
        match crate::mcp_runtime::discover_user_tools(
            &db,
            &http,
            &endpoint_url,
            server_id,
            user_id,
            &bearer_token,
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
    let exists: Option<i32> = sqlx::query_scalar(
        r#"SELECT 1 FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
    )
    .bind(server_id)
    .bind(auth_user.claims.sub)
    .bind(&account_label)
    .fetch_optional(&state.db)
    .await?;
    if exists.is_none() {
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
        AuditEntry::new("mcp.connection.tested")
            .user_id(auth_user.claims.sub)
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

/// Field names the resolver searches for, in priority order. The list
/// covers OIDC standard claims first, then the conventions every major
/// non-OIDC public OAuth provider has converged on:
///
/// - `sub` / `preferred_username` / `email`: OIDC standard claims.
/// - `accountId`: Atlassian (Jira, Confluence) `/me` response.
/// - `login`: GitHub `/user` response.
/// - `username` / `name`: Slack `users.identity`, Discord `/users/@me`.
/// - `id`: Notion `/v1/users/me`, Cloudflare `/user`, generic JSON:API.
///
/// `email` is the last resort because some providers stuff the *user's
/// own* email at top-level (good signal) but others embed an *org*
/// email under a different node (bad signal); putting it last means
/// the more specific `id`-flavoured fields win when both are present.
const SUBJECT_KEYS: &[&str] = &[
    "preferred_username",
    "sub",
    "accountId",
    "login",
    "username",
    "name",
    "id",
    "email",
];

/// JSON node names the resolver descends into when no top-level key
/// matches. Matches the shape Slack returns (`{ "user": { ... } }`)
/// and the JSON:API convention (`{ "data": { ... } }`). Limited to
/// ONE level of recursion so we don't accidentally surface a nested
/// org / team identifier as the user's identity.
const NESTED_WRAPPERS: &[&str] = &["user", "data", "account", "results"];

/// Best-effort upstream-identity resolution. Never errors — failures
/// fall through to `None` and the column stays NULL.
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

/// Try to read a subject claim out of the access_token's JWT payload.
/// Returns `None` for opaque tokens (anything that isn't 3 dot-
/// separated base64url segments whose middle segment decodes to a
/// JSON object).
fn subject_from_jwt(access_token: &str) -> Option<String> {
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload_bytes = data_encoding::BASE64URL_NOPAD
        .decode(parts[1].as_bytes())
        // Some encoders include padding even though RFC 7515 forbids
        // it. Trim and retry before giving up.
        .or_else(|_| data_encoding::BASE64URL.decode(parts[1].trim_end_matches('=').as_bytes()))
        .ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    extract_subject_from_json(&payload)
}

/// Walk a JSON object looking for the first non-empty subject-like
/// field, preferring top-level keys over nested wrappers and
/// preferring more-specific names (`preferred_username`) over less
/// (`email`).
fn extract_subject_from_json(v: &serde_json::Value) -> Option<String> {
    let obj = v.as_object()?;
    // Pass 1: prefer top-level matches in priority order.
    for key in SUBJECT_KEYS {
        if let Some(s) = obj.get(*key).and_then(stringify_subject) {
            return Some(s);
        }
    }
    // Pass 2: descend into a recognised wrapper. Only one level — we
    // don't want to surface a deeply-nested team/org identifier as
    // the user's identity.
    for wrapper in NESTED_WRAPPERS {
        let Some(inner_v) = obj.get(*wrapper) else {
            continue;
        };
        // `results` (JSON:API) wraps an array; take the first element.
        let inner = match inner_v {
            serde_json::Value::Array(items) => items.first()?,
            other => other,
        };
        let Some(inner_obj) = inner.as_object() else {
            continue;
        };
        for key in SUBJECT_KEYS {
            if let Some(s) = inner_obj.get(*key).and_then(stringify_subject) {
                return Some(s);
            }
        }
    }
    None
}

/// Coerce a JSON value into the string we'd display to the user.
/// Strings pass through unchanged; integers (some upstreams return
/// `id` as a number) get stringified; everything else is rejected so
/// we never paint `null` / `false` / `[]` into the UI.
fn stringify_subject(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// POST /api/admin/mcp/oauth-discover — RFC 8414 metadata fetch
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DiscoverRequest {
    pub issuer: String,
}

#[derive(Debug, Serialize)]
pub struct DiscoverResponse {
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub userinfo_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
}

/// Fetch OAuth metadata from `{issuer}/.well-known/oauth-authorization-server`
/// (RFC 8414) so the admin form can autofill the endpoint inputs without
/// the operator hand-copying URLs from the upstream's docs.
///
/// Falls back to `/.well-known/openid-configuration` for OIDC-style
/// providers that publish their metadata under that path instead.
/// Returns whatever fields the upstream advertised — the admin UI
/// merges them into the form, so partial responses are fine.
pub async fn oauth_discover(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<DiscoverRequest>,
) -> Result<Json<DiscoverResponse>, AppError> {
    auth_user.require_permission("mcp_servers:create")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:create")
        .await?;
    if req.issuer.is_empty() {
        return Err(AppError::BadRequest("issuer is required".into()));
    }
    (state.url_validator)(&req.issuer)?;

    let issuer = req.issuer.trim_end_matches('/');
    let candidates = [
        format!("{issuer}/.well-known/oauth-authorization-server"),
        format!("{issuer}/.well-known/openid-configuration"),
    ];

    let http = state.http_client.load();
    let mut last_err: Option<String> = None;
    for url in &candidates {
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        last_err = Some(format!("{url}: {e}"));
                        continue;
                    }
                };
                return Ok(Json(parse_oauth_metadata(&body)));
            }
            Ok(resp) => {
                last_err = Some(format!("{url}: HTTP {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(format!("{url}: {e}"));
            }
        }
    }
    Err(AppError::BadRequest(format!(
        "Discovery failed at all known well-known paths. Last error: {}",
        last_err.unwrap_or_else(|| "unknown".into())
    )))
}

fn parse_oauth_metadata(body: &serde_json::Value) -> DiscoverResponse {
    let parsed = parse_authz_server_metadata(body);
    DiscoverResponse {
        authorization_endpoint: parsed.authorization_endpoint,
        token_endpoint: parsed.token_endpoint,
        revocation_endpoint: parsed.revocation_endpoint,
        userinfo_endpoint: parsed.userinfo_endpoint,
        scopes_supported: parsed.scopes_supported,
    }
}

/// Full RFC 8414 / OIDC discovery doc shape — superset of
/// [`DiscoverResponse`] that also captures the `issuer` claim and the
/// `registration_endpoint` (RFC 7591 dynamic client registration).
/// The probe handler needs the extras; the legacy `oauth_discover`
/// keeps its narrower response shape for back-compat.
#[derive(Debug)]
struct AuthzServerMetadata {
    issuer: Option<String>,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    userinfo_endpoint: Option<String>,
    registration_endpoint: Option<String>,
    scopes_supported: Vec<String>,
    /// Raw `token_endpoint_auth_methods_supported` list. Used by DCR
    /// to pick a method the AS will accept; presence of `"none"`
    /// also flips `is_public_client` on the probe response.
    token_endpoint_auth_methods: Vec<String>,
}

impl AuthzServerMetadata {
    fn public_client_supported(&self) -> bool {
        self.token_endpoint_auth_methods.iter().any(|m| m == "none")
    }
}

fn parse_authz_server_metadata(body: &serde_json::Value) -> AuthzServerMetadata {
    fn s(v: &serde_json::Value, k: &str) -> Option<String> {
        v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string())
    }
    fn arr(v: &serde_json::Value, k: &str) -> Vec<String> {
        v.get(k)
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }
    AuthzServerMetadata {
        issuer: s(body, "issuer"),
        authorization_endpoint: s(body, "authorization_endpoint"),
        token_endpoint: s(body, "token_endpoint"),
        revocation_endpoint: s(body, "revocation_endpoint"),
        userinfo_endpoint: s(body, "userinfo_endpoint"),
        registration_endpoint: s(body, "registration_endpoint"),
        scopes_supported: arr(body, "scopes_supported"),
        token_endpoint_auth_methods: arr(body, "token_endpoint_auth_methods_supported"),
    }
}

// ---------------------------------------------------------------------------
// POST /api/admin/mcp/oauth-probe — full MCP-spec auto-discovery chain
// ---------------------------------------------------------------------------
//
// Maps the GitHub Copilot / VS Code MCP UX of "paste URL, done" onto our
// admin form. Three RFCs glued together:
//
//   1. RFC 9728 (OAuth Protected Resource Metadata) — given an MCP wire
//      endpoint, find the authorization server. Tries the
//      `WWW-Authenticate: Bearer resource_metadata="..."` hint first
//      (per MCP spec 2025-06-18 §authorization), falls back to
//      `<endpoint>/.well-known/oauth-protected-resource`.
//   2. RFC 8414 (Authorization Server Metadata) — given the issuer,
//      fetch authorize/token/revocation/userinfo + (crucially)
//      `registration_endpoint`. Reuses `parse_authz_server_metadata`.
//   3. RFC 7591 (Dynamic Client Registration) — POST to
//      `registration_endpoint` with the console's redirect_uri to
//      mint a fresh client_id/secret. This is the magic that lets the
//      admin skip "go register an OAuth app at github.com/developers"
//      entirely. Skipped when the AS doesn't advertise registration.
//
// Each step is best-effort: we return whatever we got, plus a
// human-readable `diagnostic` string so the UI can either auto-fill
// the form silently or surface "step N failed because X — please fill
// the rest manually."

#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    pub endpoint_url: String,
}

#[derive(Debug, Serialize)]
pub struct ProbeResponse {
    pub issuer: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub userinfo_endpoint: Option<String>,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    pub client_id: Option<String>,
    /// Plaintext — the frontend echoes this straight into the form for
    /// the admin to review. Persisted encrypted on save like any
    /// hand-entered secret.
    pub client_secret: Option<String>,
    /// True when the upstream AS advertises `"none"` in
    /// `token_endpoint_auth_methods_supported` — admin only needs to
    /// paste a Client ID, no Client Secret. Many MCP-spec-aligned
    /// providers (Feishu, Cloudflare, recent Anthropic-spec compliant
    /// servers) advertise this so they're compatible with native-app
    /// public-client flows.
    pub is_public_client: bool,
    /// The callback URL admins should register at the upstream's
    /// developer console (e.g. open.feishu.cn) when DCR is gated /
    /// unsupported. Surfacing this from the probe means the admin can
    /// copy-paste it directly instead of guessing the gateway's host.
    pub redirect_uri: String,
    /// Step-by-step trace: one entry per discovery step, in order.
    /// Frontend renders them as a numbered list inside an expandable
    /// "details" section — one event per line beats a `→`-joined wall
    /// of text once the chain has 4+ steps.
    pub diagnostic: Vec<String>,
}

pub async fn oauth_probe(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<ProbeRequest>,
) -> Result<Json<ProbeResponse>, AppError> {
    auth_user.require_permission("mcp_servers:create")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:create")
        .await?;

    // Each probe makes 3-4 outbound HTTP requests against admin-supplied
    // URLs. Same per-user 5/min cap as the other probe endpoints to
    // keep this from being abused as a port scanner.
    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp_oauth_probe",
    )
    .await?;

    if req.endpoint_url.is_empty() {
        return Err(AppError::BadRequest("endpoint_url is required".into()));
    }
    (state.url_validator)(&req.endpoint_url)?;

    let http = state.http_client.load();
    let validator = state.url_validator.clone();
    let mut diag: Vec<String> = Vec::new();
    let redirect_uri = callback_redirect_uri(&state)?;

    // Step 1: find issuer via Protected Resource Metadata.
    let issuer = match discover_issuer(&http, &validator, &req.endpoint_url, &mut diag).await {
        Some(iss) => iss,
        None => {
            return Ok(Json(ProbeResponse {
                issuer: None,
                authorization_endpoint: None,
                token_endpoint: None,
                revocation_endpoint: None,
                userinfo_endpoint: None,
                registration_endpoint: None,
                scopes_supported: vec![],
                client_id: None,
                client_secret: None,
                is_public_client: false,
                redirect_uri,
                diagnostic: diag,
            }));
        }
    };

    // Step 2: fetch authorization server metadata from the issuer.
    let meta = fetch_authz_server_metadata(&http, &validator, &issuer, &mut diag).await;

    // Step 3: dynamic client registration if the AS advertises it.
    let (client_id, client_secret) = match meta
        .registration_endpoint
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(reg_endpoint) => {
            match register_dynamic_client(&http, reg_endpoint, &meta, &state, &mut diag).await {
                Some((id, secret)) => (Some(id), secret),
                None => (None, None),
            }
        }
        None => {
            diag.push("no registration_endpoint — fill client_id/client_secret manually".into());
            (None, None)
        }
    };

    let is_public_client = meta.public_client_supported();
    Ok(Json(ProbeResponse {
        issuer: meta.issuer.or(Some(issuer)),
        authorization_endpoint: meta.authorization_endpoint,
        token_endpoint: meta.token_endpoint,
        revocation_endpoint: meta.revocation_endpoint,
        userinfo_endpoint: meta.userinfo_endpoint,
        registration_endpoint: meta.registration_endpoint,
        scopes_supported: meta.scopes_supported,
        client_id,
        client_secret,
        is_public_client,
        redirect_uri,
        diagnostic: diag,
    }))
}

/// Step 1 — RFC 9728. Returns the issuer URL the MCP endpoint points to.
async fn discover_issuer(
    http: &reqwest::Client,
    validator: &crate::app::UrlValidator,
    endpoint_url: &str,
    diag: &mut Vec<String>,
) -> Option<String> {
    // 1a. POST a minimal MCP `initialize` to trigger the auth challenge.
    // GET against an MCP endpoint typically returns 405 because the
    // protocol is JSON-RPC over POST — only real protocol requests get
    // the 401 + WWW-Authenticate response mandated by MCP spec
    // 2025-06-18 §authorization. We never read the body, only the
    // header; the `id` is arbitrary.
    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "ThinkWatch probe", "version": "0" }
        }
    });
    if let Ok(resp) = http
        .post(endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&init)
        .send()
        .await
        && let Some(hint) = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_resource_metadata_hint)
    {
        diag.push(format!("got resource_metadata hint from {endpoint_url}"));
        if let Some(iss) = fetch_protected_resource(http, validator, &hint, diag).await {
            return Some(iss);
        }
    }

    // 1b. Fall back to RFC 9728 §3.1 well-known transformation: for a
    // resource at `https://host/foo/bar`, metadata lives at
    // `https://host/.well-known/oauth-protected-resource/foo/bar`. We
    // try the path-aware form first, then the bare form (some
    // implementations publish at the bare path even when the resource
    // has a path).
    if let Some(parsed) = parse_origin_and_path(endpoint_url) {
        let candidates = if parsed.path.is_empty() {
            vec![format!(
                "{}/.well-known/oauth-protected-resource",
                parsed.origin
            )]
        } else {
            vec![
                format!(
                    "{}/.well-known/oauth-protected-resource{}",
                    parsed.origin, parsed.path
                ),
                format!("{}/.well-known/oauth-protected-resource", parsed.origin),
            ]
        };
        for url in &candidates {
            if let Some(iss) = fetch_protected_resource(http, validator, url, diag).await {
                return Some(iss);
            }
        }
        // 1c. Last resort — endpoint origin might already be the issuer.
        diag.push(format!(
            "no protected-resource metadata; trying {} as issuer directly",
            parsed.origin
        ));
        return Some(parsed.origin);
    }

    diag.push("could not derive issuer from endpoint_url".into());
    None
}

/// Parsed `(origin, path)` of a URL. Path is normalized to drop a
/// trailing slash and treat `/` as empty. Used to build RFC 8414 /
/// RFC 9728 well-known URLs, which require the `.well-known` segment
/// between the origin and the resource/issuer path.
struct OriginAndPath {
    origin: String,
    path: String,
}

fn parse_origin_and_path(url_str: &str) -> Option<OriginAndPath> {
    let url = url::Url::parse(url_str).ok()?;
    let host = url.host_str()?;
    let scheme = url.scheme();
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let origin = format!("{scheme}://{host}{port}");
    let raw = url.path().trim_end_matches('/');
    let path = if raw == "/" || raw.is_empty() {
        String::new()
    } else {
        raw.to_string()
    };
    Some(OriginAndPath { origin, path })
}

/// Parse `WWW-Authenticate: Bearer resource_metadata="https://..."`.
/// Returns the URL, or None if the header isn't shaped that way.
fn parse_resource_metadata_hint(header: &str) -> Option<String> {
    // The header is a comma-separated parameter list; we just want the
    // resource_metadata one. Sloppy parser, but the value is always
    // double-quoted per RFC 9728 §5.1.
    let key = "resource_metadata=";
    let idx = header.find(key)?;
    let rest = &header[idx + key.len()..];
    let trimmed = rest.trim_start_matches('"');
    let end = trimmed.find('"')?;
    Some(trimmed[..end].to_string())
}

async fn fetch_protected_resource(
    http: &reqwest::Client,
    validator: &crate::app::UrlValidator,
    url: &str,
    diag: &mut Vec<String>,
) -> Option<String> {
    if validator(url).is_err() {
        diag.push(format!("rejected {url} (SSRF guard)"));
        return None;
    }
    let resp = match http.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            diag.push(format!("{url}: HTTP {}", r.status()));
            return None;
        }
        Err(e) => {
            diag.push(format!("{url}: {e}"));
            return None;
        }
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            diag.push(format!("{url}: parse: {e}"));
            return None;
        }
    };
    let issuer = body
        .get("authorization_servers")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|x| x.as_str())
        .map(String::from);
    if issuer.is_some() {
        diag.push(format!("found issuer via {url}"));
    } else {
        diag.push(format!(
            "{url}: no authorization_servers[] in protected-resource metadata"
        ));
    }
    issuer
}

/// Step 2 — RFC 8414 / OIDC. Returns the richer struct with optional
/// fields populated from whichever well-known URL responded first.
///
/// RFC 8414 §3 requires the `.well-known` segment between the issuer's
/// origin and its path: for `issuer = https://host/foo`, the metadata
/// lives at `https://host/.well-known/oauth-authorization-server/foo`,
/// NOT `https://host/foo/.well-known/...`. We try both shapes — the
/// spec-compliant one first, then the off-spec "under the path" form
/// some implementations still use.
async fn fetch_authz_server_metadata(
    http: &reqwest::Client,
    validator: &crate::app::UrlValidator,
    issuer: &str,
    diag: &mut Vec<String>,
) -> AuthzServerMetadata {
    let candidates = build_authz_metadata_candidates(issuer);
    for url in &candidates {
        if validator(url).is_err() {
            diag.push(format!("rejected {url} (SSRF guard)"));
            continue;
        }
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(body) => {
                        diag.push(format!("found authz-server metadata at {url}"));
                        return parse_authz_server_metadata(&body);
                    }
                    Err(e) => diag.push(format!("{url}: parse: {e}")),
                }
            }
            Ok(r) => diag.push(format!("{url}: HTTP {}", r.status())),
            Err(e) => diag.push(format!("{url}: {e}")),
        }
    }
    AuthzServerMetadata {
        issuer: None,
        authorization_endpoint: None,
        token_endpoint: None,
        revocation_endpoint: None,
        userinfo_endpoint: None,
        registration_endpoint: None,
        scopes_supported: vec![],
        token_endpoint_auth_methods: vec![],
    }
}

/// Build the well-known URL candidate list for an issuer, ordered
/// most-likely-correct first. Pure function so it's easily unit tested.
fn build_authz_metadata_candidates(issuer: &str) -> Vec<String> {
    let Some(parsed) = parse_origin_and_path(issuer) else {
        return vec![];
    };
    if parsed.path.is_empty() {
        return vec![
            format!("{}/.well-known/oauth-authorization-server", parsed.origin),
            format!("{}/.well-known/openid-configuration", parsed.origin),
        ];
    }
    vec![
        // RFC 8414 §3 spec-correct form (".well-known" before path).
        format!(
            "{}/.well-known/oauth-authorization-server{}",
            parsed.origin, parsed.path
        ),
        format!(
            "{}/.well-known/openid-configuration{}",
            parsed.origin, parsed.path
        ),
        // Legacy "under the path" form (still seen in older OIDC
        // deployments that ignored RFC 8414 and concatenated naively).
        format!(
            "{}{}/.well-known/oauth-authorization-server",
            parsed.origin, parsed.path
        ),
        format!(
            "{}{}/.well-known/openid-configuration",
            parsed.origin, parsed.path
        ),
    ]
}

/// Stable RFC 7591 `software_id` for ThinkWatch's MCP gateway.
/// Same UUID across every deployment so an AS that audit-logs by
/// `software_id` can group all ThinkWatch installs as one product.
/// Don't change this — RFC 7591 §2 says it SHOULD remain the same
/// for all instances of the client software.
const TW_DCR_SOFTWARE_ID: &str = "0d1f3a2b-4c5e-4f6a-8b9c-0d1e2f3a4b5c";

/// Pick the `token_endpoint_auth_method` we'll request at registration
/// time. Prefer `none` (public client + PKCE — simplest and supported
/// by our token-endpoint code), then `client_secret_post` (also
/// supported), then fall through to whatever the AS lists. Defaults to
/// `client_secret_post` when the AS doesn't advertise the array, which
/// is the most widely accepted method.
fn pick_dcr_auth_method(supported: &[String]) -> &'static str {
    let has = |m: &str| supported.iter().any(|s| s == m);
    if has("none") {
        "none"
    } else if has("client_secret_post") {
        "client_secret_post"
    } else if has("client_secret_basic") {
        // We don't natively send the Basic header at the token
        // endpoint today, but registering for `_basic` lets the admin
        // fall back to a hand-edit later if needed. AS that strictly
        // require basic will reject `_post`.
        "client_secret_basic"
    } else {
        "client_secret_post"
    }
}

/// Step 3 — RFC 7591 dynamic client registration. POSTs an enriched
/// metadata body (RFC 7591 §2: `software_id`, `software_version`,
/// `application_type`, `scope`, picked `token_endpoint_auth_method`)
/// and returns the assigned `(client_id, client_secret?)`.
/// Best-effort: any failure leaves both fields blank for manual entry.
async fn register_dynamic_client(
    http: &reqwest::Client,
    registration_endpoint: &str,
    meta: &AuthzServerMetadata,
    state: &AppState,
    diag: &mut Vec<String>,
) -> Option<(String, Option<String>)> {
    if (state.url_validator)(registration_endpoint).is_err() {
        diag.push(format!(
            "rejected registration_endpoint {registration_endpoint} (SSRF guard)"
        ));
        return None;
    }
    let redirect_uri = match callback_redirect_uri(state) {
        Ok(u) => u,
        Err(e) => {
            diag.push(format!("redirect_uri build failed: {e}"));
            return None;
        }
    };
    let auth_method = pick_dcr_auth_method(&meta.token_endpoint_auth_methods);
    let scope_join = meta.scopes_supported.join(" ");
    let mut body = serde_json::json!({
        "client_name": "ThinkWatch MCP gateway",
        "client_uri": callback_base_url(state).ok(),
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": auth_method,
        // Always `web` — we run server-side, the redirect_uri is HTTPS,
        // PKCE is mandatory regardless of `application_type`. RFC 7591
        // §2 / OAuth 2.0 §2.1.
        "application_type": "web",
        // Stable across deployments per RFC 7591 §2 — see constant above.
        "software_id": TW_DCR_SOFTWARE_ID,
        "software_version": env!("CARGO_PKG_VERSION"),
    });
    // Only include `scope` when the AS published `scopes_supported` —
    // claiming arbitrary scopes against an AS that didn't tell us
    // what's available is more likely to be rejected than help.
    if !scope_join.is_empty()
        && let serde_json::Value::Object(ref mut obj) = body
    {
        obj.insert("scope".into(), serde_json::Value::String(scope_join));
    }
    let resp = match http
        .post(registration_endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            diag.push(format!("dynamic-registration request failed: {e}"));
            return None;
        }
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        diag.push(format!(
            "dynamic-registration HTTP {status}: {}",
            text.chars().take(120).collect::<String>()
        ));
        return None;
    }
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            diag.push(format!("dynamic-registration parse: {e}"));
            return None;
        }
    };
    let client_id = parsed
        .get("client_id")
        .and_then(|v| v.as_str())
        .map(String::from)?;
    let client_secret = parsed
        .get("client_secret")
        .and_then(|v| v.as_str())
        .map(String::from);
    diag.push(format!(
        "dynamic-registration ok (client_id={}, secret={})",
        client_id,
        client_secret.as_ref().map(|_| "yes").unwrap_or("no")
    ));
    Some((client_id, client_secret))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenEndpointResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

async fn load_server(state: &AppState, server_id: Uuid) -> Result<McpServer, AppError> {
    sqlx::query_as::<_, McpServer>(
        r#"SELECT s.*, 0::bigint AS tools_count, 0::bigint AS call_count
             FROM mcp_servers s WHERE s.id = $1"#,
    )
    .bind(server_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("MCP server not found".into()))
}

#[allow(clippy::too_many_arguments)]
async fn upsert_credential(
    state: &AppState,
    server_id: Uuid,
    user_id: Uuid,
    account_label: &str,
    credential_type: &str,
    access_encrypted: &[u8],
    refresh_encrypted: Option<&[u8]>,
    expires_at: Option<DateTime<Utc>>,
    scopes: &[String],
    upstream_subject: Option<&str>,
) -> Result<(), AppError> {
    // First credential for (server, user) becomes the default. We
    // detect that with a separate SELECT inside the same TX so a race
    // can't elect two defaults.
    let mut tx = state.db.begin().await?;
    let any_existing: Option<i32> = sqlx::query_scalar(
        r#"SELECT 1 FROM mcp_user_credentials
            WHERE mcp_server_id = $1 AND user_id = $2 LIMIT 1"#,
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let new_default = any_existing.is_none();

    sqlx::query(
        r#"INSERT INTO mcp_user_credentials (
               mcp_server_id, user_id, account_label, credential_type, is_default,
               access_token_encrypted, refresh_token_encrypted,
               expires_at, scopes, upstream_subject
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (mcp_server_id, user_id, account_label) DO UPDATE SET
               credential_type         = EXCLUDED.credential_type,
               access_token_encrypted  = EXCLUDED.access_token_encrypted,
               refresh_token_encrypted = EXCLUDED.refresh_token_encrypted,
               expires_at              = EXCLUDED.expires_at,
               scopes                  = EXCLUDED.scopes,
               upstream_subject        = EXCLUDED.upstream_subject,
               updated_at              = now()"#,
    )
    .bind(server_id)
    .bind(user_id)
    .bind(account_label)
    .bind(credential_type)
    .bind(new_default)
    .bind(access_encrypted)
    .bind(refresh_encrypted)
    .bind(expires_at)
    .bind(scopes)
    .bind(upstream_subject)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Admin: shared-credential storage
// ---------------------------------------------------------------------------

/// UPSERT into `mcp_server_shared_credentials`. Single row per server
/// — when the admin rotates the credential the new row replaces the
/// previous one. Uses `INSERT … ON CONFLICT` keyed on the server_id
/// PK so the lifecycle code in [`UserTokenResolver`] sees a fresh
/// `(access_token_encrypted, expires_at)` after a rotation without
/// any extra coordination.
#[allow(clippy::too_many_arguments)]
async fn upsert_shared_credential(
    state: &AppState,
    server_id: Uuid,
    credential_type: &str,
    access_encrypted: &[u8],
    refresh_encrypted: Option<&[u8]>,
    expires_at: Option<DateTime<Utc>>,
    scopes: &[String],
    upstream_subject: Option<&str>,
    configured_by: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO mcp_server_shared_credentials (
               mcp_server_id, credential_type,
               access_token_encrypted, refresh_token_encrypted,
               expires_at, scopes, upstream_subject, configured_by
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           ON CONFLICT (mcp_server_id) DO UPDATE SET
               credential_type         = EXCLUDED.credential_type,
               access_token_encrypted  = EXCLUDED.access_token_encrypted,
               refresh_token_encrypted = EXCLUDED.refresh_token_encrypted,
               expires_at              = EXCLUDED.expires_at,
               scopes                  = EXCLUDED.scopes,
               upstream_subject        = EXCLUDED.upstream_subject,
               configured_by           = EXCLUDED.configured_by,
               updated_at              = now()"#,
    )
    .bind(server_id)
    .bind(credential_type)
    .bind(access_encrypted)
    .bind(refresh_encrypted)
    .bind(expires_at)
    .bind(scopes)
    .bind(upstream_subject)
    .bind(configured_by)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Background tool-catalog refresh after a shared-credential write.
/// Builds the auth header from the server's `auth_header_name` /
/// `auth_value_template` so X-API-Key and other non-Bearer shapes
/// work end-to-end. Failures are logged at warn level — the
/// credential write succeeds either way.
fn spawn_shared_tool_discovery(
    db: sqlx::PgPool,
    http: reqwest::Client,
    server: McpServer,
    bearer_token: String,
) {
    tokio::spawn(async move {
        let header_value = server
            .auth_value_template
            .replace("{{token}}", &bearer_token);
        let auth = (server.auth_header_name.as_str(), header_value.as_str());
        match crate::mcp_runtime::discover_and_persist_tools_with_auth(
            &db,
            &http,
            &server,
            Some(auth),
        )
        .await
        {
            crate::mcp_runtime::SystemDiscoveryOutcome::Tools(n) => {
                tracing::info!(
                    mcp_server = %server.name,
                    tools = n,
                    "Shared-credential MCP tool discovery succeeded"
                );
                let _ = sqlx::query("UPDATE mcp_servers SET last_error = NULL WHERE id = $1")
                    .bind(server.id)
                    .execute(&db)
                    .await;
            }
            crate::mcp_runtime::SystemDiscoveryOutcome::AuthRequired => {
                tracing::warn!(
                    mcp_server = %server.name,
                    "Shared credential rejected by upstream tools/list (401/403)"
                );
                let _ = sqlx::query("UPDATE mcp_servers SET last_error = $1 WHERE id = $2")
                    .bind("Shared credential rejected by upstream — verify token / scopes")
                    .bind(server.id)
                    .execute(&db)
                    .await;
            }
            crate::mcp_runtime::SystemDiscoveryOutcome::Failed(e) => {
                tracing::warn!(
                    mcp_server = %server.name,
                    error = %e,
                    "Shared-credential MCP tool discovery failed"
                );
                let _ = sqlx::query("UPDATE mcp_servers SET last_error = $1 WHERE id = $2")
                    .bind(format!("{e}"))
                    .bind(server.id)
                    .execute(&db)
                    .await;
            }
        }
    });
}

/// PUT /api/admin/mcp/servers/:id/shared-credential/static-token
///
/// Admin pastes a shared PAT / API key. Encrypted at rest exactly
/// like per-user static tokens; replicated through to the tool
/// catalog via [`spawn_shared_tool_discovery`].
#[derive(Debug, Deserialize)]
pub struct SharedStaticTokenRequest {
    pub token: String,
}

pub async fn paste_shared_static_token(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(server_id): Path<Uuid>,
    Json(req): Json<SharedStaticTokenRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("mcp_servers:update")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:update")
        .await?;

    if req.token.is_empty() {
        return Err(AppError::BadRequest("token is required".into()));
    }

    let server = load_server(&state, server_id).await?;
    if server.credential_owner != "admin_shared" {
        return Err(AppError::BadRequest(
            "This server is not configured for admin-shared credentials. \
             Set credential_owner='admin_shared' in the server settings first."
                .into(),
        ));
    }

    let enc_key = parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let access_encrypted = crypto::encrypt(req.token.as_bytes(), &enc_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt token: {e}")))?;

    upsert_shared_credential(
        &state,
        server_id,
        "static_token",
        &access_encrypted,
        None,
        None,
        &[],
        None,
        auth_user.claims.sub,
    )
    .await?;

    // Bearer changed → every cached response was minted under the
    // previous identity.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_server_lane(&server_id)
        .await;

    spawn_shared_tool_discovery(
        state.db.clone(),
        (**state.http_client.load()).clone(),
        server,
        req.token.clone(),
    );

    state.audit.log(
        AuditEntry::new("mcp.shared_credential.token_set")
            .user_id(auth_user.claims.sub)
            .resource("mcp_server")
            .resource_id(server_id.to_string()),
    );

    Ok(Json(serde_json::json!({"status": "ok"})))
}

/// POST /api/admin/mcp/servers/:id/shared-credential/authorize
///
/// Start the OAuth flow that ends with the gateway holding a shared
/// upstream credential for this server. State blob is marked
/// `target=admin_shared` so [`oauth_callback`] writes to
/// `mcp_server_shared_credentials` instead of `mcp_user_credentials`.
pub async fn start_shared_authorize(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<AuthorizeResponse>, AppError> {
    auth_user.require_permission("mcp_servers:update")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:update")
        .await?;

    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp_oauth_shared_authorize",
    )
    .await?;

    let server = load_server(&state, server_id).await?;
    if server.credential_owner != "admin_shared" {
        return Err(AppError::BadRequest(
            "This server is not configured for admin-shared credentials".into(),
        ));
    }
    let auth_endpoint = server
        .oauth_authorization_endpoint
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("OAuth not configured for this server".into()))?;
    let client_id = server
        .oauth_client_id
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("OAuth client_id not configured".into()))?;
    if server.oauth_token_endpoint.is_none() {
        return Err(AppError::BadRequest(
            "OAuth token endpoint not configured".into(),
        ));
    }

    let state_token = random_token()?;
    let code_verifier = random_token()?;
    let code_challenge = pkce_challenge(&code_verifier);
    let redirect_uri = callback_redirect_uri(&state)?;

    let enc_key = parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let binding = state_binding(&enc_key, &state_token, &code_verifier);

    let blob = McpOauthState {
        target: OauthStateTarget::AdminShared {
            server_id,
            configured_by: auth_user.claims.sub,
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

/// GET /api/admin/mcp/servers/:id/shared-credential
///
/// Status snapshot for the admin UI: `configured` / not, expiry,
/// upstream subject, who set it up.
#[derive(Debug, Serialize)]
pub struct SharedCredentialStatus {
    pub configured: bool,
    pub credential_type: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub upstream_subject: Option<String>,
    pub configured_by: Option<Uuid>,
    pub updated_at: Option<DateTime<Utc>>,
}

pub async fn shared_credential_status(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<SharedCredentialStatus>, AppError> {
    auth_user.require_permission("mcp_servers:read")?;

    #[derive(sqlx::FromRow)]
    struct Row {
        credential_type: String,
        expires_at: Option<DateTime<Utc>>,
        upstream_subject: Option<String>,
        configured_by: Option<Uuid>,
        updated_at: DateTime<Utc>,
    }
    let row = sqlx::query_as::<_, Row>(
        r#"SELECT credential_type, expires_at, upstream_subject, configured_by, updated_at
             FROM mcp_server_shared_credentials WHERE mcp_server_id = $1"#,
    )
    .bind(server_id)
    .fetch_optional(&state.db)
    .await?;

    Ok(Json(match row {
        Some(r) => SharedCredentialStatus {
            configured: true,
            credential_type: Some(r.credential_type),
            expires_at: r.expires_at,
            upstream_subject: r.upstream_subject,
            configured_by: r.configured_by,
            updated_at: Some(r.updated_at),
        },
        None => SharedCredentialStatus {
            configured: false,
            credential_type: None,
            expires_at: None,
            upstream_subject: None,
            configured_by: None,
            updated_at: None,
        },
    }))
}

/// DELETE /api/admin/mcp/servers/:id/shared-credential
///
/// Revoke the shared credential. Best-effort upstream revoke when
/// the server has a revocation endpoint and the row was OAuth.
pub async fn revoke_shared_credential(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(server_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("mcp_servers:update")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:update")
        .await?;

    let revoked = best_effort_revoke_shared_upstream(&state, server_id).await?;
    if !revoked {
        return Err(AppError::NotFound(
            "Shared credential not configured".into(),
        ));
    }

    sqlx::query("DELETE FROM mcp_server_shared_credentials WHERE mcp_server_id = $1")
        .bind(server_id)
        .execute(&state.db)
        .await?;

    // The shared bearer is gone — every cached response was minted
    // under it and is now serving against an identity that no longer
    // has access.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_server_lane(&server_id)
        .await;

    state.audit.log(
        AuditEntry::new("mcp.shared_credential.revoked")
            .user_id(auth_user.claims.sub)
            .resource("mcp_server")
            .resource_id(server_id.to_string()),
    );

    Ok(Json(serde_json::json!({"status": "revoked"})))
}

/// Best-effort: read the server's shared credential row and, if it's
/// an OAuth grant with a known revocation endpoint, POST a revocation
/// request to the upstream so the bearer is dropped on their side too.
///
/// Returns `Ok(true)` when a row existed (regardless of whether the
/// upstream call succeeded), `Ok(false)` when no shared credential
/// was configured. Does NOT delete the row — callers do that
/// themselves so they can sequence it with their own transaction
/// (e.g., `update_server` admin_shared → per_user transitions).
///
/// `pub(super)` so `mcp_servers::update_server` can call it.
pub(super) async fn best_effort_revoke_shared_upstream(
    state: &AppState,
    server_id: Uuid,
) -> Result<bool, AppError> {
    let row: Option<(String, Vec<u8>)> = sqlx::query_as(
        r#"SELECT credential_type, access_token_encrypted
             FROM mcp_server_shared_credentials WHERE mcp_server_id = $1"#,
    )
    .bind(server_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((credential_type, access_encrypted)) = row else {
        return Ok(false);
    };

    if credential_type == "oauth_authcode" {
        let server = load_server(state, server_id).await?;
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
                    .await;
            }
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// Wizard endpoints — authorize an admin-shared OAuth credential before
// the server row exists, and read back its status from Redis.
// ---------------------------------------------------------------------------

/// Body for `POST /api/admin/mcp/oauth-wizard-authorize`. Mirrors the
/// fields the wizard's Step 2 has filled in for OAuth — the handler
/// bakes them into the OAuth state blob so the callback can run a
/// token exchange without a server row existing yet.
#[derive(Debug, Deserialize)]
pub struct WizardAuthorizeRequest {
    pub wizard_session_id: String,
    pub oauth_authorization_endpoint: String,
    pub oauth_token_endpoint: String,
    pub oauth_client_id: String,
    /// Plaintext on the wire; encrypted at rest before persisting.
    /// Empty string ⇒ public client (PKCE-only).
    #[serde(default)]
    pub oauth_client_secret: Option<String>,
    #[serde(default)]
    pub oauth_scopes: Vec<String>,
    /// Userinfo endpoint, if known. Used to populate
    /// `upstream_subject` on the resulting credential row when the
    /// wizard finalizes.
    #[serde(default)]
    pub oauth_userinfo_endpoint: Option<String>,
}

pub async fn start_wizard_authorize(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<WizardAuthorizeRequest>,
) -> Result<Json<AuthorizeResponse>, AppError> {
    auth_user.require_permission("mcp_servers:create")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:create")
        .await?;

    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp_oauth_wizard_authorize",
    )
    .await?;

    if req.wizard_session_id.is_empty() || req.wizard_session_id.len() > 64 {
        return Err(AppError::BadRequest(
            "wizard_session_id must be 1–64 characters".into(),
        ));
    }
    // SSRF guard — same check create_server applies. We're about to
    // POST credentials to these URLs; never let an admin smuggle
    // `http://169.254.169.254/...` past us.
    super::mcp_servers::validate_oauth_endpoint_urls(
        Some(req.oauth_authorization_endpoint.as_str()),
        Some(req.oauth_token_endpoint.as_str()),
        None,
        req.oauth_userinfo_endpoint.as_deref(),
    )?;
    if req.oauth_client_id.is_empty() {
        return Err(AppError::BadRequest("oauth_client_id is required".into()));
    }

    let state_token = random_token()?;
    let code_verifier = random_token()?;
    let code_challenge = pkce_challenge(&code_verifier);
    let redirect_uri = callback_redirect_uri(&state)?;

    let enc_key = parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encryption key error: {e}")))?;
    let binding = state_binding(&enc_key, &state_token, &code_verifier);

    // Encrypt the client_secret before stashing in the state blob —
    // the blob lands in Redis and we never want plaintext secrets
    // there even for the 600s state TTL.
    let oauth_client_secret_encrypted = match req.oauth_client_secret.as_deref() {
        Some(s) if !s.is_empty() => Some(
            crypto::encrypt(s.as_bytes(), &enc_key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt client_secret: {e}")))?,
        ),
        _ => None,
    };

    let blob = McpOauthState {
        target: OauthStateTarget::WizardAdminShared {
            wizard_session_id: req.wizard_session_id.clone(),
            configured_by: auth_user.claims.sub,
            oauth_token_endpoint: req.oauth_token_endpoint.clone(),
            oauth_client_id: req.oauth_client_id.clone(),
            oauth_client_secret_encrypted,
            oauth_scopes: req.oauth_scopes.clone(),
            oauth_userinfo_endpoint: req.oauth_userinfo_endpoint.clone(),
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

    let mut url = url::Url::parse(req.oauth_authorization_endpoint.as_str())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid authorization_endpoint: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", req.oauth_client_id.as_str());
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("state", &state_token);
        q.append_pair("code_challenge", &code_challenge);
        q.append_pair("code_challenge_method", "S256");
        if !req.oauth_scopes.is_empty() {
            q.append_pair("scope", &req.oauth_scopes.join(" "));
        }
    }

    Ok(Json(AuthorizeResponse {
        authorize_url: url.to_string(),
    }))
}

/// `GET /api/admin/mcp/wizards/{wizard_session_id}/credential-status`
///
/// Lets the wizard's Step 3 poll whether the OAuth dance came back
/// successfully. Reads the Redis blob written by the callback's
/// `WizardAdminShared` arm. Returns 404 when nothing's there yet
/// (admin hasn't finished the dance) — the frontend treats 404 as
/// "still pending" rather than "error".
#[derive(Debug, Serialize)]
pub struct WizardCredentialStatus {
    pub credential_type: String,
    pub upstream_subject: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
}

pub async fn wizard_credential_status(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(wizard_session_id): Path<String>,
) -> Result<Json<WizardCredentialStatus>, AppError> {
    auth_user.require_permission("mcp_servers:create")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:create")
        .await?;

    let stored: Option<String> = fred::interfaces::KeysInterface::get(
        &state.redis,
        wizard_credential_redis_key(&wizard_session_id),
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;
    let stored =
        stored.ok_or_else(|| AppError::NotFound("Wizard credential not yet ready".into()))?;

    #[derive(serde::Deserialize)]
    struct StoredBlob {
        credential_type: String,
        upstream_subject: Option<String>,
        expires_at: Option<DateTime<Utc>>,
        scopes: Vec<String>,
    }
    let parsed: StoredBlob = serde_json::from_str(&stored)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("corrupt wizard credential blob: {e}")))?;

    Ok(Json(WizardCredentialStatus {
        credential_type: parsed.credential_type,
        upstream_subject: parsed.upstream_subject,
        expires_at: parsed.expires_at,
        scopes: parsed.scopes,
    }))
}

/// Delete the pending wizard credential. Used when the admin abandons
/// the wizard or rolls back from Step 3 — the OAuth state's natural
/// 1-hour TTL would clean it up anyway, but explicit deletion lets
/// the admin re-run the dance without the old blob shadowing the
/// new one.
pub async fn discard_wizard_credential(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(wizard_session_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("mcp_servers:create")?;
    auth_user
        .assert_scope_global(&state.db, "mcp_servers:create")
        .await?;

    let _: Option<String> = fred::interfaces::KeysInterface::getdel(
        &state.redis,
        wizard_credential_redis_key(&wizard_session_id),
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;
    Ok(Json(serde_json::json!({"status": "discarded"})))
}

/// Internal helper — fetch and delete the wizard credential blob in
/// one Redis round-trip, returning the raw fields the create_server
/// handler needs to insert into `mcp_server_shared_credentials`.
/// `pub(super)` so [`mcp_servers::create_server`] can call it.
pub(super) async fn pop_wizard_credential(
    state: &AppState,
    wizard_session_id: &str,
) -> Result<Option<PoppedWizardCredential>, AppError> {
    let stored: Option<String> = fred::interfaces::KeysInterface::getdel(
        &state.redis,
        wizard_credential_redis_key(wizard_session_id),
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;
    let Some(stored) = stored else {
        return Ok(None);
    };

    #[derive(serde::Deserialize)]
    struct StoredBlob {
        credential_type: String,
        access_token_encrypted: String,
        refresh_token_encrypted: Option<String>,
        expires_at: Option<DateTime<Utc>>,
        scopes: Vec<String>,
        upstream_subject: Option<String>,
        configured_by: Uuid,
    }
    let parsed: StoredBlob = serde_json::from_str(&stored)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("corrupt wizard credential blob: {e}")))?;

    let access = BASE64URL_NOPAD
        .decode(parsed.access_token_encrypted.as_bytes())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decode access bytes: {e}")))?;
    let refresh = match parsed.refresh_token_encrypted {
        Some(s) => Some(
            BASE64URL_NOPAD
                .decode(s.as_bytes())
                .map_err(|e| AppError::Internal(anyhow::anyhow!("decode refresh bytes: {e}")))?,
        ),
        None => None,
    };

    Ok(Some(PoppedWizardCredential {
        credential_type: parsed.credential_type,
        access_token_encrypted: access,
        refresh_token_encrypted: refresh,
        expires_at: parsed.expires_at,
        scopes: parsed.scopes,
        upstream_subject: parsed.upstream_subject,
        configured_by: parsed.configured_by,
    }))
}

pub(super) struct PoppedWizardCredential {
    pub credential_type: String,
    pub access_token_encrypted: Vec<u8>,
    pub refresh_token_encrypted: Option<Vec<u8>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
    pub upstream_subject: Option<String>,
    pub configured_by: Uuid,
}

/// Insert a popped wizard credential into `mcp_server_shared_credentials`.
/// Called by [`mcp_servers::create_server`] inside the same TX as the
/// server-row insert so the credential and the row land atomically.
pub(super) async fn insert_shared_credential_from_wizard(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    server_id: Uuid,
    cred: &PoppedWizardCredential,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO mcp_server_shared_credentials (
               mcp_server_id, credential_type,
               access_token_encrypted, refresh_token_encrypted,
               expires_at, scopes, upstream_subject, configured_by
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
    )
    .bind(server_id)
    .bind(&cred.credential_type)
    .bind(&cred.access_token_encrypted)
    .bind(cred.refresh_token_encrypted.as_deref())
    .bind(cred.expires_at)
    .bind(&cred.scopes)
    .bind(cred.upstream_subject.as_deref())
    .bind(cred.configured_by)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Minimal URL-fragment encoder. Only protects the characters that
/// would corrupt the `#k=v` fragment shape (`/`, `#`, `&`, `=`, ` `).
/// Real URL crates assume reserved-character semantics in fragments
/// that don't apply here — we want a literal account_label round-tripped
/// to the SPA, not a parsed URL parameter.
fn urlencode_fragment(s: &str) -> String {
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

    #[test]
    fn pkce_challenge_matches_rfc7636_appendix_b() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_challenge(verifier), expected);
    }

    #[test]
    fn random_token_yields_43_chars() {
        // 32 bytes → 43 base64url-no-pad chars. Stable RFC 7636
        // verifier length so the upstream's sanity checks pass.
        let t = random_token().unwrap();
        assert_eq!(t.len(), 43);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn fragment_encoder_escapes_only_dangerous_chars() {
        assert_eq!(urlencode_fragment("hello"), "hello");
        assert_eq!(urlencode_fragment("a b"), "a%20b");
        assert_eq!(urlencode_fragment("a#b"), "a%23b");
        assert_eq!(urlencode_fragment("a/b"), "a%2Fb");
    }

    #[test]
    fn binding_changes_with_inputs() {
        let key = [0u8; 32];
        let a = state_binding(&key, "state1", "verifier1");
        let b = state_binding(&key, "state1", "verifier2");
        let c = state_binding(&key, "state2", "verifier1");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, state_binding(&key, "state1", "verifier1"));
    }

    // -------- RFC 9728 / 8414 / 7591 probe helpers ----------------------

    #[test]
    fn parse_resource_metadata_hint_extracts_url() {
        let header = r#"Bearer realm="mcp", resource_metadata="https://api.example.com/.well-known/oauth-protected-resource", error="invalid_token""#;
        assert_eq!(
            parse_resource_metadata_hint(header).as_deref(),
            Some("https://api.example.com/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn parse_resource_metadata_hint_returns_none_when_missing() {
        assert_eq!(parse_resource_metadata_hint("Bearer realm=\"mcp\""), None);
        assert_eq!(parse_resource_metadata_hint(""), None);
    }

    #[test]
    fn pick_dcr_auth_method_prefers_none_when_supported() {
        let supported: Vec<String> = ["client_secret_post", "none"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick_dcr_auth_method(&supported), "none");
    }

    #[test]
    fn pick_dcr_auth_method_falls_back_to_post_when_no_explicit_pref() {
        // AS lists only basic — we request `_basic` so DCR proceeds,
        // even though we don't natively send Basic at the token
        // endpoint. Admin can fall back to a hand-edit if needed.
        let basic_only: Vec<String> = vec!["client_secret_basic".to_string()];
        assert_eq!(pick_dcr_auth_method(&basic_only), "client_secret_basic");
        // AS lists nothing — default to `_post`, the most widely
        // accepted method.
        assert_eq!(pick_dcr_auth_method(&[]), "client_secret_post");
        // AS lists post — pick post.
        let post_only: Vec<String> = vec!["client_secret_post".to_string()];
        assert_eq!(pick_dcr_auth_method(&post_only), "client_secret_post");
    }

    #[test]
    fn parse_authz_server_metadata_extracts_registration_endpoint() {
        // Real-world shape — Keycloak / Auth0 / GitHub-style.
        let body = serde_json::json!({
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/oauth/token",
            "registration_endpoint": "https://auth.example.com/oauth/register",
            "scopes_supported": ["read", "write"]
        });
        let parsed = parse_authz_server_metadata(&body);
        assert_eq!(parsed.issuer.as_deref(), Some("https://auth.example.com"));
        assert_eq!(
            parsed.registration_endpoint.as_deref(),
            Some("https://auth.example.com/oauth/register")
        );
        assert_eq!(parsed.scopes_supported, vec!["read", "write"]);
    }

    #[test]
    fn build_authz_metadata_candidates_root_issuer_uses_bare_well_known() {
        // Issuer = origin (no path) — single well-known shape, no
        // ambiguity. Covers GitHub, Google, Auth0 default tenants.
        let urls = build_authz_metadata_candidates("https://github.com");
        assert_eq!(
            urls,
            vec![
                "https://github.com/.well-known/oauth-authorization-server",
                "https://github.com/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn build_authz_metadata_candidates_path_issuer_puts_well_known_before_path() {
        // RFC 8414 §3: for issuer with a path (Feishu = .../mcp), the
        // metadata MUST be at host/.well-known/oauth-authorization-server/path,
        // not host/path/.well-known/.... We try the spec-correct form
        // first and fall through to the legacy "under-path" form for
        // off-spec implementations.
        let urls = build_authz_metadata_candidates("https://accounts.feishu.cn/mcp");
        assert_eq!(urls.len(), 4);
        assert_eq!(
            urls[0],
            "https://accounts.feishu.cn/.well-known/oauth-authorization-server/mcp"
        );
        assert_eq!(
            urls[1],
            "https://accounts.feishu.cn/.well-known/openid-configuration/mcp"
        );
        assert_eq!(
            urls[2],
            "https://accounts.feishu.cn/mcp/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            urls[3],
            "https://accounts.feishu.cn/mcp/.well-known/openid-configuration"
        );
    }

    #[test]
    fn build_authz_metadata_candidates_strips_trailing_slash() {
        // `https://issuer/` is the same as `https://issuer`. No
        // accidental empty-path variant.
        let urls = build_authz_metadata_candidates("https://issuer.test/");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].ends_with("/.well-known/oauth-authorization-server"));
    }

    #[test]
    fn parse_origin_and_path_separates_origin_from_resource_path() {
        let p = parse_origin_and_path("https://api.example.com:8443/foo/bar/").unwrap();
        assert_eq!(p.origin, "https://api.example.com:8443");
        assert_eq!(p.path, "/foo/bar");
    }

    #[test]
    fn parse_origin_and_path_treats_root_as_empty_path() {
        let p = parse_origin_and_path("https://api.example.com/").unwrap();
        assert_eq!(p.path, "");
        let p = parse_origin_and_path("https://api.example.com").unwrap();
        assert_eq!(p.path, "");
    }

    #[test]
    fn parse_authz_server_metadata_handles_missing_optional_fields() {
        // Bare-minimum AS that doesn't advertise dynamic registration —
        // the probe should still extract what's there and fall through
        // to "fill client_id manually".
        let body = serde_json::json!({
            "issuer": "https://issuer.test",
            "authorization_endpoint": "https://issuer.test/authorize",
            "token_endpoint": "https://issuer.test/token",
        });
        let parsed = parse_authz_server_metadata(&body);
        assert!(parsed.registration_endpoint.is_none());
        assert!(parsed.revocation_endpoint.is_none());
        assert!(parsed.userinfo_endpoint.is_none());
        assert!(parsed.scopes_supported.is_empty());
    }

    // -------- subject_from_jwt -------------------------------------------

    fn jwt_with_payload(payload: &serde_json::Value) -> String {
        let header = data_encoding::BASE64URL_NOPAD.encode(b"{\"alg\":\"none\"}");
        let body =
            data_encoding::BASE64URL_NOPAD.encode(serde_json::to_vec(payload).unwrap().as_slice());
        format!("{header}.{body}.signature")
    }

    #[test]
    fn jwt_subject_prefers_preferred_username_over_sub() {
        // Auth0 / Keycloak conventionally include both. The
        // human-readable `preferred_username` makes a better label
        // than the opaque `sub` UUID.
        let token = jwt_with_payload(&serde_json::json!({
            "sub": "auth0|abc123",
            "preferred_username": "octocat",
            "email": "octocat@example.com",
        }));
        assert_eq!(subject_from_jwt(&token).as_deref(), Some("octocat"));
    }

    #[test]
    fn jwt_subject_falls_back_through_priority_chain() {
        let only_sub = jwt_with_payload(&serde_json::json!({"sub": "google|987"}));
        assert_eq!(subject_from_jwt(&only_sub).as_deref(), Some("google|987"));

        let only_email = jwt_with_payload(&serde_json::json!({"email": "u@x.com"}));
        assert_eq!(subject_from_jwt(&only_email).as_deref(), Some("u@x.com"));
    }

    #[test]
    fn jwt_subject_returns_none_for_opaque_token() {
        // GitHub PATs and Notion tokens are opaque random strings —
        // they're not 3 dot-separated segments, so the JWT path bails
        // and the userinfo fallback takes over.
        assert_eq!(subject_from_jwt("ghp_aaaaaaaaaaaaaaaa"), None);
        assert_eq!(subject_from_jwt(""), None);
        assert_eq!(subject_from_jwt("not.a.valid.jwt.with.5.parts"), None);
    }

    #[test]
    fn jwt_subject_returns_none_when_payload_isnt_object() {
        // Bytes that decode to something that isn't a JSON object
        // (corrupt, encrypted JWE, etc.) — refuse to guess.
        let header = data_encoding::BASE64URL_NOPAD.encode(b"{}");
        let body = data_encoding::BASE64URL_NOPAD.encode(b"\"not an object\"");
        let token = format!("{header}.{body}.signature");
        assert_eq!(subject_from_jwt(&token), None);
    }

    // -------- extract_subject_from_json ----------------------------------

    #[test]
    fn extract_subject_handles_github_user_shape() {
        // GET https://api.github.com/user
        let body = serde_json::json!({
            "login": "octocat",
            "id": 583231,
            "name": "The Octocat",
            "email": null,
        });
        assert_eq!(extract_subject_from_json(&body).as_deref(), Some("octocat"));
    }

    #[test]
    fn extract_subject_handles_atlassian_me_shape() {
        // GET https://api.atlassian.com/me — accountId beats name.
        let body = serde_json::json!({
            "accountId": "5b10ac8d82e05b22cc7d4ef5",
            "email": "alice@acme.com",
            "name": "Alice",
        });
        assert_eq!(
            extract_subject_from_json(&body).as_deref(),
            Some("5b10ac8d82e05b22cc7d4ef5")
        );
    }

    #[test]
    fn extract_subject_descends_into_user_wrapper() {
        // Slack users.identity wraps the user object. We prefer
        // `name` (the human display name) over `id` (the opaque
        // `U0G9QF9C6`) — for THIS user looking at THEIR own
        // connections, the display name is more readable and
        // sufficient to disambiguate accounts.
        let body = serde_json::json!({
            "ok": true,
            "user": {
                "id": "U0G9QF9C6",
                "name": "Bob",
            },
            "team": {"id": "T0G9PQBBK"},
        });
        assert_eq!(extract_subject_from_json(&body).as_deref(), Some("Bob"));
    }

    #[test]
    fn extract_subject_descends_into_user_wrapper_when_only_id() {
        // If the upstream's user wrapper omits `name` we still pull
        // out the opaque `id` — anything beats falling back to the
        // user-supplied account_label.
        let body = serde_json::json!({
            "ok": true,
            "user": {"id": "U0G9QF9C6"},
        });
        assert_eq!(
            extract_subject_from_json(&body).as_deref(),
            Some("U0G9QF9C6")
        );
    }

    #[test]
    fn extract_subject_stringifies_numeric_id() {
        // Notion / JSON:API style: id can be a number or a string.
        let body = serde_json::json!({"id": 42});
        assert_eq!(extract_subject_from_json(&body).as_deref(), Some("42"));
    }

    #[test]
    fn extract_subject_skips_empty_and_null() {
        let body = serde_json::json!({
            "sub": "",
            "preferred_username": null,
            "id": "the-id",
        });
        assert_eq!(extract_subject_from_json(&body).as_deref(), Some("the-id"));
    }

    #[test]
    fn extract_subject_returns_none_for_bare_array() {
        // Defensive: an unwrapped array at the top isn't the response
        // shape we expect, refuse to guess at indices.
        let body = serde_json::json!([{"id": "x"}]);
        assert_eq!(extract_subject_from_json(&body), None);
    }

    #[test]
    fn extract_subject_unwraps_results_array() {
        // JSON:API convention: `{ "results": [{ ... }] }`. Take the
        // first element.
        let body = serde_json::json!({
            "results": [{"id": "first-user"}, {"id": "second-user"}],
        });
        assert_eq!(
            extract_subject_from_json(&body).as_deref(),
            Some("first-user")
        );
    }
}
