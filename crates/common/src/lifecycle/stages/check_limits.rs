//! `check_limits` — the rate-limit gate of the request lifecycle.
//! Checks every rate-limit rule the request is held to, in one atomic
//! step, and either:
//!
//! * passes through to [`LimitsChecked`] (carrying the post-charge
//!   currents for downstream audit), having charged 1 to every
//!   `requests` rule, or
//! * short-circuits with `S::rate_limited_response(label, retry_after)`
//!   / `S::rate_limiter_unavailable_response()` after emitting the
//!   audit row for the deny, having charged nothing.
//!
//! A `tokens` rule refuses the request once its window's recorded
//! usage has reached the limit; the tokens the call uses are added
//! after it, by the surface's `record_usage` hook.
//!
//! No surface-specific logic lives here — the stage takes
//! pre-built `RateLimitRule`s. The surface's pipeline runner is
//! responsible for materialising rules from `SurfaceConstraints`
//! before calling.

use fred::clients::Client;
use uuid::Uuid;

use crate::audit::AuditLogger;
use crate::limits::{RateLimitRule, sliding};

use super::super::Surface;
use super::super::state::{LimitCheckRecord, LimitsChecked, Raw};

/// Check the rules and charge the request's `requests` counters. See
/// module-level docs. `owner` is the user the request runs as: every
/// counter carries their hash tag (`sliding::counter_key`).
///
/// `fail_closed` controls behaviour on Redis errors:
/// - `false` (default): bumps `gateway_rate_limiter_fail_open_total`
///   and lets the request through.
/// - `true`: emits the audit row and short-circuits with
///   `S::rate_limiter_unavailable_response()`. Wired up from the
///   `security.rate_limit_fail_closed` system setting.
#[tracing::instrument(
    skip_all,
    fields(trace_id = %state.trace_id, rule_count = rules.len()),
)]
pub async fn check_limits<S: Surface>(
    state: Raw<S>,
    rules: &[RateLimitRule],
    owner: Uuid,
    redis: &Client,
    fail_closed: bool,
    audit: &AuditLogger,
) -> Result<LimitsChecked<S>, S::Response> {
    // No rules configured ⇒ trivially pass. Skip Redis entirely so
    // a misconfigured surface (no rules attached) doesn't pay a
    // round-trip per request.
    if rules.is_empty() {
        return Ok(LimitsChecked {
            identity: state.identity,
            trace_id: state.trace_id,
            started_at: state.started_at,
            client_ip: state.client_ip,
            limit_check: LimitCheckRecord {
                currents: Vec::new(),
            },
        });
    }

    let outcome = match sliding::admit(redis, rules, owner, !fail_closed).await {
        Ok(o) => o,
        Err(e) => {
            // `admit` only errs when failing closed; it fails open by
            // itself.
            metrics::counter!("lifecycle_rate_limiter_unavailable_total").increment(1);
            tracing::warn!(
                error = %e,
                trace_id = %state.trace_id,
                "rate limiter unavailable; failing closed"
            );
            let entry = S::audit_entry(&state.identity, "rate_limiter_unavailable")
                .trace_id(state.trace_id.clone());
            audit.log(entry);
            return Err(S::rate_limiter_unavailable_response());
        }
    };

    if !outcome.allowed {
        let label = outcome
            .exceeded_index
            .and_then(|i| rules.get(i))
            .map(sliding::rate_label)
            .unwrap_or_else(|| "rate limit".to_string());
        metrics::counter!("lifecycle_rate_limited_total").increment(1);
        tracing::warn!(
            trace_id = %state.trace_id,
            limit = %label,
            retry_after_secs = outcome.retry_after_secs,
            "rate limited"
        );
        let entry = S::audit_entry(&state.identity, "rate_limited")
            .trace_id(state.trace_id.clone())
            .detail(serde_json::json!({
                "limit": label,
                "retry_after_secs": outcome.retry_after_secs,
            }));
        audit.log(entry);
        return Err(S::rate_limited_response(&label, outcome.retry_after_secs));
    }

    Ok(LimitsChecked {
        identity: state.identity,
        trace_id: state.trace_id,
        started_at: state.started_at,
        client_ip: state.client_ip,
        limit_check: LimitCheckRecord {
            currents: outcome.currents,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::test_surface::{TestSurface, make_raw};
    use super::*;
    use fred::types::Builder;
    use fred::types::config::Config as RedisConfig;
    use uuid::Uuid;

    /// Build a disconnected fred client. The `no_rules_…` test
    /// never reaches a Redis call so this is fine; tests that need
    /// real Redis live in `crates/test-support/tests/limits*.rs`.
    fn dummy_redis() -> fred::clients::Client {
        let cfg = RedisConfig::from_url("redis://127.0.0.1:6379").expect("parse url");
        Builder::from_config(cfg).build().expect("build client")
    }

    fn dummy_audit() -> crate::audit::AuditLogger {
        // Audit logger with no sink wired up — accepts entries
        // into its bounded channel, never flushes them. Good
        // enough for the no-rules path which doesn't emit anyway.
        crate::audit::AuditLogger::test_drain()
    }

    #[tokio::test]
    async fn passes_through_with_no_rules() {
        // No rules ⇒ stage skips Redis entirely and emits
        // LimitsChecked with empty currents. Verifies the type
        // transition and field-preservation contract.
        let user_id = Uuid::new_v4();
        let raw = make_raw(user_id);
        let trace_id = raw.trace_id.clone();
        let started_at = raw.started_at;

        let result = check_limits::<TestSurface>(
            raw,
            &[],
            user_id,
            &dummy_redis(),
            true, // fail_closed — irrelevant when no rules
            &dummy_audit(),
        )
        .await;

        let limits = result.expect("no-rules path passes through");
        assert_eq!(limits.identity.user_id, user_id);
        assert_eq!(limits.trace_id, trace_id);
        assert_eq!(limits.started_at, started_at);
        assert!(
            limits.limit_check.currents.is_empty(),
            "no rules ⇒ no currents to report"
        );
    }
}
