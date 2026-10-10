//! `check_budget` — pre-call budget peek. Reads each configured
//! [`BudgetCap`]'s current weighted-token counter without
//! incrementing and short-circuits with
//! [`Surface::budget_exceeded_response`] when any cap is already at
//! or past its limit. Pairs with the post-call
//! [`super::record_usage`] stage that actually debits the counters.
//!
//! This is a best-effort gate: a burst of simultaneous requests
//! that all read `current < limit` and then all increment can still
//! push past the cap. The gate exists to reject NEW requests once
//! the cap is already exhausted from prior traffic. The post-call
//! `add_weighted_tokens` path emits crossing alerts for the burst-
//! overrun case (see `common::limits::budget`).
//!
//! No mutation, so failure of the Redis read defaults to fail-open
//! (request allowed) unless the caller passes `fail_closed = true`
//! — matching the [`super::check_limits`] semantics. A failed read is
//! never taken for an unspent budget (`budget::current_spend` returns
//! the error).
//!
//! Ordering note: this stage runs first, on the [`Raw`] state, then
//! `check_access`, then `check_limits`. `check_limits` charges the
//! `requests` counters, so running it before either of the others
//! would charge a request they then refuse. Neither of them charges
//! anything, so a request refused before `check_limits` charges
//! nothing.

use chrono::Utc;
use fred::clients::Client;

use crate::audit::AuditLogger;
use crate::limits::{BudgetCap, budget};

use super::super::Surface;
use super::super::state::Raw;

/// Run a read-only spend check against every supplied cap. On
/// allow, returns the input state unchanged (passthrough — no
/// extra type narrowing). On deny, emits a `"budget_exceeded"`
/// audit row + short-circuits with `S::budget_exceeded_response`,
/// naming the spent cap whose period ends last — the request can't
/// succeed before then — and the seconds until it does.
///
/// `fail_closed` controls behaviour on a Redis read error:
/// - `false` (default): bumps `lifecycle_budget_fail_open_total`
///   and lets the request through.
/// - `true`: emits a `"budget_unavailable"` audit row + short-
///   circuits with `S::budget_unavailable_response`.
#[tracing::instrument(
    skip_all,
    fields(trace_id = %state.trace_id, cap_count = caps.len()),
)]
pub async fn check_budget<S: Surface>(
    state: Raw<S>,
    caps: &[BudgetCap],
    redis: &Client,
    fail_closed: bool,
    audit: &AuditLogger,
) -> Result<Raw<S>, S::Response> {
    if caps.is_empty() {
        return Ok(state);
    }

    let statuses = match budget::current_spend(redis, caps).await {
        Ok(s) => s,
        Err(e) => {
            if fail_closed {
                metrics::counter!("lifecycle_budget_unavailable_total").increment(1);
                tracing::warn!(
                    error = %e,
                    trace_id = %state.trace_id,
                    "budget peek unavailable; failing closed"
                );
                let entry = S::audit_entry(&state.identity, "budget_unavailable")
                    .trace_id(state.trace_id.clone());
                audit.log(entry);
                return Err(S::budget_unavailable_response());
            }
            metrics::counter!("lifecycle_budget_fail_open_total").increment(1);
            tracing::warn!(
                error = %e,
                trace_id = %state.trace_id,
                "budget peek unavailable; failing open"
            );
            return Ok(state);
        }
    };

    // Walk caps + statuses in lockstep — `current_spend` preserves
    // input order so the pairing is positional.
    let now = Utc::now();
    let spent = caps
        .iter()
        .zip(statuses.iter())
        .filter(|(_, status)| status.current >= status.limit)
        .map(|(cap, status)| (cap, status, budget::secs_until_period_end(cap.period, now)))
        .max_by_key(|(_, _, wait)| *wait);
    if let Some((cap, status, retry_after_secs)) = spent {
        let label = budget_label(cap);
        metrics::counter!("lifecycle_budget_exceeded_total").increment(1);
        tracing::warn!(
            trace_id = %state.trace_id,
            cap = %label,
            current = status.current,
            limit = status.limit,
            retry_after_secs,
            "budget cap exceeded"
        );
        let entry = S::audit_entry(&state.identity, "budget_exceeded")
            .trace_id(state.trace_id.clone())
            .detail(serde_json::json!({
                "limit": label,
                "current": status.current,
                "max": status.limit,
                "retry_after_secs": retry_after_secs,
            }));
        audit.log(entry);
        return Err(S::budget_exceeded_response(&label, retry_after_secs));
    }

    Ok(state)
}

/// Build the `"<subject>:budget/<period>"` label the audit row
/// surfaces and the wire response carries. Mirrors the shape of
/// `sliding::rate_label` (`"<subject>:<metric>/<window>"`) so a
/// caller seeing the two label families in the same `detail.limit`
/// field can tell which engine fired.
fn budget_label(cap: &BudgetCap) -> String {
    format!(
        "{}:budget/{}",
        cap.subject_kind.as_str(),
        cap.period.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::super::super::test_surface::{TestResponse, TestSurface, make_raw};
    use super::*;
    use crate::limits::{BudgetPeriod, BudgetSubject};
    use fred::types::Builder;
    use fred::types::config::Config as RedisConfig;
    use uuid::Uuid;

    fn dummy_redis() -> fred::clients::Client {
        // Never connected, so a command waits for a connection that
        // never comes; the timeout turns that into the error an outage
        // gives.
        let cfg = RedisConfig::from_url("redis://127.0.0.1:6379").expect("parse url");
        Builder::from_config(cfg)
            .with_performance_config(|c| {
                c.default_command_timeout = std::time::Duration::from_millis(50)
            })
            .build()
            .expect("build client")
    }

    fn dummy_audit() -> crate::audit::AuditLogger {
        crate::audit::AuditLogger::test_drain()
    }

    /// No caps configured → trivially pass through without touching
    /// Redis. The disconnected dummy client confirms the function
    /// never reached out.
    #[tokio::test]
    async fn passes_through_with_no_caps() {
        let user_id = Uuid::new_v4();
        let state = make_raw(user_id);
        let trace_id = state.trace_id.clone();
        let started_at = state.started_at;

        let result =
            check_budget::<TestSurface>(state, &[], &dummy_redis(), true, &dummy_audit()).await;

        let passed = result.expect("no caps ⇒ pass-through");
        assert_eq!(passed.identity.user_id, user_id);
        assert_eq!(passed.trace_id, trace_id);
        assert_eq!(passed.started_at, started_at);
    }

    fn one_cap() -> BudgetCap {
        BudgetCap {
            id: Uuid::nil(),
            subject_kind: BudgetSubject::User,
            subject_id: Uuid::nil(),
            period: BudgetPeriod::Daily,
            limit_tokens: 1000,
            enabled: true,
            expires_at: None,
            reason: None,
            created_by: None,
        }
    }

    /// The budget can't be read and the gateway fails closed: refused
    /// as unavailable. (The disconnected client fails every command.)
    #[tokio::test]
    async fn an_unreadable_budget_refuses_when_failing_closed() {
        let result = check_budget::<TestSurface>(
            make_raw(Uuid::new_v4()),
            &[one_cap()],
            &dummy_redis(),
            true,
            &dummy_audit(),
        )
        .await;
        assert_eq!(result.err(), Some(TestResponse::BudgetUnavailable));
    }

    /// The budget can't be read and the gateway fails open: the request
    /// goes on.
    #[tokio::test]
    async fn an_unreadable_budget_passes_when_failing_open() {
        let result = check_budget::<TestSurface>(
            make_raw(Uuid::new_v4()),
            &[one_cap()],
            &dummy_redis(),
            false,
            &dummy_audit(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[test]
    fn label_format_is_subject_budget_period() {
        let cap = BudgetCap {
            id: Uuid::nil(),
            subject_kind: BudgetSubject::User,
            subject_id: Uuid::nil(),
            period: BudgetPeriod::Monthly,
            limit_tokens: 1000,
            enabled: true,
            expires_at: None,
            reason: None,
            created_by: None,
        };
        assert_eq!(budget_label(&cap), "user:budget/monthly");
    }

    /// Smoke: the function signature constraints allow the
    /// short-circuit Response type to be propagated via `?`-style
    /// match — same shape `check_limits` uses.
    #[allow(dead_code)]
    async fn type_state_compiles(state: Raw<TestSurface>) {
        let _ = check_budget::<TestSurface>(state, &[], &dummy_redis(), false, &dummy_audit())
            .await
            .map_err(|r| match r {
                TestResponse::BudgetExceeded { .. } | TestResponse::BudgetUnavailable => {}
                _ => unreachable!(),
            });
    }
}
