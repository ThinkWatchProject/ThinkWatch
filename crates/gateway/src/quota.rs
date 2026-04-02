use agent_bastion_common::errors::AppError;
use fred::clients::Client;
use fred::interfaces::KeysInterface;

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

        let used: u64 = self
            .redis
            .get(Self::usage_key(key))
            .await
            .unwrap_or(0);

        if used >= limit {
            return Err(AppError::BadRequest(format!(
                "Token quota exceeded: used {used}/{limit} tokens this month"
            )));
        }

        Ok(limit.saturating_sub(used))
    }

    /// Consume tokens after a successful request. Returns new remaining count.
    pub async fn consume(&self, key: &str, tokens: u32) -> Result<u64, AppError> {
        let usage_key = Self::usage_key(key);

        let new_used: u64 = self
            .redis
            .incr_by(usage_key.as_str(), tokens as i64)
            .await
            .map_err(|e| {
                tracing::warn!("Quota consume failed: {e}");
                AppError::Internal(anyhow::anyhow!("Quota consume failed"))
            })?;

        // Ensure the usage key expires after ~32 days so old months auto-clean
        let _: () = self
            .redis
            .expire(usage_key.as_str(), 32 * 86400, None)
            .await
            .unwrap_or(());

        let limit: u64 = self
            .redis
            .get(Self::limit_key(key))
            .await
            .unwrap_or(0);

        Ok(limit.saturating_sub(new_used))
    }

    /// Get current usage for a quota key.
    pub async fn get_usage(&self, key: &str) -> Result<QuotaInfo, AppError> {
        let limit: u64 = self
            .redis
            .get(Self::limit_key(key))
            .await
            .unwrap_or(0);

        let period = Self::current_month();
        let used: u64 = self
            .redis
            .get(Self::usage_key(key))
            .await
            .unwrap_or(0);

        let remaining = limit.saturating_sub(used);

        Ok(QuotaInfo {
            limit,
            used,
            remaining,
            period,
        })
    }

    /// Set/update quota limit for a key.
    pub async fn set_limit(&self, key: &str, monthly_limit: u64) -> Result<(), AppError> {
        self.redis
            .set::<(), _, _>(
                Self::limit_key(key),
                monthly_limit,
                None,
                None,
                false,
            )
            .await
            .map_err(|e| {
                tracing::warn!("Quota set_limit failed: {e}");
                AppError::Internal(anyhow::anyhow!("Failed to set quota limit"))
            })
    }
}
