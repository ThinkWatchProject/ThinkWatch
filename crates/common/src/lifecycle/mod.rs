//! Surface-agnostic request lifecycle pipeline shared by the AI
//! gateway and the MCP gateway. Each surface implements [`Surface`]
//! to plug its wire-format types in; the stage functions in
//! [`stages`] then drive the request through a fixed sequence with
//! compiler-enforced ordering.
//!
//! ## Pipeline shape
//!
//! ```text
//! Raw<S>
//!   → check_limits      (rate-limit gate)
//!   → check_budget      (pre-call budget peek)
//!   → check_access      (allowed_models / allowed_tools)
//!   → surface-specific  (cache lookup, breaker, credential resolution,
//!                        invoke_upstream → Invocation<S>)
//!   → run_post_invoke
//!       → record_outcome (breaker accounting)
//!       → write_cache    (success-gated)
//!       → record_usage   (limits + budget post-debit)
//!       → emit_audit     (single emit site)
//!   → Emitted<S>
//! ```
//!
//! ## Module map
//!
//! - [`Surface`] — the trait each gateway implements to plug in
//!   `Identity` / `Response` / `StreamResponse` / `StreamCaptured`
//!   / `PostInvokeDeps` types plus the hook fns
//!   `record_outcome` / `write_cache` / `record_usage` /
//!   `emit_audit` and short-circuit response factories.
//! - [`state`] — per-stage state structs ([`state::Raw`],
//!   [`state::LimitsChecked`], [`state::Authorized`],
//!   [`state::Invoked`], [`state::Emitted`]) plus the
//!   [`state::Invocation`] / [`state::CapturedView`] enums that
//!   model the buffered/streaming fork at `invoke_upstream`.
//! - [`stages`] — surface-agnostic stage `async fn`s. No `Stage`
//!   trait, no dynamic dispatch.
//! - [`error::StageError`] — structured infrastructure-failure
//!   type. User-attributable short-circuits (rate limited, budget
//!   exceeded, access denied, …) return `Err(S::Response)`
//!   directly so the `?` operator threads them up alongside infra
//!   errors.
//! - [`streaming::StreamOutcome`] — three-state outcome
//!   (`Natural` / `UpstreamError{status_code}` / `ClientCancelled`)
//!   the streaming pump signals through its tail future.

pub mod error;
pub mod stages;
pub mod state;
pub mod streaming;
mod surface;

pub use error::StageError;
pub use streaming::StreamOutcome;
pub use surface::Surface;

#[cfg(test)]
mod test_surface;
