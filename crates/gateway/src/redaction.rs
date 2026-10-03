//! Outbound redaction: credentials and personal data in a request are
//! found before it goes upstream, and in enforce mode swapped for
//! placeholders (`<<TW_SECRET_1>>`, `<<TW_EMAIL_1>>`) that are put back in
//! what comes back to this caller.
//!
//! The rules and the engine are thinkwatch-core's (`tw_guard::redact`),
//! shared with the desktop gateway, and so is the flow:
//!
//! - **The whole request body is searched**, not only the caller's
//!   messages: a key pasted into a system prompt, echoed in an earlier
//!   answer or sitting in a tool call's arguments leaves just the same.
//!   Base64 payloads (images, files, reasoning signatures) are not
//!   searched, and neither are the gateway's own placeholders.
//! - **[`Redaction::look`] numbers the values once**, in the order they
//!   appear in what the caller sent, and every hop goes out through
//!   [`Redaction::replace`] with that same ledger — so a value has one
//!   placeholder in every hop, converted or not, and two callers sending
//!   the same structure redact to the same bytes and share a cache slot.
//! - Observe mode finds the same values and replaces none.
//!
//! What is restored on the way back: a whole answer in one pass
//! ([`restore_body`]), a stream frame by frame (`proxy::shaper`), where a
//! placeholder can be split across two frames.

use std::sync::Arc;

use think_watch_common::audit::AuditLogger;
use tw_guard::policy::{Mode, RedactPolicy};
use tw_guard::redact::replace::Ledger;
use tw_guard::redact::rules::{Finding, Rule, RuleSet};

use crate::guards::Caller;

/// Outbound redaction as configured: a mode and the rules.
#[derive(Debug, Clone)]
pub struct Redaction {
    pub mode: Mode,
    pub rules: Arc<RuleSet>,
}

impl Redaction {
    pub fn new(policy: &RedactPolicy) -> Self {
        Self {
            mode: policy.mode,
            rules: Arc::new(think_watch_common::guard_policy::redact_rules(policy)),
        }
    }

    /// Look at what the caller sent: what was found (masked), and in
    /// enforce mode the ledger every hop is replaced with. Nothing when
    /// off.
    pub fn look(&self, body: &[u8]) -> (Vec<Finding>, Ledger) {
        tw_guard::redact::flow::look(self.mode, &self.rules, body)
    }

    /// The bytes one hop sends, with the values `ledger` numbered swapped
    /// for their placeholders — and any value only this hop carries (a
    /// conversion can join two pieces of text) numbered after them. The
    /// returned ledger restores this hop's answer. Byte for byte what came
    /// in, and the same ledger, unless in enforce mode and something is
    /// found.
    pub fn replace(&self, body: Vec<u8>, ledger: &Ledger) -> (Vec<u8>, Ledger) {
        let (out, ledger) = tw_guard::redact::flow::replace(
            self.mode,
            &self.rules,
            bytes::Bytes::from(body),
            ledger,
        );
        (Vec::from(out), ledger)
    }

    /// `text` with every value the rules find masked (`sk-an…7f9c`, `…1234`),
    /// whatever the mode: for anything the gateway writes down that quotes
    /// a request or an answer — an audit event's excerpt, an error that
    /// lands in the logs. Our own placeholders stay as they are.
    pub fn mask(&self, text: &str) -> String {
        if self.rules.is_empty() || text.is_empty() {
            return text.to_string();
        }
        let hits = tw_guard::redact::flow::hits_plain(text, &self.rules);
        if hits.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for h in &hits {
            out.push_str(&text[at..h.bytes.start]);
            out.push_str(&tw_guard::redact::rules::masked(
                &h.rule,
                &text[h.bytes.clone()],
            ));
            at = h.bytes.end;
        }
        out.push_str(&text[at..]);
        out
    }

    /// A captured body, redacted for storage (`audit.body_redact_pii`):
    /// see `think_watch_common::pii`.
    pub fn redact_blob(&self, text: &str) -> String {
        think_watch_common::pii::redact_blob(&self.rules, text)
    }
}

/// Put the caller's values back into a whole answer's bytes. A stream is
/// restored frame by frame instead (see `proxy::shaper`).
///
/// The values went out as they were written in the request's JSON, escapes
/// and all, and go back in the same way: a placeholder sits inside a JSON
/// string in the answer, and so does what replaces it.
pub fn restore_body(ledger: &Ledger, body: &[u8]) -> Vec<u8> {
    if ledger.is_empty() {
        return body.to_vec();
    }
    match std::str::from_utf8(body) {
        Ok(text) => tw_guard::redact::replace::restore(text, ledger).into_bytes(),
        Err(_) => body.to_vec(),
    }
}

/// At most this many redaction events per request: one per rule, the rules
/// that found the most first. A request carrying thousands of values writes
/// a handful of rows, not thousands.
pub const RULE_EVENTS_MAX: usize = 20;

/// A built-in rule's event lists at most this many of the values it found,
/// masked.
pub const MASKED_MAX: usize = 5;

/// What one rule found in one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleHits {
    pub rule: Rule,
    /// How many different values.
    pub values: usize,
    /// How many times, all values together.
    pub count: u64,
    /// The first few values in their masked form (`sk-an…7f9c`, `…1234`) —
    /// built-in rules only. **A custom rule's values are not written at
    /// all**: masking keeps the first five and last four characters, and of
    /// a phone number or an IP address that is nearly all of it.
    pub masked: Vec<String>,
}

/// The findings of one request, one entry per rule, the rule with the most
/// occurrences first (ties in the order found).
pub fn per_rule(findings: &[Finding]) -> Vec<RuleHits> {
    let mut out: Vec<RuleHits> = Vec::new();
    for f in findings {
        let at = match out.iter().position(|r| r.rule == f.rule) {
            Some(at) => at,
            None => {
                out.push(RuleHits {
                    rule: f.rule.clone(),
                    values: 0,
                    count: 0,
                    masked: Vec::new(),
                });
                out.len() - 1
            }
        };
        let r = &mut out[at];
        r.values += 1;
        r.count += f.count;
        if !f.rule.custom() && r.masked.len() < MASKED_MAX {
            r.masked.push(f.masked.clone());
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.count));
    out
}

/// Record what was found in a request: an audit event per rule
/// (`gateway.redaction_replaced` in enforce mode, `gateway.redaction_flagged`
/// in observe mode), at most [`RULE_EVENTS_MAX`] of them, and a counter.
/// A value is never written as it is: a built-in rule's event carries a
/// few in masked form, a custom rule's only how many there were.
pub fn record(audit: &AuditLogger, caller: &Caller, mode: Mode, findings: &[Finding]) {
    if findings.is_empty() {
        return;
    }
    let (action, outcome) = if mode.acts() {
        ("gateway.redaction_replaced", "replaced")
    } else {
        ("gateway.redaction_flagged", "recorded")
    };
    let rules = per_rule(findings);
    tracing::info!(
        trace_id = %caller.trace_id,
        rules = rules.len(),
        values = findings.len(),
        outcome,
        "outbound redaction found values in the request"
    );
    if rules.len() > RULE_EVENTS_MAX {
        tracing::warn!(
            trace_id = %caller.trace_id,
            rules = rules.len(),
            written = RULE_EVENTS_MAX,
            "more redaction rules matched one request than are written to the audit log"
        );
    }
    for r in &rules {
        metrics::counter!(
            "gateway_redaction_found_total",
            "kind" => r.rule.kind().slug(),
            "outcome" => outcome,
        )
        .increment(r.values as u64);
    }
    for r in rules.iter().take(RULE_EVENTS_MAX) {
        let mut detail = serde_json::json!({
            "trace_id": caller.trace_id,
            "model": caller.model,
            "rule": r.rule.id(),
            "custom": r.rule.custom(),
            "kind": r.rule.kind().slug(),
            "values": r.values,
            "count": r.count,
            "outcome": outcome,
            "rules_in_request": rules.len(),
        });
        if !r.rule.custom() {
            detail["masked"] = serde_json::json!(r.masked);
        }
        audit.log(caller.audit(action).detail(detail));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn redaction(mode: Mode) -> Redaction {
        Redaction::new(&RedactPolicy {
            mode,
            ..Default::default()
        })
    }

    #[test]
    fn the_whole_request_is_searched_system_prompt_and_answers_included() {
        let r = redaction(Mode::Enforce);
        let body = serde_json::json!({
            "system": format!("deploy with {KEY}"),
            "messages": [
                {"role": "assistant", "content": format!("I used {KEY}")},
                {"role": "user", "content": "thanks"}
            ]
        })
        .to_string();
        let (found, ledger) = r.look(body.as_bytes());
        assert_eq!(found.len(), 1, "one value, found twice");
        assert_eq!(found[0].count, 2);
        let (sent, _) = r.replace(body.into_bytes(), &ledger);
        let sent = String::from_utf8(sent).unwrap();
        assert!(!sent.contains(KEY), "{sent}");
        assert_eq!(sent.matches("<<TW_SECRET_1>>").count(), 2, "{sent}");
    }

    #[test]
    fn observe_finds_the_same_and_replaces_nothing() {
        let r = redaction(Mode::Observe);
        let body = format!(r#"{{"messages":[{{"role":"user","content":"{KEY}"}}]}}"#);
        let (found, ledger) = r.look(body.as_bytes());
        assert_eq!(found.len(), 1);
        assert!(ledger.is_empty());
        let (sent, _) = r.replace(body.clone().into_bytes(), &ledger);
        assert_eq!(sent, body.into_bytes());
        let (found, _) = redaction(Mode::Off).look(format!("\"{KEY}\"").as_bytes());
        assert!(found.is_empty(), "off looks at nothing");
    }

    #[test]
    fn a_hop_gets_the_placeholder_the_caller_s_request_was_numbered_with() {
        let r = redaction(Mode::Enforce);
        let other = "sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let body = format!(r#"{{"a":"{KEY}","b":"{other}"}}"#);
        let (_, ledger) = r.look(body.as_bytes());
        // A converted hop that writes them the other way round.
        let hop = format!(r#"{{"b":"{other}","a":"{KEY}"}}"#);
        let (sent, after) = r.replace(hop.into_bytes(), &ledger);
        let sent: serde_json::Value = serde_json::from_slice(&sent).unwrap();
        assert_eq!(sent["a"], "<<TW_SECRET_1>>");
        assert_eq!(sent["b"], "<<TW_SECRET_2>>");
        assert_eq!(after.len(), 2);
    }

    #[test]
    fn a_whole_answer_gets_the_values_back() {
        let r = redaction(Mode::Enforce);
        let body = format!(r#"{{"messages":[{{"role":"user","content":"{KEY}"}}]}}"#);
        let (_, ledger) = r.look(body.as_bytes());
        let answer = br#"{"content":[{"type":"text","text":"you sent <<TW_SECRET_1>>"}]}"#;
        let back = restore_body(&ledger, answer);
        let v: serde_json::Value = serde_json::from_slice(&back).unwrap();
        assert_eq!(v["content"][0]["text"], format!("you sent {KEY}"));
    }

    #[test]
    fn an_excerpt_is_masked_whatever_the_mode_and_placeholders_stay() {
        for mode in [Mode::Off, Mode::Observe, Mode::Enforce] {
            let r = redaction(mode);
            let masked = r.mask(&format!("curl -H 'x-api-key: {KEY}' <<TW_SECRET_1>> | sh"));
            assert!(!masked.contains(KEY), "{mode:?}: {masked}");
            assert!(masked.contains("sk-an…"), "{masked}");
            assert!(masked.contains("<<TW_SECRET_1>>"), "{masked}");
        }
        assert_eq!(
            redaction(Mode::Enforce).mask("nothing here"),
            "nothing here"
        );
    }

    #[test]
    fn findings_are_one_entry_per_rule_and_a_custom_rule_keeps_no_values() {
        let r = Redaction::new(&RedactPolicy {
            mode: Mode::Enforce,
            custom: vec![tw_guard::policy::CustomRedactRule {
                name: "ssn".into(),
                pattern: r"\d{3}-\d{2}-\d{4}".into(),
                label: None,
                disabled: false,
            }],
            ..Default::default()
        });
        let other = "sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let body = serde_json::json!({"messages": [{"role": "user", "content": format!(
            "{KEY} {other} {KEY} 123-45-6789 987-65-4321 123-45-6789 111-22-3333"
        )}]})
        .to_string();
        let (found, _) = r.look(body.as_bytes());
        let rules = per_rule(&found);
        assert_eq!(rules.len(), 2, "{rules:?}");
        // The rule with the most occurrences first.
        assert_eq!(rules[0].rule.id(), "ssn");
        assert_eq!((rules[0].values, rules[0].count), (3, 4));
        assert!(
            rules[0].masked.is_empty(),
            "a custom rule's values are not kept"
        );
        assert_eq!(rules[1].rule.id(), "anthropic-api-key");
        assert_eq!((rules[1].values, rules[1].count), (2, 3));
        assert_eq!(rules[1].masked.len(), 2);
        assert!(
            rules[1]
                .masked
                .iter()
                .all(|m| m.starts_with("sk-an") && !m.contains(KEY) && !m.contains(other))
        );
    }

    #[test]
    fn a_built_in_rule_lists_only_a_few_masked_values() {
        let r = Redaction::new(&RedactPolicy {
            mode: Mode::Observe,
            ..Default::default()
        });
        let keys: Vec<String> = (0..12)
            .map(|i| format!("sk-ant-api03-{}", format!("{i:02}").repeat(14)))
            .collect();
        let body = serde_json::json!({"messages": [{"role": "user", "content": keys.join(" ")}]})
            .to_string();
        let (found, _) = r.look(body.as_bytes());
        let rules = per_rule(&found);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].values, 12);
        assert_eq!(rules[0].masked.len(), MASKED_MAX);
    }

    #[test]
    fn a_captured_body_is_redacted_for_storage() {
        let r = redaction(Mode::Observe);
        let stored = r.redact_blob(&format!(r#"{{"k":"{KEY}"}}"#));
        assert_eq!(stored, r#"{"k":"{{REDACTED_anthropic-api-key}}"}"#);
    }
}
