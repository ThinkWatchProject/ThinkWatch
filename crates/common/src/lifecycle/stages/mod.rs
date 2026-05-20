//! Pipeline stages — each surface-agnostic stage is a plain
//! `async fn` here. See [`super`] for the full pipeline shape.
//!
//! Short-circuit stages (`check_limits`, `check_budget`,
//! `check_access`) live as standalone fns that take the previous
//! state struct and return either the next state or
//! `Err(S::Response)`. The four post-invoke stages
//! (`record_outcome` → `write_cache` → `record_usage` →
//! `emit_audit`) live in [`run_post_invoke`] and are dispatched in
//! order by the orchestrator of the same name.
//!
//! Adding a stage:
//! 1. Define its input + output state structs in [`super::state`].
//! 2. Add the `async fn` in a new file here.
//! 3. Re-export from this module.
//! 4. Add unit tests against the in-tree
//!    [`super::test_surface::TestSurface`].

mod check_access;
mod check_budget;
mod check_limits;
mod run_post_invoke;

pub use check_access::check_access;
pub use check_budget::check_budget;
pub use check_limits::check_limits;
pub use run_post_invoke::{emit_audit, record_outcome, record_usage, run_post_invoke, write_cache};
