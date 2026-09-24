//! Wizard endpoints — authorize an admin-shared OAuth credential
//! BEFORE the server row exists, and read back its status from
//! Redis. Lifted out of `mcp_oauth.rs` so the new-server wizard's
//! state machine (Step 2 = OAuth dance, Step 3 = save → server
//! row insert) lives in its own file.
//!
//! Key difference from `shared.rs`: there's no `mcp_servers` row
//! to look up. The wizard bakes the OAuth client config into the
//! state blob and the resulting tokens land in Redis under
//! `mcp_wizard:cred:{wizard_session_id}` instead of going straight
//! to `mcp_server_shared_credentials`. The wizard's `Save` step
//! calls `claim_wizard_credential` + `insert_shared_credential_from_wizard`
//! to atomically promote the pending blob into the real
//! shared-credential table at server-create time.

use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use data_encoding::BASE64URL_NOPAD;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_auth::oauth::pkce::{pkce_challenge, random_token, state_binding};
use think_watch_common::errors::AppError;
use tw_crypto::crypto::{self, parse_encryption_key};

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::{
    AuthorizeResponse, McpOauthState, OAUTH_STATE_PREFIX, OAUTH_STATE_TTL_SECS, OauthStateTarget,
    callback_redirect_uri, wizard_credential_redis_key,
};

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
    auth_user
        .require_global_permission(&state.db, "mcp_servers:create")
        .await?;

    crate::handlers::test_rate_limit::check_test_rate_limit(
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
    crate::handlers::mcp_servers::validate_oauth_endpoint_urls(
        &state.url_validator,
        Some(req.oauth_authorization_endpoint.as_str()),
        Some(req.oauth_token_endpoint.as_str()),
        None,
        req.oauth_userinfo_endpoint.as_deref(),
    )?;
    if req.oauth_client_id.is_empty() {
        return Err(AppError::BadRequest("oauth_client_id is required".into()));
    }

    let state_token = random_token();
    let code_verifier = random_token();
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
    auth_user
        .require_global_permission(&state.db, "mcp_servers:create")
        .await?;

    let stored: Option<String> = fred::interfaces::KeysInterface::get(
        &state.redis,
        wizard_credential_redis_key(auth_user.claims.sub, &wizard_session_id),
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
    auth_user
        .require_global_permission(&state.db, "mcp_servers:create")
        .await?;

    let _: Option<String> = fred::interfaces::KeysInterface::getdel(
        &state.redis,
        wizard_credential_redis_key(auth_user.claims.sub, &wizard_session_id),
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Redis error: {e}")))?;
    Ok(Json(serde_json::json!({"status": "discarded"})))
}

/// Internal helper — atomically claim (read + delete) the wizard
/// credential blob from Redis. Called by
/// [`mcp_servers::create_server`] **inside** its DB transaction,
/// after taking a `pg_advisory_xact_lock` on the
/// `wizard_session_id`. GETDEL ensures the blob is consumed
/// exactly once even under concurrent POSTs with the same session
/// (e.g. admin opens two tabs and double-submits — a real failure
/// mode of the previous peek-then-delete-after-commit pattern).
///
/// Tradeoff: a TX rollback after the claim loses the blob and the
/// admin must re-run the OAuth dance. Pre-TX validation in
/// `create_server` (cross-axis cred-owner checks, payload shape,
/// SSRF guards) makes that path narrow enough to accept.
///
/// `pub(super)` so [`mcp_servers::create_server`] can call it.
pub async fn claim_wizard_credential(
    state: &AppState,
    configured_by: Uuid,
    wizard_session_id: &str,
) -> Result<Option<PoppedWizardCredential>, AppError> {
    let stored: Option<String> = fred::interfaces::KeysInterface::getdel(
        &state.redis,
        wizard_credential_redis_key(configured_by, wizard_session_id),
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

pub struct PoppedWizardCredential {
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
pub async fn insert_shared_credential_from_wizard(
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
