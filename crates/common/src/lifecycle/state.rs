//! Per-stage state structs. Each stage consumes one struct and
//! returns the next, encoding the lifecycle progression in the
//! type system: you literally cannot call `check_access` against a
//! [`Raw`] state because `check_access` takes `BudgetChecked`
//! (phase 2 — not yet implemented).
//!
//! The transitions move ownership, not clone. Each subsequent
//! struct destructures the previous one and adds the new field(s).
//! Stage code looks like:
//!
//! ```ignore
//! let Raw { identity, body, trace_id, started_at, client_ip } = state;
//! let limit_check = run_limit_check(&identity, &redis, &rules).await?;
//! Ok(LimitsChecked { identity, body, trace_id, started_at, client_ip, limit_check })
//! ```
//!
//! Verbose by design — the destructure makes every field carried
//! across the transition visible at the call site. Adding a field
//! to `Raw` that should also live in `LimitsChecked` is a localised
//! edit (one struct + one transition fn).

use std::time::Instant;

use super::Surface;

/// Initial state. Identity has been resolved by HTTP middleware;
/// the request body has been deserialised; nothing else has
/// happened yet.
pub struct Raw<S: Surface> {
    /// Per-surface identity (carries `SurfaceConstraints`, etc.)
    pub identity: S::Identity,
    /// Parsed request body. Stage code reads from this to derive
    /// access-control inputs (model name for the AI gateway,
    /// tool name for MCP).
    pub body: S::RequestBody,
    /// Trace id — minted by HTTP middleware or pinned via
    /// `x-trace-id` header. Stamped on every audit row this
    /// pipeline produces.
    pub trace_id: String,
    /// Wall-clock pipeline start. Used to compute `duration_ms`
    /// for the audit row.
    pub started_at: Instant,
    /// Resolved client IP. May be `None` when extraction failed.
    pub client_ip: Option<String>,
}

impl<S: Surface> Raw<S> {
    pub fn new(
        identity: S::Identity,
        body: S::RequestBody,
        trace_id: String,
        client_ip: Option<String>,
    ) -> Self {
        Self {
            identity,
            body,
            trace_id,
            started_at: Instant::now(),
            client_ip,
        }
    }
}

/// Output of `check_limits`. Carries everything from [`Raw`] plus
/// the materialised limit-check result so downstream stages (in
/// particular `emit_audit`) can record what budgets / counters
/// this request charged.
pub struct LimitsChecked<S: Surface> {
    pub identity: S::Identity,
    pub body: S::RequestBody,
    pub trace_id: String,
    pub started_at: Instant,
    pub client_ip: Option<String>,
    /// Outcome of the rate-limit check. For `check_limits` to
    /// emit `LimitsChecked` at all, the check must have *passed*;
    /// this field records the per-window currents the audit row
    /// surfaces in its `limits` block.
    pub limit_check: LimitCheckRecord,
}

/// Successful-check trace data. Mirrors the shape
/// [`crate::limits::sliding::CheckOutcome`] returns, minus the
/// "allowed: bool" — if we're carrying this in a `LimitsChecked`
/// we already know it was allowed.
#[derive(Debug, Clone)]
pub struct LimitCheckRecord {
    /// Current count after the increment, per resolved rule.
    /// Position matches the input rule order so the audit row
    /// can pair them back up.
    pub currents: Vec<i64>,
}
