//! Per-user rate limit for "test" endpoints that trigger outbound HTTP
//! requests (MCP test-connection, log forwarder test). Prevents an
//! attacker with write permissions from using these admin tools as
//! a cheap SSRF / port scanner.
//!
//! Implementation: Redis INCR with a fixed 60-second TTL. Not a rolling
//! window, but good enough for abuse prevention on a rarely-used admin
//! endpoint. The key includes a per-session fingerprint (JWT `iat`)
//! so an attacker using a stolen token runs against their own bucket
//! instead of inheriting the legit admin's quota.

use fred::interfaces::KeysInterface;
use think_watch_common::errors::AppError;
use uuid::Uuid;

const MAX_CALLS_PER_MIN: u32 = 5;

/// Bump the caller's counter for `endpoint_tag`; return 429 if over the
/// 5-per-minute cap. Best-effort: if Redis is unreachable, the check
/// passes through so the endpoint remains usable during a cache outage.
pub async fn check_test_rate_limit(
    redis: &fred::clients::Client,
    user_id: Uuid,
    credential_fingerprint: i64,
    endpoint_tag: &str,
) -> Result<(), AppError> {
    check_admin_rate_limit(
        redis,
        user_id,
        credential_fingerprint,
        endpoint_tag,
        MAX_CALLS_PER_MIN,
    )
    .await
}

/// Same fixed-window mechanism as `check_test_rate_limit` but with a
/// caller-chosen cap. Used by admin endpoints that want a more
/// permissive ceiling than the 5/min default — e.g. trace lookups,
/// which are read-only but still hit ClickHouse and shouldn't
/// support 100/sec abuse from a stolen admin token.
pub async fn check_admin_rate_limit(
    redis: &fred::clients::Client,
    user_id: Uuid,
    credential_fingerprint: i64,
    endpoint_tag: &str,
    limit_per_min: u32,
) -> Result<(), AppError> {
    let key = format!("rl:admin:{endpoint_tag}:{user_id}:{credential_fingerprint}");
    // Atomically create the counter with a TTL if absent; the follow-up
    // INCR then always lands on a key that has an expiry. Matches the
    // pattern used on the login rate-limits (see SEC-03).
    let _: Result<bool, _> = redis
        .set(
            &key,
            "0",
            Some(fred::types::Expiration::EX(60)),
            Some(fred::types::SetOptions::NX),
            false,
        )
        .await;
    let count: u32 = match redis.incr::<u32, _>(&key).await {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };
    if count > limit_per_min {
        return Err(AppError::RateLimited);
    }
    Ok(())
}
