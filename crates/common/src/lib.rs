//! # `think-watch-common`
//!
//! Types, traits, and small runtime facilities that the gateway,
//! mcp-gateway, auth, and server crates ALL reach for. The original
//! code-review flagged this crate as "becoming a junk drawer" and
//! suggested moving `audit`, `limits`, and `clickhouse_client` into
//! server — but every one of those modules is in fact consumed by
//! gateway and mcp-gateway as well (see the import graph in each
//! crate's lib.rs). Moving them down into `server` would flip the
//! dependency: the data-plane crates would have to import from the
//! control-plane crate.
//!
//! The rule of thumb for new modules:
//!
//! * **Belongs in `common`** — referenced by two or more of
//!   {auth, gateway, mcp-gateway, server}. Keep it small, no
//!   handler-specific dto, no business rules that only server uses.
//! * **Belongs in `server`** — only server/handler/middleware code
//!   touches it. Resist the temptation to "maybe someone else will
//!   need this" — promote later if that day comes.
//! * **Belongs in `auth`** — reusable auth primitives (JWT, API
//!   keys, OIDC, RBAC helpers) consumed by server + the gateways.
//!
//! Modules below are grouped by concern, not alphabetically, so new
//! contributors can see the intended organisation at a glance.

// --- Configuration & bootstrapping ---
pub mod config;
pub mod db;
pub mod dynamic_config;

// --- Shared types & errors ---
pub mod dto;
pub mod errors;
pub mod models;

// --- Data-plane primitives (referenced by gateway / mcp-gateway) ---
pub mod audit; // AuditEntry / AuditLogger — used by every ingest path
pub mod blob_store; // S3-compatible body offload for the audit pipeline
pub mod cb_registry; // circuit-breaker states both gateways write and the dashboard reads
pub mod clickhouse_client;
pub mod cost_decimal; // Decimal ↔ raw i64/i128 helpers for CH Decimal(18, 10) columns
pub mod lifecycle; // Surface-agnostic request pipeline (see lifecycle::mod docs)
pub mod limits; // rate-limit & budget evaluation

// --- Utilities ---
pub mod crypto; // AES-256-GCM envelope for secrets at rest
pub mod fixed_window;
pub mod json_secret; // `{"$enc": ...}` — a secret nested inside a JSONB column
pub mod pii; // BlobRedactor — at-rest body redaction shared by gateway + mcp-gateway
pub mod tasks; // supervised_spawn — panic-isolated background tasks
pub mod validation;
