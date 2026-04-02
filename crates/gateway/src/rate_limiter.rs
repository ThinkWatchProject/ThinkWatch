use agent_bastion_common::errors::AppError;
use fred::clients::Client;
use fred::interfaces::LuaInterface;

/// Sliding-window rate limiter backed by Redis sorted sets + Lua scripts.
///
/// All check-and-record operations are atomic via EVAL to prevent race conditions.
#[derive(Clone)]
pub struct RateLimiter {
    redis: Client,
}

/// Atomic RPM check: trim window → count → conditionally record → set TTL.
/// Returns 1 if allowed, 0 if rate limited.
const LUA_RPM_CHECK: &str = r#"
local key = KEYS[1]
local window_start = tonumber(ARGV[1])
local now_ms = tonumber(ARGV[2])
local member = ARGV[3]
local limit = tonumber(ARGV[4])

redis.call('ZREMRANGEBYSCORE', key, '-inf', window_start)
local count = redis.call('ZCARD', key)
if count >= limit then
    return 0
end
redis.call('ZADD', key, now_ms, member)
redis.call('EXPIRE', key, 120)
return 1
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

impl RateLimiter {
    pub fn new(redis: Client) -> Self {
        Self { redis }
    }

    /// Check (and atomically record) a request against the sliding window limits.
    pub async fn check_rate_limit(
        &self,
        key: &str,
        rpm_limit: u32,
        tpm_limit: Option<u32>,
        estimated_tokens: Option<u32>,
    ) -> Result<(), AppError> {
        let now_ms = chrono::Utc::now().timestamp_millis() as f64;
        let window_start = now_ms - 60_000.0;
        let member_id = uuid::Uuid::new_v4().to_string();

        // Atomic RPM check
        let rpm_key = format!("ratelimit:rpm:{key}");
        let allowed: i64 = self
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

        if allowed == 0 {
            return Err(AppError::RateLimited);
        }

        // Atomic TPM check (optional)
        if let (Some(limit), Some(tokens)) = (tpm_limit, estimated_tokens) {
            if tokens > 0 {
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
                    return Err(AppError::RateLimited);
                }
            }
        }

        Ok(())
    }
}
