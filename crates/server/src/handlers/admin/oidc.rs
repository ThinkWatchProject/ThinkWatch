//! OIDC / SSO admin handlers — the wizard-driven draft → test →
//! activate flow. Lifted out of `admin.rs` so the OIDC state
//! machine lives next to its types and helpers.
//!
//! The wizard maintains two parallel configurations:
//!
//! * `active` — what live SSO logins use. Stored as individual
//!   `oidc.*` keys (legacy shape). Mutated only by an explicit
//!   `activate` call.
//! * `draft` — work in progress. Stored as a single JSONB blob at
//!   `oidc.draft`. Free to be written / discarded without affecting
//!   active SSO.
//!
//! Activation is gated by a *recent* successful test login: the popup
//! callback flow (`sso::sso_callback` in test mode) writes its result
//! to Redis at `oidc:test:result` with a 30-min TTL, and `activate`
//! refuses to promote the draft unless that key is present and
//! `passed: true`.
//!
//! Any draft mutation invalidates the test result — the admin must
//! re-test before activating again. This keeps "I changed something
//! after testing" from sneaking through the gate.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

const OIDC_TEST_RESULT_KEY: &str = "oidc:test:result";

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct OidcTestResult {
    pub passed: bool,
    /// Unix epoch seconds when the test completed.
    pub at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Subject + email pulled from the test login's ID token. Lets the
    /// admin confirm the right account came back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claims_preview: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OidcActiveSnapshot {
    pub enabled: bool,
    pub configured: bool,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub redirect_url: Option<String>,
    pub has_secret: bool,
    pub email_claim: String,
    pub name_claim: String,
    pub provider_preset: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OidcDraftSnapshot {
    pub provider_preset: Option<String>,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    pub redirect_url: Option<String>,
    pub has_secret: bool,
    pub email_claim: Option<String>,
    pub name_claim: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OidcSettingsResponse {
    pub active: OidcActiveSnapshot,
    pub draft: Option<OidcDraftSnapshot>,
    pub test_result: Option<OidcTestResult>,
    /// Suggested redirect URL when the wizard is filling in a fresh
    /// draft — derived from `general.public_*` if available, otherwise
    /// from the server's bind address.
    pub default_redirect_url: String,
}

/// GET /api/admin/settings/oidc — return everything the wizard needs:
/// the active config snapshot, the in-progress draft (if any), and
/// the most recent test-login result.
#[utoipa::path(
    get,
    path = "/api/admin/settings/oidc",
    tag = "Settings",
    responses(
        (status = 200, description = "OIDC/SSO configuration + draft + test status", body = OidcSettingsResponse),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn get_oidc_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<OidcSettingsResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "settings:read")
        .await?;
    let dc = &state.dynamic_config;

    let active = OidcActiveSnapshot {
        enabled: dc.oidc_enabled().await,
        configured: state.oidc.read().await.is_some(),
        issuer_url: dc.oidc_issuer_url().await,
        client_id: dc.oidc_client_id().await,
        redirect_url: dc.oidc_redirect_url().await,
        has_secret: dc
            .oidc_client_secret_encrypted()
            .await
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        email_claim: dc.oidc_email_claim().await,
        name_claim: dc.oidc_name_claim().await,
        provider_preset: dc.oidc_provider_preset().await,
    };

    let draft = dc.oidc_draft().await.map(|v| {
        let d = crate::oidc_helpers::OidcDraft::from_value(&v);
        OidcDraftSnapshot {
            provider_preset: d.provider_preset,
            issuer_url: d.issuer_url,
            client_id: d.client_id,
            redirect_url: d.redirect_url,
            has_secret: d
                .client_secret_encrypted
                .as_ref()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            email_claim: d.email_claim,
            name_claim: d.name_claim,
        }
    });

    let test_result = read_test_result(&state.redis).await;

    let default_redirect_url = compute_public_redirect_url(dc, &state.config).await;

    Ok(Json(OidcSettingsResponse {
        active,
        draft,
        test_result,
        default_redirect_url,
    }))
}

/// Compute the redirect URL the wizard suggests by default. Prefers
/// `general.public_*` settings (production-correct), falls back to
/// the server bind address (dev).
async fn compute_public_redirect_url(
    dc: &think_watch_common::dynamic_config::DynamicConfig,
    app: &think_watch_common::config::AppConfig,
) -> String {
    let proto = dc
        .get_string("general.public_protocol")
        .await
        .filter(|s| !s.is_empty());
    let host = dc
        .get_string("general.public_host")
        .await
        .filter(|s| !s.is_empty());
    let port = dc.get_i64("general.public_port").await.unwrap_or(0);
    if let (Some(proto), Some(host)) = (proto, host) {
        let port_part = if port > 0
            && !((proto == "http" && port == 80) || (proto == "https" && port == 443))
        {
            format!(":{port}")
        } else {
            String::new()
        };
        format!("{proto}://{host}{port_part}/api/auth/sso/callback")
    } else {
        crate::oidc_helpers::default_redirect_url(app)
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateOidcDraftRequest {
    pub provider_preset: Option<String>,
    pub issuer_url: Option<String>,
    pub client_id: Option<String>,
    /// Plaintext client secret — encrypted before storage. Send empty
    /// string to keep the existing draft secret unchanged. Send `null`
    /// (omit) to leave it alone.
    pub client_secret: Option<String>,
    pub redirect_url: Option<String>,
    pub email_claim: Option<String>,
    pub name_claim: Option<String>,
}

/// PATCH /api/admin/settings/oidc/draft — upsert the wizard's draft
/// config. Any field-level change invalidates the most recent test
/// result, forcing the admin to re-test before activating.
#[utoipa::path(
    patch,
    path = "/api/admin/settings/oidc/draft",
    tag = "Settings",
    request_body = UpdateOidcDraftRequest,
    responses(
        (status = 200, description = "Draft updated", body = OidcDraftSnapshot),
        (status = 400, description = "Validation failed"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn update_oidc_draft(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateOidcDraftRequest>,
) -> Result<Json<OidcDraftSnapshot>, AppError> {
    auth_user
        .require_global_permission(&state.db, "system:configure_oidc")
        .await?;

    if let Some(ref issuer) = req.issuer_url
        && !issuer.is_empty()
    {
        (state.url_validator)(issuer)?;
    }

    let dc = &state.dynamic_config;
    let mut draft = dc
        .oidc_draft()
        .await
        .map(|v| crate::oidc_helpers::OidcDraft::from_value(&v))
        .unwrap_or_default();

    if let Some(v) = req.provider_preset {
        draft.provider_preset = Some(v);
    }
    if let Some(v) = req.issuer_url {
        draft.issuer_url = Some(v);
    }
    if let Some(v) = req.client_id {
        draft.client_id = Some(v);
    }
    if let Some(ref v) = req.client_secret
        && !v.is_empty()
    {
        let encrypted = crate::oidc_helpers::encrypt_client_secret(v, &state.config)
            .map_err(AppError::Internal)?;
        draft.client_secret_encrypted = Some(encrypted);
    }
    if let Some(v) = req.redirect_url {
        draft.redirect_url = Some(v);
    }
    if let Some(v) = req.email_claim {
        draft.email_claim = Some(v);
    }
    if let Some(v) = req.name_claim {
        draft.name_claim = Some(v);
    }

    let value = serde_json::to_value(&draft)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialise draft: {e}")))?;
    dc.upsert(
        "oidc.draft",
        &value,
        "oidc",
        Some("OIDC wizard draft"),
        Some(auth_user.claims.sub),
    )
    .await
    .map_err(AppError::Internal)?;

    // Any draft mutation invalidates the previous test result — the
    // admin's verified credentials no longer match what's on disk.
    let _: Result<(), _> =
        fred::interfaces::KeysInterface::del::<(), _>(&state.redis, OIDC_TEST_RESULT_KEY).await;
    think_watch_common::dynamic_config::notify_config_changed(&state.redis).await;

    Ok(Json(OidcDraftSnapshot {
        provider_preset: draft.provider_preset,
        issuer_url: draft.issuer_url,
        client_id: draft.client_id,
        redirect_url: draft.redirect_url,
        has_secret: draft
            .client_secret_encrypted
            .as_ref()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        email_claim: draft.email_claim,
        name_claim: draft.name_claim,
    }))
}

/// DELETE /api/admin/settings/oidc/draft — discard the in-progress
/// draft and any pending test result. Active SSO is unaffected.
#[utoipa::path(
    delete,
    path = "/api/admin/settings/oidc/draft",
    tag = "Settings",
    responses(
        (status = 204, description = "Draft discarded"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn delete_oidc_draft(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<axum::http::StatusCode, AppError> {
    auth_user
        .require_global_permission(&state.db, "system:configure_oidc")
        .await?;
    sqlx::query("DELETE FROM system_settings WHERE key = 'oidc.draft'")
        .execute(&state.db)
        .await?;
    state
        .dynamic_config
        .reload()
        .await
        .map_err(AppError::Internal)?;
    let _: Result<(), _> =
        fred::interfaces::KeysInterface::del::<(), _>(&state.redis, OIDC_TEST_RESULT_KEY).await;
    think_watch_common::dynamic_config::notify_config_changed(&state.redis).await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// POST /api/admin/settings/oidc/discover — run OIDC discovery
/// against the draft's issuer URL and return the metadata snapshot.
/// Pure verification; doesn't mutate config.
#[utoipa::path(
    post,
    path = "/api/admin/settings/oidc/discover",
    tag = "Settings",
    responses(
        (status = 200, description = "Discovery succeeded — returns endpoint metadata"),
        (status = 400, description = "Discovery failed or draft incomplete"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn discover_oidc_draft(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user
        .require_global_permission(&state.db, "system:configure_oidc")
        .await?;

    let draft_value = state
        .dynamic_config
        .oidc_draft()
        .await
        .ok_or(AppError::BadRequest("No OIDC draft to verify".into()))?;
    let draft = crate::oidc_helpers::OidcDraft::from_value(&draft_value);

    let issuer = draft
        .issuer_url
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or(AppError::BadRequest("Draft is missing issuer_url".into()))?;
    (state.url_validator)(&issuer)?;

    // Discovery only needs the issuer; pass placeholders for the rest
    // so the validate() check passes. We discard the manager.
    let cfg = think_watch_auth::oidc::OidcConfig {
        issuer_url: issuer,
        client_id: "discover".into(),
        client_secret: "discover".into(),
        redirect_url: draft
            .redirect_url
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| crate::oidc_helpers::default_redirect_url(&state.config)),
        email_claim: "email".into(),
        name_claim: "name".into(),
    };
    let mgr = think_watch_auth::oidc::OidcManager::discover(&cfg)
        .await
        .map_err(|e| AppError::BadRequest(format!("OIDC discovery failed: {e}")))?;
    let summary = mgr.metadata_summary();
    let value = serde_json::to_value(&summary)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialise discovery metadata: {e}")))?;
    Ok(Json(value))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StartOidcTestLoginResponse {
    /// Provider authorization URL — frontend opens this in a popup.
    pub authorize_url: String,
    /// CSRF state token. The frontend stamps it on the popup so it
    /// can later correlate the BroadcastChannel result, and the
    /// callback uses it as the Redis lookup key.
    pub state: String,
}

/// POST /api/admin/settings/oidc/test-login — initiate a draft-config
/// authorization flow. Stores `{mode: Test, ...}` at `oidc:state:{csrf}`
/// in Redis; `sso_callback` later picks up the test mode and writes
/// the result to `oidc:test:result` instead of issuing a session.
#[utoipa::path(
    post,
    path = "/api/admin/settings/oidc/test-login",
    tag = "Settings",
    responses(
        (status = 200, description = "Authorize URL generated", body = StartOidcTestLoginResponse),
        (status = 400, description = "Draft is incomplete or invalid"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn start_oidc_test_login(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<StartOidcTestLoginResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "system:configure_oidc")
        .await?;

    let draft_value = state
        .dynamic_config
        .oidc_draft()
        .await
        .ok_or(AppError::BadRequest("No OIDC draft to test".into()))?;
    let draft = crate::oidc_helpers::OidcDraft::from_value(&draft_value);
    let cfg = draft
        .to_oidc_config(&state.config)
        .map_err(AppError::Internal)?
        .ok_or(AppError::BadRequest(
            "Draft missing issuer_url, client_id, or client_secret".into(),
        ))?;
    cfg.validate().map_err(|e| AppError::BadRequest(e.into()))?;

    let mgr = think_watch_auth::oidc::OidcManager::discover(&cfg)
        .await
        .map_err(|e| AppError::BadRequest(format!("OIDC discovery failed: {e}")))?;
    let (auth_url, csrf, nonce) = mgr.authorize_url();

    // Snapshot the draft into the test session so a subsequent draft
    // edit (which would invalidate the stored test result) can't leak
    // into the in-flight callback.
    let snapshot = crate::handlers::sso::TestConfigSnapshot {
        issuer_url: cfg.issuer_url.clone(),
        client_id: cfg.client_id.clone(),
        client_secret_encrypted: draft.client_secret_encrypted.clone().unwrap_or_default(),
        redirect_url: cfg.redirect_url.clone(),
        email_claim: cfg.email_claim.clone(),
        name_claim: cfg.name_claim.clone(),
    };
    crate::handlers::sso::store_oidc_session_with_snapshot(
        &state.redis,
        &state.config,
        csrf.secret(),
        nonce.secret(),
        crate::handlers::sso::SessionMode::Test,
        Some(snapshot),
        // Test mode runs from an admin path (this handler) and the
        // callback is opened in the admin's popup — login-CSRF doesn't
        // apply because there's no end-user session to steal. Leave
        // browser_binding None; sso_callback skips the check for
        // SessionMode::Test.
        None,
    )
    .await?;

    Ok(Json(StartOidcTestLoginResponse {
        authorize_url: auth_url,
        state: csrf.secret().clone(),
    }))
}

/// POST /api/admin/settings/oidc/activate — promote draft → active.
/// Refuses unless a recent successful test-login result is present.
#[utoipa::path(
    post,
    path = "/api/admin/settings/oidc/activate",
    tag = "Settings",
    responses(
        (status = 200, description = "Activation succeeded"),
        (status = 400, description = "No passing test result, or draft incomplete"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn activate_oidc_draft(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user
        .require_global_permission(&state.db, "system:configure_oidc")
        .await?;

    let test = read_test_result(&state.redis).await;
    let test_passed = matches!(&test, Some(t) if t.passed);
    if !test_passed {
        return Err(AppError::BadRequest(
            "A successful test login is required before activation. Run \"Test login\" first."
                .into(),
        ));
    }

    let draft_value = state
        .dynamic_config
        .oidc_draft()
        .await
        .ok_or(AppError::BadRequest("No OIDC draft to activate".into()))?;
    let draft = crate::oidc_helpers::OidcDraft::from_value(&draft_value);
    let cfg = draft
        .to_oidc_config(&state.config)
        .map_err(AppError::Internal)?
        .ok_or(AppError::BadRequest(
            "Draft is incomplete (issuer / client_id / client_secret required)".into(),
        ))?;

    // Re-discover so the cached manager reflects the activated config.
    // If discovery now fails we refuse activation outright — better to
    // surface the breakage than promote a config we can't actually use.
    let mgr = think_watch_auth::oidc::OidcManager::discover(&cfg)
        .await
        .map_err(|e| {
            AppError::BadRequest(format!(
                "OIDC discovery failed during activation: {e}. Re-test the draft."
            ))
        })?;

    let dc = &state.dynamic_config;
    let user_id = Some(auth_user.claims.sub);
    let upserts: &[(&str, serde_json::Value, &str)] = &[
        ("oidc.enabled", serde_json::json!(true), "SSO enabled"),
        (
            "oidc.issuer_url",
            serde_json::json!(cfg.issuer_url.clone()),
            "OIDC issuer URL",
        ),
        (
            "oidc.client_id",
            serde_json::json!(cfg.client_id.clone()),
            "OIDC client ID",
        ),
        (
            "oidc.client_secret_encrypted",
            serde_json::json!(draft.client_secret_encrypted.clone().unwrap_or_default()),
            "OIDC client secret (encrypted)",
        ),
        (
            "oidc.redirect_url",
            serde_json::json!(cfg.redirect_url.clone()),
            "OIDC redirect URL",
        ),
        (
            "oidc.email_claim",
            serde_json::json!(cfg.email_claim.clone()),
            "JWT claim that carries email",
        ),
        (
            "oidc.name_claim",
            serde_json::json!(cfg.name_claim.clone()),
            "JWT claim that carries display name",
        ),
        (
            "oidc.provider_preset",
            serde_json::json!(draft.provider_preset.clone().unwrap_or_default()),
            "Provider catalog entry used",
        ),
    ];
    for (key, value, desc) in upserts {
        dc.upsert(key, value, "oidc", Some(*desc), user_id)
            .await
            .map_err(AppError::Internal)?;
    }

    sqlx::query("DELETE FROM system_settings WHERE key = 'oidc.draft'")
        .execute(&state.db)
        .await?;
    dc.reload().await.map_err(AppError::Internal)?;
    let _: Result<(), _> =
        fred::interfaces::KeysInterface::del::<(), _>(&state.redis, OIDC_TEST_RESULT_KEY).await;

    *state.oidc.write().await = Some(mgr);

    think_watch_common::dynamic_config::notify_config_changed(&state.redis).await;
    state
        .audit
        .log(auth_user.audit("settings.oidc_activated").resource("oidc"));

    Ok(Json(serde_json::json!({"status": "activated"})))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct DisableOidcRequest {
    pub enabled: bool,
}

/// PATCH /api/admin/settings/oidc — flip the active SSO on/off
/// switch. The wizard exposes this only when an active configuration
/// already exists (`active.configured == true`); flipping enabled
/// requires a previously activated draft.
#[utoipa::path(
    patch,
    path = "/api/admin/settings/oidc",
    tag = "Settings",
    request_body = DisableOidcRequest,
    responses(
        (status = 200, description = "Active SSO toggled"),
        (status = 400, description = "No active configuration to toggle"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn toggle_oidc_active(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<DisableOidcRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user
        .require_global_permission(&state.db, "system:configure_oidc")
        .await?;

    let dc = &state.dynamic_config;
    if req.enabled
        && let Ok(None) = crate::oidc_helpers::active_config(dc, &state.config).await
    {
        return Err(AppError::BadRequest(
            "Cannot enable SSO: no active configuration. Run the setup wizard first.".into(),
        ));
    }

    dc.upsert(
        "oidc.enabled",
        &serde_json::json!(req.enabled),
        "oidc",
        Some("SSO enabled"),
        Some(auth_user.claims.sub),
    )
    .await
    .map_err(AppError::Internal)?;

    if req.enabled {
        if let Ok(Some(cfg)) = crate::oidc_helpers::active_config(dc, &state.config).await {
            match think_watch_auth::oidc::OidcManager::discover(&cfg).await {
                Ok(mgr) => *state.oidc.write().await = Some(mgr),
                Err(e) => {
                    tracing::error!("OIDC discovery failed after enable: {e}");
                    *state.oidc.write().await = None;
                    return Err(AppError::BadRequest(format!(
                        "OIDC discovery failed: {e}. SSO has been disabled."
                    )));
                }
            }
        }
    } else {
        *state.oidc.write().await = None;
    }

    think_watch_common::dynamic_config::notify_config_changed(&state.redis).await;
    state.audit.log(
        auth_user
            .audit(if req.enabled {
                "settings.oidc_enabled"
            } else {
                "settings.oidc_disabled"
            })
            .resource("oidc"),
    );

    Ok(Json(serde_json::json!({"status": "updated"})))
}

/// Read the most recent OIDC test-login result from Redis.
async fn read_test_result(redis: &fred::clients::Client) -> Option<OidcTestResult> {
    let stored: Option<String> = fred::interfaces::KeysInterface::get(redis, OIDC_TEST_RESULT_KEY)
        .await
        .ok()?;
    serde_json::from_str(&stored?).ok()
}
