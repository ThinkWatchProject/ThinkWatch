//! Admin-shared MCP credentials. Distinct from the per-user OAuth
//! flow (which lives in the parent module) — admin-shared
//! credentials are owned by the admin who provisioned them, not by
//! the calling user, and they drive `mcp_server_shared_credentials`
//! instead of `mcp_user_credentials`.
//!
//! Three entry points:
//! - `paste_shared_static_token` — admin pastes a PAT / static key.
//! - `start_shared_authorize` — admin starts a fresh OAuth authorize
//!   for an existing server (e.g. rotating the credential).
//! - `revoke_shared_credential` — admin tears it down.
//!
//! Plus a `pub` helper (`best_effort_revoke_shared_upstream`) that
//! the `mcp_servers` delete path calls — leaving an upstream OAuth
//! grant orphaned when an admin deletes the server is rude, so we
//! try to call the upstream's revocation endpoint with the soon-
//! to-be-dead refresh token.

use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_auth::oauth::pkce::{pkce_challenge, random_token, state_binding};
use think_watch_common::crypto::{self, parse_encryption_key};
use think_watch_common::errors::AppError;
use think_watch_common::models::McpServer;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::{
    AuthorizeResponse, McpOauthState, OAUTH_STATE_PREFIX, OAUTH_STATE_TTL_SECS, OauthStateTarget,
    callback_redirect_uri, load_server,
};

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
pub(super) async fn upsert_shared_credential(
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
pub(super) fn spawn_shared_tool_discovery(
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
    auth_user
        .require_global_permission(&state.db, "mcp_servers:update")
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
        auth_user
            .audit("mcp.shared_credential.token_set")
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
    auth_user
        .require_global_permission(&state.db, "mcp_servers:update")
        .await?;

    crate::handlers::test_rate_limit::check_test_rate_limit(
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

    let state_token = random_token();
    let code_verifier = random_token();
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
    // Match the rest of the shared-credential surface — every
    // authorize/paste/delete endpoint goes through the global gate.
    // Without it a team-scoped reader could see configured /
    // upstream_subject / configured_by metadata for shared
    // credentials they have no business knowing about.
    auth_user
        .require_global_permission(&state.db, "mcp_servers:read")
        .await?;

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
    auth_user
        .require_global_permission(&state.db, "mcp_servers:update")
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
        auth_user
            .audit("mcp.shared_credential.revoked")
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
pub async fn best_effort_revoke_shared_upstream(
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
