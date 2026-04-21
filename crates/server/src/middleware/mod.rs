//! HTTP middleware stack.
//!
//! Each layer wraps the router it's mounted on and runs for every
//! request that passes through. Ordering is `Layer::layer` order —
//! outermost first — so reads read bottom-up in `app.rs`.
//!
//! ## Audit / rate-limit layering decisions
//!
//! The review suggested hoisting audit logging and rate-limiting
//! into dedicated middleware layers. That isn't quite right for how
//! the two concerns actually split in this codebase:
//!
//! * **Access logging** is already middleware — [`access_log`] emits
//!   one row per HTTP request with path / status / latency / user_id
//!   / trace_id. That covers "log every request".
//! * **Audit logging** is content-aware: it records semantic facts
//!   like "admin X rotated API key Y, reason Z". Those need data only
//!   the handler has (before + after, actor, resource-specific
//!   detail), so the calls live inline. A middleware can't reliably
//!   reconstruct "what happened" from the request/response bytes
//!   alone without duplicating what each handler already knows.
//! * **Per-request rate limiting** layers correctly via
//!   [`api_key_auth`] (per-key soft cap, see SEC-14) and the login-
//!   path counters in [`crate::handlers::auth`] (per-IP + per-email,
//!   see SEC-03). These are the request-admission gate.
//! * **Domain rate limiting** (test-connection endpoints, trace
//!   lookups, MCP probes) is scoped per-endpoint-tag and sits in
//!   [`crate::handlers::test_rate_limit`] so each handler picks its
//!   own tag and cap. Generalising those into one layer would force
//!   every handler to share one tag, which breaks the "per tag"
//!   cap semantics the audit helpers rely on.
//!
//! New cross-cutting concerns should still go here; the existing
//! split is the deliberate answer to the review.

pub mod access_log;
pub mod api_key_auth;
pub mod auth_guard;
pub mod verify_signature;
