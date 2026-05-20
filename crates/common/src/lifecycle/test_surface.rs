//! Reusable [`Surface`] impl for `common::lifecycle` unit tests.
//! Carries minimal types so stage tests can build a [`Raw`] state
//! without dragging in MCP / AI-gateway specifics.
//!
//! `#[cfg(test)]` — never compiled into release.

use uuid::Uuid;

use crate::audit::{AuditActor, AuditEntry, GatewayActor};

use super::Surface;
use super::state::Raw;

/// Test identity. `user_id` is what audit attribution uses; the
/// `limits_constraints` field is intentionally absent because the
/// `check_limits` stage takes pre-materialised rules as a parameter.
#[derive(Debug, Clone)]
pub struct TestIdentity {
    pub user_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestResponse {
    /// Buffered-success terminal. Future phase-2 stages will
    /// construct this in `emit_audit`; for the phase-1 pilot
    /// (check_limits / check_access) the variant exists so the
    /// response enum is exhaustive — short-circuit-path tests
    /// don't construct it directly.
    #[allow(dead_code)]
    Ok,
    RateLimited {
        label: String,
    },
    RateLimiterUnavailable,
    AccessDenied {
        candidate: String,
    },
}

pub struct TestSurface;

impl Surface for TestSurface {
    type Identity = TestIdentity;
    type RequestBody = serde_json::Value;
    type Response = TestResponse;
    type AuditDetail = serde_json::Value;

    fn audit_entry(identity: &Self::Identity, action: &str) -> AuditEntry {
        // Reuse GatewayActor's wire shape (string-typed identity
        // fields) — all we need is something with a `user_id`
        // marker, and converting the test UUID to its string form
        // is fine here.
        let user_id_str = identity.user_id.to_string();
        GatewayActor {
            user_id: Some(user_id_str.as_str()),
            user_email: Some("test@example.com"),
            api_key_id: None,
            api_key_lineage_id: None,
            ip: None,
            session_id: None,
        }
        .audit(action)
    }

    fn rate_limited_response(label: &str) -> Self::Response {
        TestResponse::RateLimited {
            label: label.to_owned(),
        }
    }

    fn rate_limiter_unavailable_response() -> Self::Response {
        TestResponse::RateLimiterUnavailable
    }

    fn is_access_allowed(_identity: &Self::Identity, candidate: &str) -> bool {
        // Toy policy for unit tests: allow anything starting with
        // "allowed_". The check_access test exercises both arms by
        // passing "allowed_tool" / "blocked_tool".
        candidate.starts_with("allowed_")
    }

    fn access_denied_response(candidate: &str) -> Self::Response {
        TestResponse::AccessDenied {
            candidate: candidate.to_owned(),
        }
    }
}

/// Build a minimal [`Raw`] for tests.
pub fn make_raw(user_id: Uuid) -> Raw<TestSurface> {
    Raw::new(
        TestIdentity { user_id },
        serde_json::json!({}),
        format!("test-trace-{user_id}"),
        Some("127.0.0.1".to_owned()),
    )
}
