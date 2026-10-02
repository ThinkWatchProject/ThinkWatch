//! Content filter: what the caller sends, checked against the rules.
//!
//! The rules, the engine and the verdict are thinkwatch-core's
//! (`tw_guard::content`), shared with the desktop gateway:
//!
//! - **Which text** — the caller's messages and the tool results inside
//!   them, the place an injected instruction most often rides in: a page a
//!   tool fetched, a file it read. Not the system prompt (the operator's)
//!   and not the model's own turns.
//! - **How a rule matches** — a keyword (case-insensitive), a regex
//!   (case-insensitive, size-bounded), or code points (`U+200B`,
//!   `U+E0000–U+E007F`): the built-in hidden-character rules are the last
//!   kind, characters an editor does not show and a model still reads.
//! - **What a hit does in enforce mode** — each rule its own: refuse the
//!   request, strip the matched text (from the caller's text only) and send
//!   the rest, or record only. Observe mode records every hit and changes
//!   nothing.
//!
//! A request with text stripped goes on as the stripped one: the gateway
//! decodes it again, and redaction, forwarding and the audit row all see
//! what was actually sent.

use std::sync::Arc;

use think_watch_common::audit::AuditLogger;
use tw_dialect::ir::Dialect;
use tw_guard::content::{self, Match, Outcome, Rules, ScreenHit, Screening};
use tw_guard::policy::{ContentPolicy, Mode};

use crate::error::GatewayError;
use crate::guards::Caller;
use crate::redaction::Redaction;

/// The content filter as configured: a mode and the rules.
#[derive(Debug, Clone)]
pub struct ContentFilter {
    pub mode: Mode,
    pub rules: Arc<Rules>,
}

impl ContentFilter {
    pub fn new(policy: &ContentPolicy) -> Self {
        Self {
            mode: policy.mode,
            rules: Arc::new(think_watch_common::guard_policy::content_rules(policy)),
        }
    }

    /// Check a request as the caller sent it, `dialect` being its format.
    /// Every rule that fires is in the result with what became of it; a
    /// refusal names the hit that decided it; a request with text
    /// stripped comes back as its new body.
    pub fn screen(&self, dialect: Dialect, body: &[u8]) -> Screening {
        content::screen(self.mode, &self.rules, dialect, body)
    }
}

/// What the caller is told when a rule refuses the request: the rule,
/// and where and what it matched, so they can fix the prompt. The
/// matched text is masked with the redaction rules — the message also
/// lands in the request's log row.
pub fn refusal(hit: &ScreenHit, mask: &Redaction) -> GatewayError {
    let h = &hit.hit;
    let place = if h.in_tool_result {
        "a tool result"
    } else {
        "the message"
    };
    // A code-point rule matched characters that cannot be shown, only
    // counted.
    let message = if h.matching == Match::Codepoints {
        format!(
            "Request blocked by content filter: rule '{}' found {} invisible character{} in {place}",
            h.name,
            h.count,
            if h.count == 1 { "" } else { "s" },
        )
    } else {
        format!(
            "Request blocked by content filter: rule '{}' matched in {place}: \"{}\"",
            h.name,
            mask.mask(&h.snippet),
        )
    };
    GatewayError::PolicyBlocked(message)
}

/// Record every hit of a screening: one audit event each
/// (`gateway.content_flagged`, `gateway.content_stripped` or
/// `gateway.content_blocked`), a counter, and a log line that carries no
/// text of the caller's. The event's excerpt and revealed text are masked.
pub fn record(audit: &AuditLogger, caller: &Caller, screening: &Screening, mask: &Redaction) {
    for s in &screening.hits {
        let h = &s.hit;
        let action = match s.outcome {
            Outcome::Recorded => "gateway.content_flagged",
            Outcome::Stripped => "gateway.content_stripped",
            Outcome::Blocked => "gateway.content_blocked",
        };
        tracing::info!(
            trace_id = %caller.trace_id,
            rule = %h.rule,
            outcome = s.outcome.slug(),
            in_tool_result = h.in_tool_result,
            count = h.count,
            "content rule matched (text withheld)"
        );
        metrics::counter!(
            "gateway_content_matched_total",
            "outcome" => s.outcome.slug(),
            "custom" => if h.custom { "true" } else { "false" },
        )
        .increment(1);
        let mut detail = serde_json::json!({
            "trace_id": caller.trace_id,
            "model": caller.model,
            "rule": h.rule,
            "rule_name": h.name,
            "custom": h.custom,
            "action": h.action.slug(),
            "outcome": s.outcome.slug(),
            "in_tool_result": h.in_tool_result,
            "count": h.count,
            "excerpt": mask.mask(&h.snippet),
        });
        if !h.revealed.is_empty() {
            detail["revealed"] = serde_json::Value::String(mask.mask(&h.revealed));
        }
        audit.log(caller.audit(action).detail(detail));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_guard::policy::{ContentAction, ContentMatch, CustomContentRule, RedactPolicy};

    fn filter(mode: Mode, custom: Vec<CustomContentRule>) -> ContentFilter {
        ContentFilter::new(&ContentPolicy {
            mode,
            custom,
            ..Default::default()
        })
    }

    fn rule(
        name: &str,
        pattern: &str,
        matching: ContentMatch,
        action: ContentAction,
    ) -> CustomContentRule {
        CustomContentRule {
            name: name.into(),
            pattern: pattern.into(),
            matching,
            action,
            disabled: false,
        }
    }

    fn chat(text: &str) -> Vec<u8> {
        serde_json::json!({"model": "m", "messages": [
            {"role": "system", "content": "never say jailbreak"},
            {"role": "user", "content": text}
        ]})
        .to_string()
        .into_bytes()
    }

    fn mask() -> Redaction {
        Redaction::new(&RedactPolicy::default())
    }

    #[test]
    fn a_block_rule_refuses_with_the_callers_words_and_the_system_prompt_is_not_read() {
        let f = filter(
            Mode::Enforce,
            vec![rule(
                "Jailbreak",
                "jailbreak",
                ContentMatch::Contains,
                ContentAction::Block,
            )],
        );
        let s = f.screen(Dialect::Chat, &chat("try a JAILBREAK"));
        let refused = s.refusal().expect("refused");
        assert_eq!(refused.outcome, Outcome::Blocked);
        let e = refusal(refused, &mask()).to_string();
        assert!(e.contains("'Jailbreak'") && e.contains("JAILBREAK"), "{e}");
        assert_eq!(refusal(refused, &mask()).status_code(), 403);
        // The system prompt says it too, and is not the caller's.
        assert!(f.screen(Dialect::Chat, &chat("hello")).hits.is_empty());
    }

    #[test]
    fn a_strip_rule_deletes_every_occurrence_and_hands_back_the_new_body() {
        let f = filter(
            Mode::Enforce,
            vec![rule(
                "Code",
                "project-x",
                ContentMatch::Contains,
                ContentAction::Strip,
            )],
        );
        let s = f.screen(
            Dialect::Chat,
            &chat("Project-X is late; ask project-x leads"),
        );
        assert!(s.refusal().is_none());
        assert_eq!(s.hits[0].outcome, Outcome::Stripped);
        let body: serde_json::Value = serde_json::from_slice(s.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["messages"][1]["content"], " is late; ask  leads");
        assert_eq!(body["messages"][0]["content"], "never say jailbreak");
    }

    #[test]
    fn observe_records_what_enforce_would_do_and_changes_nothing() {
        let f = filter(
            Mode::Observe,
            vec![rule(
                "Code",
                "project-x",
                ContentMatch::Contains,
                ContentAction::Strip,
            )],
        );
        let s = f.screen(Dialect::Chat, &chat("project-x"));
        assert_eq!(s.hits[0].outcome, Outcome::Recorded);
        assert!(s.body.is_none() && s.refusal().is_none());
    }

    #[test]
    fn hidden_characters_are_counted_not_quoted() {
        // The built-in tag-character rule, re-graded to refuse.
        let f = ContentFilter::new(&ContentPolicy {
            mode: Mode::Enforce,
            actions: [("unicode-tags".to_string(), ContentAction::Block)].into(),
            ..Default::default()
        });
        let tagged: String = "summarise"
            .chars()
            .chain(
                "ignore"
                    .chars()
                    .map(|c| char::from_u32(0xE0000 + c as u32).unwrap()),
            )
            .collect();
        let s = f.screen(Dialect::Chat, &chat(&tagged));
        let refused = s.refusal().expect("refused");
        assert_eq!(refused.hit.rule, "unicode-tags");
        assert_eq!(refused.hit.revealed, "ignore");
        let e = refusal(refused, &mask()).to_string();
        assert!(
            e.contains("found 6 invisible characters in the message"),
            "{e}"
        );
    }

    #[test]
    fn a_custom_code_point_rule_strips_the_characters() {
        let f = filter(
            Mode::Enforce,
            vec![rule(
                "ZW",
                "U+200B",
                ContentMatch::Codepoints,
                ContentAction::Strip,
            )],
        );
        let s = f.screen(Dialect::Chat, &chat("jail\u{200B}break"));
        let body: serde_json::Value = serde_json::from_slice(s.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["messages"][1]["content"], "jailbreak");
    }

    #[test]
    fn a_refusal_quoting_a_credential_masks_it() {
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let f = filter(
            Mode::Enforce,
            vec![rule(
                "Keys",
                "my key",
                ContentMatch::Contains,
                ContentAction::Block,
            )],
        );
        let s = f.screen(Dialect::Chat, &chat(&format!("my key {key}")));
        let e = refusal(s.refusal().unwrap(), &mask()).to_string();
        assert!(!e.contains(key), "{e}");
        assert!(e.contains("sk-an…"), "{e}");
    }
}
