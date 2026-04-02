use crate::providers::traits::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, GatewayError,
};
use crate::providers::DynAiProvider;
use futures::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Load-balancing strategy for selecting a backend.
#[derive(Debug, Clone, Copy)]
pub enum LoadBalanceStrategy {
    RoundRobin,
    Random,
    LeastFailures,
}

/// A single backend in the failover pool.
struct FailoverBackend {
    provider: Arc<dyn DynAiProvider>,
    healthy: AtomicBool,
    consecutive_failures: AtomicU32,
    failure_threshold: u32,
    last_failure: RwLock<Option<Instant>>,
}

impl FailoverBackend {
    fn new(provider: Arc<dyn DynAiProvider>, failure_threshold: u32) -> Self {
        Self {
            provider,
            healthy: AtomicBool::new(true),
            consecutive_failures: AtomicU32::new(0),
            failure_threshold,
            last_failure: RwLock::new(None),
        }
    }

    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.healthy.store(true, Ordering::Relaxed);
    }

    async fn record_failure(&self) {
        let prev = self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        if prev + 1 >= self.failure_threshold {
            self.healthy.store(false, Ordering::Relaxed);
            *self.last_failure.write().await = Some(Instant::now());
            tracing::warn!(
                provider = self.provider.name(),
                "Backend marked unhealthy after {} consecutive failures",
                prev + 1
            );
        }
    }

    /// Check whether enough time has passed to try recovering this backend.
    async fn maybe_recover(&self, recovery_secs: u64) {
        if self.is_healthy() {
            return;
        }
        let guard = self.last_failure.read().await;
        if let Some(ts) = *guard {
            if ts.elapsed() >= Duration::from_secs(recovery_secs) {
                drop(guard);
                tracing::info!(
                    provider = self.provider.name(),
                    "Attempting health recovery for backend"
                );
                self.healthy.store(true, Ordering::Relaxed);
                self.consecutive_failures.store(0, Ordering::Relaxed);
            }
        }
    }
}

/// Wraps multiple provider instances (same provider type, different API keys)
/// with automatic failover and health tracking.
pub struct FailoverProvider {
    name: String,
    backends: Vec<FailoverBackend>,
    strategy: LoadBalanceStrategy,
    next_index: AtomicU32,
    /// Seconds before an unhealthy backend is retried.
    recovery_secs: u64,
}

impl FailoverProvider {
    pub fn new(
        name: String,
        providers: Vec<Arc<dyn DynAiProvider>>,
        strategy: LoadBalanceStrategy,
        failure_threshold: u32,
    ) -> Self {
        let backends = providers
            .into_iter()
            .map(|p| FailoverBackend::new(p, failure_threshold))
            .collect();
        Self {
            name,
            backends,
            strategy,
            next_index: AtomicU32::new(0),
            recovery_secs: 60,
        }
    }

    /// Attempt to recover any unhealthy backends that have been down long enough.
    async fn try_recover_backends(&self) {
        for backend in &self.backends {
            backend.maybe_recover(self.recovery_secs).await;
        }
    }

    /// Pick the starting backend index based on the load-balance strategy.
    fn pick_index(&self) -> usize {
        let len = self.backends.len();
        match self.strategy {
            LoadBalanceStrategy::RoundRobin => {
                let idx = self.next_index.fetch_add(1, Ordering::Relaxed);
                idx as usize % len
            }
            LoadBalanceStrategy::Random => {
                // Simple pseudo-random using timestamp nanos
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as usize;
                t % len
            }
            LoadBalanceStrategy::LeastFailures => {
                let mut min_failures = u32::MAX;
                let mut min_idx = 0;
                for (i, b) in self.backends.iter().enumerate() {
                    let f = b.consecutive_failures.load(Ordering::Relaxed);
                    if f < min_failures {
                        min_failures = f;
                        min_idx = i;
                    }
                }
                min_idx
            }
        }
    }

    /// Whether an error is retryable (connection-level, not content-level).
    fn is_retryable(err: &GatewayError) -> bool {
        matches!(
            err,
            GatewayError::NetworkError(_)
                | GatewayError::UpstreamAuthError
                | GatewayError::UpstreamRateLimited
        )
    }
}

impl DynAiProvider for FailoverProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn chat_completion_boxed(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ChatCompletionResponse, GatewayError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.try_recover_backends().await;

            let len = self.backends.len();
            let start = self.pick_index();
            let mut last_err = GatewayError::ProviderError("No backends available".into());

            for attempt in 0..len {
                let idx = (start + attempt) % len;
                let backend = &self.backends[idx];

                if !backend.is_healthy() {
                    continue;
                }

                match backend
                    .provider
                    .chat_completion_boxed(request.clone())
                    .await
                {
                    Ok(resp) => {
                        backend.record_success();
                        return Ok(resp);
                    }
                    Err(e) => {
                        let retryable = Self::is_retryable(&e);
                        tracing::warn!(
                            backend = backend.provider.name(),
                            attempt,
                            retryable,
                            "Backend failed: {e}"
                        );
                        backend.record_failure().await;
                        last_err = e;
                        if !retryable {
                            return Err(last_err);
                        }
                    }
                }
            }

            Err(last_err)
        })
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        // For streaming we can only retry before the stream starts producing data.
        // We try each healthy backend in order until one returns a stream successfully.
        let len = self.backends.len();
        let start = self.pick_index();

        // Find the first healthy backend (we can't do async recovery in a sync fn,
        // so we just use current health state).
        let mut chosen_idx = None;
        for attempt in 0..len {
            let idx = (start + attempt) % len;
            if self.backends[idx].is_healthy() {
                chosen_idx = Some(idx);
                break;
            }
        }

        match chosen_idx {
            Some(idx) => self.backends[idx].provider.stream_chat_completion(request),
            None => {
                // All backends unhealthy — return an error stream
                Box::pin(futures::stream::once(async {
                    Err(GatewayError::ProviderError(
                        "All failover backends are unhealthy".into(),
                    ))
                }))
            }
        }
    }
}
