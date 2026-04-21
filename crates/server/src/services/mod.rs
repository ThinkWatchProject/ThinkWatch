//! # Service layer
//!
//! Handlers under `crates/server/src/handlers/*` are Axum entry points:
//! they extract request state, do permission checks via `AuthUser`,
//! call into a service, and shape the response. They should NOT:
//!
//! * carry raw SQL — that belongs in a repository under this module
//! * own multi-step business rules (transaction orchestration, cascade
//!   effects across tables, cache invalidation, audit hookups) — pull
//!   those into a service here
//! * reach across sibling handlers via `super::other_handler::*` —
//!   the function being shared should be lifted into a service first
//!
//! The rule of thumb when adding code:
//!
//! * Pure read/write of a single aggregate → a `Repository` (e.g.
//!   `UserRepository::find_by_id`, `ApiKeyRepository::list_for_user`).
//!   Thin wrapper over sqlx with the query collocated — no business
//!   rules, no side effects.
//! * Cross-repository orchestration with rules attached → a `Service`
//!   (e.g. `UserService::deactivate_and_revoke_keys`, which calls
//!   UserRepository + ApiKeyRepository + AuditLogger together).
//!
//! Modules are added incrementally — see the plan in
//! [`REVIEW_PLAN_2026-04-20.md`] (gitignored). Not every handler has
//! a service yet; the migration is iterative.

pub mod rbac_service;
pub mod user_repository;
