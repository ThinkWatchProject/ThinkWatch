//! Surface-agnostic request lifecycle pipeline. See
//! [`DESIGN.md`](./DESIGN.md) for the architecture decisions and
//! migration plan; this module is the implementation.
//!
//! ## At a glance
//!
//! - [`Surface`] is the trait each gateway implements to plug its
//!   wire-format types (identity / request body / response /
//!   audit detail) into the pipeline.
//! - [`state`] holds the per-stage state structs. Each stage
//!   consumes one struct and returns the next, producing a
//!   compiler-enforced execution order — you literally cannot
//!   call `check_access` before `check_limits`.
//! - [`stages`] holds the surface-agnostic stage implementations
//!   as plain `async fn`s. No `Stage` trait, no dynamic dispatch.
//! - [`error::StageError`] is the structured infrastructure-
//!   failure type. User-attributable short-circuits (rate limited,
//!   access denied, …) return `Err(S::Response)` directly so the
//!   `?` operator threads them up alongside infra errors.
//!
//! ## Hello-world
//!
//! ```ignore
//! async fn handle_request<S: Surface>(
//!     identity: S::Identity,
//!     body: S::RequestBody,
//!     deps: &PipelineDeps,
//! ) -> Result<S::Response, S::Response> {
//!     let raw = state::Raw::new(identity, body);
//!     let limits = stages::check_limits::<S>(raw, &deps.limit_rules, &deps.redis, &deps.audit).await?;
//!     // …subsequent stages chain here…
//!     todo!("pipeline mid-build; see DESIGN.md phase plan")
//! }
//! ```

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
