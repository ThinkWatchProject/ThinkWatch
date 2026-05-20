//! MCP-specific lifecycle stages. Each stage takes
//! `Authorized<McpSurface>` (the state every stage past
//! [`common::lifecycle::stages::check_access`] sees) and either
//! returns it unchanged on a continue, or returns
//! `Err(JsonRpcResponse)` on a short-circuit (cache hit,
//! breaker-open, etc.). Audit emission for short-circuits is
//! handled inside each stage so the pattern stays uniform with
//! the common stages.
//!
//! Stages here are MCP-specific because they touch
//! [`crate::cache::McpResponseCache`] /
//! [`crate::circuit_breaker::McpCircuitBreakers`] — types that
//! don't have a cross-surface equivalent (the AI gateway's
//! cache + breaker have their own concrete types). Each surface
//! crate owns its surface-specific stages; only the truly
//! cross-cutting ones (limits / access / emit-audit) live in
//! `common::lifecycle::stages`.

mod check_breaker;
mod check_cache;

pub use check_breaker::check_breaker;
pub use check_cache::check_cache;
