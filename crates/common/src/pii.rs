//! At-rest redaction of captured bodies.
//!
//! With body capture on, request and response bodies, tool arguments and
//! tool results are written to the audit log. With `audit.body_redact_pii`
//! on as well, what the outbound redaction rules find in them is masked
//! before they are stored: a match becomes `{{REDACTED_<rule>}}` and
//! nothing is kept to restore it — the row is write-only. Both gateways
//! capture bodies, so both use [`BlobRedactor`].
//!
//! The rules are the outbound redaction policy's (`security.redact`, see
//! [`crate::guard_policy`]): the built-in rules it has on and its custom
//! rules, **whatever its mode** — the mode decides what happens to a
//! request on the wire, the capture setting what is kept. Matching is
//! thinkwatch-core's (`tw_guard::redact`), the engine the desktop gateway
//! redacts with.

use std::borrow::Cow;
use std::sync::Arc;

use tw_guard::policy::RedactPolicy;
use tw_guard::redact::rules::RuleSet;

/// Replace every match in `input` with `{{REDACTED_<rule>}}` — the
/// built-in rule's id or the custom rule's name. Nothing is kept to restore
/// them: the result is write-only audit data.
///
/// Found the way a request body is searched (`tw_guard::redact::flow::hits`):
/// JSON escapes are read, a custom rule's match stays inside one JSON
/// string so the result is still JSON, base64 payloads are left alone,
/// and so are the gateway's own placeholders (a captured answer still
/// carries them). The one base64 payload that is read is a compaction
/// summary a conversion carries (see [`redact_carried_summaries`]).
pub fn redact_blob(rules: &RuleSet, input: &str) -> String {
    if rules.is_empty() {
        return input.to_string();
    }
    let input = redact_carried_summaries(rules, input);
    let hits = tw_guard::redact::flow::hits(&input, rules);
    let mut out = input.into_owned();
    for h in hits.iter().rev() {
        out.replace_range(h.bytes.clone(), &redacted(&h.rule));
    }
    out
}

fn redacted(rule: &tw_guard::redact::rules::Rule) -> String {
    format!("{{{{REDACTED_{}}}}}", rule.id())
}

/// The summaries in `input` that a conversion carries, redacted.
///
/// A Codex compaction sent to an upstream of another format comes back as
/// the summary that upstream wrote — the conversation restated, with its
/// paths, commands and values — base64-encoded in the item's
/// `encrypted_content` (`tw1.c.…`, see `tw_dialect::compaction`), and
/// later requests carry it back. Unlike OpenAI's own compactions it is not
/// encrypted, but as base64 the search above passes over it. Each one is
/// read, redacted as plain text and written back the same way, so the
/// body keeps its shape.
fn redact_carried_summaries<'a>(rules: &RuleSet, input: &'a str) -> Cow<'a, str> {
    const PREFIX: &str = tw_dialect::compaction::CARRIED_PREFIX;
    if !input.contains(PREFIX) {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(PREFIX) {
        let start = at + PREFIX.len();
        let end = rest[start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .map_or(rest.len(), |n| start + n);
        let carried = &rest[at..end];
        out.push_str(&rest[..at]);
        match tw_dialect::compaction::read(carried) {
            Some(summary) => {
                let hits = tw_guard::redact::flow::hits_plain(&summary, rules);
                if hits.is_empty() {
                    out.push_str(carried);
                } else {
                    let mut text = summary;
                    for h in hits.iter().rev() {
                        text.replace_range(h.bytes.clone(), &redacted(&h.rule));
                    }
                    out.push_str(&tw_dialect::compaction::carry(&text));
                }
            }
            None => out.push_str(carried),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The at-rest redactor, hot-swapped with the outbound redaction policy.
#[derive(Clone)]
pub struct BlobRedactor {
    rules: Arc<RuleSet>,
}

impl Default for BlobRedactor {
    /// The factory policy's rules.
    fn default() -> Self {
        Self::from_policy(&RedactPolicy::default())
    }
}

impl std::fmt::Debug for BlobRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobRedactor").finish_non_exhaustive()
    }
}

impl BlobRedactor {
    pub fn from_policy(policy: &RedactPolicy) -> Self {
        Self {
            rules: Arc::new(crate::guard_policy::redact_rules(policy)),
        }
    }

    /// No rule on: callers can skip the pass (and its copy) entirely.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn redact_blob(&self, input: &str) -> String {
        redact_blob(&self.rules, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_guard::policy::CustomRedactRule;

    fn only(rules: &[(&str, &str)]) -> RuleSet {
        rules.iter().fold(RuleSet::none(), |set, (name, pattern)| {
            set.with_custom(name, pattern).unwrap()
        })
    }

    #[test]
    fn no_rules_is_a_no_op() {
        let rules = RuleSet::none();
        assert_eq!(redact_blob(&rules, "hello world"), "hello world");
    }

    #[test]
    fn a_match_becomes_the_rule_it_matched() {
        let rules = only(&[("EMAIL", r"[\w.]+@[\w.]+")]);
        assert_eq!(
            redact_blob(&rules, "contact: alice@example.com"),
            "contact: {{REDACTED_EMAIL}}"
        );
    }

    #[test]
    fn redacting_twice_changes_nothing_more() {
        let rules = only(&[("EMAIL", r"[\w.]+@[\w.]+")]);
        let once = redact_blob(&rules, "a@b.com and c@d.com");
        assert_eq!(redact_blob(&rules, &once), once);
    }

    #[test]
    fn of_two_overlapping_matches_the_longer_wins() {
        let rules = only(&[("SHORT", "foo"), ("LONG", "foobar1")]);
        assert_eq!(
            redact_blob(&rules, "foobar1 trail"),
            "{{REDACTED_LONG}} trail"
        );
    }

    #[test]
    fn a_captured_json_body_stays_json() {
        // Read as plain text, `secret.*` would run on past the closing
        // quote and take the rest of the body with it.
        let rules = only(&[("TAIL", "secret.*")]);
        let body = r#"{"a":"my secret value","b":"keep"}"#;
        let out = redact_blob(&rules, body);
        let v: serde_json::Value = serde_json::from_str(&out).expect("still JSON");
        assert_eq!(v["a"], "my {{REDACTED_TAIL}}", "{out}");
        assert_eq!(v["b"], "keep", "{out}");
    }

    #[test]
    fn a_carried_compaction_summary_is_redacted_where_it_is() {
        use tw_dialect::compaction::{carry, read};
        let rules = only(&[("EMAIL", r"[\w.]+@[\w.]+")]);
        let carried = carry("Reply to alice@example.com about src/lib.rs.");
        let body = format!(
            r#"{{"input":[{{"type":"compaction","encrypted_content":"{carried}"}},{{"role":"user","content":"bob@example.com"}}]}}"#
        );
        let out = redact_blob(&rules, &body);
        let v: serde_json::Value = serde_json::from_str(&out).expect("still JSON");
        let summary = read(v["input"][0]["encrypted_content"].as_str().unwrap());
        assert_eq!(
            summary.as_deref(),
            Some("Reply to {{REDACTED_EMAIL}} about src/lib.rs."),
            "{out}"
        );
        assert_eq!(v["input"][1]["content"], "{{REDACTED_EMAIL}}", "{out}");

        // Nothing found in it, or not one of ours: left as it was.
        let clean = format!(r#"{{"encrypted_content":"{}"}}"#, carry("tests pass"));
        assert_eq!(redact_blob(&rules, &clean), clean);
        let theirs = r#"{"encrypted_content":"gAAAAABoQ2xpZW50"}"#;
        assert_eq!(redact_blob(&rules, theirs), theirs);
    }

    #[test]
    fn the_policys_rules_are_the_ones_used_built_in_and_custom() {
        let key = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let r = BlobRedactor::default();
        assert!(!r.is_empty(), "the credential rules ship on");
        assert_eq!(
            r.redact_blob(&format!("key {key}")),
            "key {{REDACTED_anthropic-api-key}}"
        );

        let r = BlobRedactor::from_policy(&RedactPolicy {
            disable: tw_guard::redact::rules::BUILTINS
                .iter()
                .map(|b| b.id.to_string())
                .collect(),
            custom: vec![CustomRedactRule {
                name: "project".into(),
                pattern: r"PRJ-\d{6}".into(),
                label: None,
                disabled: false,
            }],
            ..Default::default()
        });
        assert_eq!(
            r.redact_blob(&format!("{key} PRJ-123456")),
            format!("{key} {{{{REDACTED_project}}}}")
        );
    }
}
