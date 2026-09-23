//! Cross-crate PII redaction primitive.
//!
//! Lives in `common` (not `gateway`) so the mcp-gateway crate can
//! use it without inverting the dep graph. The gateway crate's
//! `pii_redactor::PiiRedactor` keeps its request-level redaction
//! API (which walks the decoded request from `tw-dialect`) and
//! delegates blob redaction to this module's
//! [`BlobRedactor`].
//!
//! ## Why blob vs message redaction is split
//!
//! Two distinct use cases:
//!
//! * **In-flight redaction** (gateway only): the user's request goes
//!   upstream with PII replaced by placeholders (`{{EMAIL_1}}`),
//!   and the upstream response is restored back to the original PII
//!   for THIS caller. Needs a per-request restoration context.
//!   `gateway::pii_redactor::PiiRedactor::redact_messages` owns
//!   this.
//!
//! * **At-rest redaction**: the audit pipeline serializes
//!   `request_body` / `response_body` / `tool_arguments` /
//!   `tool_result` and writes them into ClickHouse. The audit row
//!   is WRITE-ONLY (the user's response was already restored from
//!   the in-flight context); no restoration needed. Pure substring
//!   replacement with a `{{REDACTED_<name>}}` marker is sufficient.
//!   This is what [`BlobRedactor`] does.
//!
//! Both halves load the same pattern set from
//! `security.pii_redactor_patterns` so a rule added via the admin
//! UI applies to BOTH redaction surfaces consistently.

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Pattern config as persisted in `system_settings`. Mirrors the
/// gateway-side shape exactly because both crates deserialize from
/// the same JSON value. The `placeholder_prefix` field is unused
/// by `BlobRedactor` (placeholders are write-only, no per-match
/// salt needed) but kept on the struct so config edits don't have
/// to fork into two schemas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiPatternConfig {
    pub name: String,
    pub regex: String,
    pub placeholder_prefix: String,
}

#[derive(Clone)]
struct CompiledPattern {
    name: String,
    regex: Regex,
}

/// Stateless, thread-safe blob redactor. Construct once at startup
/// (or hot-swap when the operator edits patterns), wrap in
/// `Arc<ArcSwap<...>>` for cheap reads on the hot path. `redact_blob`
/// is `O(N · M)` worst case where N is pattern count and M is body
/// length — same as the gateway's in-flight redactor.
#[derive(Clone, Default)]
pub struct BlobRedactor {
    patterns: Vec<CompiledPattern>,
}

impl std::fmt::Debug for BlobRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobRedactor")
            .field("pattern_count", &self.patterns.len())
            .finish()
    }
}

impl BlobRedactor {
    /// Build a redactor from the same config shape the gateway uses.
    /// Invalid regexes are skipped with a loud `tracing::error!` and
    /// a metric increment — same fail-soft posture as
    /// `gateway::pii_redactor::PiiRedactor::from_config` because an
    /// operator save-time validator should have rejected the bad
    /// pattern before it reached us, and an unparseable rule
    /// shouldn't keep ALL redaction offline.
    pub fn from_configs(configs: &[PiiPatternConfig]) -> Self {
        let patterns = configs
            .iter()
            .filter_map(|c| match crate::regex_util::compile_bounded(&c.regex) {
                Ok(regex) => Some(CompiledPattern {
                    name: c.name.clone(),
                    regex,
                }),
                Err(e) => {
                    tracing::error!(
                        pattern = %c.name,
                        error = %e,
                        "Invalid PII regex — pattern is DISABLED for blob redaction"
                    );
                    metrics::counter!(
                        "blob_redactor_pattern_invalid_total",
                        "pattern" => c.name.clone(),
                    )
                    .increment(1);
                    None
                }
            })
            .collect();
        Self { patterns }
    }

    /// `true` when there are no compiled patterns — callers can
    /// skip the redact pass entirely (avoids the per-message
    /// `.to_string()` copy).
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Apply all configured patterns to an arbitrary serialized blob.
    /// Result has matched substrings replaced by
    /// `{{REDACTED_<pattern_name>}}` markers. Overlapping matches are
    /// resolved deterministically (longest-match-wins on tie) so
    /// re-running the redactor on the same input is idempotent.
    pub fn redact_blob(&self, input: &str) -> String {
        if self.patterns.is_empty() {
            return input.to_string();
        }
        // Gather all matches first so overlapping patterns get a
        // deterministic non-overlapping resolution.
        let mut all_matches: Vec<(usize, usize, usize)> = Vec::new();
        for (pattern_idx, pattern) in self.patterns.iter().enumerate() {
            for m in pattern.regex.find_iter(input) {
                all_matches.push((m.start(), m.end(), pattern_idx));
            }
        }
        if all_matches.is_empty() {
            return input.to_string();
        }
        // Sort earliest-start first, longest-match-wins on tie.
        all_matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| (b.1 - b.0).cmp(&(a.1 - a.0))));
        let mut filtered: Vec<(usize, usize, usize)> = Vec::new();
        for m in &all_matches {
            if filtered.iter().all(|f| m.0 >= f.1 || m.1 <= f.0) {
                filtered.push(*m);
            }
        }
        // Reverse so replace_range from-end-first keeps earlier
        // indices valid.
        filtered.sort_by_key(|b| std::cmp::Reverse(b.0));
        let mut result = input.to_string();
        for (start, end, pattern_idx) in filtered {
            let replacement = format!("{{{{REDACTED_{}}}}}", self.patterns[pattern_idx].name);
            result.replace_range(start..end, &replacement);
        }
        result
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
