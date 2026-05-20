//! `check_access` — gates the candidate subject (tool / model)
//! against the identity's access policy via
//! [`Surface::is_access_allowed`]. Wraps the surface-specific
//! decision in the same audit + short-circuit pattern
//! [`super::check_limits`] uses, so the deny path is uniform
//! regardless of which surface invoked the stage.

use crate::audit::AuditLogger;

use super::super::Surface;
use super::super::state::{Authorized, LimitsChecked};

/// Run the access check. On allow, transition to
/// [`Authorized`] carrying the `candidate` string for downstream
/// audit. On deny, emit a `"access_denied"` audit row tagged with
/// the subject and short-circuit with
/// [`Surface::access_denied_response`].
#[tracing::instrument(
    skip_all,
    fields(trace_id = %state.trace_id, candidate = %candidate),
)]
pub async fn check_access<S: Surface>(
    state: LimitsChecked<S>,
    candidate: &str,
    audit: &AuditLogger,
) -> Result<Authorized<S>, S::Response> {
    if !S::is_access_allowed(&state.identity, candidate) {
        metrics::counter!("lifecycle_access_denied_total").increment(1);
        tracing::warn!(
            trace_id = %state.trace_id,
            candidate = %candidate,
            "access denied"
        );
        let entry = S::audit_entry(&state.identity, "access_denied")
            .trace_id(state.trace_id.clone())
            .detail(serde_json::json!({ "subject": candidate }));
        audit.log(entry);
        return Err(S::access_denied_response(candidate));
    }

    Ok(Authorized {
        identity: state.identity,
        body: state.body,
        trace_id: state.trace_id,
        started_at: state.started_at,
        client_ip: state.client_ip,
        limit_check: state.limit_check,
        access_candidate: candidate.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::state::{LimitCheckRecord, LimitsChecked};
    use super::super::super::test_surface::{TestResponse, TestSurface, make_raw};
    use super::*;
    use uuid::Uuid;

    fn make_limits_checked(user_id: Uuid) -> LimitsChecked<TestSurface> {
        let raw = make_raw(user_id);
        LimitsChecked {
            identity: raw.identity,
            body: raw.body,
            trace_id: raw.trace_id,
            started_at: raw.started_at,
            client_ip: raw.client_ip,
            limit_check: LimitCheckRecord {
                currents: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn allow_passes_through() {
        // `TestSurface::is_access_allowed` returns true iff
        // candidate starts with "allowed_" — see test_surface.rs.
        let user_id = Uuid::new_v4();
        let state = make_limits_checked(user_id);
        let result = check_access::<TestSurface>(
            state,
            "allowed_tool",
            &crate::audit::AuditLogger::test_drain(),
        )
        .await;
        match result {
            Ok(authorized) => {
                assert_eq!(authorized.access_candidate, "allowed_tool");
                assert_eq!(authorized.identity.user_id, user_id);
            }
            Err(_) => panic!("allow path should not short-circuit"),
        }
    }

    #[tokio::test]
    async fn deny_short_circuits_with_response() {
        let user_id = Uuid::new_v4();
        let state = make_limits_checked(user_id);
        let result = check_access::<TestSurface>(
            state,
            "blocked_tool",
            &crate::audit::AuditLogger::test_drain(),
        )
        .await;
        match result {
            Ok(_) => panic!("deny path should short-circuit"),
            Err(response) => {
                // TestSurface::access_denied_response returns
                // AccessDenied carrying the candidate.
                assert_eq!(
                    response,
                    TestResponse::AccessDenied {
                        candidate: "blocked_tool".to_owned(),
                    }
                );
            }
        }
    }
}
