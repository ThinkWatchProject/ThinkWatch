//! Process-wide circuit-breaker state registry.
//!
//! Both the AI gateway (`think-watch-gateway`) and the MCP gateway
//! (`think-watch-mcp-gateway`) write into this shared map every time a
//! circuit transitions. The dashboard handler in `think-watch-server` reads
//! a snapshot to render real-time CB state in the upstream-health panel.
//!
//! Living in `think-watch-common` keeps the two gateways decoupled while
//! still letting them share a single global view.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::RwLock;

/// Public, stable representation of a circuit-breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CbState {
    Closed,
    HalfOpen,
    Open,
}

impl CbState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CbState::Closed => "Closed",
            CbState::HalfOpen => "HalfOpen",
            CbState::Open => "Open",
        }
    }
}

static CB_REGISTRY: OnceLock<RwLock<HashMap<String, CbState>>> = OnceLock::new();

/// Signature of the Open-transition listener. Named so the static's
/// type declaration stays short and clippy::type_complexity happy.
pub type OpenListener = Box<dyn Fn(&str, &str) + Send + Sync>;

/// Optional listener invoked whenever a key transitions *into* `Open`.
/// The server installs this at startup to emit a `provider.circuit_open`
/// audit event; the gateways themselves don't need to know about audit.
/// `kind` is "ai" for provider breakers and "mcp" for MCP server
/// breakers so downstream subscribers can distinguish them.
static OPEN_LISTENER: OnceLock<OpenListener> = OnceLock::new();

fn cb_registry() -> &'static RwLock<HashMap<String, CbState>> {
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
pub fn record_cb_with_kind(key: &str, state: CbState, kind: &str) {
    let prev = if let Ok(mut m) = cb_registry().write() {
        m.insert(key.to_string(), state)
    } else {
        None
    };
    if state == CbState::Open
        && prev != Some(CbState::Open)
        && let Some(listener) = OPEN_LISTENER.get()
    {
        listener(key, kind);
    }
}

/// Backward-compatible shim for callers that don't specify a `kind`.
/// New call sites should use `record_cb_with_kind`.
pub fn record_cb(key: &str, state: CbState) {
    record_cb_with_kind(key, state, "unknown");
}

/// Snapshot the current state of every key the registry has seen.
pub fn snapshot_cb_states() -> HashMap<String, CbState> {
    cb_registry().read().map(|m| m.clone()).unwrap_or_default()
}
