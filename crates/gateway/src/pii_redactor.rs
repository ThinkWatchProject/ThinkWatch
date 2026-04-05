use crate::providers::traits::{ChatCompletionResponse, ChatMessage};
use rand::RngExt;
use regex::Regex;
use std::collections::HashMap;

/// Serializable PII pattern for storage in system_settings.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PiiPatternConfig {
    pub name: String,
    pub regex: String,
    pub placeholder_prefix: String,
}

/// Detects and replaces PII in user messages before sending to upstream LLMs,
/// then restores original values in the response.
#[derive(Clone)]
pub struct PiiRedactor {
    patterns: Vec<PiiPattern>,
}

#[derive(Clone)]
struct PiiPattern {
    name: String,
    regex: Regex,
    placeholder_prefix: String,
}

/// Holds the mapping from placeholders back to original PII values.
pub struct RedactionContext {
    /// Maps placeholder (e.g. `{{EMAIL_a3f1_1}}`) to original value.
    pub replacements: HashMap<String, String>,
}

impl Default for PiiRedactor {
    fn default() -> Self {
        Self::new()
    }
}

impl PiiRedactor {
    /// Create a PII redactor from a list of pattern configs (from DynamicConfig).
    pub fn from_config(configs: &[PiiPatternConfig]) -> Self {
        let patterns = configs
            .iter()
            .filter_map(|c| match Regex::new(&c.regex) {
                Ok(regex) => Some(PiiPattern {
                    name: c.name.clone(),
                    regex,
                    placeholder_prefix: c.placeholder_prefix.clone(),
                }),
                Err(e) => {
                    tracing::warn!("Invalid PII regex pattern '{}': {e}", c.name);
                    None
                }
            })
            .collect();
        Self { patterns }
    }

    pub fn new() -> Self {
        // Order matters: longer/more specific patterns must come before shorter ones
        // to prevent partial matches (e.g. phone patterns matching inside credit cards).
        let patterns = vec![
            PiiPattern {
                name: "email".into(),
                regex: Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap(),
                placeholder_prefix: "EMAIL".into(),
            },
            PiiPattern {
                name: "id_card_cn".into(),
                regex: Regex::new(r"\b\d{17}[\dXx]\b").unwrap(),
                placeholder_prefix: "ID".into(),
            },
            PiiPattern {
                name: "credit_card".into(),
                regex: Regex::new(r"\b\d{4}[-\s]?\d{4}[-\s]?\d{4}[-\s]?\d{4}\b").unwrap(),
                placeholder_prefix: "CARD".into(),
            },
            PiiPattern {
                name: "phone_cn".into(),
                regex: Regex::new(r"1[3-9]\d{9}").unwrap(),
                placeholder_prefix: "PHONE".into(),
            },
            PiiPattern {
                name: "phone_us".into(),
                regex: Regex::new(r"\b\d{3}[-.]?\d{3}[-.]?\d{4}\b").unwrap(),
                placeholder_prefix: "PHONE".into(),
            },
            PiiPattern {
                name: "ipv4".into(),
                regex: Regex::new(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b").unwrap(),
                placeholder_prefix: "IP".into(),
            },
        ];

        Self { patterns }
    }

    /// Redact PII from user messages, returning modified messages and a context
    /// that can be used to restore original values in the response.
    ///
    /// Only `user` role messages are redacted; `system` and `assistant` messages
    /// are left unchanged.
    ///
    /// Uses a single-pass approach: build a combined regex from all patterns,
    /// find all matches with positions, sort by position (descending), and
    /// replace in reverse order to avoid invalidating offsets.
    pub fn redact_messages(
        &self,
        messages: &[ChatMessage],
    ) -> (Vec<ChatMessage>, RedactionContext) {
        let mut counters: HashMap<String, u32> = HashMap::new();
        let mut replacements: HashMap<String, String> = HashMap::new();

        // Per-request random salt to prevent placeholder prediction
        let salt: u16 = rand::rng().random();
        let salt_hex = format!("{salt:04x}");

        let redacted = messages
            .iter()
            .map(|msg| {
                if msg.role != "user" {
                    return msg.clone();
                }

                let content_str = match msg.content.as_str() {
                    Some(s) => s.to_string(),
                    None => return msg.clone(),
                };

                // Collect all matches across all patterns with their positions
                let mut all_matches: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, pattern_idx)
                for (pattern_idx, pattern) in self.patterns.iter().enumerate() {
                    for m in pattern.regex.find_iter(&content_str) {
                        all_matches.push((m.start(), m.end(), pattern_idx));
                    }
                }

                if all_matches.is_empty() {
                    return msg.clone();
                }

                // Sort by start position ascending, then by length descending (prefer longer matches)
                all_matches
                    .sort_by(|a, b| a.0.cmp(&b.0).then_with(|| (b.1 - b.0).cmp(&(a.1 - a.0))));

                // Remove overlapping matches — keep the longest match at each position
                let mut filtered: Vec<(usize, usize, usize)> = Vec::new();
                for m in &all_matches {
                    // Only add if it doesn't overlap with any already-accepted match
                    if filtered.iter().all(|f| m.0 >= f.1 || m.1 <= f.0) {
                        filtered.push(*m);
                    }
                }
                // Sort descending by start for safe replacement
                filtered.sort_by(|a, b| b.0.cmp(&a.0));

                // Collect pattern names for logging before consuming filtered
                let redacted_pattern_names: Vec<String> = filtered
                    .iter()
                    .map(|(_, _, idx)| self.patterns[*idx].name.clone())
                    .collect();

                let mut redacted_content = content_str;
                for (start, end, pattern_idx) in filtered {
                    let pattern = &self.patterns[pattern_idx];
                    let matched_value = &redacted_content[start..end];
                    let counter = counters
                        .entry(pattern.placeholder_prefix.clone())
                        .or_insert(0);
                    *counter += 1;
                    let placeholder = format!(
                        "{{{{{}_{}_{}}}}}",
                        pattern.placeholder_prefix, salt_hex, counter
                    );
                    replacements.insert(placeholder.clone(), matched_value.to_string());
                    redacted_content.replace_range(start..end, &placeholder);
                }

                if !redacted_pattern_names.is_empty() {
                    tracing::debug!(
                        patterns = ?redacted_pattern_names,
                        count = redacted_pattern_names.len(),
                        "PII redacted from user message"
                    );
                }

                ChatMessage {
                    role: msg.role.clone(),
                    content: serde_json::Value::String(redacted_content),
                }
            })
            .collect();

        (redacted, RedactionContext { replacements })
    }

    /// Restore placeholders in the response content back to original PII values.
    pub fn restore_response(&self, response: &mut ChatCompletionResponse, ctx: &RedactionContext) {
        if ctx.replacements.is_empty() {
            return;
        }

        for choice in &mut response.choices {
            if let Some(content_str) = choice.message.content.as_str() {
                let mut restored = content_str.to_string();
                for (placeholder, original) in &ctx.replacements {
                    restored = restored.replace(placeholder, original);
                }
                choice.message.content = serde_json::Value::String(restored);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::{ChatCompletionResponse, ChatMessage, Choice, Usage};

    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(content.to_string()),
        }
    }

    fn system_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "system".to_string(),
            content: serde_json::Value::String(content.to_string()),
        }
    }

    fn make_response(content: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "test".to_string(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(content.to_string()),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
            }),
        }
    }

    /// Find the placeholder replacement that maps to the given original value.
    fn find_placeholder(ctx: &RedactionContext, original: &str) -> String {
        ctx.replacements
            .iter()
            .find(|(_, v)| v.as_str() == original)
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| panic!("no placeholder for {original}"))
    }

    #[test]
    fn redact_email() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Contact me at alice@example.com please")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(!content.contains("alice@example.com"));
        let ph = find_placeholder(&ctx, "alice@example.com");
        assert!(ph.starts_with("{{EMAIL_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_china_phone() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Call me at 13812345678")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("PHONE"), "got: {content}");
        assert!(!content.contains("13812345678"));
        let ph = find_placeholder(&ctx, "13812345678");
        assert!(ph.starts_with("{{PHONE_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_us_phone() {
        let redactor = PiiRedactor::new();
        // Simplified US phone regex matches 10-digit patterns like 555-123-4567
        let messages = vec![user_msg("Call 555-123-4567")];
        let (redacted, _ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(
            content.contains("PHONE"),
            "phone should be redacted, got: {content}"
        );
        assert!(!content.contains("123-4567"));
    }

    #[test]
    fn redact_credit_card() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("My card is 4111-1111-1111-1111")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("CARD"), "got: {content}");
        assert!(!content.contains("4111"));
        let ph = find_placeholder(&ctx, "4111-1111-1111-1111");
        assert!(ph.starts_with("{{CARD_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_china_id_card() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("ID: 110101199001011234")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("ID"), "got: {content}");
        assert!(!content.contains("110101199001011234"));
        let ph = find_placeholder(&ctx, "110101199001011234");
        assert!(ph.starts_with("{{ID_"), "placeholder format: {ph}");
    }

    #[test]
    fn redact_ipv4() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Server is at 192.168.1.100")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("IP"), "got: {content}");
        assert!(!content.contains("192.168.1.100"));
        let ph = find_placeholder(&ctx, "192.168.1.100");
        assert!(ph.starts_with("{{IP_"), "placeholder format: {ph}");
    }

    #[test]
    fn does_not_redact_system_messages() {
        let redactor = PiiRedactor::new();
        let messages = vec![system_msg("Contact admin@example.com for help")];
        let (redacted, _ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("admin@example.com"));
    }

    #[test]
    fn restore_response_replaces_placeholders() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Email alice@example.com and bob@test.org")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        // Simulate the LLM echoing back the redacted content
        let redacted_content = redacted[0].content.as_str().unwrap();
        let mut response = make_response(redacted_content);
        redactor.restore_response(&mut response, &ctx);

        let content = response.choices[0].message.content.as_str().unwrap();
        assert!(content.contains("alice@example.com"), "got: {content}");
        assert!(content.contains("bob@test.org"), "got: {content}");
        assert!(!content.contains("{{EMAIL_"));
    }

    #[test]
    fn multiple_pii_types() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg(
            "Email alice@example.com, IP 10.0.0.1, card 4111 1111 1111 1111",
        )];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(content.contains("IP"), "got: {content}");
        assert!(content.contains("CARD"), "got: {content}");
        assert!(!content.contains("alice@example.com"));
        assert!(!content.contains("10.0.0.1"));

        // Verify restore round-trip
        let mut response = make_response(content);
        redactor.restore_response(&mut response, &ctx);
        let restored = response.choices[0].message.content.as_str().unwrap();
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

        let messages = vec![user_msg("Contact test@example.com for info")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
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
        let messages = vec![user_msg("Contact me at alice@test.org")];
        let (redacted, _ctx) = redactor.redact_messages(&messages);
        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("EMAIL"), "got: {content}");
        assert!(!content.contains("alice@test.org"));
    }
}
