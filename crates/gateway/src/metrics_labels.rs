//! Cardinality guards for Prometheus labels.
//!
//! A naive `metrics::counter!("foo", "provider" => name.into())` emits
//! one time series per distinct value of `name`. If an operator stands
//! up 1000 custom providers (or a bug makes them look custom by string-
//! differing), the Prometheus scrape queue starves and the dashboard
//! goes dark. Labels that come from user-controlled config always need
//! a cap.
//!
//! `normalize_provider_label` is the gate for the `provider` dimension:
//! recognised providers pass through verbatim, everything else collapses
//! to `"other"`. Extending the allow-list is a deliberate code change,
//! not a config flip — which keeps the worst-case cardinality pinned.

/// Well-known provider names kept as individual series. Order doesn't
/// matter; keep the list short and memorable. Adding a value here is
/// a deliberate decision about dashboard cardinality.
const KNOWN_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "azure",
    "bedrock",
    "mistral",
    "together",
    "groq",
    "openrouter",
    "deepseek",
];

/// Collapse any provider name not in [`KNOWN_PROVIDERS`] to `"other"`
/// so the Prometheus cardinality for the `provider` label stays bounded
/// regardless of how many custom provider rows sit in the database.
pub fn normalize_provider_label(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    for known in KNOWN_PROVIDERS {
        if lower == *known {
            return known;
        }
    }
    "other"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_preserved() {
        assert_eq!(normalize_provider_label("openai"), "openai");
        assert_eq!(normalize_provider_label("Anthropic"), "anthropic");
        assert_eq!(normalize_provider_label("BEDROCK"), "bedrock");
    }

    #[test]
    fn unknown_collapses_to_other() {
        assert_eq!(normalize_provider_label("my-custom-proxy"), "other");
        assert_eq!(normalize_provider_label(""), "other");
        assert_eq!(normalize_provider_label("openaix"), "other");
    }
}
