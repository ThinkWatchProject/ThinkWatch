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
    match patterns {
        None => true,
        Some(pats) => pats.iter().any(|p| {
            p == "*"
                || (p.ends_with("__*") && namespaced_name.starts_with(&p[..p.len() - 1]))
                || p == namespaced_name
        }),
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
}
