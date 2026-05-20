//! Reusable [`Surface`] impl for `common::lifecycle` unit tests.
//! Carries minimal types so stage tests can build a [`Raw`] state
//! without dragging in MCP / AI-gateway specifics.
//!
//! `#[cfg(test)]` — never compiled into release.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use uuid::Uuid;

use crate::audit::{AuditActor, AuditEntry, GatewayActor};

use super::Surface;
use super::state::{Invoked, Raw};

/// Test identity. `user_id` is what audit attribution uses; the
/// `limits_constraints` field is intentionally absent because the
/// `check_limits` stage takes pre-materialised rules as a parameter.
#[derive(Debug, Clone)]
pub struct TestIdentity {
    pub user_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestResponse {
    /// Buffered-success terminal. Used by post-invoke stage tests
    /// to assert that `emit_audit` propagates the buffered response
    /// into `Emitted.response`.
    Ok,
    RateLimited {
        label: String,
    },
    RateLimiterUnavailable,
    AccessDenied {
        candidate: String,
    },
}

/// Streaming capture for test surface. Phase-2 unit tests only
/// need to assert the view branches; we don't need to model real
/// chunks, so a single counter is enough to distinguish hook
/// invocations from no-ops.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestStreamCaptured {
    pub events: u32,
}

/// Test post-invoke deps. Each hook bumps its own counter so unit
/// tests can verify call order + skip behaviour. Using
/// [`AtomicUsize`] keeps the deps `Sync` without forcing a borrow
/// dance through `Mutex` for the common case (only `last_invoked`
/// needs a mutex because it stores a non-Copy clone).
pub struct TestDeps {
    pub record_outcome_calls: AtomicUsize,
    pub write_cache_calls: AtomicUsize,
    pub record_usage_calls: AtomicUsize,
    pub emit_audit_calls: AtomicUsize,
    /// Ordered list of hook names as they fire. Used to assert
    /// `record_outcome → write_cache → record_usage → emit_audit`
    /// ordering.
    pub call_log: Mutex<Vec<&'static str>>,
    /// The view passed to `write_cache` last — `None` if the stage
    /// gated it. Tests assert this stays `None` for non-Natural
    /// streaming captures.
    pub write_cache_view_kind: Mutex<Option<&'static str>>,
}

impl Default for TestDeps {
    fn default() -> Self {
        Self {
            record_outcome_calls: AtomicUsize::new(0),
            write_cache_calls: AtomicUsize::new(0),
            record_usage_calls: AtomicUsize::new(0),
            emit_audit_calls: AtomicUsize::new(0),
            call_log: Mutex::new(Vec::new()),
            write_cache_view_kind: Mutex::new(None),
        }
    }
}

impl TestDeps {
    pub fn record_outcome(&self) -> usize {
        self.record_outcome_calls.load(Ordering::SeqCst)
    }
    pub fn write_cache(&self) -> usize {
        self.write_cache_calls.load(Ordering::SeqCst)
    }
    pub fn record_usage(&self) -> usize {
        self.record_usage_calls.load(Ordering::SeqCst)
    }
    pub fn emit_audit(&self) -> usize {
        self.emit_audit_calls.load(Ordering::SeqCst)
    }
    pub fn order(&self) -> Vec<&'static str> {
        self.call_log.lock().unwrap().clone()
    }
    pub fn write_cache_kind(&self) -> Option<&'static str> {
        *self.write_cache_view_kind.lock().unwrap()
    }
}

pub struct TestSurface;

impl Surface for TestSurface {
    type Identity = TestIdentity;
    type RequestBody = serde_json::Value;
    type Response = TestResponse;
    /// Marker shape — unit tests don't exercise the streaming
    /// wire body so a unit struct is sufficient.
    type StreamResponse = ();
    type AuditDetail = serde_json::Value;
    type StreamCaptured = TestStreamCaptured;
    type PostInvokeDeps = TestDeps;

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

    async fn record_outcome(deps: &Self::PostInvokeDeps, _invoked: &Invoked<Self>) {
        deps.record_outcome_calls.fetch_add(1, Ordering::SeqCst);
        deps.call_log.lock().unwrap().push("record_outcome");
    }

    async fn write_cache(deps: &Self::PostInvokeDeps, invoked: &Invoked<Self>) {
        deps.write_cache_calls.fetch_add(1, Ordering::SeqCst);
        deps.call_log.lock().unwrap().push("write_cache");
        let kind = match &invoked.view {
            super::state::CapturedView::Buffered(_) => "buffered",
            super::state::CapturedView::Streaming { .. } => "streaming",
        };
        *deps.write_cache_view_kind.lock().unwrap() = Some(kind);
    }

    async fn record_usage(deps: &Self::PostInvokeDeps, _invoked: &Invoked<Self>) {
        deps.record_usage_calls.fetch_add(1, Ordering::SeqCst);
        deps.call_log.lock().unwrap().push("record_usage");
    }

    async fn emit_audit(deps: &Self::PostInvokeDeps, _invoked: &Invoked<Self>) {
        deps.emit_audit_calls.fetch_add(1, Ordering::SeqCst);
        deps.call_log.lock().unwrap().push("emit_audit");
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

/// Build an [`Invoked`] with a [`CapturedView::Buffered`] payload.
/// Convenience for post-invoke stage tests.
pub fn make_buffered_invoked(user_id: Uuid, response: TestResponse) -> Invoked<TestSurface> {
    let raw = make_raw(user_id);
    Invoked {
        identity: raw.identity,
        trace_id: raw.trace_id,
        started_at: raw.started_at,
        client_ip: raw.client_ip,
        limit_check: super::state::LimitCheckRecord {
            currents: Vec::new(),
        },
        access_candidate: "test-candidate".to_owned(),
        view: super::state::CapturedView::Buffered(response),
    }
}

/// Build an [`Invoked`] with a [`CapturedView::Streaming`] payload.
/// `outcome` controls the success gate inside `write_cache`.
pub fn make_streaming_invoked(
    user_id: Uuid,
    outcome: super::StreamOutcome,
) -> Invoked<TestSurface> {
    let raw = make_raw(user_id);
    Invoked {
        identity: raw.identity,
        trace_id: raw.trace_id,
        started_at: raw.started_at,
        client_ip: raw.client_ip,
        limit_check: super::state::LimitCheckRecord {
            currents: Vec::new(),
        },
        access_candidate: "test-candidate".to_owned(),
        view: super::state::CapturedView::Streaming {
            outcome,
            captured: TestStreamCaptured::default(),
        },
    }
}
