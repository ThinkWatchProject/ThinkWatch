use fred::clients::Client;
use fred::interfaces::{KeysInterface, LuaInterface};
use think_watch_common::errors::AppError;

/// Per-user/team token quota information.
#[derive(Debug, Clone)]
pub struct QuotaInfo {
    /// Monthly token limit (0 means unlimited).
    pub limit: u64,
    /// Tokens used this month.
    pub used: u64,
    /// Remaining tokens (limit - used), clamped to 0.
    pub remaining: u64,
    /// The month key (e.g. "2026-04").
    pub period: String,
}

/// Per-user/team token quota with hard limits, backed by Redis.
#[derive(Clone)]
pub struct QuotaManager {
    redis: Client,
}

impl QuotaManager {
    pub fn new(redis: Client) -> Self {
        Self { redis }
    }

    /// Returns the current month string for Redis keys (e.g. "2026-04").
    fn current_month() -> String {
        chrono::Utc::now().format("%Y-%m").to_string()
    }

    fn limit_key(key: &str) -> String {
        format!("quota:{key}:limit")
    }

    fn usage_key(key: &str) -> String {
        let month = Self::current_month();
        format!("quota:{key}:used:{month}")
    }

    /// Check if user/team has enough quota. Returns remaining tokens.
    /// Returns an error if quota is exceeded.
    pub async fn check_quota(&self, key: &str) -> Result<u64, AppError> {
        let limit: Option<u64> = self.redis.get(Self::limit_key(key)).await.map_err(|e| {
            tracing::warn!("Quota limit read failed: {e}");
            AppError::Internal(anyhow::anyhow!("Quota check failed"))
        })?;

        // No limit set means unlimited
        let limit = match limit {
            Some(l) => l,
            None => return Ok(u64::MAX),
        };

        if limit == 0 {
            return Ok(u64::MAX);
        }

        let used: u64 = self.redis.get(Self::usage_key(key)).await.unwrap_or(0);

        if used >= limit {
            return Err(AppError::BadRequest(format!(
                "Token quota exceeded: used {used}/{limit} tokens this month"
            )));
        }

        Ok(limit.saturating_sub(used))
    }

    /// Consume tokens after a successful request. Returns the new
    /// remaining count.
    ///
    /// Atomically reads the limit, INCRs usage, refreshes TTL, and
    /// reports whether the new total has crossed the limit — all in
    /// a single Redis round-trip via Lua. This closes the
    /// check-then-incr TOCTOU window where two concurrent requests
    /// could both pass `check_quota` and double-spend.
    ///
    /// Crossing the limit during consume bumps
    /// `gateway_quota_overflow_total` and logs a warning, but the
    /// request is **not** rejected — the upstream has already
    /// returned its response by this point, and refusing here would
    /// just hide the spend without recovering the cost. Use the
    /// metric to alert on operators who need to tighten limits.
    pub async fn consume(&self, key: &str, tokens: u32) -> Result<u64, AppError> {
        const LUA_CONSUME: &str = r#"
local limit_key = KEYS[1]
local usage_key = KEYS[2]
local tokens    = tonumber(ARGV[1])
local ttl_secs  = tonumber(ARGV[2])

local limit = tonumber(redis.call('GET', limit_key) or '0')
local new_used = redis.call('INCRBY', usage_key, tokens)
redis.call('EXPIRE', usage_key, ttl_secs)

-- Returns: {new_used, limit, overflowed_flag}
local overflowed = 0
if limit > 0 and new_used > limit then
    overflowed = 1
end
return {new_used, limit, overflowed}
"#;

        let limit_key = Self::limit_key(key);
        let usage_key = Self::usage_key(key);

        let result: Vec<i64> = self
            .redis
            .eval(
                LUA_CONSUME,
                vec![limit_key, usage_key],
                vec![tokens.to_string(), (32 * 86400).to_string()],
            )
            .await
            .map_err(|e| {
                tracing::warn!("Quota consume failed: {e}");
                AppError::Internal(anyhow::anyhow!("Quota consume failed"))
            })?;

        let new_used = result.first().copied().unwrap_or(0).max(0) as u64;
        let limit = result.get(1).copied().unwrap_or(0).max(0) as u64;
        let overflowed = result.get(2).copied().unwrap_or(0) == 1;

        if overflowed {
            metrics::counter!("gateway_quota_overflow_total").increment(1);
            tracing::warn!(
                key = %key,
                used = new_used,
                limit = limit,
                "quota overflow on consume — request was already served, soft cap exceeded"
            );
        }

        Ok(limit.saturating_sub(new_used))
    }

    /// Get current usage for a quota key.
    pub async fn get_usage(&self, key: &str) -> Result<QuotaInfo, AppError> {
        let limit: u64 = self.redis.get(Self::limit_key(key)).await.unwrap_or(0);

        let period = Self::current_month();
        let used: u64 = self.redis.get(Self::usage_key(key)).await.unwrap_or(0);

        let remaining = limit.saturating_sub(used);

        Ok(QuotaInfo {
            limit,
            used,
            remaining,
            period,
        })
    }

    /// Atomically check quota and consume tokens in a single Redis round-trip.
    /// Returns remaining tokens after consumption, or an error if quota would be exceeded.
    pub async fn check_and_consume(&self, key: &str, tokens: u32) -> Result<u64, AppError> {
        const LUA_QUOTA_CHECK_AND_CONSUME: &str = r#"
local limit_key = KEYS[1]
local usage_key = KEYS[2]
local tokens = tonumber(ARGV[1])

local limit = tonumber(redis.call('GET', limit_key) or '0')
if limit <= 0 then return -1 end

local used = tonumber(redis.call('GET', usage_key) or '0')
if used + tokens > limit then return 0 end

redis.call('INCRBY', usage_key, tokens)
return limit - used - tokens
"#;

        let limit_key = Self::limit_key(key);
        let usage_key = Self::usage_key(key);

        let result: i64 = self
            .redis
            .eval::<i64, _, _, _>(
                LUA_QUOTA_CHECK_AND_CONSUME,
                vec![limit_key, usage_key],
                vec![tokens.to_string()],
            )
            .await
            .map_err(|e| {
                tracing::warn!("Atomic quota check failed: {e}");
                AppError::Internal(anyhow::anyhow!("Quota check failed"))
            })?;

        match result {
            -1 => Ok(u64::MAX), // No limit set (unlimited)
            0 => Err(AppError::BadRequest(
                "Token quota exceeded for this month".into(),
            )),
            remaining => Ok(remaining as u64),
        }
    }

    /// Set/update quota limit for a key.
    pub async fn set_limit(&self, key: &str, monthly_limit: u64) -> Result<(), AppError> {
        self.redis
            .set::<(), _, _>(Self::limit_key(key), monthly_limit, None, None, false)
            .await
            .map_err(|e| {
                tracing::warn!("Quota set_limit failed: {e}");
                AppError::Internal(anyhow::anyhow!("Failed to set quota limit"))
            })
    }
}
