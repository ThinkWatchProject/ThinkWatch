//! Pipeline stages — each surface-agnostic stage is a plain
//! `async fn` here. See [DESIGN.md](../DESIGN.md) for the full
//! stage inventory and migration plan.
//!
//! Phase 1 lands the first stage (`check_limits`); subsequent
//! phases add the rest. Adding a stage:
//!
//! 1. Define its input + output state structs in
//!    [`super::state`].
//! 2. Add the `async fn` in a new file here.
//! 3. Re-export from this module.
//! 4. Add unit tests against the in-tree
//!    [`super::test_surface::TestSurface`].

mod check_access;
mod check_limits;
mod run_post_invoke;

pub use check_access::check_access;
pub use check_limits::check_limits;
pub use run_post_invoke::{emit_audit, record_outcome, run_post_invoke, write_cache};
