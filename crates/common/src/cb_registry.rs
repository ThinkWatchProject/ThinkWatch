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

fn cb_registry() -> &'static RwLock<HashMap<String, CbState>> {
    CB_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Record a circuit-breaker state transition for `key` (typically the
/// upstream provider/server name). Cheap; safe to call from any thread.
pub fn record_cb(key: &str, state: CbState) {
    if let Ok(mut m) = cb_registry().write() {
        m.insert(key.to_string(), state);
    }
}

/// Snapshot the current state of every key the registry has seen.
pub fn snapshot_cb_states() -> HashMap<String, CbState> {
    cb_registry().read().map(|m| m.clone()).unwrap_or_default()
}
