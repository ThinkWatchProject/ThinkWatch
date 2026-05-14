//! Bounded regex compilation for operator-supplied patterns.
//!
//! Any code path where the regex source comes from `system_settings`,
//! a tenant admin's API call, or any other place an authenticated
//! human can write a pattern MUST go through [`compile_bounded`] —
//! the default `regex::Regex::new` has 10 MiB NFA + 2 MiB DFA limits
//! which let a pathological pattern like `(a|aa){200}` take seconds
//! to compile, occupy MBs of memory, and fire on every gateway
//! request that touches the rule.
//!
//! [`content_filter.rs`] already uses the bounded form; this module
//! exists so `pii_redactor.rs`, the `/system_settings` validators in
//! `handlers/admin.rs`, and any future operator-configurable regex
//! reuses the same caps instead of reinventing them.

use regex::{Regex, RegexBuilder};

use crate::errors::AppError;

/// Compile a regex with both NFA and DFA size capped at 1 MiB. Used
/// for any pattern that ultimately originated from operator input.
///
/// Case-insensitivity is OPT-IN — pass it explicitly via
/// [`compile_bounded_ci`] when needed (content filter wants it, PII
/// redactor patterns supply their own `(?i)` flag).
pub fn compile_bounded(pattern: &str) -> Result<Regex, AppError> {
    RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|e| AppError::BadRequest(format!("Invalid or oversized regex: {e}")))
}

/// Same as [`compile_bounded`] but forces case-insensitive matching.
pub fn compile_bounded_ci(pattern: &str) -> Result<Regex, AppError> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|e| AppError::BadRequest(format!("Invalid or oversized regex: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversize_pattern() {
        // Large bounded repetition + alternation balloons the compiled
        // automaton past the 1 MiB cap. The default
        // `regex::Regex::new` accepts this at 10 MiB. Without the cap
        // a privileged operator could DOS every request that runs the
        // pattern against incoming text. Pattern is empirical — the
        // regex crate version determines the exact byte size, so the
        // test only proves the cap *is enforced at some threshold*,
        // not the precise threshold.
        let pat = "(a|aa|aaa){5000}";
        assert!(
            compile_bounded(pat).is_err(),
            "1 MiB cap should reject the heavy alternation pattern"
        );
    }

    #[test]
    fn accepts_realistic_pattern() {
        // Typical PII / deny-list regex sizes are well under 1 MiB.
        assert!(compile_bounded(r"[A-Z]{2}\d{6}").is_ok());
        assert!(compile_bounded_ci(r"(secret|password)").is_ok());
    }

    #[test]
    fn ci_flag_is_applied() {
        let re = compile_bounded_ci("HELLO").unwrap();
        assert!(re.is_match("hello"));
    }
}
