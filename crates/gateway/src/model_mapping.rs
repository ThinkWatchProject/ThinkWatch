use std::collections::HashMap;

/// Maps model aliases to actual model names.
///
/// Applied before routing to allow friendly names (e.g. "fast" → "gpt-4o-mini")
/// and prefix stripping (e.g. "internal/gpt-4o" → "gpt-4o").
pub struct ModelMapper {
    /// Exact alias → target model.
    exact: HashMap<String, String>,
    /// Prefix alias → target prefix replacement, sorted by length descending
    /// so that longer prefixes match first.
    prefix: Vec<(String, String)>,
}

impl Default for ModelMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelMapper {
    pub fn new() -> Self {
        Self {
            exact: HashMap::new(),
            prefix: Vec::new(),
        }
    }

    /// Create a mapper from a config map.
    ///
    /// Keys ending with `*` are treated as prefix rules (the `*` is stripped):
    ///   `"internal/*" → ""` means strip "internal/" prefix.
    ///
    /// All other keys are exact aliases:
    ///   `"fast" → "gpt-4o-mini"`.
    pub fn from_config(config: HashMap<String, String>) -> Self {
        let mut exact = HashMap::new();
        let mut prefix = Vec::new();

        for (key, value) in config {
            if let Some(stripped) = key.strip_suffix('*') {
                prefix.push((stripped.to_string(), value));
            } else {
                exact.insert(key, value);
            }
        }

        // Sort prefixes by length descending so longest prefix matches first
        prefix.sort_by_key(|b| std::cmp::Reverse(b.0.len()));

        Self { exact, prefix }
    }

    /// Add an exact alias mapping.
    pub fn add_exact(&mut self, alias: &str, target: &str) {
        self.exact.insert(alias.to_string(), target.to_string());
    }

    /// Add a prefix mapping rule.
    pub fn add_prefix(&mut self, from_prefix: &str, to_prefix: &str) {
        self.prefix
            .push((from_prefix.to_string(), to_prefix.to_string()));
        // Re-sort by length descending
        self.prefix.sort_by_key(|b| std::cmp::Reverse(b.0.len()));
    }

    /// Map a model name through aliases.
    ///
    /// Returns the mapped name, or the original name if no mapping matches.
    pub fn map(&self, model: &str) -> String {
        // Try exact match first
        if let Some(target) = self.exact.get(model) {
            return target.clone();
        }

        // Try prefix match (longest first)
        for (from, to) in &self.prefix {
            if let Some(suffix) = model.strip_prefix(from.as_str()) {
                return format!("{to}{suffix}");
            }
        }

        // No mapping — return original
        model.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_alias() {
        let mut mapper = ModelMapper::new();
        mapper.add_exact("fast", "gpt-4o-mini");
        mapper.add_exact("smart", "claude-sonnet-4");

        assert_eq!(mapper.map("fast"), "gpt-4o-mini");
        assert_eq!(mapper.map("smart"), "claude-sonnet-4");
    }

    #[test]
    fn no_match_returns_original() {
        let mapper = ModelMapper::new();
        assert_eq!(mapper.map("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn prefix_stripping() {
        let mut mapper = ModelMapper::new();
        mapper.add_prefix("internal/", "");

        assert_eq!(mapper.map("internal/gpt-4o"), "gpt-4o");
        assert_eq!(mapper.map("internal/claude-sonnet-4"), "claude-sonnet-4");
    }

    #[test]
    fn prefix_replacement() {
        let mut mapper = ModelMapper::new();
        mapper.add_prefix("azure/", "");
        mapper.add_prefix("custom/", "my-org/");

        assert_eq!(mapper.map("azure/gpt-4o"), "gpt-4o");
        assert_eq!(mapper.map("custom/llama-70b"), "my-org/llama-70b");
    }

    #[test]
    fn exact_takes_precedence_over_prefix() {
        let mut mapper = ModelMapper::new();
        mapper.add_exact("internal/special", "custom-model-v2");
        mapper.add_prefix("internal/", "");

        // Exact match should win
        assert_eq!(mapper.map("internal/special"), "custom-model-v2");
        // Prefix still works for non-exact
        assert_eq!(mapper.map("internal/gpt-4o"), "gpt-4o");
    }

    #[test]
    fn longest_prefix_wins() {
        let mut mapper = ModelMapper::new();
        mapper.add_prefix("a/", "short/");
        mapper.add_prefix("a/b/", "long/");

        assert_eq!(mapper.map("a/b/model"), "long/model");
        assert_eq!(mapper.map("a/model"), "short/model");
    }

    #[test]
    fn from_config() {
        let mut config = HashMap::new();
        config.insert("fast".into(), "gpt-4o-mini".into());
        config.insert("internal/*".into(), String::new());

        let mapper = ModelMapper::from_config(config);
        assert_eq!(mapper.map("fast"), "gpt-4o-mini");
        assert_eq!(mapper.map("internal/gpt-4o"), "gpt-4o");
        assert_eq!(mapper.map("unknown"), "unknown");
    }
}
