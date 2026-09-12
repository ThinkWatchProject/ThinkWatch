use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, RedirectUrl,
    Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
    additional_trusted_audiences: Arc<HashSet<String>>,
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

        let trusted_audiences_raw = match std::env::var("OIDC_ADDITIONAL_TRUSTED_AUDIENCES") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => String::new(),
            Err(std::env::VarError::NotUnicode(_)) => {
                anyhow::bail!("OIDC_ADDITIONAL_TRUSTED_AUDIENCES must contain valid UTF-8")
            }
        };
        let additional_trusted_audiences =
            Arc::new(parse_additional_trusted_audiences(&trusted_audiences_raw));

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
                additional_trusted_audiences,
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

        // Only install the custom other-audience verifier when we actually
        // have trusted audiences configured. `set_other_audience_verifier_fn`
        // *replaces* the crate's built-in `StandardAudienceVerifier` (which
        // additionally accepts a multi-audience token when `azp` matches the
        // client ID, per OIDC Core 3.1.3.7) with a callback that only
        // consults our allowlist. Skipping the call entirely when the
        // allowlist is empty keeps that unset case byte-for-byte identical
        // to stock `openidconnect` behavior — no ENV means no behavior
        // change, full stop.
        let claims = if self.inner.additional_trusted_audiences.is_empty() {
            let verifier = client.id_token_verifier();
            id_token
                .claims(&verifier, nonce)
                .map_err(|e| anyhow::anyhow!("ID token verification failed: {e}"))?
        } else {
            let trusted_audiences = Arc::clone(&self.inner.additional_trusted_audiences);
            let verifier =
                client
                    .id_token_verifier()
                    .set_other_audience_verifier_fn(move |audience| {
                        trusted_audiences.contains(audience.as_str())
                    });
            id_token
                .claims(&verifier, nonce)
                .map_err(|e| anyhow::anyhow!("ID token verification failed: {e}"))?
        };

        let audiences = claims.audiences();
        let client_id_in_audiences = audiences
            .iter()
            .any(|audience| audience.as_str() == self.inner.client_id.as_str());
        validate_authorized_party(
            audiences.len(),
            client_id_in_audiences,
            claims.authorized_party().map(|party| party.as_str()),
            self.inner.client_id.as_str(),
        )?;

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

        // Drop the email if the IdP explicitly signals it is unverified.
        // In multi-tenant IdPs a user can set their account's email to
        // any string they like; provisioning a local user keyed on that
        // unverified value would let one tenant forge another's
        // identity. When `email_verified` is missing entirely we keep
        // the email — OIDC providers that never emit the claim are
        // common, and the subject+issuer pair still anchors identity.
        let email = match claims.email_verified() {
            Some(false) => None,
            _ => email,
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

fn parse_additional_trusted_audiences(value: &str) -> HashSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|audience| !audience.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Enforce OIDC Core 3.1.3.7's `azp` steps, which `openidconnect` leaves
/// to the caller — its own implementation of steps 4 and 5 is commented
/// out upstream, with a note preferring that clients supply the check.
///
/// Step 5 (`azp`, if present, must be our client ID) is checked
/// unconditionally. Step 4 (a multi-audience token should carry `azp`)
/// makes a *missing* `azp` fatal only when the audiences don't already
/// prove sole intent for us.
///
/// `client_id_in_audiences` is false-only in theory: `IdTokenVerifier`
/// enforces `client_id ∈ aud` in its `aud_match_required` block *before*
/// `other_aud_verifier_fn` is consulted, so a token that omits our client
/// ID never reaches this function no matter what the allowlist says. It
/// stays a parameter as defense in depth against that guarantee changing
/// (an `aud_match_required(false)`, or an upstream rewrite) — it is not
/// load-bearing today, and the test covering it documents a state the
/// crate currently makes unreachable.
fn validate_authorized_party(
    audience_count: usize,
    client_id_in_audiences: bool,
    authorized_party: Option<&str>,
    client_id: &str,
) -> anyhow::Result<()> {
    match authorized_party {
        // Step 5: present but pointing at somebody else. Nothing about
        // the audience count can excuse this — the IdP is telling us the
        // token was authorized for a different client.
        Some(party) if party != client_id => {
            anyhow::bail!("ID token azp does not match the client ID")
        }
        // Step 4: absent, and the audiences alone don't establish that
        // this token was minted for us.
        None if audience_count > 1 || !client_id_in_audiences => {
            anyhow::bail!("ID token audiences require azp to match the client ID")
        }
        _ => Ok(()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_audiences_are_parsed_as_trimmed_exact_values() {
        let parsed = parse_additional_trusted_audiences(" project-1,project-2 , project-1 ,, ");

        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains("project-1"));
        assert!(parsed.contains("project-2"));
    }

    #[test]
    fn multiple_audiences_require_matching_authorized_party() {
        assert!(
            validate_authorized_party(2, true, Some("thinkwatch-client"), "thinkwatch-client")
                .is_ok()
        );
        assert!(validate_authorized_party(2, true, None, "thinkwatch-client").is_err());
        assert!(
            validate_authorized_party(2, true, Some("other-client"), "thinkwatch-client").is_err()
        );
    }

    #[test]
    fn mismatched_authorized_party_is_rejected_even_with_a_single_audience() {
        // OIDC Core step 5 has no audience-count precondition. aud =
        // [thinkwatch-client] with azp = other-client means the IdP
        // authorized a different client, and stock `openidconnect` would
        // let it through because its azp check is commented out.
        assert!(
            validate_authorized_party(1, true, Some("other-client"), "thinkwatch-client").is_err()
        );
        // The same shape with a matching azp stays fine.
        assert!(
            validate_authorized_party(1, true, Some("thinkwatch-client"), "thinkwatch-client")
                .is_ok()
        );
    }

    #[test]
    fn single_audience_matching_client_id_does_not_require_authorized_party() {
        assert!(validate_authorized_party(1, true, None, "thinkwatch-client").is_ok());
    }

    #[test]
    fn single_trusted_audience_other_than_client_id_requires_authorized_party() {
        // aud = [trusted-project-id] only, client ID absent from aud — the
        // trusted-audience allowlist accepted the token, but azp is the
        // only remaining proof it was issued for us.
        assert!(
            validate_authorized_party(1, false, Some("thinkwatch-client"), "thinkwatch-client")
                .is_ok()
        );
        assert!(validate_authorized_party(1, false, None, "thinkwatch-client").is_err());
        assert!(
            validate_authorized_party(1, false, Some("other-client"), "thinkwatch-client").is_err()
        );
    }

    #[test]
    fn empty_trusted_audiences_set_skips_custom_verifier_path() {
        // Documents the intended behavior distinguished at the call site
        // in `exchange_code`: an empty allowlist must take the plain
        // `client.id_token_verifier()` branch (stock upstream behavior),
        // never the `set_other_audience_verifier_fn` branch. Enforced by
        // code review / the `is_empty()` branch above; a full integration
        // test would require a signed JWT fixture and issuer discovery,
        // so it belongs with the other network-bound auth tests rather
        // than here.
        let empty = parse_additional_trusted_audiences("");
        assert!(empty.is_empty());
    }
}
