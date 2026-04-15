//! Per-user rate limit for "test" endpoints that trigger outbound HTTP
//! requests (MCP test-connection, log forwarder test). Prevents an
//! attacker with write permissions from using these admin tools as
//! a cheap SSRF / port scanner.
//!
//! Implementation: Redis INCR with a fixed 60-second TTL. Not a rolling
//! window, but good enough for abuse prevention on a rarely-used admin
//! endpoint.

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
    endpoint_tag: &str,
) -> Result<(), AppError> {
    check_admin_rate_limit(redis, user_id, endpoint_tag, MAX_CALLS_PER_MIN).await
}

/// Same fixed-window mechanism as `check_test_rate_limit` but with a
/// caller-chosen cap. Used by admin endpoints that want a more
/// permissive ceiling than the 5/min default — e.g. trace lookups,
/// which are read-only but still hit ClickHouse and shouldn't
/// support 100/sec abuse from a stolen admin token.
pub async fn check_admin_rate_limit(
    redis: &fred::clients::Client,
    user_id: Uuid,
    endpoint_tag: &str,
    limit_per_min: u32,
) -> Result<(), AppError> {
    let key = format!("rl:admin:{endpoint_tag}:{user_id}");
    let count: u32 = match redis.incr::<u32, _>(&key).await {
        Ok(n) => n,
        Err(_) => return Ok(()),
    };
    if count == 1 {
        // First hit in the window — set TTL. Ignore errors; worst case
        // the key stays slightly longer, which tightens the limit.
        let _: Result<bool, _> = redis.expire(&key, 60, None).await;
    }
    if count > limit_per_min {
        return Err(AppError::RateLimited);
    }
    Ok(())
}
