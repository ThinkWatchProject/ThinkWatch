use fred::clients::Client;
use fred::interfaces::LuaInterface;
use think_watch_common::errors::AppError;

/// Sliding-window rate limiter backed by Redis sorted sets + Lua scripts.
///
/// All check-and-record operations are atomic via EVAL to prevent race conditions.
#[derive(Clone)]
pub struct RateLimiter {
    redis: Client,
}

/// Atomic RPM check: trim window → count → conditionally record → set TTL.
/// Returns {allowed (0/1), current_count, ttl_ms}.
const LUA_RPM_CHECK: &str = r#"
local key = KEYS[1]
local window_start = tonumber(ARGV[1])
local now_ms = tonumber(ARGV[2])
local member = ARGV[3]
local limit = tonumber(ARGV[4])

redis.call('ZREMRANGEBYSCORE', key, '-inf', window_start)
local count = redis.call('ZCARD', key)
if count >= limit then
    return {0, count, 60000}
end
redis.call('ZADD', key, now_ms, member)
redis.call('EXPIRE', key, 120)
return {1, count + 1, 60000}
"#;

/// Atomic combined RPM + TPM check. Evaluates BOTH limits and only
/// records (in both sets) if BOTH pass. The split-call shape
/// (`RPM_CHECK` then `TPM_CHECK`) had a bug: RPM recorded its hit
/// *before* TPM ran, so a TPM-blocked 429 still burned the user's
/// RPM budget. Returns `{allowed, rpm_count, denied_by}` where
/// `denied_by` is `0` (allowed), `1` (rpm), or `2` (tpm).
const LUA_COMBINED_CHECK: &str = r#"
local rpm_key = KEYS[1]
local tpm_key = KEYS[2]
local window_start = tonumber(ARGV[1])
local now_ms = tonumber(ARGV[2])
local rpm_member = ARGV[3]
local rpm_limit = tonumber(ARGV[4])
local tpm_member = ARGV[5]
local tokens = tonumber(ARGV[6])
local tpm_limit = tonumber(ARGV[7])

redis.call('ZREMRANGEBYSCORE', rpm_key, '-inf', window_start)
local rpm_count = redis.call('ZCARD', rpm_key)
if rpm_count >= rpm_limit then
    return {0, rpm_count, 1}
end

redis.call('ZREMRANGEBYSCORE', tpm_key, '-inf', window_start)
local members = redis.call('ZRANGEBYSCORE', tpm_key, window_start, '+inf')
local current_tokens = 0
for _, m in ipairs(members) do
    local t = m:match(':(%d+)$')
    if t then current_tokens = current_tokens + tonumber(t) end
end
if current_tokens + tokens > tpm_limit then
    return {0, rpm_count, 2}
end

redis.call('ZADD', rpm_key, now_ms, rpm_member)
redis.call('EXPIRE', rpm_key, 120)
redis.call('ZADD', tpm_key, now_ms, tpm_member)
redis.call('EXPIRE', tpm_key, 120)
return {1, rpm_count + 1, 0}
"#;

/// Rate limit check result with metadata for response headers.
#[derive(Debug, Clone)]
pub struct RateLimitInfo {
    pub limit: u32,
    pub remaining: u32,
    pub reset_at: i64, // Unix timestamp
}

impl RateLimiter {
    pub fn new(redis: Client) -> Self {
        Self { redis }
    }

    /// Check (and atomically record) a request against the sliding window limits.
    /// Returns `RateLimitInfo` on success for setting response headers.
    pub async fn check_rate_limit(
        &self,
        key: &str,
        rpm_limit: u32,
        tpm_limit: Option<u32>,
        estimated_tokens: Option<u32>,
    ) -> Result<RateLimitInfo, AppError> {
        let now_ms = chrono::Utc::now().timestamp_millis() as f64;
        let window_start = now_ms - 60_000.0;
        let member_id = uuid::Uuid::new_v4().to_string();
        let rpm_key = format!("ratelimit:rpm:{key}");
        let reset_at = chrono::Utc::now().timestamp() + 60; // window resets in ~60s

        // When BOTH RPM and TPM are configured, evaluate them atomically
        // in one Lua call. Previously RPM was checked + recorded BEFORE
        // TPM ran; if TPM then rejected, the user's RPM slot was already
        // burned on a 429'd request. The combined script only records
        // when both pass.
        if let (Some(tpm_limit), Some(tokens)) = (tpm_limit, estimated_tokens)
            && tokens > 0
        {
            let tpm_key = format!("ratelimit:tpm:{key}");
            let member_with_tokens = format!("{member_id}:{tokens}");
            let result: Vec<i64> = self
                .redis
                .eval(
                    LUA_COMBINED_CHECK,
                    vec![rpm_key.as_str(), tpm_key.as_str()],
                    vec![
                        window_start.to_string(),
                        now_ms.to_string(),
                        member_id,
                        rpm_limit.to_string(),
                        member_with_tokens,
                        tokens.to_string(),
                        tpm_limit.to_string(),
                    ],
                )
                .await
                .map_err(|e| {
                    tracing::warn!("Combined rate limit check failed: {e}");
                    AppError::Internal(anyhow::anyhow!("Rate limit check failed"))
                })?;
            let allowed = result.first().copied().unwrap_or(1);
            let current = result.get(1).copied().unwrap_or(0) as u32;
            let denied_by = result.get(2).copied().unwrap_or(0);
            if allowed == 0 {
                let label = if denied_by == 2 { "tpm" } else { "rpm" };
                metrics::counter!("gateway_rate_limited_total", "type" => label).increment(1);
                return Err(AppError::RateLimited);
            }
            return Ok(RateLimitInfo {
                limit: rpm_limit,
                remaining: rpm_limit.saturating_sub(current),
                reset_at,
            });
        }

        // RPM-only path (no TPM rule). Single Lua call records the hit
        // when allowed.
        let result: Vec<i64> = self
            .redis
            .eval(
                LUA_RPM_CHECK,
                vec![rpm_key.as_str()],
                vec![
                    window_start.to_string(),
                    now_ms.to_string(),
                    member_id,
                    rpm_limit.to_string(),
                ],
            )
            .await
            .map_err(|e| {
                tracing::warn!("Rate limit RPM check failed: {e}");
                AppError::Internal(anyhow::anyhow!("Rate limit check failed"))
            })?;

        let allowed = result.first().copied().unwrap_or(1);
        let current = result.get(1).copied().unwrap_or(0) as u32;

        if allowed == 0 {
            metrics::counter!("gateway_rate_limited_total", "type" => "rpm").increment(1);
            return Err(AppError::RateLimited);
        }

        Ok(RateLimitInfo {
            limit: rpm_limit,
            remaining: rpm_limit.saturating_sub(current),
            reset_at,
        })
    }
}
