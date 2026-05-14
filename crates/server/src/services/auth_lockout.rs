//! Progressive lockout helper, shared between the login handler and
//! the change-password handler.
//!
//! The two prior copies kept the same 5/8/10 → 60/300/900s ladder
//! and the same `SET NX EX 900` counter-init recipe in lockstep by
//! convention only. Centralising the choreography here means
//!
//! - the ladder constants live in exactly one place
//! - the next site that needs failure-counted lockout (TOTP retry,
//!   recovery-code use, API-key rotation, …) inherits the policy
//!   without re-rolling its own thresholds
//! - the Redis error → `AppError::Internal` fail-closed mapping
//!   stays uniform: an outage during brute-force protection rejects
//!   requests instead of silently disabling the gate.
//!
//! The login handler trips the ladder on `max(per-ip-and-email,
//! per-email)`; the change_password handler trips it on a single
//! `per-user` counter. So this module deliberately exposes the
//! primitives (`record_failure`, `apply_lockout`, `is_locked`,
//! `clear`, `ladder_secs`) rather than a single `record_and_lock`,
//! so each caller can compose them around its own trigger logic
//! without baking in the wrong shape.

use fred::clients::Client;
use fred::interfaces::KeysInterface;
use fred::types::{Expiration, SetOptions};

use think_watch_common::errors::AppError;

/// Counter TTL: how long a failure counter remembers attempts when
/// no new failure increments it. 15 minutes matches the rate-limit
/// window in /login.
pub const COUNTER_WINDOW_SECS: i64 = 900;

/// Map a running failure count to a lockout duration. Returns `None`
/// below the 5-failure floor — caller should not set a lockout key
/// in that case. The 60 / 300 / 900 ladder is intentional: short
/// enough to recover from a real typo, long enough to be costly to
/// an attacker.
pub fn ladder_secs(failures: u64) -> Option<i64> {
    if failures >= 10 {
        Some(900)
    } else if failures >= 8 {
        Some(300)
    } else if failures >= 5 {
        Some(60)
    } else {
        None
    }
}

/// Return true if `locked_key` is currently set. Fail-closed: a
/// Redis error returns `Err`, which the caller maps to a 5xx so
/// brute-force protection doesn't silently disable mid-attack.
pub async fn is_locked(redis: &Client, locked_key: &str) -> Result<bool, AppError> {
    let v: Option<String> = redis.get(locked_key).await.map_err(|e| {
        tracing::error!(error = %e, key = %locked_key, "Redis lockout check failed (fail-closed)");
        AppError::Internal(anyhow::anyhow!("Authentication temporarily unavailable"))
    })?;
    Ok(v.is_some())
}

/// Initialise (NX, TTL 900s) then increment the failure counter,
/// returning the post-increment count. The NX-init guarantees the
/// counter has a TTL even if INCR creates the key — bare INCR on a
/// fresh key leaves it without expiry, which would keep stale
/// lockouts alive forever.
pub async fn record_failure(redis: &Client, counter_key: &str) -> Result<u64, AppError> {
    let _: () = redis
        .set(
            counter_key,
            "0",
            Some(Expiration::EX(COUNTER_WINDOW_SECS)),
            Some(SetOptions::NX),
            false,
        )
        .await
        .map_err(|e| {
            tracing::error!(error = %e, key = %counter_key, "Redis lockout counter init failed (fail-closed)");
            AppError::Internal(anyhow::anyhow!("Authentication temporarily unavailable"))
        })?;
    let n: u64 = redis.incr_by(counter_key, 1).await.map_err(|e| {
        tracing::error!(error = %e, key = %counter_key, "Redis lockout counter incr failed (fail-closed)");
        AppError::Internal(anyhow::anyhow!("Authentication temporarily unavailable"))
    })?;
    Ok(n)
}

/// Set the lockout key with the supplied TTL. Fail-closed on Redis
/// errors — without a lockout an attacker would otherwise be free
/// to keep grinding.
pub async fn apply_lockout(redis: &Client, locked_key: &str, secs: i64) -> Result<(), AppError> {
    redis
        .set::<(), _, _>(
            locked_key,
            "1",
            Some(Expiration::EX(secs)),
            None,
            false,
        )
        .await
        .map_err(|e| {
            tracing::error!(error = %e, key = %locked_key, "Redis lockout SET failed (fail-closed)");
            AppError::Internal(anyhow::anyhow!("Authentication temporarily unavailable"))
        })
}

/// Drop counter + lockout keys on a successful authentication.
/// Best-effort: a Redis hiccup here would only leave stale counters
/// behind, which expire naturally; not worth failing the success
/// path over.
pub async fn clear(redis: &Client, keys: &[&str]) {
    for k in keys {
        let _: i64 = redis.del(*k).await.unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_breakpoints() {
        assert_eq!(ladder_secs(0), None);
        assert_eq!(ladder_secs(4), None);
        assert_eq!(ladder_secs(5), Some(60));
        assert_eq!(ladder_secs(7), Some(60));
        assert_eq!(ladder_secs(8), Some(300));
        assert_eq!(ladder_secs(9), Some(300));
        assert_eq!(ladder_secs(10), Some(900));
        assert_eq!(ladder_secs(1_000), Some(900));
    }
}
