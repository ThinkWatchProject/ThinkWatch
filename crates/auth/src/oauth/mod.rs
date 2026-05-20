//! OAuth 2.0 primitives used by the MCP-OAuth flow. Pulled out of
//! `server::handlers::mcp_oauth` so they live in a place that
//! doesn't depend on Axum / AppState — making them unit-testable
//! in isolation and reusable from other surfaces (e.g. SSO if we
//! ever consolidate).
//!
//! Today owns:
//! - [`pkce`] — PKCE / state-binding / token-endpoint error helpers.
//! - [`subject`] — JWT / userinfo subject extraction (pure parsers).
//! - [`client`] — token-endpoint POST + structured error type.
//!
//! Coming next sessions (see ROADMAP notes in the handler refactor):
//! - `flow` — `(user, server)` flow state machine.
//! - `storage` — encrypted token I/O trait + Postgres impl.

pub mod client;
pub mod pkce;
pub mod subject;
