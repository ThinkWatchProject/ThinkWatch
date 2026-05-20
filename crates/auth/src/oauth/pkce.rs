//! Pure PKCE / state-binding helpers for OAuth Authorization Code
//! flow. No DB, no axum, no AppState — these are crypto primitives
//! that any OAuth consumer can pull in for unit tests or its own
//! flow implementation.
//!
//! - [`random_token`] mints the 43-char URL-safe token used as the
//!   `state` parameter AND the PKCE `code_verifier` (RFC 7636).
//! - [`pkce_challenge`] derives the S256 code_challenge from the
//!   verifier.
//! - [`state_binding`] HMAC-binds the `(state, code_verifier)` pair
//!   so the callback can verify a state blob it pulled from Redis
//!   wasn't swapped in by an attacker who only learned the state.
//! - [`parse_token_endpoint_error`] decodes the OAuth 2.0 error
//!   response shape (RFC 6749 §5.2).

use data_encoding::BASE64URL_NOPAD;
use hmac::{Hmac, Mac, digest::KeyInit};
use rand::RngExt;
use sha2::{Digest, Sha256};

/// 32 random bytes → URL-safe base64 with no padding. Used for both
/// the state token and the PKCE code_verifier (RFC 7636 mandates
/// `[A-Z][a-z][0-9]-._~`, 43–128 chars; base64url-no-pad of 32 bytes
/// gives 43 URL-safe characters).
///
/// Infallible — uses the thread-local default RNG.
pub fn random_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    BASE64URL_NOPAD.encode(&bytes)
}

/// S256 PKCE code_challenge per RFC 7636 §4.2:
/// `base64url(sha256(verifier))`.
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    BASE64URL_NOPAD.encode(&digest)
}

/// HMAC-SHA256 binding of `(state, code_verifier)` with the server's
/// encryption key. The callback persists this binding alongside the
/// blob in Redis; on return it recomputes the HMAC over what it
/// pulled out and constant-time-compares. Without this, an attacker
/// who learned the `state` value (e.g. via referrer leak) could
/// swap in their own code_verifier and steal the upstream token.
pub fn state_binding(enc_key: &[u8; 32], state: &str, verifier: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(enc_key).expect("HMAC-SHA256 accepts any key length");
    mac.update(state.as_bytes());
    mac.update(b":");
    mac.update(verifier.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Best-effort decode of an OAuth 2.0 token endpoint error response
/// (RFC 6749 §5.2). Returns `Some(human-readable)` on the standard
/// `{"error": "...", "error_description": "..."}` shape, formatted
/// `"<description> (<code>)"` when both fields are present, or just
/// the `error` code alone. Returns `None` when the body isn't even
/// JSON or doesn't carry a top-level `error` field.
///
/// We tolerate non-spec upstreams that omit `error_description` and
/// just fall back to `error` alone.
pub fn parse_token_endpoint_error(body: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_token_is_43_chars_url_safe() {
        // 32 bytes → base64url-no-pad gives ceil(32*4/3) = 43 chars
        // from the unpadded URL-safe alphabet.
        let t = random_token();
        assert_eq!(t.len(), 43);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "token contains non-URL-safe char: {t:?}"
        );
    }

    #[test]
    fn random_token_changes_between_calls() {
        // Sanity check the RNG isn't returning a constant. 1 in 2^256
        // chance of collision so this is fine.
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_test_vector() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = pkce_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn state_binding_is_deterministic() {
        let key = [7u8; 32];
        let a = state_binding(&key, "state-abc", "verifier-xyz");
        let b = state_binding(&key, "state-abc", "verifier-xyz");
        assert_eq!(a, b);
    }

    #[test]
    fn state_binding_changes_with_state() {
        let key = [7u8; 32];
        let a = state_binding(&key, "state-abc", "verifier-xyz");
        let b = state_binding(&key, "state-def", "verifier-xyz");
        assert_ne!(a, b);
    }

    #[test]
    fn state_binding_changes_with_verifier() {
        let key = [7u8; 32];
        let a = state_binding(&key, "state-abc", "verifier-xyz");
        let b = state_binding(&key, "state-abc", "verifier-qrs");
        assert_ne!(a, b);
    }

    #[test]
    fn state_binding_changes_with_key() {
        let key_a = [7u8; 32];
        let key_b = [8u8; 32];
        let a = state_binding(&key_a, "state-abc", "verifier-xyz");
        let b = state_binding(&key_b, "state-abc", "verifier-xyz");
        assert_ne!(a, b);
    }

    #[test]
    fn state_binding_separates_state_from_verifier_via_delimiter() {
        // Without the `:` delimiter, `state="ab"` `verifier="cd"`
        // would HMAC over the same bytes as `state="abc"` `verifier="d"`.
        // The delimiter prevents that collision.
        let key = [7u8; 32];
        let a = state_binding(&key, "ab", "cd");
        let b = state_binding(&key, "abc", "d");
        assert_ne!(a, b);
    }

    #[test]
    fn parse_error_full_shape() {
        let body = r#"{"error":"invalid_grant","error_description":"code expired"}"#;
        assert_eq!(
            parse_token_endpoint_error(body),
            Some("code expired (invalid_grant)".to_owned())
        );
    }

    #[test]
    fn parse_error_code_only() {
        let body = r#"{"error":"invalid_request"}"#;
        assert_eq!(
            parse_token_endpoint_error(body),
            Some("invalid_request".to_owned())
        );
    }

    #[test]
    fn parse_error_empty_description_falls_back_to_code() {
        // Some upstreams send empty string instead of omitting.
        let body = r#"{"error":"invalid_grant","error_description":""}"#;
        assert_eq!(
            parse_token_endpoint_error(body),
            Some("invalid_grant".to_owned())
        );
    }

    #[test]
    fn parse_error_returns_none_on_garbage() {
        assert_eq!(parse_token_endpoint_error("not json"), None);
        assert_eq!(parse_token_endpoint_error(""), None);
        assert_eq!(parse_token_endpoint_error("{}"), None);
    }
}
