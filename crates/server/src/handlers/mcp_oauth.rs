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

#[derive(Debug, Serialize, Deserialize)]
struct McpOauthState {
    user_id: Uuid,
    server_id: Uuid,
    account_label: String,
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
    pub namespace_prefix: String,
    /// Whether this server has an OAuth client registered (admin
    /// has filled `oauth_*`). Drives the "Connect via OAuth" button.
    pub oauth_capable: bool,
    /// Whether users are allowed to paste a static token. Drives
    /// the alternate "Paste token" UI.
    pub allow_static_token: bool,
    pub static_token_help_url: Option<String>,
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
        let oauth_capable = s.oauth_token_endpoint.is_some()
            && s.oauth_authorization_endpoint.is_some()
            && s.oauth_client_id.is_some();
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
            namespace_prefix: s.namespace_prefix,
            oauth_capable,
            allow_static_token: s.allow_static_token,
            static_token_help_url: s.static_token_help_url,
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

    if req.account_label.trim().is_empty() || req.account_label.len() > 64 {
        return Err(AppError::BadRequest(
            "account_label must be 1–64 characters".into(),
        ));
    }

    let server = load_server(&state, server_id).await?;
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
        user_id: auth_user.claims.sub,
        server_id,
        account_label: req.account_label.trim().to_string(),
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

    let server = load_server(&state, blob.server_id).await?;
    let token_endpoint = server
        .oauth_token_endpoint
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("OAuth token endpoint not configured".into()))?;
    let client_id = server
        .oauth_client_id
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("OAuth client_id not configured".into()))?;
    let client_secret_encrypted = server
        .oauth_client_secret_encrypted
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("OAuth client_secret not configured".into()))?;

    let client_secret_bytes = crypto::decrypt(client_secret_encrypted, &enc_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt client_secret: {e}")))?;
    let client_secret = String::from_utf8(client_secret_bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("client_secret not utf8: {e}")))?;

    // POST to token endpoint with PKCE verifier.
    let form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", blob.redirect_uri.as_str()),
        ("client_id", client_id),
        ("client_secret", client_secret.as_str()),
        ("code_verifier", blob.code_verifier.as_str()),
    ];
    let body = serde_urlencoded::to_string(&form)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encode token form: {e}")))?;

    let http = state.http_client.load();
    let resp = http
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Token endpoint unreachable: {e}")))?;

    let status = resp.status();
    let resp_text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AppError::BadRequest(format!(
            "Upstream token endpoint returned {status}: {resp_text}"
        )));
    }
    let token: TokenEndpointResponse = serde_json::from_str(&resp_text)
        .map_err(|e| AppError::BadRequest(format!("Token response not JSON: {e}: {resp_text}")))?;

    // Encrypt + persist. Convert RFC 6749's `expires_in` (seconds from
    // now) into an absolute timestamp the resolver can compare against
    // a clock reading without re-doing arithmetic on every request.
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
        .map(|s| {
            s.split_whitespace()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| server.oauth_scopes.clone());

    // First credential for (server, user) wins is_default; subsequent
    // ones land non-default so the user keeps their existing routing.
    upsert_credential(
        &state,
        blob.server_id,
        blob.user_id,
        &blob.account_label,
        "oauth_authcode",
        &access_encrypted,
        refresh_encrypted.as_deref(),
        expires_at,
        &scopes,
        // upstream_subject not parsed in v1 — would require a second
        // userinfo round-trip. Surfaced as `null` until then; the UI
        // falls back to `account_label` in that case.
        None,
    )
    .await?;

    state.audit.log(
        AuditEntry::new("mcp.connection.authorized")
            .user_id(blob.user_id)
            .resource("mcp_server")
            .resource_id(blob.server_id.to_string())
            .detail(serde_json::json!({
                "account_label": blob.account_label,
                "scopes": scopes,
            })),
    );

    let base = callback_base_url(&state)?;
    let url = format!(
        "{}/connections#connected={}/{}",
        base,
        blob.server_id,
        urlencode_fragment(&blob.account_label),
    );
    Ok(Redirect::temporary(&url).into_response())
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

    if account_label.trim().is_empty() || account_label.len() > 64 {
        return Err(AppError::BadRequest(
            "account_label must be 1–64 characters".into(),
        ));
    }
    if req.token.is_empty() {
        return Err(AppError::BadRequest("token is required".into()));
    }

    let server = load_server(&state, server_id).await?;
    if !server.allow_static_token {
        return Err(AppError::BadRequest(
            "This server doesn't accept user-provided static tokens".into(),
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
    think_watch_common::validation::validate_url(&req.issuer)?;

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
    fn s(v: &serde_json::Value, k: &str) -> Option<String> {
        v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string())
    }
    let scopes_supported = body
        .get("scopes_supported")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    DiscoverResponse {
        authorization_endpoint: s(body, "authorization_endpoint"),
        token_endpoint: s(body, "token_endpoint"),
        revocation_endpoint: s(body, "revocation_endpoint"),
        scopes_supported,
    }
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
}
