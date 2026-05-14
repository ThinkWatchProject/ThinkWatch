//! Refresh-token blacklist — single-use enforcement on the rotation
//! token. Both the `/auth/refresh` handler (where a second claim is
//! a replay attempt) and the `/auth/logout` handler (where we just
//! want to neuter the cookie before the response goes out) share
//! this helper. Centralising the SHA-256 → hex → `SET NX EX` recipe
//! means the two call sites can't drift on key prefix, TTL floor,
//! or atomicity semantics — a previous version of the codebase had
//! `refresh` using SET-NX-EX while `logout` used plain SET-EX,
//! which meant logout would silently overwrite a legitimate
//! refresh's claim entry under concurrent calls.

use fred::clients::Client;
use fred::interfaces::KeysInterface;
use fred::types::{Expiration, SetOptions};
use sha2::{Digest, Sha256};

/// Outcome of attempting to claim a refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This caller was the first to blacklist the token. The refresh
    /// handler proceeds to mint a new session; logout treats this as
    /// a success.
    Claimed,
    /// Another caller (or a prior call) already blacklisted this
    /// token. The refresh handler MUST treat this as a replay and
    /// refuse to mint a session; logout ignores it (the token was
    /// already neutered, mission accomplished).
    AlreadyClaimed,
}

/// Build the Redis key. Public so tests can pin the wire format.
pub fn key_for(raw_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw_token.as_bytes());
    format!("refresh_blacklist:{}", hex::encode(hasher.finalize()))
}

/// Atomically claim a refresh token. The TTL is `max(60, exp_unix -
/// now)` — the 60s floor protects against tokens claimed at the very
/// last instant before natural expiry from leaving a zero-TTL key
/// that Redis would evict immediately. Errors are mapped to the
/// `Err` arm so callers can decide whether to fail-closed (refresh)
/// or fail-soft (logout).
pub async fn claim(
    redis: &Client,
    raw_token: &str,
    token_exp_unix: i64,
) -> Result<ClaimOutcome, fred::error::Error> {
    let blacklist_key = key_for(raw_token);
    let now = chrono::Utc::now().timestamp();
    let remaining_secs = (token_exp_unix - now).max(60);
    let claimed: Option<String> = redis
        .set(
            &blacklist_key,
            "1",
            Some(Expiration::EX(remaining_secs)),
            Some(SetOptions::NX),
            false,
        )
        .await?;
    Ok(if claimed.is_some() {
        ClaimOutcome::Claimed
    } else {
        ClaimOutcome::AlreadyClaimed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_for_is_deterministic_and_prefixed() {
        let a = key_for("tw-refresh-abc");
        let b = key_for("tw-refresh-abc");
        assert_eq!(a, b);
        assert!(a.starts_with("refresh_blacklist:"));
        // SHA-256 hex is 64 chars; full key is prefix + 64.
        assert_eq!(a.len(), "refresh_blacklist:".len() + 64);
    }

    #[test]
    fn key_for_differs_for_different_tokens() {
        assert_ne!(key_for("a"), key_for("b"));
    }
}
