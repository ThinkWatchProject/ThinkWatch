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
use tw_guard::redact::rules::{Finding, RuleSet};

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

/// Record what was found in a request: one audit event per value
/// (`gateway.redaction_replaced` in enforce mode, `gateway.redaction_flagged`
/// in observe mode) and a counter. The value itself is never written —
/// only its masked form.
pub fn record(audit: &AuditLogger, caller: &Caller, mode: Mode, findings: &[Finding]) {
    if findings.is_empty() {
        return;
    }
    let replaced = mode.acts();
    let (action, outcome) = if replaced {
        ("gateway.redaction_replaced", "replaced")
    } else {
        ("gateway.redaction_flagged", "recorded")
    };
    tracing::info!(
        trace_id = %caller.trace_id,
        found = findings.len(),
        outcome,
        "outbound redaction found values in the request"
    );
    for f in findings {
        metrics::counter!(
            "gateway_redaction_found_total",
            "kind" => f.rule.kind().slug(),
            "outcome" => outcome,
        )
        .increment(1);
        audit.log(caller.audit(action).detail(serde_json::json!({
            "trace_id": caller.trace_id,
            "model": caller.model,
            "rule": f.rule.id(),
            "custom": f.rule.custom(),
            "kind": f.rule.kind().slug(),
            "masked": f.masked,
            "count": f.count,
            "outcome": outcome,
        })));
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
    fn a_captured_body_is_redacted_for_storage() {
        let r = redaction(Mode::Observe);
        let stored = r.redact_blob(&format!(r#"{{"k":"{KEY}"}}"#));
        assert_eq!(stored, r#"{"k":"{{REDACTED_anthropic-api-key}}"}"#);
    }
}
