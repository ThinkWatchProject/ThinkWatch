use std::time::Duration;

/// Retry policy with exponential backoff and optional jitter.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_delay_ms: 500,
            max_delay_ms: 5000,
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// Compute the delay for a given attempt (0-based).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base = self.initial_delay_ms * 2u64.pow(attempt);
        let capped = base.min(self.max_delay_ms);

        let delay = if self.jitter {
            // Add random jitter up to 25% of the delay
            let jitter_range = capped / 4;
            let jitter = rand::random_range(0..=jitter_range);
            capped + jitter
        } else {
            capped
        };

        Duration::from_millis(delay)
    }

    /// Execute an async operation with retry.
    ///
    /// Only retries on errors where `should_retry` returns true.
    pub async fn execute<F, Fut, T, E>(
        &self,
        mut operation: F,
        should_retry: fn(&E) -> bool,
    ) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let mut last_error;

        match operation().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !should_retry(&e) || self.max_retries == 0 {
                    return Err(e);
                }
                tracing::warn!("Retryable error (attempt 1/{}): {e}", self.max_retries + 1);
                last_error = e;
            }
        }

        for attempt in 1..=self.max_retries {
            let delay = self.delay_for_attempt(attempt - 1);
            tokio::time::sleep(delay).await;

            match operation().await {
                Ok(v) => {
                    tracing::info!("Succeeded on retry attempt {attempt}");
                    return Ok(v);
                }
                Err(e) => {
                    if !should_retry(&e) {
                        return Err(e);
                    }
                    tracing::warn!(
                        "Retryable error (attempt {}/{}):{e}",
                        attempt + 1,
                        self.max_retries + 1
                    );
                    last_error = e;
                }
            }
        }

        Err(last_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn succeeds_on_first_try() {
        let policy = RetryPolicy {
            max_retries: 2,
            initial_delay_ms: 10,
            max_delay_ms: 100,
            jitter: false,
        };

        let result = policy
            .execute(|| async { Ok::<_, String>(42) }, |_| true)
            .await;

        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn retries_on_failure() {
        let counter = AtomicU32::new(0);
        let policy = RetryPolicy {
            max_retries: 2,
            initial_delay_ms: 10,
            max_delay_ms: 100,
            jitter: false,
        };

        let result = policy
            .execute(
                || {
                    let attempt = counter.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if attempt < 2 {
                            Err(format!("fail {attempt}"))
                        } else {
                            Ok(42)
                        }
                    }
                },
                |_| true,
            )
            .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable() {
        let counter = AtomicU32::new(0);
        let policy = RetryPolicy {
            max_retries: 3,
            initial_delay_ms: 10,
            max_delay_ms: 100,
            jitter: false,
        };

        let result: Result<i32, String> = policy
            .execute(
                || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    async { Err("auth error".to_string()) }
                },
                |e| !e.contains("auth"),
            )
            .await;

        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 1); // Only tried once
    }

    #[test]
    fn delay_increases_exponentially() {
        let policy = RetryPolicy {
            max_retries: 5,
            initial_delay_ms: 100,
            max_delay_ms: 5000,
            jitter: false,
        };

        assert_eq!(policy.delay_for_attempt(0), Duration::from_millis(100));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(200));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(400));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(800));
    }

    #[test]
    fn delay_capped_at_max() {
        let policy = RetryPolicy {
            max_retries: 5,
            initial_delay_ms: 1000,
            max_delay_ms: 3000,
            jitter: false,
        };

        assert_eq!(policy.delay_for_attempt(0), Duration::from_millis(1000));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(2000));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(3000)); // capped
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(3000)); // capped
    }
}
