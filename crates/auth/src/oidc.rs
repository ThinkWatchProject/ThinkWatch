use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, RedirectUrl,
    Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// User info extracted from an OIDC ID token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcUserInfo {
    pub subject: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub issuer: String,
}

/// Inputs needed to materialise an `OidcManager`. Used by both the
/// active-config startup path and the wizard's draft test-login path.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    /// JWT claim name carrying the user's email. Defaults to `email`.
    pub email_claim: String,
    /// JWT claim name carrying the display name. Defaults to `name`.
    pub name_claim: String,
}

impl OidcConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.issuer_url.is_empty() {
            return Err("issuer_url is required");
        }
        if self.client_id.is_empty() {
            return Err("client_id is required");
        }
        if self.client_secret.is_empty() {
            return Err("client_secret is required");
        }
        if self.redirect_url.is_empty() {
            return Err("redirect_url is required");
        }
        Ok(())
    }
}

/// Configured OIDC client. One instance per active config; the wizard
/// builds throwaway instances for discovery and test-login.
#[derive(Clone)]
pub struct OidcManager {
    inner: Arc<OidcInner>,
}

struct OidcInner {
    provider_metadata: CoreProviderMetadata,
    client_id: ClientId,
    client_secret: ClientSecret,
    redirect_url: RedirectUrl,
    auth_url: AuthUrl,
    token_url: TokenUrl,
    http_client: openidconnect::reqwest::Client,
    issuer: String,
    email_claim: String,
    name_claim: String,
}

impl OidcManager {
    /// Run OIDC discovery and produce a manager. Both the active config
    /// path and the wizard's "Verify issuer" / "Test login" flows go
    /// through here; the heavy work (fetching `/.well-known/...`) is
    /// done once per call.
    pub async fn discover(config: &OidcConfig) -> anyhow::Result<Self> {
        config.validate().map_err(|e| anyhow::anyhow!(e))?;
        let issuer = IssuerUrl::new(config.issuer_url.clone())?;
        let http_client = openidconnect::reqwest::Client::new();

        let provider_metadata = CoreProviderMetadata::discover_async(issuer.clone(), &http_client)
            .await
            .map_err(|e| anyhow::anyhow!("OIDC discovery failed: {e}"))?;

        let auth_url = provider_metadata.authorization_endpoint().clone();
        let token_url = provider_metadata
            .token_endpoint()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("OIDC provider has no token endpoint"))?;

        Ok(Self {
            inner: Arc::new(OidcInner {
                provider_metadata,
                client_id: ClientId::new(config.client_id.clone()),
                client_secret: ClientSecret::new(config.client_secret.clone()),
                redirect_url: RedirectUrl::new(config.redirect_url.clone())?,
                auth_url,
                token_url,
                http_client,
                issuer: config.issuer_url.clone(),
                email_claim: config.email_claim.clone(),
                name_claim: config.name_claim.clone(),
            }),
        })
    }

    /// Lightweight metadata snapshot — used by the wizard's discovery
    /// step to show the admin which endpoints we discovered without
    /// committing the config.
    pub fn metadata_summary(&self) -> OidcDiscoveryMetadata {
        OidcDiscoveryMetadata {
            authorization_endpoint: self.inner.auth_url.to_string(),
            token_endpoint: self.inner.token_url.to_string(),
            issuer: self.inner.issuer.clone(),
            userinfo_endpoint: self
                .inner
                .provider_metadata
                .userinfo_endpoint()
                .map(|u| u.to_string()),
            jwks_uri: Some(self.inner.provider_metadata.jwks_uri().to_string()),
        }
    }

    /// Generate the authorization URL to redirect the user to.
    pub fn authorize_url(&self) -> (String, CsrfToken, Nonce) {
        let client = CoreClient::from_provider_metadata(
            self.inner.provider_metadata.clone(),
            self.inner.client_id.clone(),
            Some(self.inner.client_secret.clone()),
        )
        .set_auth_uri(self.inner.auth_url.clone())
        .set_token_uri(self.inner.token_url.clone())
        .set_redirect_uri(self.inner.redirect_url.clone());

        let (auth_url, csrf_token, nonce) = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("openid".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .add_scope(Scope::new("profile".to_string()))
            .url();

        (auth_url.to_string(), csrf_token, nonce)
    }

    /// Exchange authorization code for tokens, return user info.
    /// Supports admin-configured claim mapping: when the standard
    /// `email`/`name` claim is missing we fall back to the configured
    /// alternate field name by parsing the verified ID token body.
    pub async fn exchange_code(&self, code: &str, nonce: &Nonce) -> anyhow::Result<OidcUserInfo> {
        let client = CoreClient::from_provider_metadata(
            self.inner.provider_metadata.clone(),
            self.inner.client_id.clone(),
            Some(self.inner.client_secret.clone()),
        )
        .set_auth_uri(self.inner.auth_url.clone())
        .set_token_uri(self.inner.token_url.clone())
        .set_redirect_uri(self.inner.redirect_url.clone());

        let token_response = client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .request_async(&self.inner.http_client)
            .await
            .map_err(|e| anyhow::anyhow!("Token exchange failed: {e}"))?;

        let id_token = token_response
            .id_token()
            .ok_or_else(|| anyhow::anyhow!("No ID token in response"))?;

        let verifier = client.id_token_verifier();
        let claims = id_token
            .claims(&verifier, nonce)
            .map_err(|e| anyhow::anyhow!("ID token verification failed: {e}"))?;

        let subject = claims.subject().to_string();

        // Standard typed accessors for email / name, then fall back to
        // a configured alternate claim name (Microsoft Entra hides
        // email in `preferred_username`, etc.) by re-parsing the
        // already-verified ID token JSON.
        let standard_email = claims.email().map(|e| e.to_string());
        let standard_name = claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.to_string());

        let (email, name) = if self.inner.email_claim != "email" || self.inner.name_claim != "name"
        {
            let raw = decode_id_token_payload(id_token.to_string().as_str()).ok();
            let email = if self.inner.email_claim != "email" {
                raw.as_ref()
                    .and_then(|v| v.get(&self.inner.email_claim))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or(standard_email)
            } else {
                standard_email
            };
            let name = if self.inner.name_claim != "name" {
                raw.as_ref()
                    .and_then(|v| v.get(&self.inner.name_claim))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or(standard_name)
            } else {
                standard_name
            };
            (email, name)
        } else {
            (standard_email, standard_name)
        };

        Ok(OidcUserInfo {
            subject,
            email,
            name,
            issuer: self.inner.issuer.clone(),
        })
    }
}

/// Endpoint summary returned to the wizard after a successful discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcDiscoveryMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userinfo_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
}

/// Pull the JSON payload (middle segment) out of a signed ID token.
/// The token has already been verified by `id_token.claims()`, so we
/// just need access to the raw fields the typed accessors don't expose.
fn decode_id_token_payload(jwt: &str) -> anyhow::Result<serde_json::Value> {
    let mut parts = jwt.split('.');
    let _header = parts.next();
    let payload = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("malformed JWT"))?;
    let bytes = data_encoding::BASE64URL_NOPAD.decode(payload.as_bytes())?;
    Ok(serde_json::from_slice(&bytes)?)
}
