//! Audit logging — the load-bearing pipeline for every actor-attributed
//! event in the system. Splits across files for readability; the
//! public surface stays the same as the old single-file `audit.rs`.
//!
//! - [`types`]      `LogType`, `BodyCaptureStatus`, [`AuditEntry`] + builder
//! - [`actors`]     [`AuditActor`] trait + 5 actor shapes
//! - [`sanitize`]   detail-blob secret redaction
//! - [`forwarders`] per-transport delivery (syslog / kafka / webhook)
//! - [`outbox`]     durable webhook redelivery with exponential backoff
//! - [`logger`]     [`AuditLogger`] + background worker + forwarder reload
//! - [`clickhouse`] per-table flush + schema bootstrap

mod actors;
mod clickhouse;
mod forwarders;
mod logger;
mod outbox;
mod sanitize;
mod types;

// Public surface — re-exported flat so external callers' import paths
// don't change after the split.
pub use actors::{
    AnonymousActor, AuditActor, GatewayActor, McpActor, OAuthCallbackActor, SystemActor,
};
pub use clickhouse::ensure_clickhouse_tables;
pub use logger::{AuditConfig, AuditLogger};
pub use types::{AuditEntry, BodyCaptureStatus, LogType};
