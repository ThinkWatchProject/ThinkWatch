//! Structured infrastructure-failure type for pipeline stages.
//! User-attributable short-circuits (rate limited, access denied,
//! …) are NOT errors — they return `Err(S::Response)` directly so
//! `?` threads them up to the surface handler alongside true
//! infrastructure failures.
//!
//! The surface handler maps `StageError` → its wire-format error
//! (`AppError`, `GatewayError`, JSON-RPC INTERNAL_ERROR, …) in
//! exactly one place per surface.

/// Failures stages can have that are NOT the user's fault.
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    /// Redis-backed limiter unavailable AND the dynamic-config
    /// `rate_limit_fail_closed` flag was set. Surface should map
    /// to "service unavailable, try again".
    #[error("rate limiter unavailable: {0}")]
    RateLimiterUnavailable(#[source] anyhow::Error),

    /// Cache backing store unavailable. Most stages treat this as
    /// "skip the cache, keep going" (so it never reaches the
    /// surface as an error) — this variant is here for callers
    /// that want strict-mode behaviour.
    #[error("cache backing store unavailable: {0}")]
    CacheUnavailable(#[source] anyhow::Error),

    /// Audit pipeline rejected the row. Bounded-channel back-
    /// pressure — shouldn't happen unless the audit worker has
    /// truly stalled, in which case we want a visible failure
    /// rather than silent drop.
    #[error("audit pipeline channel full — audit worker stalled?")]
    AuditChannelFull,

    /// Catch-all for unexpected upstream / DB failures. Anything
    /// that doesn't fit a specific variant goes here; the surface
    /// adapter renders it as "internal error" by default.
    #[error("internal: {0}")]
    Internal(#[source] anyhow::Error),
}
