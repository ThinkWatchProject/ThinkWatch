use crate::providers::DynAiProvider;
use crate::providers::traits::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, GatewayError,
};
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use think_watch_common::cb_registry::{CbState, record_cb_with_kind};

// ---------------------------------------------------------------------------
// Circuit-breaker state machine
//
// One breaker per upstream provider, three states:
//
//                 success
//      ┌────────────────────────────┐
//      │                            ▼
//   ┌──────────┐  N consec failures  ┌──────────┐
//   │  Closed  │ ──────────────────► │   Open   │
//   └──────────┘                     └──────────┘
//        ▲                                │
//        │ M consecutive successes        │ recovery_secs elapsed
//        │                                ▼
//        │                          ┌──────────┐
//        └─────────────────────────  │ HalfOpen │
//             any failure ───────►   └──────────┘
//
// Closed:   normal traffic. record_failure increments the failure
//           counter; after `failure_threshold` consecutive failures
//           the breaker trips to Open and `last_failure` is captured.
//
// Open:     all calls short-circuit (no upstream connection); after
//           `recovery_secs` since `last_failure` the next caller
//           transitions us to HalfOpen for a probe.
//
// HalfOpen: a small budget of probe requests is allowed through. M
//           consecutive successes (`half_open_max`) close the
//           breaker; any failure trips back to Open.
//
// All state transitions execute under one mutex (`BreakerInner`) so
// two concurrent failures can't both observe Closed and both trip the
// counter independently. Edge changes propagate to the global
// `cb_registry` so the dashboard reflects them in real time.
// ---------------------------------------------------------------------------

/// Load-balancing strategy for selecting a backend.
#[derive(Debug, Clone, Copy)]
pub enum LoadBalanceStrategy {
    RoundRobin,
    Random,
    LeastFailures,
}

/// Mutable inner state of a single breaker. Always accessed under the
/// outer `Mutex`, never split across multiple locks. This mirrors the
/// MCP gateway's `BreakerInner` design that fixes the race condition
/// where concurrent failures could both observe `Closed`, both bump
/// the counter, and both trip Open separately.
struct BreakerInner {
    state: CbState,
    consecutive_failures: u32,
    half_open_successes: u32,
    last_failure: Option<Instant>,
}

/// A single backend in the failover pool with circuit breaker logic.
///
/// All breaker state lives behind a single `Mutex<BreakerInner>` so
/// every state transition is atomic.
struct FailoverBackend {
    provider: Arc<dyn DynAiProvider>,
    inner: Mutex<BreakerInner>,
    failure_threshold: u32,
    half_open_max: u32,
}

impl FailoverBackend {
    fn new(provider: Arc<dyn DynAiProvider>, failure_threshold: u32) -> Self {
        // Seed the global CB registry so new providers show up as `Closed`
        // before they have served their first request.
        record_cb_with_kind(provider.name(), CbState::Closed, "ai");
        Self {
            provider,
            inner: Mutex::new(BreakerInner {
                state: CbState::Closed,
                consecutive_failures: 0,
                half_open_successes: 0,
                last_failure: None,
            }),
            failure_threshold,
            half_open_max: 3,
        }
    }

    fn is_healthy_fast(&self) -> bool {
        // Quick non-blocking check via try_lock for the hot path.
        // If the lock is contended, assume healthy and let the full
        // check decide.
        self.inner
            .try_lock()
            .map(|inner| inner.consecutive_failures < self.failure_threshold)
            .unwrap_or(true)
    }

    async fn record_success(&self) {
        let mut inner = self.inner.lock().await;
        inner.consecutive_failures = 0;

        match inner.state {
            CbState::HalfOpen => {
                inner.half_open_successes += 1;
                if inner.half_open_successes >= self.half_open_max {
                    inner.state = CbState::Closed;
                    inner.half_open_successes = 0;
                    metrics::gauge!("circuit_breaker_state", "provider" => crate::metrics_labels::normalize_provider_label(self.provider.name())).set(0.0);
                    record_cb_with_kind(self.provider.name(), CbState::Closed, "ai");
                    tracing::info!(
                        provider = self.provider.name(),
                        "Circuit breaker closed (recovered)"
                    );
                }
            }
            CbState::Open => {
                // Should not happen, but handle gracefully
                inner.state = CbState::Closed;
                metrics::gauge!("circuit_breaker_state", "provider" => crate::metrics_labels::normalize_provider_label(self.provider.name())).set(0.0);
                record_cb_with_kind(self.provider.name(), CbState::Closed, "ai");
            }
            CbState::Closed => {}
        }
    }

    async fn record_failure(&self) {
        let mut inner = self.inner.lock().await;
        inner.consecutive_failures += 1;

        match inner.state {
            CbState::Closed => {
                if inner.consecutive_failures >= self.failure_threshold {
                    inner.state = CbState::Open;
                    inner.last_failure = Some(Instant::now());
                    let failures = inner.consecutive_failures;
                    metrics::gauge!("circuit_breaker_state", "provider" => crate::metrics_labels::normalize_provider_label(self.provider.name())).set(2.0);
                    record_cb_with_kind(self.provider.name(), CbState::Open, "ai");
                    tracing::warn!(
                        provider = self.provider.name(),
                        "Circuit breaker OPEN after {failures} consecutive failures",
                    );
                }
            }
            CbState::HalfOpen => {
                // Probe failed — go back to Open
                inner.state = CbState::Open;
                inner.last_failure = Some(Instant::now());
                inner.half_open_successes = 0;
                metrics::gauge!("circuit_breaker_state", "provider" => crate::metrics_labels::normalize_provider_label(self.provider.name())).set(2.0);
                record_cb_with_kind(self.provider.name(), CbState::Open, "ai");
                tracing::warn!(
                    provider = self.provider.name(),
                    "Circuit breaker back to OPEN (half-open probe failed)"
                );
            }
            CbState::Open => {}
        }
    }

    /// Check whether enough time has passed to try recovering (transition Open → HalfOpen).
    async fn maybe_recover(&self, recovery_secs: u64) {
        let mut inner = self.inner.lock().await;
        if inner.state != CbState::Open {
            return;
        }
        let elapsed_ok = inner
            .last_failure
            .map(|t| t.elapsed() >= Duration::from_secs(recovery_secs))
            .unwrap_or(false);
        if elapsed_ok {
            inner.state = CbState::HalfOpen;
            inner.half_open_successes = 0;
            inner.consecutive_failures = 0;
            metrics::gauge!("circuit_breaker_state", "provider" => crate::metrics_labels::normalize_provider_label(self.provider.name())).set(1.0);
            record_cb_with_kind(self.provider.name(), CbState::HalfOpen, "ai");
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
                    let f = b
                        .inner
                        .try_lock()
                        .map(|inner| inner.consecutive_failures)
                        .unwrap_or(0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::*;
    use futures::Stream;

    struct DummyProvider {
        name: &'static str,
    }

    impl AiProvider for DummyProvider {
        fn name(&self) -> &str {
            self.name
        }

        async fn chat_completion(
            &self,
            _request: ChatCompletionRequest,
        ) -> Result<ChatCompletionResponse, GatewayError> {
            Err(GatewayError::ProviderError("dummy".into()))
        }

        fn stream_chat_completion(
            &self,
            _request: ChatCompletionRequest,
        ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
            Box::pin(futures::stream::empty())
        }
    }

    fn backend(failure_threshold: u32) -> FailoverBackend {
        let provider: Arc<dyn DynAiProvider> = Arc::new(DummyProvider { name: "dummy" });
        FailoverBackend::new(provider, failure_threshold)
    }

    #[tokio::test]
    async fn initial_state_is_closed() {
        let b = backend(3);
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::Closed);
        assert_eq!(inner.consecutive_failures, 0);
        assert_eq!(inner.half_open_successes, 0);
    }

    #[tokio::test]
    async fn closed_stays_closed_below_threshold() {
        let b = backend(3);
        b.record_failure().await;
        b.record_failure().await;
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::Closed);
        assert_eq!(inner.consecutive_failures, 2);
    }

    #[tokio::test]
    async fn n_consecutive_failures_trip_open() {
        let b = backend(3);
        for _ in 0..3 {
            b.record_failure().await;
        }
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::Open);
        assert!(inner.last_failure.is_some());
    }

    #[tokio::test]
    async fn success_resets_failure_counter_while_closed() {
        let b = backend(3);
        b.record_failure().await;
        b.record_failure().await;
        b.record_success().await;
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::Closed);
        assert_eq!(inner.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn maybe_recover_is_noop_when_closed() {
        let b = backend(3);
        b.maybe_recover(0).await;
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::Closed);
    }

    #[tokio::test]
    async fn maybe_recover_holds_open_before_recovery_window() {
        let b = backend(3);
        for _ in 0..3 {
            b.record_failure().await;
        }
        // 3600s window means we should NOT transition for a long time.
        b.maybe_recover(3600).await;
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::Open);
    }

    #[tokio::test]
    async fn maybe_recover_transitions_open_to_half_open_after_window() {
        let b = backend(3);
        for _ in 0..3 {
            b.record_failure().await;
        }
        // Force last_failure into the past so the elapsed check passes
        // without needing a real wall-clock sleep.
        {
            let mut inner = b.inner.lock().await;
            inner.last_failure = Some(Instant::now() - Duration::from_secs(120));
        }
        b.maybe_recover(60).await;
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::HalfOpen);
        // half_open_successes and consecutive_failures both zeroed on entry.
        assert_eq!(inner.half_open_successes, 0);
        assert_eq!(inner.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn half_open_failure_trips_back_to_open() {
        let b = backend(3);
        // Drop into HalfOpen directly.
        {
            let mut inner = b.inner.lock().await;
            inner.state = CbState::HalfOpen;
            inner.half_open_successes = 2;
        }
        b.record_failure().await;
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::Open);
        // Half-open success budget must reset so the next probe cycle
        // starts from zero rather than carrying credit through Open.
        assert_eq!(inner.half_open_successes, 0);
        assert!(inner.last_failure.is_some());
    }

    #[tokio::test]
    async fn half_open_closes_after_m_consecutive_successes() {
        let b = backend(3);
        // Enter HalfOpen.
        {
            let mut inner = b.inner.lock().await;
            inner.state = CbState::HalfOpen;
        }
        // half_open_max defaults to 3 in FailoverBackend::new.
        for _ in 0..3 {
            b.record_success().await;
        }
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::Closed);
        assert_eq!(inner.half_open_successes, 0);
    }

    #[tokio::test]
    async fn record_success_in_open_state_recovers_gracefully() {
        // Documented as "should not happen, but handle gracefully".
        // Locks in the contract that an out-of-band success doesn't leave
        // the breaker stuck Open forever.
        let b = backend(3);
        {
            let mut inner = b.inner.lock().await;
            inner.state = CbState::Open;
        }
        b.record_success().await;
        let inner = b.inner.lock().await;
        assert_eq!(inner.state, CbState::Closed);
    }

    #[tokio::test]
    async fn is_healthy_fast_reflects_failure_count() {
        let b = backend(3);
        assert!(b.is_healthy_fast());
        b.record_failure().await;
        b.record_failure().await;
        // Still under threshold.
        assert!(b.is_healthy_fast());
        b.record_failure().await;
        assert!(!b.is_healthy_fast());
    }

    #[test]
    fn is_retryable_only_for_transport_errors() {
        assert!(FailoverProvider::is_retryable(&GatewayError::NetworkError(
            "x".into()
        )));
        assert!(FailoverProvider::is_retryable(
            &GatewayError::UpstreamAuthError
        ));
        assert!(FailoverProvider::is_retryable(
            &GatewayError::UpstreamRateLimited
        ));
        // ProviderError represents a content-level failure (model-specific),
        // not transport-level — retrying the same upstream would just
        // reproduce the same answer.
        assert!(!FailoverProvider::is_retryable(
            &GatewayError::ProviderError("x".into())
        ));
    }
}
