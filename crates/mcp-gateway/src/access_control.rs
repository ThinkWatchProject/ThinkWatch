/// Check if a namespaced MCP tool name is allowed by the given patterns.
///
/// Patterns follow these rules:
/// - `None` (NULL in DB) → unrestricted, all tools allowed
/// - `Some([])` (empty array) → deny all
/// - `"*"` → allow all
/// - `"mysql__*"` → allow all tools from the `mysql` server
/// - `"mysql__query"` → allow only the exact tool
///
/// Used by the MCP proxy to filter `tools/list` and gate `tools/call`.
pub fn is_tool_allowed(patterns: Option<&[String]>, namespaced_name: &str) -> bool {
    let Some(pats) = patterns else {
        return true;
    };
    pats.iter().any(|p| pattern_matches(p, namespaced_name))
}

/// Match a single pattern against a namespaced name with strict parsing.
///
/// The namespace grammar is `<server>__<tool>` with exactly one `__`
/// separator. Patterns are rejected (never match) unless they fit one
/// of three shapes, which prevents loose prefix matches from granting
/// broader access than the operator intended — e.g. `"mysql__"` as a
/// typo'd pattern used to match `"mysql__query"` via starts_with.
fn pattern_matches(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    // Server wildcard: `<server>__*`. The server segment must be a
    // single non-empty identifier with no embedded `__` or `*`, so
    // stray patterns like `__*` or `a__*__b` never widen the match.
    if let Some(server) = pattern.strip_suffix("__*") {
        if server.is_empty() || server.contains("__") || server.contains('*') {
            return false;
        }
        return matches!(
            name.split_once("__"),
            Some((s, t)) if s == server && !t.is_empty()
        );
    }

    // Exact match: `<server>__<tool>` with at least one `__`
    // separator (the FIRST one demarcates server from tool) and no
    // stray `*`. The tool segment is allowed to contain `__` —
    // upstream MCP servers like AWS Knowledge ("aws___list_regions")
    // use embedded double-underscores as a real part of the tool
    // identifier, and rejecting those would silently make
    // exact-match allow-lists unable to grant any of those tools.
    if pattern.contains('*') {
        return false;
    }
    match pattern.split_once("__") {
        Some((s, t)) if !s.is_empty() && !t.is_empty() => pattern == name,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_allows_all() {
        assert!(is_tool_allowed(None, "any__tool"));
    }

    #[test]
    fn empty_denies_all() {
        assert!(!is_tool_allowed(Some(&[]), "any__tool"));
    }

    #[test]
    fn star_allows_all() {
        assert!(is_tool_allowed(Some(&["*".into()]), "any__tool"));
    }

    #[test]
    fn server_wildcard() {
        let pats = vec!["mysql__*".into()];
        assert!(is_tool_allowed(Some(&pats), "mysql__query"));
        assert!(is_tool_allowed(Some(&pats), "mysql__execute"));
        assert!(!is_tool_allowed(Some(&pats), "github__get_issue"));
    }

    #[test]
    fn exact_match() {
        let pats = vec!["mysql__query".into()];
        assert!(is_tool_allowed(Some(&pats), "mysql__query"));
        assert!(!is_tool_allowed(Some(&pats), "mysql__execute"));
    }

    #[test]
    fn mixed_patterns() {
        let pats = vec!["mysql__*".into(), "github__get_issue".into()];
        assert!(is_tool_allowed(Some(&pats), "mysql__query"));
        assert!(is_tool_allowed(Some(&pats), "mysql__execute"));
        assert!(is_tool_allowed(Some(&pats), "github__get_issue"));
        assert!(!is_tool_allowed(Some(&pats), "github__create_pr"));
        assert!(!is_tool_allowed(Some(&pats), "slack__send"));
    }

    #[test]
    fn bare_server_pattern_does_not_prefix_match() {
        let pats = vec!["mysql__".into()];
        assert!(!is_tool_allowed(Some(&pats), "mysql__query"));
        assert!(!is_tool_allowed(Some(&pats), "mysql__"));
    }

    #[test]
    fn malformed_wildcard_patterns_are_invalid() {
        for bad in ["__*", "a__b__*", "a*b__*", "*__*"] {
            assert!(
                !is_tool_allowed(Some(&[bad.into()]), "mysql__query"),
                "pattern {bad:?} should never match"
            );
        }
    }

    #[test]
    fn tool_name_with_double_underscore_matches_exactly() {
        // Real MCP servers (AWS Knowledge MCP, langchain tools, …)
        // use embedded `__` inside the tool segment as part of the
        // upstream identifier. Exact-match patterns must allow them
        // through — the FIRST `__` separates server from tool, and
        // anything after is the tool name verbatim.
        let pats = vec!["aws_docs__aws___list_regions".into()];
        assert!(is_tool_allowed(Some(&pats), "aws_docs__aws___list_regions"));
        // Different tool in the same namespace still doesn't match.
        assert!(!is_tool_allowed(
            Some(&pats),
            "aws_docs__aws___search_documentation"
        ));
    }

    #[test]
    fn star_in_middle_of_exact_pattern_rejected() {
        let pats = vec!["mysql*__query".into()];
        assert!(!is_tool_allowed(Some(&pats), "mysql__query"));
    }

    #[test]
    fn wildcard_does_not_match_server_only_name() {
        let pats = vec!["mysql__*".into()];
        assert!(!is_tool_allowed(Some(&pats), "mysql__"));
        assert!(!is_tool_allowed(Some(&pats), "mysql"));
    }
}
