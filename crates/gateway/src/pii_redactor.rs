//! In-flight PII redaction: swap the caller's PII for placeholders before
//! the request goes upstream, and put it back in what comes back.
//!
//! The patterns, and the engine that matches and restores them, are shared
//! (see `think_watch_common::pii`); this file is the part only an
//! in-flight redactor needs — which parts of a request to look at, and
//! how the values found there reach the request actually sent.

use serde_json::Value;
use think_watch_common::pii::PiiPatternConfig;
use tw_guard::redact::replace::{Ledger, Scheme};
use tw_guard::redact::rules::RuleSet;

/// `{{EMAIL_1}}`. The label tells the model what used to be there, so it
/// can still answer sensibly; a pattern without one would read `{{PII_1}}`.
pub const SCHEME: Scheme = Scheme {
    open: "{{",
    close: "}}",
    label: "PII",
};

/// Keys that carry base64 in a request. Replacement never enters them: a
/// digit run landing inside an encoded image is unlikely, but where it
/// happens the thing changed is the image, not the PII.
const BASE64_CARRIERS: &[&str] = &["data", "bytes"];

/// Detects PII in the caller's text and swaps it for placeholders.
#[derive(Clone)]
pub struct PiiRedactor {
    rules: RuleSet,
}

impl PiiRedactor {
    pub fn from_config(configs: &[PiiPatternConfig]) -> Self {
        Self {
            rules: think_watch_common::pii::rules(configs),
        }
    }

    /// Redact one piece of text. For the admin "try these patterns"
    /// endpoint, and anything else that holds plain text rather than a
    /// request.
    pub fn redact_str(&self, text: &str) -> (String, Ledger) {
        let r = tw_guard::redact::replace::redact_text(text, &self.rules, Ledger::new(SCHEME));
        (r.text, r.ledger)
    }

    /// Redact the caller's text in a decoded request.
    ///
    /// The decoded form's structure is known: text nested in a tool
    /// result, the array form of `system`, a Responses part whose text
    /// field is not called `text` — all of them are just parts here.
    ///
    /// Only user messages are redacted; assistant turns pass through.
    ///
    /// `Request.system` is not redacted. The system prompt is written by
    /// the operator, not typed by the caller; redacting an address or IP
    /// in it rewrites the operator's instructions, and such values there
    /// are configuration, not user PII.
    pub fn redact_request(&self, request: &mut tw_dialect::ir::Request) -> Ledger {
        use tw_dialect::ir::Role;

        let mut ledger = Ledger::new(SCHEME);
        if self.rules.is_empty() {
            return ledger;
        }
        for msg in &mut request.messages {
            if msg.role == Role::User {
                ledger = self.redact_parts(&mut msg.parts, ledger);
            }
        }
        if !ledger.is_empty() {
            tracing::debug!(values = ledger.len(), "PII redacted");
        }
        ledger
    }

    /// Redact a list of parts in place.
    ///
    /// `Part::ToolResult` is recursed into. Tool results often carry data
    /// a tool fetched on the user's behalf — a mailbox, an order.
    ///
    /// `Image` / `File` / `Thinking` / `ToolCall` are left alone: media is
    /// not redactable text, thinking is the model's own reasoning, and
    /// changing `ToolCall.input` would break the call itself.
    fn redact_parts(&self, parts: &mut [tw_dialect::ir::Part], mut ledger: Ledger) -> Ledger {
        use tw_dialect::ir::Part;

        for part in parts {
            match part {
                Part::Text(s) => {
                    let r = tw_guard::redact::replace::redact_text(s, &self.rules, ledger);
                    *s = r.text;
                    ledger = r.ledger;
                }
                Part::ToolResult(r) => ledger = self.redact_parts(&mut r.content, ledger),
                Part::Image(_) | Part::File { .. } | Part::Thinking(_) | Part::ToolCall(_) => {}
            }
        }
        ledger
    }

    /// Redact a serialized blob for the audit log. See
    /// `think_watch_common::pii::redact_blob`.
    pub fn redact_blob(&self, input: &str) -> String {
        think_watch_common::pii::redact_blob(&self.rules, input)
    }
}

/// Carry the PII found on the decoded request onto the **raw** one.
///
/// A request forwarded in its own format never goes through the decoded
/// form — that is how `cache_control` and everything else the decoded
/// form does not model survive. But PII is found on the decoded form,
/// where the structure is known, so the value → placeholder mapping has
/// to be carried back onto the raw JSON.
///
/// **On the parsed `Value`, not the bytes**: a client may send `@` as an
/// escape sequence, and the bytes would not contain the value at all.
///
/// Longer values first, so `a@x.com` does not eat part of `aa@x.com`.
///
/// A value that also appears in the system prompt is replaced there too —
/// which only happens when the caller also wrote it.
pub fn apply_to(ledger: &Ledger, value: &mut Value) {
    if ledger.is_empty() {
        return;
    }
    let mut pairs: Vec<(&str, &str)> = ledger.replacements().collect();
    pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(b.0)));
    walk_strings(value, &mut |s| {
        for (orig, ph) in &pairs {
            if s.contains(orig) {
                *s = s.replace(orig, ph);
            }
        }
    });
}

/// Paint the original values back into a whole response's bytes, each
/// JSON-escaped. A stream is restored frame by frame instead (see
/// `proxy::shaper`): there a placeholder can be split across frames.
pub fn restore_body(ledger: &Ledger, body: &[u8]) -> Vec<u8> {
    if ledger.is_empty() {
        return body.to_vec();
    }
    match std::str::from_utf8(body) {
        Ok(text) => tw_guard::redact::replace::restore_json(text, ledger).into_bytes(),
        Err(_) => body.to_vec(),
    }
}

fn walk_strings(v: &mut Value, f: &mut impl FnMut(&mut String)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(items) => items.iter_mut().for_each(|i| walk_strings(i, f)),
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if !BASE64_CARRIERS.contains(&k.as_str()) {
                    walk_strings(child, f);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The patterns `db/seeds.sql` ships with.
    fn seeded() -> PiiRedactor {
        let p = |name: &str, regex: &str, prefix: &str| PiiPatternConfig {
            name: name.into(),
            regex: regex.into(),
            placeholder_prefix: prefix.into(),
        };
        PiiRedactor::from_config(&[
            p(
                "email",
                r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}",
                "EMAIL",
            ),
            p("id_card_cn", r"\b\d{17}[\dXx]\b", "ID"),
            p(
                "credit_card",
                r"\b\d{4}[-\s]?\d{4}[-\s]?\d{4}[-\s]?\d{4}\b",
                "CARD",
            ),
            p("phone_cn", r"1[3-9]\d{9}", "PHONE"),
            p("phone_us", r"\b\d{3}[-.]?\d{3}[-.]?\d{4}\b", "PHONE"),
            p("ipv4", r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b", "IP"),
        ])
    }

    /// A ledger holding exactly these `(label, value)` pairs, issued in
    /// order — built through the redactor, the only way to get one.
    fn ledger_of(pairs: &[(&str, &str)]) -> Ledger {
        let configs: Vec<PiiPatternConfig> = pairs
            .iter()
            .enumerate()
            .map(|(i, (label, value))| PiiPatternConfig {
                name: format!("p{i}"),
                regex: regex::escape(value),
                placeholder_prefix: label.to_string(),
            })
            .collect();
        let text: Vec<&str> = pairs.iter().map(|p| p.1).collect();
        PiiRedactor::from_config(&configs)
            .redact_str(&text.join(" "))
            .1
    }

    #[test]
    fn applying_to_a_raw_request_touches_nothing_but_the_redacted_text() {
        // A request forwarded as sent must reach the upstream whole —
        // `name`, `cache_control`, everything — apart from the PII.
        let ctx = ledger_of(&[("EMAIL", "alice@example.com")]);
        let mut v = serde_json::json!({
            "role": "user", "name": "alice",
            "content": [{"type": "text", "text": "mail alice@example.com",
                         "cache_control": {"type": "ephemeral"}}]
        });
        apply_to(&ctx, &mut v);
        assert_eq!(v["name"], "alice");
        assert_eq!(v["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(v["content"][0]["text"], "mail {{EMAIL_1}}");
    }

    fn find_placeholder(ctx: &Ledger, original: &str) -> String {
        ctx.replacements()
            .find(|(v, _)| *v == original)
            .map(|(_, ph)| ph.to_string())
            .unwrap_or_else(|| panic!("no placeholder for {original}"))
    }

    #[test]
    fn redact_email() {
        let redactor = seeded();
        let (redacted, ctx) = redactor.redact_str("Contact me at alice@example.com please");

        let content = redacted.as_str();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(!content.contains("alice@example.com"));
        let ph = find_placeholder(&ctx, "alice@example.com");
        assert!(ph.starts_with("{{EMAIL_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_china_phone() {
        let redactor = seeded();
        let (redacted, ctx) = redactor.redact_str("Call me at 13812345678");

        let content = redacted.as_str();
        assert!(content.contains("PHONE"), "got: {content}");
        assert!(!content.contains("13812345678"));
        let ph = find_placeholder(&ctx, "13812345678");
        assert!(ph.starts_with("{{PHONE_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_us_phone() {
        let redactor = seeded();
        // Simplified US phone regex matches 10-digit patterns like 555-123-4567
        let (redacted, _ctx) = redactor.redact_str("Call 555-123-4567");

        let content = redacted.as_str();
        assert!(
            content.contains("PHONE"),
            "phone should be redacted, got: {content}"
        );
        assert!(!content.contains("123-4567"));
    }

    #[test]
    fn redact_credit_card() {
        let redactor = seeded();
        let (redacted, ctx) = redactor.redact_str("My card is 4111-1111-1111-1111");

        let content = redacted.as_str();
        assert!(content.contains("CARD"), "got: {content}");
        assert!(!content.contains("4111"));
        let ph = find_placeholder(&ctx, "4111-1111-1111-1111");
        assert!(ph.starts_with("{{CARD_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_china_id_card() {
        let redactor = seeded();
        let (redacted, ctx) = redactor.redact_str("ID: 110101199001011234");

        let content = redacted.as_str();
        assert!(content.contains("ID"), "got: {content}");
        assert!(!content.contains("110101199001011234"));
        let ph = find_placeholder(&ctx, "110101199001011234");
        assert!(ph.starts_with("{{ID_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_ipv4() {
        let redactor = seeded();
        let (redacted, ctx) = redactor.redact_str("Server is at 192.168.1.100");

        let content = redacted.as_str();
        assert!(content.contains("IP"), "got: {content}");
        assert!(!content.contains("192.168.1.100"));
        let ph = find_placeholder(&ctx, "192.168.1.100");
        assert!(ph.starts_with("{{IP_"), "placeholder format: {ph}");
    }

    #[test]
    fn restore_response_replaces_placeholders() {
        let redactor = seeded();
        let (redacted, ctx) = redactor.redact_str("Email alice@example.com and bob@test.org");

        // Simulate the LLM echoing back the redacted content
        let redacted_content = redacted.as_str();
        let content = String::from_utf8(restore_body(&ctx, redacted_content.as_bytes())).unwrap();
        assert!(content.contains("alice@example.com"), "got: {content}");
        assert!(content.contains("bob@test.org"), "got: {content}");
        assert!(!content.contains("{{EMAIL_"));
    }

    #[test]
    fn placeholders_are_stable_counter_only() {
        // Stable placeholder format: `{{EMAIL_<counter>}}`. The salt
        // was dropped intentionally — see DESIGN-001 in proxy.rs —
        // so that two callers with identical pre-redaction prompts
        // produce identical redacted bodies, allowing the response
        // cache to actually hit. Two callers with identical text
        // must also have identical contexts (PII values come from
        // the text itself), so the symmetry is safe.
        let redactor = seeded();
        let (_redacted, ctx) = redactor.redact_str("Reach me at alice@example.com");
        let placeholder = find_placeholder(&ctx, "alice@example.com");
        assert_eq!(
            placeholder, "{{EMAIL_1}}",
            "placeholder must be stable counter-only form"
        );
    }

    #[test]
    fn placeholders_are_identical_across_two_calls_with_same_input() {
        // The cache keys on the redacted request and stores the
        // placeholder-form response; two callers sharing a slot only
        // works if redaction is deterministic on the input.
        let redactor = seeded();
        let (_, ctx_a) = redactor.redact_str("alice@example.com");
        let (_, ctx_b) = redactor.redact_str("alice@example.com");
        let ph_a = find_placeholder(&ctx_a, "alice@example.com");
        let ph_b = find_placeholder(&ctx_b, "alice@example.com");
        assert_eq!(
            ph_a, ph_b,
            "redaction must be deterministic so cache hits restore correctly"
        );
    }

    #[test]
    fn multiple_pii_types() {
        let redactor = seeded();
        let (redacted, ctx) =
            redactor.redact_str("Email alice@example.com, IP 10.0.0.1, card 4111 1111 1111 1111");

        let content = redacted.as_str();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(content.contains("IP"), "got: {content}");
        assert!(content.contains("CARD"), "got: {content}");
        assert!(!content.contains("alice@example.com"));
        assert!(!content.contains("10.0.0.1"));

        // Verify restore round-trip
        let restored = String::from_utf8(restore_body(&ctx, content.as_bytes())).unwrap();
        assert!(restored.contains("alice@example.com"), "got: {restored}");
        assert!(restored.contains("10.0.0.1"), "got: {restored}");
    }

    #[test]
    fn from_config_loads_patterns() {
        let configs = vec![PiiPatternConfig {
            name: "email_custom".into(),
            regex: r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}".into(),
            placeholder_prefix: "CUSTOM_EMAIL".into(),
        }];
        let redactor = PiiRedactor::from_config(&configs);

        let (redacted, ctx) = redactor.redact_str("Contact test@example.com for info");

        let content = redacted.as_str();
        assert!(content.contains("CUSTOM_EMAIL"), "got: {content}");
        assert!(!content.contains("test@example.com"));
        let ph = find_placeholder(&ctx, "test@example.com");
        assert!(
            ph.starts_with("{{CUSTOM_EMAIL_"),
            "placeholder format: {ph}"
        );
    }

    #[test]
    fn from_config_invalid_regex_skipped() {
        let configs = vec![
            PiiPatternConfig {
                name: "bad_regex".into(),
                regex: r"[invalid((".into(), // malformed regex
                placeholder_prefix: "BAD".into(),
            },
            PiiPatternConfig {
                name: "good_email".into(),
                regex: r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}".into(),
                placeholder_prefix: "EMAIL".into(),
            },
        ];
        // Should not panic — invalid regex is skipped
        let redactor = PiiRedactor::from_config(&configs);

        // The valid pattern should still work
        let (redacted, _ctx) = redactor.redact_str("Contact me at alice@test.org");
        let content = redacted.as_str();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(!content.contains("alice@test.org"));
    }

    // ── redact_request: redaction on the decoded request ──────────────
    use tw_dialect::ir::{Message, Part, Request, Role, ToolResult};

    fn ir_user_message(parts: Vec<Part>) -> Message {
        Message {
            role: Role::User,
            parts,
        }
    }

    fn ir_assistant_message(parts: Vec<Part>) -> Message {
        Message {
            role: Role::Assistant,
            parts,
        }
    }

    fn ir_request(messages: Vec<Message>) -> Request {
        Request {
            model: "test".into(),
            messages,
            ..Default::default()
        }
    }

    #[test]
    fn redact_request_redacts_a_plain_text_part_in_a_user_message() {
        let redactor = seeded();
        let mut request = ir_request(vec![ir_user_message(vec![Part::Text(
            "Email me at alice@example.com".into(),
        )])]);

        let ctx = redactor.redact_request(&mut request);

        let Part::Text(text) = &request.messages[0].parts[0] else {
            panic!("expected a text part");
        };
        assert!(text.contains("EMAIL"), "got: {text}");
        assert!(!text.contains("alice@example.com"));
        let ph = find_placeholder(&ctx, "alice@example.com");
        assert!(ph.starts_with("{{EMAIL_"));
    }

    /// Pins the hole in the earlier version, which guessed at a
    /// `serde_json::Value` and had no notion of a tool result, so text
    /// nested in one went through unredacted.
    /// Tool results are exactly where user data sits — a mailbox, an
    /// order — fed back into the same conversation.
    #[test]
    fn redact_request_redacts_pii_nested_inside_a_tool_result() {
        let redactor = seeded();
        let mut request = ir_request(vec![ir_user_message(vec![Part::ToolResult(ToolResult {
            id: "call_1".into(),
            content: vec![Part::Text(
                "Found the order, shipped to alice@example.com".into(),
            )],
            is_error: false,
        })])]);

        let ctx = redactor.redact_request(&mut request);

        let Part::ToolResult(result) = &request.messages[0].parts[0] else {
            panic!("expected a tool result part");
        };
        let Part::Text(text) = &result.content[0] else {
            panic!("expected a text part inside the tool result");
        };
        assert!(text.contains("EMAIL"), "got: {text}");
        assert!(!text.contains("alice@example.com"));
        let ph = find_placeholder(&ctx, "alice@example.com");
        assert!(ph.starts_with("{{EMAIL_"));
    }

    #[test]
    fn redact_request_does_not_redact_assistant_messages() {
        let redactor = seeded();
        let mut request = ir_request(vec![ir_assistant_message(vec![Part::Text(
            "Sure, contact alice@example.com".into(),
        )])]);

        let ctx = redactor.redact_request(&mut request);

        let Part::Text(text) = &request.messages[0].parts[0] else {
            panic!("expected a text part");
        };
        assert_eq!(text, "Sure, contact alice@example.com");
        assert!(ctx.is_empty());
    }

    /// The system prompt is the operator's, not the caller's: redacting
    /// it rewrites the instructions, and values there are configuration.
    #[test]
    fn redact_request_does_not_redact_the_system_prompt() {
        let redactor = seeded();
        let mut request = Request {
            model: "test".into(),
            system: vec!["Escalate to ops@example.com when unsure.".into()],
            messages: vec![ir_user_message(vec![Part::Text("hi".into())])],
            ..Default::default()
        };

        redactor.redact_request(&mut request);

        assert_eq!(
            request.system[0],
            "Escalate to ops@example.com when unsure."
        );
    }

    /// A value gets the same placeholder whether it sits in plain text or
    /// inside a tool result — restoration depends on that mapping.
    #[test]
    fn a_value_repeated_across_a_tool_result_restores_everywhere() {
        // The same value twice gets one placeholder, and both places restore
        // — including the one inside the tool result.
        let redactor = seeded();
        let mut request = ir_request(vec![ir_user_message(vec![
            Part::Text("Contact alice@example.com".into()),
            Part::ToolResult(ToolResult {
                id: "call_1".into(),
                content: vec![Part::Text("Confirmed: alice@example.com".into())],
                is_error: false,
            }),
        ])]);

        let ctx = redactor.redact_request(&mut request);

        let Part::Text(first) = &request.messages[0].parts[0] else {
            panic!("expected a text part");
        };
        let Part::ToolResult(result) = &request.messages[0].parts[1] else {
            panic!("expected a tool result part");
        };
        let Part::Text(second) = &result.content[0] else {
            panic!("expected a text part inside the tool result");
        };

        assert!(!first.contains("alice@example.com"), "{first}");
        assert!(!second.contains("alice@example.com"), "{second}");
        assert_eq!(
            first.strip_prefix("Contact "),
            second.strip_prefix("Confirmed: "),
            "the same value should get the same placeholder"
        );

        let restore = |s: &str| tw_guard::redact::replace::restore(s, &ctx);
        assert_eq!(restore(first), "Contact alice@example.com");
        assert_eq!(restore(second), "Confirmed: alice@example.com");
    }
    #[test]
    fn the_same_value_gets_the_same_placeholder() {
        // Two placeholders read as two people to a model, and a forwarded
        // request needs value → placeholder to be a function.
        let redactor = seeded();
        let mut request = ir_request(vec![ir_user_message(vec![Part::Text(
            "to a@example.com, cc a@example.com, bcc b@example.com".into(),
        )])]);
        let ctx = redactor.redact_request(&mut request);
        assert_eq!(ctx.len(), 2, "{ctx:?}");
    }

    #[test]
    fn applying_to_a_raw_request_reaches_text_the_client_escaped() {
        // A client may send `\u0040`, and then the bytes hold no `@`.
        // On the parsed Value the string is already unescaped.
        let redactor = seeded();
        let mut ir = ir_request(vec![ir_user_message(vec![Part::Text(
            "mail a@example.com".into(),
        )])]);
        let ctx = redactor.redact_request(&mut ir);

        let raw = r#"{"messages":[{"role":"user","content":"mail a\u0040example.com"}]}"#;
        let mut v: serde_json::Value = serde_json::from_str(raw).unwrap();
        apply_to(&ctx, &mut v);
        let text = v["messages"][0]["content"].as_str().unwrap();
        assert!(!text.contains("a@example.com"), "{text}");
        assert!(text.starts_with("mail {{EMAIL_"), "{text}");
    }

    #[test]
    fn applying_to_a_raw_request_leaves_base64_alone() {
        let ctx = ledger_of(&[("PHONE", "13800138000")]);
        let mut v = serde_json::json!({
            "content": [
                {"type": "text", "text": "call 13800138000"},
                {"type": "image", "source": {"type": "base64", "data": "AB13800138000CD"}}
            ]
        });
        apply_to(&ctx, &mut v);
        assert_eq!(v["content"][0]["text"], "call {{PHONE_1}}");
        assert_eq!(
            v["content"][1]["source"]["data"], "AB13800138000CD",
            "that would change the image, not the PII"
        );
    }

    #[test]
    fn the_longer_value_is_replaced_first() {
        let ctx = ledger_of(&[("EMAIL", "a@x.com"), ("EMAIL", "aa@x.com")]);
        let mut v = serde_json::json!({"text": "aa@x.com and a@x.com"});
        apply_to(&ctx, &mut v);
        assert_eq!(v["text"], "{{EMAIL_2}} and {{EMAIL_1}}");
    }

    #[test]
    fn restoring_bytes_escapes_the_original_so_the_json_survives() {
        // An original containing a quote, put back as-is, breaks the JSON.
        let ctx = ledger_of(&[("NAME", r#"O"Brien"#)]);
        let body = br#"{"content":[{"type":"text","text":"Hi {{NAME_1}}"}]}"#;
        let out = restore_body(&ctx, body);
        let v: serde_json::Value = serde_json::from_slice(&out).expect("still valid JSON");
        assert_eq!(v["content"][0]["text"], r#"Hi O"Brien"#);
    }
}
