//! PII patterns, and the at-rest redactor.
//!
//! The patterns live in `security.pii_redactor_patterns`. Two surfaces use
//! them, and both must see the same set — a pattern added in the admin UI
//! that one surface skips is a leak nobody notices:
//!
//! * **In flight** (gateway only): PII in the caller's request is swapped
//!   for placeholders (`{{EMAIL_1}}`) before it goes upstream, and put back
//!   in the response for this caller. `gateway::pii_redactor` owns that.
//! * **At rest** (both gateways): request and response bodies, tool
//!   arguments and tool results are written to the audit log. The row is
//!   write-only, so matches become `{{REDACTED_<name>}}` and nothing is
//!   kept to restore them. That is [`BlobRedactor`].
//!
//! Matching is thinkwatch-core's (`tw-guard`), the same engine the desktop
//! gateway redacts with; the patterns are ours.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tw_guard::redact::rules::RuleSet;

/// A pattern as persisted in `system_settings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiPatternConfig {
    pub name: String,
    pub regex: String,
    /// The label in the placeholder: `EMAIL` in `{{EMAIL_1}}`.
    pub placeholder_prefix: String,
}

/// The rule set for these patterns: one rule per pattern, labelled with
/// its prefix.
///
/// A pattern that does not compile is skipped, loudly — the save-time
/// validator should have refused it, and one bad row should not take all
/// redaction offline.
pub fn rules(configs: &[PiiPatternConfig]) -> RuleSet {
    configs.iter().fold(RuleSet::none(), |set, c| {
        match set
            .clone()
            .with_labeled(&c.name, &c.regex, Some(&c.placeholder_prefix))
        {
            Ok(next) => next,
            Err(e) => {
                tracing::error!(
                    pattern = %c.name,
                    error = %e,
                    "Invalid PII regex — pattern is DISABLED for redaction"
                );
                metrics::counter!("pii_pattern_invalid_total", "pattern" => c.name.clone())
                    .increment(1);
                set
            }
        }
    })
}

/// Replace every match in `input` with `{{REDACTED_<pattern name>}}`.
/// Nothing is kept to restore them: the result is write-only audit data.
pub fn redact_blob(rules: &RuleSet, input: &str) -> String {
    if rules.is_empty() {
        return input.to_string();
    }
    let hits = tw_guard::redact::rules::scan_text(input, rules);
    let mut out = input.to_string();
    for h in hits.iter().rev() {
        out.replace_range(
            h.bytes.clone(),
            &format!("{{{{REDACTED_{}}}}}", h.rule.id()),
        );
    }
    out
}

/// The at-rest redactor, for a caller that holds no in-flight redactor
/// (the MCP gateway). Hot-swapped with the patterns.
#[derive(Clone)]
pub struct BlobRedactor {
    rules: Arc<RuleSet>,
}

impl Default for BlobRedactor {
    fn default() -> Self {
        Self {
            rules: Arc::new(RuleSet::none()),
        }
    }
}

impl std::fmt::Debug for BlobRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobRedactor").finish_non_exhaustive()
    }
}

impl BlobRedactor {
    pub fn from_configs(configs: &[PiiPatternConfig]) -> Self {
        Self {
            rules: Arc::new(rules(configs)),
        }
    }

    /// No patterns: callers can skip the pass (and its copy) entirely.
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

    fn p(name: &str, regex: &str) -> PiiPatternConfig {
        PiiPatternConfig {
            name: name.to_string(),
            regex: regex.to_string(),
            placeholder_prefix: name.to_string(),
        }
    }

    #[test]
    fn empty_redactor_is_no_op() {
        let r = BlobRedactor::default();
        assert!(r.is_empty());
        assert_eq!(r.redact_blob("hello world"), "hello world");
    }

    #[test]
    fn single_pattern_replaces_match() {
        let r = BlobRedactor::from_configs(&[p("EMAIL", r"[\w.]+@[\w.]+")]);
        let out = r.redact_blob("contact: alice@example.com");
        assert_eq!(out, "contact: {{REDACTED_EMAIL}}");
    }

    #[test]
    fn idempotent_on_already_redacted_text() {
        let r = BlobRedactor::from_configs(&[p("EMAIL", r"[\w.]+@[\w.]+")]);
        let once = r.redact_blob("a@b.com and c@d.com");
        let twice = r.redact_blob(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn overlapping_patterns_resolve_longest_wins() {
        // Two patterns matching overlapping spans — `LONG` covers
        // chars 0..7, `SHORT` covers chars 0..3. Longest should win.
        let r = BlobRedactor::from_configs(&[p("SHORT", r"foo"), p("LONG", r"foobar1")]);
        let out = r.redact_blob("foobar1 trail");
        assert_eq!(out, "{{REDACTED_LONG}} trail");
    }

    #[test]
    fn invalid_pattern_is_skipped_not_panicking() {
        let r = BlobRedactor::from_configs(&[
            p("OK", r"\d+"),
            p("BAD", r"["), // unclosed character class
        ]);
        // Only the valid pattern compiled — body should still get
        // redacted on numbers.
        assert_eq!(r.redact_blob("count=42"), "count={{REDACTED_OK}}");
    }

    #[test]
    fn no_matches_returns_input_unchanged() {
        let r = BlobRedactor::from_configs(&[p("EMAIL", r"[\w.]+@[\w.]+")]);
        assert_eq!(r.redact_blob("no email here"), "no email here");
    }
}
