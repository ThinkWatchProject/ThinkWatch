use crate::providers::traits::{ChatCompletionResponse, ChatMessage};
use regex::Regex;
use std::collections::HashMap;

/// Detects and replaces PII in user messages before sending to upstream LLMs,
/// then restores original values in the response.
#[derive(Clone)]
pub struct PiiRedactor {
    patterns: Vec<PiiPattern>,
}

#[derive(Clone)]
struct PiiPattern {
    name: &'static str,
    regex: Regex,
    placeholder_prefix: &'static str,
}

/// Holds the mapping from placeholders back to original PII values.
pub struct RedactionContext {
    /// Maps placeholder (e.g. `{{EMAIL_1}}`) to original value.
    pub replacements: HashMap<String, String>,
}

impl Default for PiiRedactor {
    fn default() -> Self {
        Self::new()
    }
}

impl PiiRedactor {
    pub fn new() -> Self {
        // Order matters: longer/more specific patterns must come before shorter ones
        // to prevent partial matches (e.g. phone patterns matching inside credit cards).
        let patterns = vec![
            PiiPattern {
                name: "email",
                regex: Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap(),
                placeholder_prefix: "EMAIL",
            },
            PiiPattern {
                name: "id_card_cn",
                regex: Regex::new(r"\b\d{17}[\dXx]\b").unwrap(),
                placeholder_prefix: "ID",
            },
            PiiPattern {
                name: "credit_card",
                regex: Regex::new(r"\b\d{4}[-\s]?\d{4}[-\s]?\d{4}[-\s]?\d{4}\b").unwrap(),
                placeholder_prefix: "CARD",
            },
            PiiPattern {
                name: "phone_cn",
                regex: Regex::new(r"1[3-9]\d{9}").unwrap(),
                placeholder_prefix: "PHONE",
            },
            PiiPattern {
                name: "phone_us",
                regex: Regex::new(
                    r"(\+1[-.\s]?)?(\(?\d{3}\)?[-.\s]?)?\d{3}[-.\s]?\d{4}",
                )
                .unwrap(),
                placeholder_prefix: "PHONE",
            },
            PiiPattern {
                name: "ipv4",
                regex: Regex::new(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b").unwrap(),
                placeholder_prefix: "IP",
            },
        ];

        Self { patterns }
    }

    /// Redact PII from user messages, returning modified messages and a context
    /// that can be used to restore original values in the response.
    ///
    /// Only `user` role messages are redacted; `system` and `assistant` messages
    /// are left unchanged.
    pub fn redact_messages(
        &self,
        messages: &[ChatMessage],
    ) -> (Vec<ChatMessage>, RedactionContext) {
        let mut counters: HashMap<&str, u32> = HashMap::new();
        let mut replacements: HashMap<String, String> = HashMap::new();

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

                let mut redacted_content = content_str;

                for pattern in &self.patterns {
                    // Collect matches first to avoid borrow issues
                    let matches: Vec<String> = pattern
                        .regex
                        .find_iter(&redacted_content.clone())
                        .map(|m| m.as_str().to_string())
                        .collect();

                    for matched_value in matches {
                        let counter = counters
                            .entry(pattern.placeholder_prefix)
                            .or_insert(0);
                        *counter += 1;
                        let placeholder =
                            format!("{{{{{}_{}}}}}",  pattern.placeholder_prefix, counter);
                        replacements.insert(placeholder.clone(), matched_value.clone());
                        redacted_content =
                            redacted_content.replacen(&matched_value, &placeholder, 1);
                    }
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
    pub fn restore_response(
        &self,
        response: &mut ChatCompletionResponse,
        ctx: &RedactionContext,
    ) {
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

    #[test]
    fn redact_email() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Contact me at alice@example.com please")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("{{EMAIL_1}}"), "got: {content}");
        assert!(!content.contains("alice@example.com"));
        assert_eq!(ctx.replacements["{{EMAIL_1}}"], "alice@example.com");
    }

    #[test]
    fn redact_china_phone() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Call me at 13812345678")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("{{PHONE_1}}"), "got: {content}");
        assert!(!content.contains("13812345678"));
        assert_eq!(ctx.replacements["{{PHONE_1}}"], "13812345678");
    }

    #[test]
    fn redact_us_phone() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Call +1 (555) 123-4567")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        // The phone should be redacted (may match as PHONE_1 or PHONE_2 depending on pattern order)
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
        assert!(content.contains("{{CARD_1}}"), "got: {content}");
        assert!(!content.contains("4111"));
        assert_eq!(ctx.replacements["{{CARD_1}}"], "4111-1111-1111-1111");
    }

    #[test]
    fn redact_china_id_card() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("ID: 110101199001011234")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("{{ID_1}}"), "got: {content}");
        assert!(!content.contains("110101199001011234"));
        assert_eq!(ctx.replacements["{{ID_1}}"], "110101199001011234");
    }

    #[test]
    fn redact_ipv4() {
        let redactor = PiiRedactor::new();
        let messages = vec![user_msg("Server is at 192.168.1.100")];
        let (redacted, ctx) = redactor.redact_messages(&messages);

        let content = redacted[0].content.as_str().unwrap();
        assert!(content.contains("{{IP_1}}"), "got: {content}");
        assert!(!content.contains("192.168.1.100"));
        assert_eq!(ctx.replacements["{{IP_1}}"], "192.168.1.100");
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
        let (_, ctx) = redactor.redact_messages(&messages);

        let mut response = make_response(
            "I found {{EMAIL_1}} and {{EMAIL_2}} in your message.",
        );
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
}
