//! Per-MCP-server circuit breaker.
//!
//! Dead-simple: one MCP server = one breaker. There is no failover
//! pool because each MCP server is unique (a different tool surface), so
//! when its CB trips we just fail fast on subsequent calls until the
//! recovery window elapses.
//!
//! The state machine is thinkwatch-core's `tw-breaker` — the one the AI
//! gateway's route health and the desktop gateway also run. Each
//! transition happens under one lock, so two concurrent failures cannot
//! both trip the breaker.
//!
//! Every state transition is mirrored into the global `cb_registry` in
//! `think-watch-common`, which the dashboard handler in the server crate
//! reads to render real upstream-health on the UI.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tw_breaker::{Breakers, Policy, State, Trip};
use uuid::Uuid;

use think_watch_common::cb_registry::record_cb_with_kind;

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

impl CircuitConfig {
    fn policy(&self) -> Policy {
        Policy {
            trip: Trip::Consecutive(self.failure_threshold),
            cooldown: Duration::from_secs(self.recovery_secs),
            probes: self.half_open_max,
        }
    }
}

#[derive(Debug)]
pub struct CircuitOpen;

impl std::fmt::Display for CircuitOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "circuit open")
    }
}

impl std::error::Error for CircuitOpen {}

/// Keyed by `server_id` so a rename, or a second server that happens to
/// share a name, does not inherit the other's state. The display name is
/// passed in on every call rather than stored, so a rename shows up in
/// the dashboard on the next state change.
#[derive(Clone)]
pub struct McpCircuitBreakers {
    breakers: Arc<Breakers<Uuid>>,
    /// Servers already announced to the dashboard as closed.
    known: Arc<Mutex<HashSet<Uuid>>>,
}

impl Default for McpCircuitBreakers {
    fn default() -> Self {
        Self::new()
    }
}

impl McpCircuitBreakers {
    pub fn new() -> Self {
        Self::with_config(CircuitConfig::default())
    }

    pub fn with_config(config: CircuitConfig) -> Self {
        Self {
            breakers: Arc::new(Breakers::new(config.policy())),
            known: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The first time a server is seen, the dashboard learns it as closed.
    fn announce(&self, server_id: Uuid, display_name: &str) {
        let mut known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        if known.insert(server_id) {
            record_cb_with_kind(display_name, State::Closed, "mcp");
        }
    }

    fn report(&self, display_name: &str, change: Option<State>) {
        let Some(s) = change else { return };
        record_cb_with_kind(display_name, s, "mcp");
        match s {
            State::Open => tracing::warn!(server = %display_name, "MCP circuit breaker OPEN"),
            State::HalfOpen => tracing::info!(
                server = %display_name,
                "MCP circuit breaker HALF-OPEN (probing recovery)"
            ),
            State::Closed => {
                tracing::info!(server = %display_name, "MCP circuit breaker CLOSED (recovered)")
            }
        }
    }

    /// May a call go through? Once the recovery window has elapsed, the
    /// breaker turns half-open here and the call is a probe.
    pub fn check(&self, server_id: Uuid, display_name: &str) -> Result<(), CircuitOpen> {
        self.announce(server_id, display_name);
        let (admitted, change) = self.breakers.admit(&server_id);
        self.report(display_name, change);
        if admitted { Ok(()) } else { Err(CircuitOpen) }
    }

    pub fn record_success(&self, server_id: Uuid, display_name: &str) {
        self.announce(server_id, display_name);
        let change = self.breakers.record(&server_id, true);
        self.report(display_name, change);
    }

    pub fn record_failure(&self, server_id: Uuid, display_name: &str) {
        self.announce(server_id, display_name);
        let change = self.breakers.record(&server_id, false);
        self.report(display_name, change);
    }

    /// Show a newly added server on the dashboard before its first call.
    pub fn register(&self, server_id: Uuid, display_name: &str) {
        self.announce(server_id, display_name);
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
            cb.record_failure(id, "srv-a");
        }
        assert!(cb.check(id, "srv-a").is_err());
    }

    #[tokio::test]
    async fn closed_servers_pass_through() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        assert!(cb.check(id, "srv-a").is_ok());
        cb.record_success(id, "srv-a");
        assert!(cb.check(id, "srv-a").is_ok());
    }

    #[tokio::test]
    async fn half_open_recovers_after_successes() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        for _ in 0..3 {
            cb.record_failure(id, "srv-b");
        }
        assert!(cb.check(id, "srv-b").is_err());

        // Wait past the recovery window then probe.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(cb.check(id, "srv-b").is_ok()); // transitions to HalfOpen

        cb.record_success(id, "srv-b");
        cb.record_success(id, "srv-b"); // half_open_max = 2
        // Should now be Closed again.
        assert!(cb.check(id, "srv-b").is_ok());
    }

    #[tokio::test]
    async fn half_open_failure_reopens() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        for _ in 0..3 {
            cb.record_failure(id, "srv-c");
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(cb.check(id, "srv-c").is_ok()); // HalfOpen
        cb.record_failure(id, "srv-c"); // probe fails
        assert!(cb.check(id, "srv-c").is_err()); // back to Open
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
            tokio::spawn(async move { cb1.record_failure(id, "srv-d") }),
            tokio::spawn(async move { cb2.record_failure(id, "srv-d") }),
            tokio::spawn(async move { cb3.record_failure(id, "srv-d") }),
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        // Threshold = 3 → all three failures together must trip Open exactly once.
        assert!(cb.check(id, "srv-d").is_err());
    }

    /// Concurrent half-open probes must not all be allowed through at once
    /// — only the first transition wins, the rest see HalfOpen and pass too
    /// (which is fine for the probe semantics).
    #[tokio::test]
    async fn concurrent_open_to_halfopen_one_winner() {
        let cb = McpCircuitBreakers::with_config(cfg());
        let id = Uuid::new_v4();
        for _ in 0..3 {
            cb.record_failure(id, "srv-e");
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        // Three concurrent checks — all should succeed (HalfOpen lets
        // multiple probes through up to half_open_max).
        let cb1 = cb.clone();
        let cb2 = cb.clone();
        let cb3 = cb.clone();
        let r = tokio::join!(
            tokio::spawn(async move { cb1.check(id, "srv-e").is_ok() }),
            tokio::spawn(async move { cb2.check(id, "srv-e").is_ok() }),
            tokio::spawn(async move { cb3.check(id, "srv-e").is_ok() }),
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
            cb.record_failure(id_a, "github");
        }
        assert!(cb.check(id_a, "github").is_err());
        // B has the same display name but a different ID — must remain Closed.
        assert!(cb.check(id_b, "github").is_ok());
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
        cb.register(id, &old_name);
        assert!(snapshot_cb_states().contains_key(&old_name));

        // Admin renames the server. Subsequent state changes pass the
        // new name; the registry must learn it. If the breaker had
        // cached the name at construction time, every event would
        // continue to emit under the old key and `new_name` would
        // never appear.
        for _ in 0..3 {
            cb.record_failure(id, &new_name);
        }
        let snap = snapshot_cb_states();
        assert!(
            snap.contains_key(&new_name),
            "rename did not propagate to cb_registry; keys = {:?}",
            snap.keys().collect::<Vec<_>>()
        );
    }
}
