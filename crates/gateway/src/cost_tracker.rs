use std::collections::HashMap;

/// Tracks and calculates costs for AI API calls based on per-model pricing.
///
/// Prices are in USD per 1 million tokens.
pub struct CostTracker {
    /// (model_pattern, input_price_per_1m, output_price_per_1m)
    price_table: Vec<(String, f64, f64)>,
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CostTracker {
    /// Built-in default price table (USD per 1M tokens).
    fn default_price_table() -> Vec<(String, f64, f64)> {
        vec![
            // OpenAI models
            ("gpt-4o-mini".into(), 0.15, 0.60),
            ("gpt-4o".into(), 2.50, 10.00),
            ("gpt-4-turbo".into(), 10.00, 30.00),
            ("gpt-4".into(), 30.00, 60.00),
            ("gpt-3.5-turbo".into(), 0.50, 1.50),
            ("o1-mini".into(), 3.00, 12.00),
            ("o1-preview".into(), 15.00, 60.00),
            ("o1".into(), 15.00, 60.00),
            ("o3-mini".into(), 1.10, 4.40),
            // Anthropic models
            ("claude-sonnet-4".into(), 3.00, 15.00),
            ("claude-3-5-sonnet".into(), 3.00, 15.00),
            ("claude-3-5-haiku".into(), 0.80, 4.00),
            ("claude-3-opus".into(), 15.00, 75.00),
            ("claude-3-haiku".into(), 0.25, 1.25),
            ("claude-3-sonnet".into(), 3.00, 15.00),
            ("claude-haiku".into(), 0.80, 4.00),
            ("claude-opus".into(), 15.00, 75.00),
            // Google models
            ("gemini-1.5-pro".into(), 3.50, 10.50),
            ("gemini-1.5-flash".into(), 0.075, 0.30),
            ("gemini-2.0-flash".into(), 0.10, 0.40),
            ("gemini-2.5-pro".into(), 1.25, 10.00),
        ]
    }

    pub fn new() -> Self {
        Self {
            price_table: Self::default_price_table(),
        }
    }

    /// Create a `CostTracker` with custom price overrides.
    ///
    /// Entries in `overrides` take precedence over built-in defaults.
    /// Keys are model prefixes, values are `(input_per_1m, output_per_1m)`.
    pub fn with_overrides(overrides: HashMap<String, (f64, f64)>) -> Self {
        let defaults = Self::default_price_table();

        // Custom prices first so they are matched before defaults
        let mut table: Vec<(String, f64, f64)> = overrides
            .into_iter()
            .map(|(k, (i, o))| (k, i, o))
            .collect();
        // Sort custom entries by pattern length descending (longer = more specific first)
        table.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        // Append defaults for any models not overridden
        for entry in defaults {
            if !table.iter().any(|(p, _, _)| p == &entry.0) {
                table.push(entry);
            }
        }

        Self { price_table: table }
    }

    /// Calculate the cost in USD for a given model and token counts.
    ///
    /// Looks up the model in the price table using prefix matching.
    /// Returns 0.0 if the model is not found.
    pub fn calculate_cost(&self, model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
        let (input_price, output_price) = self.lookup_price(model);

        let input_cost = (f64::from(input_tokens) / 1_000_000.0) * input_price;
        let output_cost = (f64::from(output_tokens) / 1_000_000.0) * output_price;

        input_cost + output_cost
    }

    /// Look up the per-1M-token prices for a model.
    /// Returns (input_price, output_price) or (0.0, 0.0) if not found.
    fn lookup_price(&self, model: &str) -> (f64, f64) {
        let model_lower = model.to_lowercase();

        for (pattern, input, output) in &self.price_table {
            if model_lower.starts_with(pattern.as_str()) || model_lower.contains(pattern.as_str())
            {
                return (*input, *output);
            }
        }

        tracing::debug!("No pricing found for model: {model}");
        (0.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt4o_cost() {
        let tracker = CostTracker::new();
        // 1000 input + 500 output tokens on gpt-4o
        // input: 1000/1M * 2.50 = 0.0025
        // output: 500/1M * 10.00 = 0.005
        let cost = tracker.calculate_cost("gpt-4o", 1000, 500);
        assert!((cost - 0.0075).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn gpt4o_mini_matches_before_gpt4o() {
        let tracker = CostTracker::new();
        let cost = tracker.calculate_cost("gpt-4o-mini", 1_000_000, 0);
        assert!((cost - 0.15).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn unknown_model_returns_zero() {
        let tracker = CostTracker::new();
        let cost = tracker.calculate_cost("some-unknown-model", 1000, 1000);
        assert!((cost - 0.0).abs() < 1e-9);
    }

    #[test]
    fn claude_sonnet_cost() {
        let tracker = CostTracker::new();
        let cost = tracker.calculate_cost("claude-sonnet-4-20250514", 1_000_000, 1_000_000);
        // input: 3.00, output: 15.00 -> 18.00
        assert!((cost - 18.0).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn custom_price_override() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("gpt-4o".into(), (5.00, 20.00));
        let tracker = CostTracker::with_overrides(overrides);
        let cost = tracker.calculate_cost("gpt-4o", 1_000_000, 0);
        assert!((cost - 5.0).abs() < 1e-9, "got {cost}");
        // Models not overridden should still use defaults
        let cost2 = tracker.calculate_cost("claude-3-opus", 1_000_000, 0);
        assert!((cost2 - 15.0).abs() < 1e-9, "got {cost2}");
    }
}
