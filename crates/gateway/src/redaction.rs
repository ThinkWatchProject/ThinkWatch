//! Outbound redaction: credentials and personal data in a request are
//! found before it goes upstream, and in enforce mode swapped for
//! placeholders (`<<TW_SECRET_1>>`, `<<TW_EMAIL_1>>`) that are put back in
//! what comes back to this caller.
//!
//! The rules and the engine are thinkwatch-core's (`tw_guard::redact`),
//! shared with the desktop gateway, and so is the flow:
//!
//! - **Only the text written in the request is searched**, located by the
//!   structure of the request's format ([`Shape`]): system prompts, every
//!   role's messages, reasoning text and summaries, tool-call arguments and
//!   inputs, tool results and tool descriptions. Not only the caller's
//!   messages — a key pasted into a system prompt, echoed in an earlier
//!   answer or sitting in a tool call's arguments leaves just the same.
//!   The protocol's own values — ids, types, roles, names, the model,
//!   signatures, encrypted content, media, settings — are never searched:
//!   change one character of a reasoning item's id and the upstream
//!   refuses the whole request, and nobody wrote them. Fields and blocks
//!   the list does not know are not searched either. Within the text,
//!   base64 payloads and the gateway's own placeholders are left alone.
//! - **[`Redaction::look`] numbers the values once**, in the order they
//!   appear in what the caller sent, and every hop goes out through
//!   [`Redaction::replace`] with that same ledger — so a value has one
//!   placeholder in every hop, converted or not, and two callers sending
//!   the same structure redact to the same bytes and share a cache slot.
//! - Observe mode finds the same values and replaces none, and so does a
//!   rule whose action is to record only: its values go out as they are,
//!   and there is nothing of theirs to put back.
//!
//! What is restored on the way back: a whole answer in one pass
//! ([`restore_body`]), a stream frame by frame (`proxy::shaper`), where a
//! placeholder can be split across two frames.

use std::sync::Arc;

use think_watch_common::audit::AuditLogger;
use think_watch_common::guard_policy::saved::{self, SavedSecrets};
use tw_guard::policy::{Mode, RedactPolicy};
use tw_guard::redact::flow::ScanCache;
use tw_guard::redact::replace::{Ledger, Scheme};
use tw_guard::redact::rules::{Finding, Kind, Rule, RuleSet, SavedSource};

pub use tw_guard::redact::flow::Shape;

use crate::guards::Caller;

/// Outbound redaction as configured: a mode, and the rules — with the
/// providers' credentials, recognised by value, and the exception values
/// that are not.
///
/// **One scan cache per compiled rule set** (thinkwatch-core's
/// [`ScanCache`]): a client sends the whole conversation every turn, and
/// the parts of it already scanned last turn are taken from the cache
/// instead of being scanned again. What is found is exactly what a scan
/// without it finds. The cache keeps hit positions and rule ids, never a
/// value, in this process's memory only; it is built with the rules and
/// dropped with them when a policy, the providers' keys or the exceptions
/// change.
#[derive(Debug, Clone)]
pub struct Redaction {
    pub mode: Mode,
    pub rules: Arc<RuleSet>,
    cache: Arc<ScanCache>,
}

impl Redaction {
    pub fn new(policy: &RedactPolicy, saved: &SavedSecrets) -> Self {
        Self {
            mode: policy.mode,
            rules: Arc::new(saved::with_saved(
                think_watch_common::guard_policy::redact_rules(policy),
                saved,
            )),
            cache: Arc::new(ScanCache::new()),
        }
    }

    /// Look at what the caller sent, a request of `shape`: what was found
    /// in its written text (masked), and in enforce mode the ledger every
    /// hop is replaced with. Nothing when off.
    pub fn look(&self, body: &[u8], shape: Shape) -> (Vec<Finding>, Ledger) {
        let l = tw_guard::redact::flow::look_hits_with(
            self.mode,
            &self.rules,
            body,
            shape,
            Ledger::new(Scheme::SECRET),
            Some(&self.cache),
        );
        (l.found, l.ledger)
    }

    /// The bytes one hop sends, a request of `shape` — the format it goes
    /// out in — with the values `ledger` numbered swapped for their
    /// placeholders, and any value only this hop carries (a conversion can
    /// join two pieces of text) numbered after them. Only the hop's written
    /// text is searched. The returned ledger restores this hop's answer.
    /// Byte for byte what came in, and the same ledger, unless in enforce
    /// mode and something is found.
    pub fn replace(&self, body: Vec<u8>, ledger: &Ledger, shape: Shape) -> (Vec<u8>, Ledger) {
        let (out, ledger) = tw_guard::redact::flow::replace_with(
            self.mode,
            &self.rules,
            bytes::Bytes::from(body),
            ledger,
            shape,
            Some(&self.cache),
        );
        (Vec::from(out), ledger)
    }

    /// [`Self::replace`] over the whole text rather than a request's
    /// written text: what is stored is an answer (or a stream of them), not
    /// a request, and it is never sent anywhere. Every value a rule finds
    /// anywhere but in a key or a base64 payload is replaced, in enforce
    /// mode, unless its rule only records.
    fn replace_whole(&self, body: Vec<u8>, ledger: &Ledger) -> (Vec<u8>, Ledger) {
        if !self.mode.acts() || self.rules.is_empty() {
            return (body, ledger.clone());
        }
        let Ok(text) = std::str::from_utf8(&body) else {
            return (body, ledger.clone());
        };
        let found = tw_guard::redact::flow::hits(text, &self.rules);
        let hits = self.rules.to_replace(&found);
        if hits.is_empty() {
            return (body, ledger.clone());
        }
        let r = tw_guard::redact::replace::apply(text, &hits, ledger.clone());
        (r.text.into_bytes(), r.ledger)
    }

    /// Restored text masked again for storage — a log row, never what is
    /// sent: every value the rules find gets its placeholder from `ledger`
    /// (or a new one), and then **every value `ledger` holds is put back
    /// as its placeholder wherever it appears as written**, JSON-escaped
    /// once or twice. The rules alone miss a value that needs its context
    /// to be recognised — a connection string's password, a custom rule
    /// written as `OPENAI_API_KEY=…` — when the model repeats it on its
    /// own; the ledger knows the value itself.
    pub fn mask_for_storage(&self, body: Vec<u8>, ledger: &Ledger) -> Vec<u8> {
        let (body, ledger) = self.replace_whole(body, ledger);
        match std::str::from_utf8(&body) {
            Ok(text) => match tw_guard::redact::replace::hide_values(
                text,
                &ledger,
                tw_guard::redact::replace::Form::Json,
            ) {
                std::borrow::Cow::Borrowed(_) => body,
                std::borrow::Cow::Owned(hidden) => hidden.into_bytes(),
            },
            Err(_) => body,
        }
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
/// The ledger holds each value as it reads, its JSON escapes undone, and
/// each one goes back escaped as deeply as its placeholder sits: once in a
/// JSON string, such as the answer's text, and twice inside a tool call's
/// arguments, which are JSON written into a JSON string. So a private
/// key's line breaks come back as line breaks, and the arguments still
/// parse. Bytes around the placeholders are left exactly as they were.
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
    /// a phone number or an IP address that is nearly all of it. **Nor is
    /// a saved secret's**, in any form: the event names where it came from
    /// instead ([`Self::sources`]).
    pub masked: Vec<String>,
    /// The saved secrets found, by where they came from (a provider's
    /// key), each once: the rule that recognises them by value only.
    pub sources: Vec<SavedSource>,
}

/// Whether a rule's values may be written masked: built-in rules, except
/// the one that recognises the providers' credentials by value.
fn writes_masked(rule: &Rule) -> bool {
    !rule.custom() && rule.kind() != Kind::Saved
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
                    sources: Vec::new(),
                });
                out.len() - 1
            }
        };
        let r = &mut out[at];
        r.values += 1;
        r.count += f.count;
        if writes_masked(&f.rule) && r.masked.len() < MASKED_MAX {
            r.masked.push(f.masked.clone());
        }
        if let Some(source) = &f.source
            && !r.sources.contains(source)
        {
            r.sources.push(source.clone());
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.count));
    out
}

/// Whether a rule's values were replaced or only recorded: replaced in
/// enforce mode, unless the rule's action is to record only.
fn replaced(redaction: &Redaction, rule: &Rule) -> bool {
    redaction.mode.acts() && !redaction.rules.records(rule)
}

/// Record what was found in a request: an audit event per rule
/// (`gateway.redaction_replaced` for values replaced with placeholders,
/// `gateway.redaction_flagged` for values only recorded — in observe mode,
/// or by a rule whose action is to record), at most [`RULE_EVENTS_MAX`] of
/// them, and a counter. A value is never written as it is: a built-in
/// rule's event carries a few in masked form, a custom rule's only how
/// many there were. A saved secret's event names where each came from
/// (`sources`: `[{"kind": "upstream-key", "name": <provider>}]`).
pub fn record(audit: &AuditLogger, caller: &Caller, redaction: &Redaction, findings: &[Finding]) {
    if findings.is_empty() {
        return;
    }
    let mode = redaction.mode;
    let rules = per_rule(findings);
    tracing::info!(
        trace_id = %caller.trace_id,
        rules = rules.len(),
        values = findings.len(),
        mode = mode.slug(),
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
        let outcome = if replaced(redaction, &r.rule) {
            "replaced"
        } else {
            "recorded"
        };
        metrics::counter!(
            "gateway_redaction_found_total",
            "kind" => r.rule.kind().slug(),
            "outcome" => outcome,
        )
        .increment(r.values as u64);
    }
    for r in rules.iter().take(RULE_EVENTS_MAX) {
        let (action, outcome) = if replaced(redaction, &r.rule) {
            ("gateway.redaction_replaced", "replaced")
        } else {
            ("gateway.redaction_flagged", "recorded")
        };
        let mut detail = serde_json::json!({
            "trace_id": caller.trace_id,
            "model": caller.model,
            "rule": r.rule.id(),
            "custom": r.rule.custom(),
            "kind": r.rule.kind().slug(),
            "values": r.values,
            "count": r.count,
            "action": if redaction.rules.records(&r.rule) { "record" } else { "placeholder" },
            "outcome": outcome,
            "mode": mode.slug(),
            "rules_in_request": rules.len(),
        });
        if r.rule.kind() == Kind::Saved {
            detail["sources"] = serde_json::json!(r.sources);
        } else if writes_masked(&r.rule) {
            detail["masked"] = serde_json::json!(r.masked);
        }
        audit.log(caller.audit(action).detail(detail));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tw_dialect::ir::Dialect;

    const KEY: &str = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    const ANTHROPIC: Shape = Shape::Generate(Dialect::Anthropic);
    const CHAT: Shape = Shape::Generate(Dialect::Chat);
    const RESPONSES: Shape = Shape::Generate(Dialect::Responses);

    fn redaction(mode: Mode) -> Redaction {
        Redaction::new(
            &RedactPolicy {
                mode,
                ..Default::default()
            },
            &Default::default(),
        )
    }

    #[test]
    fn every_written_text_is_searched_system_prompt_and_answers_included() {
        let r = redaction(Mode::Enforce);
        let body = serde_json::json!({
            "system": format!("deploy with {KEY}"),
            "messages": [
                {"role": "assistant", "content": format!("I used {KEY}")},
                {"role": "user", "content": "thanks"}
            ]
        })
        .to_string();
        let (found, ledger) = r.look(body.as_bytes(), ANTHROPIC);
        assert_eq!(found.len(), 1, "one value, found twice");
        assert_eq!(found[0].count, 2);
        let (sent, _) = r.replace(body.into_bytes(), &ledger, ANTHROPIC);
        let sent = String::from_utf8(sent).unwrap();
        assert!(!sent.contains(KEY), "{sent}");
        assert_eq!(sent.matches("<<TW_SECRET_1>>").count(), 2, "{sent}");
    }

    #[test]
    fn observe_finds_the_same_and_replaces_nothing() {
        let r = redaction(Mode::Observe);
        let body = format!(r#"{{"messages":[{{"role":"user","content":"{KEY}"}}]}}"#);
        let (found, ledger) = r.look(body.as_bytes(), CHAT);
        assert_eq!(found.len(), 1);
        assert!(ledger.is_empty());
        let (sent, _) = r.replace(body.clone().into_bytes(), &ledger, CHAT);
        assert_eq!(sent, body.clone().into_bytes());
        let (found, _) = redaction(Mode::Off).look(body.as_bytes(), CHAT);
        assert!(found.is_empty(), "off looks at nothing");
    }

    #[test]
    fn a_hop_gets_the_placeholder_the_caller_s_request_was_numbered_with() {
        let r = redaction(Mode::Enforce);
        let other = "sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        // Written out: `json!` would sort `messages` before `system`.
        let body =
            format!(r#"{{"system":"{KEY}","messages":[{{"role":"user","content":"{other}"}}]}}"#);
        let (_, ledger) = r.look(body.as_bytes(), ANTHROPIC);
        // A converted hop, in another format, that writes them the other
        // way round.
        let hop = serde_json::json!({"messages": [
            {"role": "user", "content": other},
            {"role": "system", "content": KEY}
        ]})
        .to_string();
        let (sent, after) = r.replace(hop.into_bytes(), &ledger, CHAT);
        let sent: serde_json::Value = serde_json::from_slice(&sent).unwrap();
        assert_eq!(sent["messages"][1]["content"], "<<TW_SECRET_1>>");
        assert_eq!(sent["messages"][0]["content"], "<<TW_SECRET_2>>");
        assert_eq!(after.len(), 2);
    }

    #[test]
    fn a_whole_answer_gets_the_values_back() {
        let r = redaction(Mode::Enforce);
        let body = format!(r#"{{"messages":[{{"role":"user","content":"{KEY}"}}]}}"#);
        let (_, ledger) = r.look(body.as_bytes(), CHAT);
        let answer = br#"{"content":[{"type":"text","text":"you sent <<TW_SECRET_1>>"}]}"#;
        let back = restore_body(&ledger, answer);
        let v: serde_json::Value = serde_json::from_slice(&back).unwrap();
        assert_eq!(v["content"][0]["text"], format!("you sent {KEY}"));
    }

    const PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Qu\n-----END RSA PRIVATE KEY-----";

    /// The ledger of a request that pasted a private key, and the request.
    fn pem_request() -> (String, Ledger) {
        let r = redaction(Mode::Enforce);
        let body = serde_json::json!({"messages": [
            {"role": "user", "content": format!("deploy with this key:\n{PEM}")}
        ]})
        .to_string();
        // In the JSON the key's line breaks are written `\n`.
        assert!(body.contains("KEY-----\\nMIIB"), "{body}");
        let (_, ledger) = r.look(body.as_bytes(), CHAT);
        assert_eq!(ledger.len(), 1);
        (body, ledger)
    }

    #[test]
    fn a_value_with_line_breaks_comes_back_whole_in_text_and_in_tool_arguments() {
        let (body, ledger) = pem_request();
        let (sent, _) = redaction(Mode::Enforce).replace(body.into_bytes(), &ledger, CHAT);
        let sent = String::from_utf8(sent).unwrap();
        assert!(
            sent.contains("<<TW_SECRET_1>>") && !sent.contains("MIIB"),
            "{sent}"
        );

        // A tool call's arguments are JSON inside a JSON string: the key
        // goes back escaped twice there, once in the text.
        let args = serde_json::json!({"path": "id_rsa", "content": "<<TW_SECRET_1>>"}).to_string();
        let answer = serde_json::json!({"choices": [{"message": {
            "content": "Saved <<TW_SECRET_1>>",
            "tool_calls": [{"id": "c1", "type": "function",
                "function": {"name": "write_file", "arguments": args}}]
        }}]})
        .to_string();
        let back = restore_body(&ledger, answer.as_bytes());
        let v: serde_json::Value = serde_json::from_slice(&back).expect("still JSON");
        let m = &v["choices"][0]["message"];
        assert_eq!(m["content"], format!("Saved {PEM}"));
        let args: serde_json::Value = serde_json::from_str(
            m["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .expect("the arguments still parse");
        assert_eq!(args["content"], PEM);

        // Anthropic's tool input is an object: one level, like the text.
        let answer = serde_json::json!({"content": [
            {"type": "tool_use", "id": "t1", "name": "write_file",
             "input": {"content": "<<TW_SECRET_1>>"}}
        ]})
        .to_string();
        let v: serde_json::Value =
            serde_json::from_slice(&restore_body(&ledger, answer.as_bytes())).unwrap();
        assert_eq!(v["content"][0]["input"]["content"], PEM);
    }

    #[test]
    fn an_answer_with_nothing_to_restore_is_passed_on_byte_for_byte() {
        let (_, ledger) = pem_request();
        let answer = b"{ \"text\": \"caf\\u00e9\",\n  \"arguments\": \"{}\" }";
        assert_eq!(restore_body(&ledger, answer), answer.to_vec());
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
    fn a_saved_secret_s_hits_name_their_provider_and_never_the_value() {
        use think_watch_common::guard_policy::saved::ProviderCredentials;
        let a = [(
            "Authorization".to_string(),
            "Bearer test-alpha-key-0123456789".to_string(),
        )];
        let b = [(
            "x-api-key".to_string(),
            "test-beta-key-0123456789".to_string(),
        )];
        let saved = SavedSecrets::collect([
            ProviderCredentials {
                name: "alpha",
                base_url: "https://a.example.com",
                headers: &a,
                aws_secret_access_key: None,
            },
            ProviderCredentials {
                name: "beta",
                base_url: "https://b.example.com",
                headers: &b,
                aws_secret_access_key: None,
            },
        ]);
        let r = Redaction::new(
            &RedactPolicy {
                mode: Mode::Enforce,
                ..Default::default()
            },
            &saved,
        );
        let body = serde_json::json!({"messages": [{"role": "user", "content":
            "key test-beta-key-0123456789, again test-beta-key-0123456789, and test-alpha-key-0123456789"
        }]})
        .to_string();
        let (found, ledger) = r.look(body.as_bytes(), CHAT);
        let rules = per_rule(&found);
        assert_eq!(rules.len(), 1, "{rules:?}");
        let hits = &rules[0];
        assert_eq!(hits.rule.id(), "thinkwatch-saved-secret");
        assert_eq!((hits.values, hits.count), (2, 3));
        assert!(hits.masked.is_empty(), "never a value, masked or not");
        let names: Vec<&str> = hits.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["beta", "alpha"]);
        assert_eq!(
            serde_json::json!(hits.sources[0]),
            serde_json::json!({"kind": "upstream-key", "name": "beta"})
        );
        // Replaced on the wire like any other value.
        let (sent, _) = r.replace(body.into_bytes(), &ledger, CHAT);
        let sent = String::from_utf8(sent).unwrap();
        assert!(!sent.contains("test-beta-key"), "{sent}");
        assert!(!writes_masked(&Rule::Custom("ssn".into())));
    }

    #[test]
    fn an_exception_value_is_neither_replaced_nor_recorded() {
        let other = "sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let r = Redaction::new(
            &RedactPolicy {
                mode: Mode::Enforce,
                exceptions: vec![tw_guard::policy::RedactException {
                    sha256: tw_guard::redact::exceptions::sha256_hex(KEY),
                    masked: "sk-an…AAAA".into(),
                    rule: "anthropic-api-key".into(),
                    added_at: 1,
                }],
                ..Default::default()
            },
            &Default::default(),
        );
        let body = serde_json::json!({"messages": [
            {"role": "user", "content": format!("{KEY} and {other}")}
        ]})
        .to_string();
        let (found, ledger) = r.look(body.as_bytes(), CHAT);
        assert_eq!(found.len(), 1, "{found:?}");
        let (sent, _) = r.replace(body.into_bytes(), &ledger, CHAT);
        let sent = String::from_utf8(sent).unwrap();
        assert!(sent.contains(KEY) && !sent.contains(other), "{sent}");
    }

    /// The scan cache takes the unchanged history from the turn before; the
    /// findings, the numbering and what is sent are the same as without it.
    #[test]
    fn a_growing_conversation_redacts_the_same_turn_after_turn() {
        let r = redaction(Mode::Enforce);
        let filler = "lorem ipsum dolor sit amet ".repeat(200);
        let mut messages = Vec::new();
        for turn in 0..30 {
            let content = if turn % 7 == 0 {
                format!(
                    "{filler} turn {turn} key sk-ant-api03-{}",
                    format!("{turn:02}").repeat(14)
                )
            } else {
                format!("{filler} turn {turn}")
            };
            messages.push(serde_json::json!({"role": "user", "content": content}));
            let body = serde_json::json!({"messages": messages}).to_string();
            let cached = r.look(body.as_bytes(), CHAT);
            let plain =
                tw_guard::redact::flow::look(Mode::Enforce, &r.rules, body.as_bytes(), CHAT);
            assert_eq!(cached.0, plain.0, "turn {turn}");
            let (a, _) = r.replace(body.clone().into_bytes(), &cached.1, CHAT);
            let (b, _) = tw_guard::redact::flow::replace(
                Mode::Enforce,
                &r.rules,
                bytes::Bytes::from(body),
                &plain.1,
                CHAT,
            );
            assert_eq!(a, b.to_vec(), "turn {turn}");
        }
    }

    #[test]
    fn findings_are_one_entry_per_rule_and_a_custom_rule_keeps_no_values() {
        let r = Redaction::new(
            &RedactPolicy {
                mode: Mode::Enforce,
                custom: vec![tw_guard::policy::CustomRedactRule {
                    name: "ssn".into(),
                    pattern: r"\d{3}-\d{2}-\d{4}".into(),
                    matching: tw_guard::policy::ContentMatch::Regex,
                    case_sensitive: true,
                    label: None,
                    action: Default::default(),
                    disabled: false,
                }],
                ..Default::default()
            },
            &Default::default(),
        );
        let other = "sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let body = serde_json::json!({"messages": [{"role": "user", "content": format!(
            "{KEY} {other} {KEY} 123-45-6789 987-65-4321 123-45-6789 111-22-3333"
        )}]})
        .to_string();
        let (found, _) = r.look(body.as_bytes(), CHAT);
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
        let r = Redaction::new(
            &RedactPolicy {
                mode: Mode::Observe,
                ..Default::default()
            },
            &Default::default(),
        );
        let keys: Vec<String> = (0..12)
            .map(|i| format!("sk-ant-api03-{}", format!("{i:02}").repeat(14)))
            .collect();
        let body = serde_json::json!({"messages": [{"role": "user", "content": keys.join(" ")}]})
            .to_string();
        let (found, _) = r.look(body.as_bytes(), CHAT);
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

    #[test]
    fn a_rule_that_records_only_is_found_and_reported_but_not_replaced() {
        let r = Redaction::new(
            &RedactPolicy {
                mode: Mode::Enforce,
                actions: [(
                    "anthropic-api-key".to_string(),
                    tw_guard::policy::RedactAction::Record,
                )]
                .into(),
                custom: vec![tw_guard::policy::CustomRedactRule {
                    name: "project".into(),
                    pattern: "PRJ-7".into(),
                    matching: tw_guard::policy::ContentMatch::Contains,
                    case_sensitive: false,
                    label: Some("PROJECT".into()),
                    action: tw_guard::policy::RedactAction::Placeholder,
                    disabled: false,
                }],
                ..Default::default()
            },
            &Default::default(),
        );
        let body =
            serde_json::json!({"messages": [{"role": "user", "content": format!("{KEY} PRJ-7")}]})
                .to_string();
        let (found, ledger) = r.look(body.as_bytes(), CHAT);
        let rules = per_rule(&found);
        assert_eq!(rules.len(), 2, "both are found and reported");
        let key = rules
            .iter()
            .find(|x| x.rule.id() == "anthropic-api-key")
            .unwrap();
        let project = rules.iter().find(|x| x.rule.id() == "project").unwrap();
        assert!(!replaced(&r, &key.rule), "the key is only recorded");
        assert!(replaced(&r, &project.rule));
        let (sent, _) = r.replace(body.into_bytes(), &ledger, CHAT);
        let sent = String::from_utf8(sent).unwrap();
        assert!(sent.contains(KEY), "{sent}");
        assert!(
            sent.contains("<<TW_PROJECT_1>>") && !sent.contains("PRJ-7"),
            "{sent}"
        );

        // Observe mode records every rule.
        let observing = Redaction {
            mode: Mode::Observe,
            ..r.clone()
        };
        assert!(!replaced(&observing, &project.rule));
    }

    #[test]
    fn a_value_repeated_without_its_context_is_masked_for_storage() {
        let r = redaction(Mode::Enforce);
        let body = serde_json::json!({"messages": [{"role": "user",
            "content": "connect with postgres://app:hunter2@db.internal:5432/orders"}]})
        .to_string();
        let (_, ledger) = r.look(body.as_bytes(), CHAT);
        assert_eq!(ledger.len(), 1, "the password is numbered");
        // The model repeats the password alone: no rule recognises it there.
        let answer =
            br#"{"content":[{"type":"text","text":"The password <<TW_SECRET_1>> is wrong."}]}"#;
        let restored = restore_body(&ledger, answer);
        assert!(String::from_utf8_lossy(&restored).contains("hunter2"));
        let (rescanned, _) = r.replace_whole(restored.clone(), &ledger);
        assert!(
            String::from_utf8_lossy(&rescanned).contains("hunter2"),
            "rules alone miss it"
        );
        let stored = String::from_utf8(r.mask_for_storage(restored, &ledger)).unwrap();
        assert!(!stored.contains("hunter2"), "{stored}");
        assert!(
            stored.contains("The password <<TW_SECRET_1>> is wrong."),
            "{stored}"
        );
    }

    /// Codex sends back its reasoning items by id. One whose id held an
    /// 11-digit run read as a phone number had it replaced, and the
    /// upstream refused the whole request.
    #[test]
    fn a_reasoning_item_id_is_sent_as_it_is_and_the_same_number_in_text_is_not() {
        let r = Redaction::new(
            &RedactPolicy {
                mode: Mode::Enforce,
                enable: vec!["cn-mobile-phone".into()],
                ..Default::default()
            },
            &Default::default(),
        );
        let id = "rs_0d1f5e8a13800138000b6f2c9e4a7d3c1b0e9f8a7d6c5b4a3";
        assert!(
            !tw_guard::redact::flow::hits(&format!("\"{id}\""), &r.rules).is_empty(),
            "the rule reads a phone number in the id on its own"
        );
        let body = serde_json::json!({
            "model": "gpt-5-codex",
            "input": [
                {"type": "reasoning", "id": id, "summary": [],
                 "encrypted_content": "gAAAAABo-opaque"},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "call me on 13800138000"}]}
            ]
        })
        .to_string();
        let (found, ledger) = r.look(body.as_bytes(), RESPONSES);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].count, 1, "only the text");
        let (sent, _) = r.replace(body.into_bytes(), &ledger, RESPONSES);
        let sent: serde_json::Value = serde_json::from_slice(&sent).unwrap();
        assert_eq!(sent["input"][0]["id"], id);
        let text = sent["input"][1]["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains("13800138000"), "{text}");
        assert!(text.contains("<<TW_"), "{text}");
    }

    /// With every rule on, the protocol's own values — the model, ids,
    /// types, roles, names, encrypted content — go out as they came, while
    /// the text written in the request, reasoning and tool calls included,
    /// is replaced.
    #[test]
    fn protocol_values_are_never_replaced_written_text_always_is() {
        let r = Redaction::new(
            &RedactPolicy {
                mode: Mode::Enforce,
                enable: tw_guard::redact::rules::BUILTINS
                    .iter()
                    .map(|b| b.id.to_string())
                    .collect(),
                custom: vec![tw_guard::policy::CustomRedactRule {
                    name: "tool".into(),
                    pattern: "notify".into(),
                    matching: tw_guard::policy::ContentMatch::Contains,
                    case_sensitive: false,
                    label: None,
                    action: Default::default(),
                    disabled: false,
                }],
                ..Default::default()
            },
            &Default::default(),
        );
        const PHONE: &str = "13800138000";
        const EMAIL: &str = "alice@corp.example.org";
        let body = serde_json::json!({
            "model": "gpt-5-codex",
            "instructions": format!("Deploy with {KEY}"),
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": format!("notify {EMAIL} at {PHONE}")}
                ]},
                {"type": "reasoning", "id": format!("rs_0a1b{PHONE}c2d3e4f5"),
                 "summary": [{"type": "summary_text", "text": format!("The number is {PHONE}.")}],
                 "encrypted_content": "gAAAAABo-opaque"},
                {"type": "function_call", "id": format!("fc_{PHONE}"),
                 "call_id": format!("call_{PHONE}"), "name": "notify",
                 "arguments": serde_json::json!({"to": EMAIL}).to_string()},
                {"type": "function_call_output", "call_id": format!("call_{PHONE}"),
                 "output": format!("sent to {PHONE}")}
            ],
            "tools": [{"type": "function", "name": "notify",
                       "description": format!("Escalations go to {EMAIL}."),
                       "parameters": {"type": "object"}}]
        })
        .to_string();
        let (_, ledger) = r.look(body.as_bytes(), RESPONSES);
        let (sent, _) = r.replace(body.clone().into_bytes(), &ledger, RESPONSES);
        let sent: serde_json::Value = serde_json::from_slice(&sent).unwrap();
        let came: serde_json::Value = serde_json::from_str(&body).unwrap();

        // Protocol values, byte for byte.
        assert_eq!(sent["model"], came["model"]);
        for (i, keys) in [
            (0, &["type", "role"][..]),
            (1, &["type", "id", "encrypted_content"][..]),
            (2, &["type", "id", "call_id", "name"][..]),
            (3, &["type", "call_id"][..]),
        ] {
            for k in keys {
                assert_eq!(sent["input"][i][k], came["input"][i][k], "input[{i}].{k}");
            }
        }
        assert_eq!(sent["input"][1]["summary"][0]["type"], "summary_text");
        assert_eq!(sent["tools"][0]["name"], "notify");
        assert_eq!(sent["tools"][0]["type"], "function");

        // Written text, replaced.
        let written = [
            sent["instructions"].as_str().unwrap(),
            sent["input"][0]["content"][0]["text"].as_str().unwrap(),
            sent["input"][1]["summary"][0]["text"].as_str().unwrap(),
            sent["input"][2]["arguments"].as_str().unwrap(),
            sent["input"][3]["output"].as_str().unwrap(),
            sent["tools"][0]["description"].as_str().unwrap(),
        ];
        for text in written {
            assert!(text.contains("<<TW_"), "{text}");
            for value in [KEY, PHONE, EMAIL, "notify"] {
                assert!(!text.contains(value), "{value} in {text}");
            }
        }
        let args: serde_json::Value =
            serde_json::from_str(sent["input"][2]["arguments"].as_str().unwrap())
                .expect("the arguments still parse");
        assert!(args["to"].as_str().unwrap().starts_with("<<TW_"));
    }

    #[test]
    fn a_body_that_is_not_json_has_no_written_text() {
        let r = redaction(Mode::Enforce);
        let body = format!("user: {KEY}");
        let (found, ledger) = r.look(body.as_bytes(), CHAT);
        assert!(found.is_empty() && ledger.is_empty());
        let (sent, _) = r.replace(body.clone().into_bytes(), &ledger, CHAT);
        assert_eq!(sent, body.into_bytes());
    }
}
