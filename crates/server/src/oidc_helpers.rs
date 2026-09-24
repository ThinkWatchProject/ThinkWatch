//! OIDC configuration assembly + crypto helpers.
//!
//! Three places need to materialise an [`OidcConfig`] from settings:
//!  1. Server startup (`init::build_oidc`) — reads the **active** config,
//!     runs discovery, and parks the resulting [`OidcManager`] in
//!     `AppState::oidc` for live SSO logins.
//!  2. The wizard's "Verify issuer" / "Test login" endpoints — read
//!     the **draft** config, run discovery on demand, and either
//!     return the metadata snapshot or stash a throwaway manager in
//!     Redis for the popup callback to pick up.
//!  3. The active-config save flow — re-runs discovery after the draft
//!     is promoted, replacing the live manager.
//!
//! Centralising the secret decrypt + redirect-URL defaulting here
//! keeps those three paths from drifting.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use think_watch_auth::oidc::OidcConfig;
use think_watch_common::config::AppConfig;
use think_watch_common::crypto;
use think_watch_common::dynamic_config::DynamicConfig;

/// Decrypt a hex-encoded AES-256-GCM client secret using the app's
/// master encryption key. Returns an empty string when no secret is
/// stored — the caller decides whether that's an error.
pub fn decrypt_client_secret(
    encrypted_hex: &str,
    app_config: &AppConfig,
) -> anyhow::Result<String> {
    if encrypted_hex.is_empty() {
        return Ok(String::new());
    }
    let key = crypto::parse_encryption_key(&app_config.encryption_key)
        .map_err(|e| anyhow::anyhow!("encryption key error: {e}"))?;
    let bytes = hex::decode(encrypted_hex)
        .map_err(|e| anyhow::anyhow!("encrypted secret hex decode failed: {e}"))?;
    let plain = crypto::decrypt(&bytes, &key)
        .map_err(|e| anyhow::anyhow!("client secret decrypt failed: {e}"))?;
    String::from_utf8(plain).map_err(|e| anyhow::anyhow!("client secret is not valid UTF-8: {e}"))
}

/// Encrypt a plaintext client secret to its hex-encoded persistence
/// form. The inverse of [`decrypt_client_secret`].
pub fn encrypt_client_secret(plaintext: &str, app_config: &AppConfig) -> anyhow::Result<String> {
    let key = crypto::parse_encryption_key(&app_config.encryption_key)
        .map_err(|e| anyhow::anyhow!("encryption key error: {e}"))?;
    let encrypted = crypto::encrypt(plaintext.as_bytes(), &key)
        .map_err(|e| anyhow::anyhow!("client secret encrypt failed: {e}"))?;
    Ok(hex::encode(encrypted))
}

/// Default redirect URL when the admin hasn't picked one. Computed
/// from the dev server bind address — production deploys override
/// via the wizard.
pub fn default_redirect_url(app_config: &AppConfig) -> String {
    format!(
        "http://{}:{}/api/auth/sso/callback",
        app_config.server_host, app_config.console_port
    )
}

/// Build an [`OidcConfig`] from the **active** settings. Returns
/// `Ok(None)` when the active config isn't viable (SSO disabled or
/// any required field empty) — the caller treats that as "no SSO".
pub async fn active_config(
    dc: &DynamicConfig,
    app_config: &AppConfig,
) -> anyhow::Result<Option<OidcConfig>> {
    if !dc.oidc_enabled().await {
        return Ok(None);
    }
    let issuer_url = dc.oidc_issuer_url().await.unwrap_or_default();
    let client_id = dc.oidc_client_id().await.unwrap_or_default();
    let secret_enc = dc.oidc_client_secret_encrypted().await.unwrap_or_default();
    let redirect_url = dc
        .oidc_redirect_url()
        .await
        .unwrap_or_else(|| default_redirect_url(app_config));
    let client_secret = decrypt_client_secret(&secret_enc, app_config)?;
    if issuer_url.is_empty() || client_id.is_empty() || client_secret.is_empty() {
        return Ok(None);
    }
    Ok(Some(OidcConfig {
        issuer_url,
        client_id,
        client_secret,
        redirect_url,
        email_claim: dc.oidc_email_claim().await,
        name_claim: dc.oidc_name_claim().await,
    }))
}

/// JSON shape of the `oidc.draft` blob in `system_settings`. Stored
/// raw in the DB so we don't need a migration to evolve fields —
/// missing fields just decode as `None`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, utoipa::ToSchema)]
pub struct OidcDraft {
    /// Provider catalog entry the wizard chose (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Hex AES-256-GCM ciphertext. Plaintext never persists, even in
    /// the draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_encrypted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_claim: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_claim: Option<String>,
}

impl OidcDraft {
    pub fn from_value(v: &Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }

    /// Build the runtime config the OidcManager needs from this draft
    /// + the app's encryption key. Returns `None` if the draft is too
    ///   incomplete to discover/test (any required field missing).
    pub fn to_oidc_config(&self, app_config: &AppConfig) -> anyhow::Result<Option<OidcConfig>> {
        let issuer_url = self.issuer_url.clone().unwrap_or_default();
        let client_id = self.client_id.clone().unwrap_or_default();
        let secret_enc = self.client_secret_encrypted.clone().unwrap_or_default();
        let redirect_url = self
            .redirect_url
            .clone()
            .unwrap_or_else(|| default_redirect_url(app_config));
        let client_secret = decrypt_client_secret(&secret_enc, app_config)?;
        if issuer_url.is_empty() || client_id.is_empty() || client_secret.is_empty() {
            return Ok(None);
        }
        Ok(Some(OidcConfig {
            issuer_url,
            client_id,
            client_secret,
            redirect_url,
            email_claim: self
                .email_claim
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "email".to_string()),
            name_claim: self
                .name_claim
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "name".to_string()),
        }))
    }
}
