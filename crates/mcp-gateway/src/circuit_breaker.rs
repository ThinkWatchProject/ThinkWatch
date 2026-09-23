//! Per-MCP-server circuit breaker.
//!
//! Dead-simple: one MCP server = one breaker. There is no failover
//! pool because each MCP server is unique (a different tool surface), so
//! when its CB trips we just fail fast on subsequent calls until the
//! recovery window elapses.
//!
//! All breaker state lives behind a single `Mutex<BreakerInner>`. The
//! previous design split state across an `RwLock<CbState>`, two
//! `AtomicU32`s, and an `RwLock<Option<Instant>>`, which made the
//! check / record_failure / record_success transitions racy: two
//! concurrent failures could both observe `Closed`, both bump the
//! counter, and both trip Open separately (writing `last_failure`
//! twice). Holding one mutex for the entire transition makes every
//! state change atomic.
//!
//! Every state transition is mirrored into the global `cb_registry` in
//! `think-watch-common`, which the dashboard handler in the server crate
//! reads to render real upstream-health on the UI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use think_watch_common::cb_registry::{CbState, record_cb_with_kind};

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

/// Mutable inner state of a single breaker. Always accessed under the
/// outer `Mutex`, never split across multiple locks.
#[derive(Debug)]
struct BreakerInner {
    state: CbState,
    consecutive_failures: u32,
    half_open_successes: u32,
    last_failure: Option<Instant>,
}

/// One circuit breaker, scoped to a single MCP server.
///
/// Keyed by `server_id` (UUID) at the registry level so a rename or
/// a second server that happens to share a name doesn't inherit the
/// other's open/closed state. The display name is passed in on every
/// call rather than stored on the breaker, so a rename takes effect
/// in dashboard / log output immediately — caching the name at
/// breaker construction would freeze the old label until process
/// restart.
struct Breaker {
    #[allow(dead_code)]
    server_id: Uuid,
    config: CircuitConfig,
    inner: Mutex<BreakerInner>,
}

impl Breaker {
    fn new(server_id: Uuid, display_name: &str, config: CircuitConfig) -> Self {
        record_cb_with_kind(display_name, CbState::Closed, "mcp");
        Self {
            server_id,
            config,
            inner: Mutex::new(BreakerInner {
                state: CbState::Closed,
                consecutive_failures: 0,
                half_open_successes: 0,
                last_failure: None,
            }),
        }
    }

    /// Decide whether a new request is allowed through. Side effect: if
    /// the breaker is `Open` and the recovery window has elapsed, this
    /// transitions it to `HalfOpen` so the caller's request acts as a
    /// probe. The whole check-then-transition runs under one mutex so
    /// concurrent callers can't both "win" the half-open promotion.
    async fn check(&self, display_name: &str) -> Result<(), CircuitOpen> {
        let mut inner = self.inner.lock().await;
        match inner.state {
            CbState::Closed | CbState::HalfOpen => Ok(()),
            CbState::Open => {
                let elapsed_ok = inner
                    .last_failure
                    .map(|t| t.elapsed() >= Duration::from_secs(self.config.recovery_secs))
                    .unwrap_or(false);
                if elapsed_ok {
                    inner.state = CbState::HalfOpen;
                    inner.half_open_successes = 0;
                    inner.consecutive_failures = 0;
                    record_cb_with_kind(display_name, CbState::HalfOpen, "mcp");
                    tracing::info!(
                        server = %display_name,
                        "MCP circuit breaker HALF-OPEN (probing recovery)"
                    );
                    Ok(())
                } else {
                    Err(CircuitOpen)
                }
            }
        }
    }

    async fn record_success(&self, display_name: &str) {
        let mut inner = self.inner.lock().await;
        inner.consecutive_failures = 0;
        match inner.state {
            CbState::HalfOpen => {
                inner.half_open_successes += 1;
                if inner.half_open_successes >= self.config.half_open_max {
                    inner.state = CbState::Closed;
                    inner.half_open_successes = 0;
                    record_cb_with_kind(display_name, CbState::Closed, "mcp");
                    tracing::info!(
                        server = %display_name,
                        "MCP circuit breaker CLOSED (recovered)"
                    );
                }
            }
            CbState::Open => {
                // Shouldn't happen — `check` would have rejected — but if
                // a stale request lands, recover gracefully.
                inner.state = CbState::Closed;
                record_cb_with_kind(display_name, CbState::Closed, "mcp");
            }
            CbState::Closed => {}
        }
    }

    /// Record a failure. Transitions the breaker to Open on either
    /// Closed-past-threshold or HalfOpen-probe-failed. The
    /// `record_cb_with_kind` side-effect fires the global OPEN_LISTENER,
    /// which is where the `provider.circuit_open` audit event gets
    /// emitted — no return-value threading required.
    async fn record_failure(&self, display_name: &str) {
        let mut inner = self.inner.lock().await;
        inner.consecutive_failures += 1;
        match inner.state {
            CbState::Closed => {
                if inner.consecutive_failures >= self.config.failure_threshold {
                    inner.state = CbState::Open;
                    inner.last_failure = Some(Instant::now());
                    let failures = inner.consecutive_failures;
                    record_cb_with_kind(display_name, CbState::Open, "mcp");
                    tracing::warn!(
                        server = %display_name,
                        failures,
                        "MCP circuit breaker OPEN"
                    );
                }
            }
            CbState::HalfOpen => {
                // Probe failed → go back to Open and restart the timer.
                inner.state = CbState::Open;
                inner.last_failure = Some(Instant::now());
                inner.half_open_successes = 0;
                record_cb_with_kind(display_name, CbState::Open, "mcp");
                tracing::warn!(
                    server = %display_name,
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

/// Per-process registry of one circuit breaker per MCP server, keyed
/// by the server's stable UUID (NOT name).
///
/// Renaming a server, or accidentally registering two servers with the
/// same name, used to share or inherit breaker state because the map
/// was string-keyed. Concretely: server `srv` flaps, breaker trips
/// OPEN; admin deletes `srv` and creates a fresh, healthy `srv` →
/// new server immediately rejects every call until the recovery
/// window elapses. UUID keys eliminate both failure modes.
#[derive(Clone, Default)]
pub struct McpCircuitBreakers {
    inner: Arc<RwLock<HashMap<Uuid, Arc<Breaker>>>>,
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

    /// Get the breaker for `server_id`, creating it on first touch.
    /// `display_name` is used only at creation time for the initial
    /// `record_cb_with_kind` event; subsequent state-change events
    /// pick up whatever name the current caller is using, so a server
    /// rename takes effect on the very next breaker event.
    async fn breaker_for(&self, server_id: Uuid, display_name: &str) -> Arc<Breaker> {
        if let Some(b) = self.inner.read().await.get(&server_id) {
            return Arc::clone(b);
        }
        let mut w = self.inner.write().await;
        if let Some(b) = w.get(&server_id) {
            return Arc::clone(b);
        }
        let b = Arc::new(Breaker::new(server_id, display_name, self.config));
        w.insert(server_id, Arc::clone(&b));
        b
    }

    /// Returns `Ok(())` if the server can be called. Returns `Err(CircuitOpen)`
    /// if the breaker is currently rejecting requests. `display_name`
    /// is used for log lines and the dashboard cb_registry on every
    /// state-change event — pass the current name so a rename is
    /// reflected immediately.
    pub async fn check(&self, server_id: Uuid, display_name: &str) -> Result<(), CircuitOpen> {
        self.breaker_for(server_id, display_name)
            .await
            .check(display_name)
            .await
    }

    pub async fn record_success(&self, server_id: Uuid, display_name: &str) {
        self.breaker_for(server_id, display_name)
            .await
            .record_success(display_name)
            .await;
    }

    pub async fn record_failure(&self, server_id: Uuid, display_name: &str) {
        self.breaker_for(server_id, display_name)
            .await
            .record_failure(display_name)
            .await;
    }

    /// Pre-register a server so it shows up in the dashboard CB snapshot
    /// even before its first call.
    pub async fn register(&self, server_id: Uuid, display_name: &str) {
        let _ = self.breaker_for(server_id, display_name).await;
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
        let id = Uuid::new_v4();
        for _ in 0..3 {
            cb.record_failure(id, "srv-a").await;
        }
        assert!(cb.check(id, "srv-a").await.is_err());
    }

    #[tokio::test]
    async fn closed_servers_pass_through() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        assert!(cb.check(id, "srv-a").await.is_ok());
        cb.record_success(id, "srv-a").await;
        assert!(cb.check(id, "srv-a").await.is_ok());
    }

    #[tokio::test]
    async fn half_open_recovers_after_successes() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        for _ in 0..3 {
            cb.record_failure(id, "srv-b").await;
        }
        assert!(cb.check(id, "srv-b").await.is_err());

        // Wait past the recovery window then probe.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(cb.check(id, "srv-b").await.is_ok()); // transitions to HalfOpen

        cb.record_success(id, "srv-b").await;
        cb.record_success(id, "srv-b").await; // half_open_max = 2
        // Should now be Closed again.
        assert!(cb.check(id, "srv-b").await.is_ok());
    }

    #[tokio::test]
    async fn half_open_failure_reopens() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        for _ in 0..3 {
            cb.record_failure(id, "srv-c").await;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(cb.check(id, "srv-c").await.is_ok()); // HalfOpen
        cb.record_failure(id, "srv-c").await; // probe fails
        assert!(cb.check(id, "srv-c").await.is_err()); // back to Open
    }

    /// Concurrent failures must not bump the breaker past Open multiple
    /// times — everything happens under one mutex.
    #[tokio::test]
    async fn concurrent_failures_serialize() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        let cb1 = cb.clone();
        let cb2 = cb.clone();
        let cb3 = cb.clone();
        let (a, b, c) = tokio::join!(
            tokio::spawn(async move { cb1.record_failure(id, "srv-d").await }),
            tokio::spawn(async move { cb2.record_failure(id, "srv-d").await }),
            tokio::spawn(async move { cb3.record_failure(id, "srv-d").await }),
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        // Threshold = 3 → all three failures together must trip Open exactly once.
        assert!(cb.check(id, "srv-d").await.is_err());
    }

    /// Concurrent half-open probes must not all be allowed through at once
    /// — only the first transition wins, the rest see HalfOpen and pass too
    /// (which is fine for the probe semantics).
    #[tokio::test]
    async fn concurrent_open_to_halfopen_one_winner() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        for _ in 0..3 {
            cb.record_failure(id, "srv-e").await;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        // Three concurrent checks — all should succeed (HalfOpen lets
        // multiple probes through up to half_open_max).
        let cb1 = cb.clone();
        let cb2 = cb.clone();
        let cb3 = cb.clone();
        let r = tokio::join!(
            tokio::spawn(async move { cb1.check(id, "srv-e").await.is_ok() }),
            tokio::spawn(async move { cb2.check(id, "srv-e").await.is_ok() }),
            tokio::spawn(async move { cb3.check(id, "srv-e").await.is_ok() }),
        );
        // All three should be permitted as HalfOpen probes.
        assert!(r.0.unwrap() && r.1.unwrap() && r.2.unwrap());
    }

    /// The bug this fix targets: two servers sharing a name must not
    /// share breaker state. Servers A and B both display "github" but
    /// have different UUIDs — flapping A must not open B's breaker.
    #[tokio::test]
    async fn same_name_distinct_ids_are_isolated() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        for _ in 0..3 {
            cb.record_failure(id_a, "github").await;
        }
        assert!(cb.check(id_a, "github").await.is_err());
        // B has the same display name but a different ID — must remain Closed.
        assert!(cb.check(id_b, "github").await.is_ok());
    }

    /// Rename: same UUID, new display name. The breaker is keyed by
    /// UUID so it reuses the existing entry, but the dashboard /
    /// cb_registry events emitted by subsequent state changes must
    /// pick up the NEW name — earlier implementations froze the name
    /// at first-touch and never updated it.
    #[tokio::test]
    async fn rename_takes_effect_on_next_state_change() {
        use think_watch_common::cb_registry::snapshot_cb_states;

        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        // Use uuid-derived names so we don't crosstalk with the
        // process-global cb_registry that other tests share.
        let old_name = format!("rename-old-{}", id.simple());
        let new_name = format!("rename-new-{}", id.simple());

        // First touch registers under the OLD name.
        cb.register(id, &old_name).await;
        assert!(snapshot_cb_states().contains_key(&old_name));

        // Admin renames the server. Subsequent state changes pass the
        // new name; the registry must learn it. If the breaker had
        // cached the name at construction time, every event would
        // continue to emit under the old key and `new_name` would
        // never appear.
        for _ in 0..3 {
            cb.record_failure(id, &new_name).await;
        }
        let snap = snapshot_cb_states();
        assert!(
            snap.contains_key(&new_name),
            "rename did not propagate to cb_registry; keys = {:?}",
            snap.keys().collect::<Vec<_>>()
        );
    }
}
