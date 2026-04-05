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

/// Atomic TPM check: trim → sum token weights from member names → conditionally record.
/// Members stored as "uuid:token_count" format.
const LUA_TPM_CHECK: &str = r#"
local key = KEYS[1]
local window_start = tonumber(ARGV[1])
local now_ms = tonumber(ARGV[2])
local member = ARGV[3]
local tokens = tonumber(ARGV[4])
local limit = tonumber(ARGV[5])

redis.call('ZREMRANGEBYSCORE', key, '-inf', window_start)
local members = redis.call('ZRANGEBYSCORE', key, window_start, '+inf')
local current = 0
for _, m in ipairs(members) do
    local t = m:match(':(%d+)$')
    if t then current = current + tonumber(t) end
end
if current + tokens > limit then
    return 0
end
redis.call('ZADD', key, now_ms, member)
redis.call('EXPIRE', key, 120)
return 1
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

        // Atomic RPM check — returns [allowed, count, ttl_ms]
        let rpm_key = format!("ratelimit:rpm:{key}");
        let result: Vec<i64> = self
            .redis
            .eval(
                LUA_RPM_CHECK,
                vec![rpm_key.as_str()],
                vec![
                    window_start.to_string(),
                    now_ms.to_string(),
                    member_id.clone(),
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
        let reset_at = (chrono::Utc::now().timestamp()) + 60; // window resets in ~60s

        if allowed == 0 {
            metrics::counter!("gateway_rate_limited_total", "type" => "rpm").increment(1);
            return Err(AppError::RateLimited);
        }

        let rate_info = RateLimitInfo {
            limit: rpm_limit,
            remaining: rpm_limit.saturating_sub(current),
            reset_at,
        };

        // Atomic TPM check (optional)
        if let (Some(limit), Some(tokens)) = (tpm_limit, estimated_tokens)
            && tokens > 0
        {
            let tpm_key = format!("ratelimit:tpm:{key}");
            let member_with_tokens = format!("{member_id}:{tokens}");

            let allowed: i64 = self
                .redis
                .eval(
                    LUA_TPM_CHECK,
                    vec![tpm_key.as_str()],
                    vec![
                        window_start.to_string(),
                        now_ms.to_string(),
                        member_with_tokens,
                        tokens.to_string(),
                        limit.to_string(),
                    ],
                )
                .await
                .unwrap_or(1); // Fail open on TPM errors

            if allowed == 0 {
                metrics::counter!("gateway_rate_limited_total", "type" => "tpm").increment(1);
                return Err(AppError::RateLimited);
            }
        }

        Ok(rate_info)
    }
}
