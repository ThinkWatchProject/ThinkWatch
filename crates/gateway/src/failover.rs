use crate::providers::DynAiProvider;
use crate::providers::traits::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, GatewayError,
};
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

// Circuit-breaker state lives in `think-watch-common` so the MCP gateway
// can write into the same registry. Re-exported here for backwards compat
// with anything that already imported from this module.
pub use think_watch_common::cb_registry::{
    CbState, record_cb, record_cb_with_kind, snapshot_cb_states,
};

/// Load-balancing strategy for selecting a backend.
#[derive(Debug, Clone, Copy)]
pub enum LoadBalanceStrategy {
    RoundRobin,
    Random,
    LeastFailures,
}

/// Circuit breaker state for a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitState {
    /// Normal operation — requests flow through.
    Closed,
    /// Backend is failing — requests are rejected immediately.
    Open,
    /// Probing — allow limited requests to test recovery.
    HalfOpen,
}

/// A single backend in the failover pool with circuit breaker logic.
struct FailoverBackend {
    provider: Arc<dyn DynAiProvider>,
    state: RwLock<CircuitState>,
    consecutive_failures: AtomicU32,
    failure_threshold: u32,
    last_failure: RwLock<Option<Instant>>,
    half_open_successes: AtomicU32,
    half_open_max: u32,
}

impl FailoverBackend {
    fn new(provider: Arc<dyn DynAiProvider>, failure_threshold: u32) -> Self {
        // Seed the global CB registry so new providers show up as `Closed`
        // before they have served their first request.
        record_cb(provider.name(), CbState::Closed);
        Self {
            provider,
            state: RwLock::new(CircuitState::Closed),
            consecutive_failures: AtomicU32::new(0),
            failure_threshold,
            last_failure: RwLock::new(None),
            half_open_successes: AtomicU32::new(0),
            half_open_max: 3,
        }
    }

    fn is_healthy_fast(&self) -> bool {
        // Quick non-async check: if consecutive failures < threshold, likely healthy
        self.consecutive_failures.load(Ordering::Relaxed) < self.failure_threshold
    }

    async fn record_success(&self) {
        let state = *self.state.read().await;
        self.consecutive_failures.store(0, Ordering::Relaxed);

        match state {
            CircuitState::HalfOpen => {
                let successes = self.half_open_successes.fetch_add(1, Ordering::Relaxed) + 1;
                if successes >= self.half_open_max {
                    *self.state.write().await = CircuitState::Closed;
                    self.half_open_successes.store(0, Ordering::Relaxed);
                    metrics::gauge!("circuit_breaker_state", "provider" => self.provider.name().to_string()).set(0.0);
                    record_cb(self.provider.name(), CbState::Closed);
                    tracing::info!(
                        provider = self.provider.name(),
                        "Circuit breaker closed (recovered)"
                    );
                }
            }
            CircuitState::Open => {
                // Should not happen, but handle gracefully
                *self.state.write().await = CircuitState::Closed;
                metrics::gauge!("circuit_breaker_state", "provider" => self.provider.name().to_string()).set(0.0);
                record_cb(self.provider.name(), CbState::Closed);
            }
            CircuitState::Closed => {}
        }
    }

    async fn record_failure(&self) {
        let prev = self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        let state = *self.state.read().await;

        match state {
            CircuitState::Closed => {
                if prev + 1 >= self.failure_threshold {
                    *self.state.write().await = CircuitState::Open;
                    *self.last_failure.write().await = Some(Instant::now());
                    metrics::gauge!("circuit_breaker_state", "provider" => self.provider.name().to_string()).set(2.0);
                    record_cb_with_kind(self.provider.name(), CbState::Open, "ai");
                    tracing::warn!(
                        provider = self.provider.name(),
                        "Circuit breaker OPEN after {} consecutive failures",
                        prev + 1
                    );
                }
            }
            CircuitState::HalfOpen => {
                // Probe failed — go back to Open
                *self.state.write().await = CircuitState::Open;
                *self.last_failure.write().await = Some(Instant::now());
                self.half_open_successes.store(0, Ordering::Relaxed);
                metrics::gauge!("circuit_breaker_state", "provider" => self.provider.name().to_string()).set(2.0);
                record_cb_with_kind(self.provider.name(), CbState::Open, "ai");
                tracing::warn!(
                    provider = self.provider.name(),
                    "Circuit breaker back to OPEN (half-open probe failed)"
                );
            }
            CircuitState::Open => {}
        }
    }

    /// Check whether enough time has passed to try recovering (transition Open → HalfOpen).
    async fn maybe_recover(&self, recovery_secs: u64) {
        let state = *self.state.read().await;
        if state != CircuitState::Open {
            return;
        }
        let guard = self.last_failure.read().await;
        if let Some(ts) = *guard
            && ts.elapsed() >= Duration::from_secs(recovery_secs)
        {
            drop(guard);
            *self.state.write().await = CircuitState::HalfOpen;
            self.half_open_successes.store(0, Ordering::Relaxed);
            self.consecutive_failures.store(0, Ordering::Relaxed);
            metrics::gauge!("circuit_breaker_state", "provider" => self.provider.name().to_string()).set(1.0);
            record_cb(self.provider.name(), CbState::HalfOpen);
            tracing::info!(
                provider = self.provider.name(),
                "Circuit breaker HALF-OPEN (probing recovery)"
            );
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

                if !backend.is_healthy_fast() {
                    continue;
                }

                match backend
                    .provider
                    .chat_completion_boxed(request.clone())
                    .await
                {
                    Ok(resp) => {
                        backend.record_success().await;
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
            if self.backends[idx].is_healthy_fast() {
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
