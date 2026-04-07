//! Per-MCP-server circuit breaker.
//!
//! Mirrors the design of `think_watch_gateway::failover::FailoverBackend`
//! but is dead-simple: one MCP server = one breaker. There is no failover
//! pool because each MCP server is unique (a different tool surface), so
//! when its CB trips we just fail fast on subsequent calls until the
//! recovery window elapses.
//!
//! Every state transition is mirrored into the global `cb_registry` in
//! `think-watch-common`, which the dashboard handler in the server crate
//! reads to render real upstream-health on the UI.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use think_watch_common::cb_registry::{CbState, record_cb};

/// Tunables for a single circuit breaker.
#[derive(Debug, Clone, Copy)]
pub struct CircuitConfig {
    /// Consecutive failures before tripping Closed → Open.
    pub failure_threshold: u32,
    /// Seconds to stay Open before transitioning to HalfOpen for probing.
    pub recovery_secs: u64,
    /// Successful probes required in HalfOpen before going back to Closed.
    pub half_open_max: u32,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            recovery_secs: 60,
            half_open_max: 3,
        }
    }
}

/// One circuit breaker, scoped to a single MCP server (keyed by name).
struct Breaker {
    server_name: String,
    config: CircuitConfig,
    state: RwLock<CbState>,
    consecutive_failures: AtomicU32,
    half_open_successes: AtomicU32,
    last_failure: RwLock<Option<Instant>>,
}

impl Breaker {
    fn new(server_name: String, config: CircuitConfig) -> Self {
        record_cb(&server_name, CbState::Closed);
        Self {
            server_name,
            config,
            state: RwLock::new(CbState::Closed),
            consecutive_failures: AtomicU32::new(0),
            half_open_successes: AtomicU32::new(0),
            last_failure: RwLock::new(None),
        }
    }

    /// Decide whether a new request is allowed through. Side effect: if
    /// the breaker is `Open` and the recovery window has elapsed, this
    /// transitions it to `HalfOpen` so the caller's request acts as a
    /// probe.
    async fn check(&self) -> Result<(), CircuitOpen> {
        let state = *self.state.read().await;
        match state {
            CbState::Closed | CbState::HalfOpen => Ok(()),
            CbState::Open => {
                let last = *self.last_failure.read().await;
                let elapsed_ok = last
                    .map(|t| t.elapsed() >= Duration::from_secs(self.config.recovery_secs))
                    .unwrap_or(false);
                if elapsed_ok {
                    // Transition Open → HalfOpen and let this request probe.
                    *self.state.write().await = CbState::HalfOpen;
                    self.half_open_successes.store(0, Ordering::Relaxed);
                    self.consecutive_failures.store(0, Ordering::Relaxed);
                    record_cb(&self.server_name, CbState::HalfOpen);
                    tracing::info!(
                        server = %self.server_name,
                        "MCP circuit breaker HALF-OPEN (probing recovery)"
                    );
                    Ok(())
                } else {
                    Err(CircuitOpen)
                }
            }
        }
    }

    async fn record_success(&self) {
        let state = *self.state.read().await;
        self.consecutive_failures.store(0, Ordering::Relaxed);
        match state {
            CbState::HalfOpen => {
                let n = self.half_open_successes.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= self.config.half_open_max {
                    *self.state.write().await = CbState::Closed;
                    self.half_open_successes.store(0, Ordering::Relaxed);
                    record_cb(&self.server_name, CbState::Closed);
                    tracing::info!(
                        server = %self.server_name,
                        "MCP circuit breaker CLOSED (recovered)"
                    );
                }
            }
            CbState::Open => {
                // Shouldn't happen — `check` would have rejected — but if
                // a stale request lands, recover gracefully.
                *self.state.write().await = CbState::Closed;
                record_cb(&self.server_name, CbState::Closed);
            }
            CbState::Closed => {}
        }
    }

    async fn record_failure(&self) {
        let prev = self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        let state = *self.state.read().await;
        match state {
            CbState::Closed => {
                if prev + 1 >= self.config.failure_threshold {
                    *self.state.write().await = CbState::Open;
                    *self.last_failure.write().await = Some(Instant::now());
                    record_cb(&self.server_name, CbState::Open);
                    tracing::warn!(
                        server = %self.server_name,
                        failures = prev + 1,
                        "MCP circuit breaker OPEN"
                    );
                }
            }
            CbState::HalfOpen => {
                // Probe failed → go back to Open and restart the timer.
                *self.state.write().await = CbState::Open;
                *self.last_failure.write().await = Some(Instant::now());
                self.half_open_successes.store(0, Ordering::Relaxed);
                record_cb(&self.server_name, CbState::Open);
                tracing::warn!(
                    server = %self.server_name,
                    "MCP circuit breaker back to OPEN (probe failed)"
                );
            }
            CbState::Open => {}
        }
    }
}

/// Sentinel returned when a request is rejected because its server's
/// circuit is currently `Open`.
#[derive(Debug)]
pub struct CircuitOpen;

impl std::fmt::Display for CircuitOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "circuit open")
    }
}

impl std::error::Error for CircuitOpen {}

/// Per-process registry of one circuit breaker per MCP server, keyed by
/// the server's stable name. Cheap to clone; everything inside is `Arc`.
#[derive(Clone, Default)]
pub struct McpCircuitBreakers {
    inner: Arc<RwLock<HashMap<String, Arc<Breaker>>>>,
    config: CircuitConfig,
}

impl McpCircuitBreakers {
    pub fn new() -> Self {
        Self::with_config(CircuitConfig::default())
    }

    pub fn with_config(config: CircuitConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Get the breaker for `server_name`, creating it on first touch.
    async fn breaker_for(&self, server_name: &str) -> Arc<Breaker> {
        if let Some(b) = self.inner.read().await.get(server_name) {
            return Arc::clone(b);
        }
        let mut w = self.inner.write().await;
        if let Some(b) = w.get(server_name) {
            return Arc::clone(b);
        }
        let b = Arc::new(Breaker::new(server_name.to_string(), self.config));
        w.insert(server_name.to_string(), Arc::clone(&b));
        b
    }

    /// Returns `Ok(())` if the server can be called. Returns `Err(CircuitOpen)`
    /// if the breaker is currently rejecting requests.
    pub async fn check(&self, server_name: &str) -> Result<(), CircuitOpen> {
        self.breaker_for(server_name).await.check().await
    }

    pub async fn record_success(&self, server_name: &str) {
        self.breaker_for(server_name).await.record_success().await;
    }

    pub async fn record_failure(&self, server_name: &str) {
        self.breaker_for(server_name).await.record_failure().await;
    }

    /// Pre-register a server so it shows up in the dashboard CB snapshot
    /// even before its first call.
    pub async fn register(&self, server_name: &str) {
        let _ = self.breaker_for(server_name).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CircuitConfig {
        CircuitConfig {
            failure_threshold: 3,
            recovery_secs: 1,
            half_open_max: 2,
        }
    }

    #[tokio::test]
    async fn opens_after_threshold_failures() {
        let cb = McpCircuitBreakers::with_config(cfg());
        for _ in 0..3 {
            cb.record_failure("srv-a").await;
        }
        assert!(cb.check("srv-a").await.is_err());
    }

    #[tokio::test]
    async fn closed_servers_pass_through() {
        let cb = McpCircuitBreakers::with_config(cfg());
        assert!(cb.check("srv-a").await.is_ok());
        cb.record_success("srv-a").await;
        assert!(cb.check("srv-a").await.is_ok());
    }

    #[tokio::test]
    async fn half_open_recovers_after_successes() {
        let cb = McpCircuitBreakers::with_config(cfg());
        for _ in 0..3 {
            cb.record_failure("srv-b").await;
        }
        assert!(cb.check("srv-b").await.is_err());

        // Wait past the recovery window then probe.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(cb.check("srv-b").await.is_ok()); // transitions to HalfOpen

        cb.record_success("srv-b").await;
        cb.record_success("srv-b").await; // half_open_max = 2
        // Should now be Closed again.
        assert!(cb.check("srv-b").await.is_ok());
    }

    #[tokio::test]
    async fn half_open_failure_reopens() {
        let cb = McpCircuitBreakers::with_config(cfg());
        for _ in 0..3 {
            cb.record_failure("srv-c").await;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(cb.check("srv-c").await.is_ok()); // HalfOpen
        cb.record_failure("srv-c").await; // probe fails
        assert!(cb.check("srv-c").await.is_err()); // back to Open
    }
}
