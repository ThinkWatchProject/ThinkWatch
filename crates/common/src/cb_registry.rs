//! Process-wide circuit-breaker state registry.
//!
//! The MCP gateway (`think-watch-mcp-gateway`) writes into this map every
//! time one of its in-process breakers transitions, and the Open listener
//! turns an opening into an audit event. The dashboard handler reads a
//! snapshot to render MCP rows.
//!
//! AI routes are not here: their breakers live in Redis, shared by every
//! replica, and the dashboard reads them from there
//! (`HealthTracker::state`). A process-local map would show each replica
//! only its own view.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::RwLock;

/// A breaker's state: thinkwatch-core's, the state machine every breaker
/// here runs.
use tw_breaker::State;

/// How the dashboard spells a state (`Closed` / `HalfOpen` / `Open`).
pub fn label(state: State) -> &'static str {
    match state {
        State::Closed => "Closed",
        State::HalfOpen => "HalfOpen",
        State::Open => "Open",
    }
}

static CB_REGISTRY: OnceLock<RwLock<HashMap<String, State>>> = OnceLock::new();

/// Signature of the Open-transition listener. Named so the static's
/// type declaration stays short and clippy::type_complexity happy.
pub type OpenListener = Box<dyn Fn(&str, &str) + Send + Sync>;

/// Optional listener invoked whenever a key transitions *into* `Open`.
/// The server installs this at startup to emit a `provider.circuit_open`
/// audit event; the gateways themselves don't need to know about audit.
/// `kind` is "ai" for provider breakers and "mcp" for MCP server
/// breakers so downstream subscribers can distinguish them.
static OPEN_LISTENER: OnceLock<OpenListener> = OnceLock::new();

fn cb_registry() -> &'static RwLock<HashMap<String, State>> {
    CB_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Install a listener for Open transitions. Called once at startup; a
/// second call is a no-op (OnceLock) so tests and double-init are safe.
pub fn set_open_listener<F>(f: F)
where
    F: Fn(&str, &str) + Send + Sync + 'static,
{
    let _ = OPEN_LISTENER.set(Box::new(f));
}

/// Record a circuit-breaker state transition for `key` (typically the
/// upstream provider/server name). Cheap; safe to call from any thread.
///
/// `kind` is a free-form discriminator the caller picks — in practice
/// either "ai" or "mcp" — used by the Open listener to tag emitted
/// audit events. When the transition is Closed→Open or HalfOpen→Open
/// the listener (if installed) fires once.
pub fn record_cb_with_kind(key: &str, state: State, kind: &str) {
    let prev = if let Ok(mut m) = cb_registry().write() {
        m.insert(key.to_string(), state)
    } else {
        None
    };
    if state == State::Open
        && prev != Some(State::Open)
        && let Some(listener) = OPEN_LISTENER.get()
    {
        listener(key, kind);
    }
}

/// Snapshot the current state of every key the registry has seen.
pub fn snapshot_cb_states() -> HashMap<String, State> {
    cb_registry().read().map(|m| m.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    /// Tests share the global `OPEN_LISTENER` and `CB_REGISTRY` —
    /// running them in parallel would crosstalk. A serial mutex
    /// keeps them ordered without forcing the whole crate single-
    /// threaded.
    fn serial_lock() -> &'static Mutex<()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    /// Captures listener invocations into a static so a second test
    /// run within the same process still sees calls made *after*
    /// the first set_open_listener wins. (OnceLock semantics: only
    /// the first set takes effect, so we install once at first use.)
    fn captured() -> &'static Mutex<Vec<(String, String)>> {
        static C: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
        let cell = C.get_or_init(|| Mutex::new(Vec::new()));
        // Idempotent install — only the first call actually wires the
        // listener, subsequent ones no-op (OnceLock::set returns Err).
        let cap = cell;
        set_open_listener(move |key, kind| {
            if let Ok(mut g) = cap.lock() {
                g.push((key.to_string(), kind.to_string()));
            }
        });
        cell
    }

    fn drain_captured() -> Vec<(String, String)> {
        let cap = captured();
        cap.lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }

    #[test]
    fn open_listener_fires_once_per_transition_to_open() {
        let _g = serial_lock().lock().unwrap();
        let _ = drain_captured(); // discard residue from earlier tests

        record_cb_with_kind("test-fires-once", State::Closed, "ai");
        record_cb_with_kind("test-fires-once", State::Open, "ai");
        record_cb_with_kind("test-fires-once", State::Open, "ai");
        record_cb_with_kind("test-fires-once", State::Open, "ai");

        let calls = drain_captured();
        let calls_for_key: Vec<_> = calls
            .iter()
            .filter(|(k, _)| k == "test-fires-once")
            .collect();
        assert_eq!(
            calls_for_key.len(),
            1,
            "expected exactly one Open transition, got {calls_for_key:?}"
        );
        assert_eq!(calls_for_key[0].1, "ai");
    }

    #[test]
    fn open_listener_re_fires_after_close_then_open() {
        let _g = serial_lock().lock().unwrap();
        let _ = drain_captured();

        record_cb_with_kind("test-re-fires", State::Open, "mcp");
        record_cb_with_kind("test-re-fires", State::Closed, "mcp");
        record_cb_with_kind("test-re-fires", State::Open, "mcp");

        let calls = drain_captured();
        let calls_for_key: Vec<_> = calls.iter().filter(|(k, _)| k == "test-re-fires").collect();
        assert_eq!(
            calls_for_key.len(),
            2,
            "Closed→Open should re-fire, got {calls_for_key:?}"
        );
        assert!(calls_for_key.iter().all(|(_, kind)| kind == "mcp"));
    }

    #[test]
    fn open_listener_does_not_fire_on_half_open() {
        let _g = serial_lock().lock().unwrap();
        let _ = drain_captured();

        record_cb_with_kind("test-no-half-open", State::Closed, "ai");
        record_cb_with_kind("test-no-half-open", State::HalfOpen, "ai");

        let calls = drain_captured();
        assert!(
            !calls.iter().any(|(k, _)| k == "test-no-half-open"),
            "HalfOpen transition must not fire the Open listener"
        );
    }

    #[test]
    fn snapshot_reflects_latest_state_for_each_key() {
        let _g = serial_lock().lock().unwrap();

        record_cb_with_kind("snapshot-test-1", State::Closed, "ai");
        record_cb_with_kind("snapshot-test-2", State::Open, "mcp");
        record_cb_with_kind("snapshot-test-1", State::HalfOpen, "ai");

        let snap = snapshot_cb_states();
        assert_eq!(snap.get("snapshot-test-1"), Some(&State::HalfOpen));
        assert_eq!(snap.get("snapshot-test-2"), Some(&State::Open));
    }
}
