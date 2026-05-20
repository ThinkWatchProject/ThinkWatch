//! Token-endpoint HTTP client for the OAuth 2.0 Authorization Code
//! flow with PKCE. The pure shape: given the resolved endpoint +
//! client credentials + auth code + code_verifier, POST to the
//! upstream and return the parsed `TokenEndpointResponse`.
//!
//! The handler crate owns the wiring around this (resolving the
//! endpoint from `mcp_servers` row vs wizard state blob, decrypting
//! the client secret, mapping the structured error to `AppError`
//! for HTTP rendering). Pulling the HTTP call out makes it
//! unit-testable in isolation and lets a future test (e.g.
//! `mockito`-backed) exercise the upstream rejection / non-JSON
//! response branches without booting the server.

use serde::Deserialize;

use super::pkce::parse_token_endpoint_error;

/// RFC 6749 §5.1 successful token response. Vendor extensions
/// (`token_type`, `id_token`, etc.) are tolerated via serde's
/// default ignore-unknown-fields behaviour.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenEndpointResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Structured failure modes from a token exchange. Handler crate
/// maps these to `AppError` + user-facing messages; keeping the
/// auth crate ignorant of `AppError` lets it stay reusable from
/// other surfaces (a CLI debugger, an SSO callback, etc.).
#[derive(Debug, thiserror::Error)]
pub enum TokenExchangeError {
    /// Form encoding failed. In practice never fires — every field
    /// passed in is a plain string with no encoding surprises — but
    /// included so the call site doesn't have to `.expect()`.
    #[error("encode token form: {0}")]
    EncodeForm(#[from] serde_urlencoded::ser::Error),
    /// Transport error: DNS, TCP, TLS handshake, body read.
    #[error("token endpoint unreachable: {0}")]
    Unreachable(reqwest::Error),
    /// Upstream returned a non-2xx, OR a 2xx body that contained an
    /// RFC 6749 §5.2 error envelope. `detail` is the user-facing
    /// summary already extracted via [`parse_token_endpoint_error`]
    /// when the body looked like an error envelope, else "HTTP {status}".
    #[error("upstream rejected the OAuth exchange: {detail}")]
    UpstreamRejected { detail: String },
    /// Upstream replied 2xx but the body wasn't valid JSON shaped
    /// like a [`TokenEndpointResponse`]. The body itself is NOT
    /// included in the error message — on malformed-but-token-bearing
    /// OAuth responses (some misbehaving servers) we don't want to
    /// leak `access_token` / `refresh_token` into upstream logs.
    #[error("token response not JSON: {0}")]
    InvalidResponse(String),
}

/// Run the OAuth 2.0 Authorization Code + PKCE token exchange. The
/// caller has already:
///   1. Persisted the `code_verifier` alongside the state blob.
///   2. Re-derived it on callback after verifying the state binding.
///   3. Decrypted `client_secret` (if any) to plaintext.
///
/// `client_secret` is `None` for public clients (PKCE-only without
/// a confidential client). Per RFC 6749 the parameter is just
/// omitted from the form in that case.
pub async fn exchange_authorization_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenEndpointResponse, TokenExchangeError> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    let body = serde_urlencoded::to_string(&form)?;

    let resp = http
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(TokenExchangeError::Unreachable)?;

    let status = resp.status();
    let resp_text = resp.text().await.map_err(TokenExchangeError::Unreachable)?;

    // Two cases land here:
    //   * non-2xx status (always treated as failure)
    //   * 2xx with an error envelope in the body (some upstreams
    //     return 200 with `{"error":"..."}` instead of the proper
    //     400 — Atlassian famously does this)
    if !status.is_success() {
        let detail =
            parse_token_endpoint_error(&resp_text).unwrap_or_else(|| format!("HTTP {status}"));
        return Err(TokenExchangeError::UpstreamRejected { detail });
    }
    if let Some(detail) = parse_token_endpoint_error(&resp_text) {
        return Err(TokenExchangeError::UpstreamRejected { detail });
    }

    serde_json::from_str(&resp_text).map_err(|e| {
        tracing::warn!(error = %e, body_len = resp_text.len(), "OAuth token response was not JSON");
        TokenExchangeError::InvalidResponse(e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sanity check the error envelope parser still composes through
    // a real-shaped Atlassian-style 200-with-error response. The
    // transport layer of `exchange_authorization_code` itself needs
    // a mock HTTP server to exercise end-to-end (mockito-backed
    // tests would belong here when the auth crate grows that test
    // dep); for now we verify the parse layer that decides whether
    // a body is "rejection" vs "success".
    #[test]
    fn upstream_rejected_detail_format_matches_parse_helper() {
        let body = r#"{"error":"invalid_grant","error_description":"code expired"}"#;
        let detail = parse_token_endpoint_error(body).expect("envelope parses");
        assert_eq!(detail, "code expired (invalid_grant)");
    }

    #[test]
    fn error_display_includes_detail() {
        let e = TokenExchangeError::UpstreamRejected {
            detail: "invalid_client".to_owned(),
        };
        assert!(e.to_string().contains("invalid_client"));
    }
}
