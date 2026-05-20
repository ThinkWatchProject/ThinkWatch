//! `check_limits` — the first stage of the request lifecycle.
//! Charges the user's `requests` rate-limit counters and either:
//!
//! * passes through to [`LimitsChecked`] (carrying the post-INCR
//!   currents for downstream audit), or
//! * short-circuits with `S::rate_limited_response(label)` /
//!   `S::rate_limiter_unavailable_response()` after emitting the
//!   audit row for the deny.
//!
//! No surface-specific logic lives here — the stage takes
//! pre-built `RateLimitRule`s. The surface's pipeline runner is
//! responsible for materialising rules from `SurfaceConstraints`
//! before calling.

use fred::clients::Client;

use crate::audit::AuditLogger;
use crate::limits::{RateLimitRule, RateMetric, sliding};

use super::super::Surface;
use super::super::state::{LimitCheckRecord, LimitsChecked, Raw};

/// Resolve and charge `requests` counters. See module-level docs.
///
/// `fail_closed` controls behaviour on Redis errors:
/// - `false` (default): bumps `lifecycle_rate_limiter_fail_open_total`
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
    redis: &Client,
    fail_closed: bool,
    audit: &AuditLogger,
) -> Result<LimitsChecked<S>, S::Response> {
    let resolved = sliding::resolve_rules(rules, RateMetric::Requests);

    // No rules configured ⇒ trivially pass. Skip Redis entirely so
    // a misconfigured surface (no rules attached) doesn't pay a
    // round-trip per request.
    if resolved.is_empty() {
        return Ok(LimitsChecked {
            identity: state.identity,
            body: state.body,
            trace_id: state.trace_id,
            started_at: state.started_at,
            client_ip: state.client_ip,
            limit_check: LimitCheckRecord {
                currents: Vec::new(),
            },
        });
    }

    let outcome = match sliding::check_and_record(redis, &resolved, 1, !fail_closed).await {
        Ok(o) => o,
        Err(e) => {
            if fail_closed {
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
            // Fail-open path: bump the counter, log at warn, and
            // synthesise an "allowed" outcome with empty currents
            // so downstream stages see an unobjectionable result.
            metrics::counter!("lifecycle_rate_limiter_fail_open_total").increment(1);
            tracing::warn!(
                error = %e,
                trace_id = %state.trace_id,
                "rate limiter unavailable; failing open"
            );
            sliding::CheckOutcome {
                allowed: true,
                exceeded_index: -1,
                currents: Vec::new(),
            }
        }
    };

    if !outcome.allowed {
        // `exceeded_index` is into the `requests`-filtered slice;
        // re-filter to find the rule that tripped so we can label
        // the response with `subject:metric/window`.
        let label = (outcome.exceeded_index >= 0)
            .then(|| {
                rules
                    .iter()
                    .filter(|r| r.metric == RateMetric::Requests)
                    .nth(outcome.exceeded_index as usize)
                    .map(sliding::rate_label)
            })
            .flatten()
            .unwrap_or_else(|| "rate limit".to_string());
        metrics::counter!("lifecycle_rate_limited_total").increment(1);
        tracing::warn!(
            trace_id = %state.trace_id,
            limit = %label,
            "rate limited"
        );
        let entry = S::audit_entry(&state.identity, "rate_limited")
            .trace_id(state.trace_id.clone())
            .detail(serde_json::json!({ "limit": label }));
        audit.log(entry);
        return Err(S::rate_limited_response(&label));
    }

    Ok(LimitsChecked {
        identity: state.identity,
        body: state.body,
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
    /// real Redis are integration-only (see DESIGN.md phase 2).
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
