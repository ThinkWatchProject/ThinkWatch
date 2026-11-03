//! The request guards in force — secrets and privacy (outbound redaction),
//! the content filter (requests and answer text) and dangerous commands
//! (tool-call inspection) — compiled from their policies
//! (`security.redact`, `security.content`, `security.inspect_tools`; see
//! `think_watch_common::guard_policy`).
//!
//! One snapshot, swapped whole when a policy changes. A request takes it
//! once and runs every guard from it, so a change that lands mid-request
//! cannot redact one hop with the old rules and restore with the new.
//!
//! Every hit is written to the audit log, one event per rule and request
//! (and, for the content filter, per side), so the log forwarders send it
//! on. A hit that is only recorded — the guard monitoring (`mode:
//! observe`), or the rule's action being to record — has an event of its
//! own:
//!
//! | guard | events |
//! |---|---|
//! | content filter | `gateway.content_flagged` (recorded), `gateway.content_stripped`, `gateway.content_replaced`, `gateway.content_blocked` (requests only); `direction` is `request` or `response` |
//! | secrets and privacy | `gateway.redaction_flagged` (recorded), `gateway.redaction_replaced` (replaced with placeholders) |
//! | dangerous commands | `gateway.tool_call_flagged` (recorded), `gateway.tool_call_blocked` (cut) |
//!
//! Each event's detail carries the rule, its action, the outcome and the
//! guard's `mode`.
//! **The audit log gets no text of the request.** It is forwarded and read
//! far more widely than the request it came from (`logs:read_all`, against
//! `logs:read_bodies` for bodies). A content event names the rule and
//! counts the matches; a redaction event names the rule and counts the
//! values, and only a built-in rule's carries a few, masked
//! (`sk-an…7f9c`) — except a saved provider key's, which names the
//! provider instead (`sources`); a tool-call event quotes the arguments that matched —
//! the upstream's text, not the caller's — masked with the redaction rules
//! ([`crate::redaction::Redaction::mask`]). A request writes at most a
//! score of events per guard, one per rule.

use std::sync::Arc;

use think_watch_common::audit::{AuditActor, AuditEntry, GatewayActor, LogType};
use think_watch_common::guard_policy::saved::SavedSecrets;
use tw_guard::policy::Security;

use crate::content_filter::ContentFilter;
use crate::redaction::Redaction;
use crate::tool_inspection::ToolInspection;

/// The three guards, compiled.
#[derive(Debug, Clone)]
pub struct Guards {
    pub redaction: Redaction,
    pub content: ContentFilter,
    pub tools: Arc<ToolInspection>,
}

impl Guards {
    /// Compile the policies, with the providers' credentials (`saved`):
    /// outbound redaction recognises them by value, and dangerous
    /// commands counts them as credentials, each with its provider's host. A custom rule that does
    /// not compile is left out, loudly, and the rest still run (see
    /// `think_watch_common::guard_policy::content_rules`).
    pub fn new(policy: &Security, saved: &SavedSecrets) -> Self {
        Self {
            redaction: Redaction::new(&policy.redact, saved),
            content: ContentFilter::new(&policy.content),
            tools: Arc::new(ToolInspection::new(&policy.inspect_tools, saved)),
        }
    }
}

impl Default for Guards {
    /// The factory policies: every guard observing.
    fn default() -> Self {
        Self::new(&Security::default(), &SavedSecrets::default())
    }
}

/// Who asked, for the audit events a hit writes.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub api_key_id: Option<String>,
    pub api_key_lineage_id: Option<String>,
    pub ip: Option<String>,
    pub trace_id: String,
    pub model: String,
}

impl Caller {
    pub fn of(
        identity: &crate::proxy::GatewayRequestIdentity,
        trace_id: &str,
        model: &str,
    ) -> Self {
        Self {
            user_id: identity.user_id.clone(),
            user_email: identity.user_email.clone(),
            api_key_id: identity.api_key_id.clone(),
            api_key_lineage_id: identity.api_key_lineage_id.clone(),
            ip: identity.ip_address.clone(),
            trace_id: trace_id.to_string(),
            model: model.to_string(),
        }
    }

    /// An audit event of `action` on behalf of this caller.
    pub(crate) fn audit(&self, action: &str) -> AuditEntry {
        GatewayActor {
            user_id: self.user_id.as_deref(),
            user_email: self.user_email.as_deref(),
            api_key_id: self.api_key_id.as_deref(),
            api_key_lineage_id: self.api_key_lineage_id.as_deref(),
            ip: self.ip.as_deref(),
            session_id: None,
        }
        .audit(action)
        .log_type(LogType::Audit)
    }
}
