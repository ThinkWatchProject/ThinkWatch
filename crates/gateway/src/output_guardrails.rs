//! Output guardrails — server-side validation of provider responses.
//!
//! Input-side controls already exist (content_filter denies on the
//! way in, pii_redactor scrubs caller data). This module is the
//! symmetric output check: enforce schemas / format constraints on
//! what the model returned BEFORE the caller sees it. Today the
//! library only carries a JSON-schema validator stub; the wiring
//! point is `apply_output_guardrails`, called from the proxy after
//! the upstream response lands but before serialisation.
//!
//! Roadmap (each lands as its own enum variant + a `validate` impl):
//!
//!   * `JsonSchema(String)` — assert response.choices[0].message.content
//!     parses + validates against the supplied JSON schema. Useful for
//!     tool-style models that the operator wants to enforce as
//!     `tool_call(arguments: T)` instead of free-form text.
//!   * `MaxLength(usize)`   — bound the completion size on the way out
//!     for cost / display safety, after the model has already returned
//!     more than a buyer would tolerate.
//!   * `Toxicity(f32)`      — score the completion via a configured
//!     classifier and reject above the threshold.
//!
//! On rejection the helper returns `GatewayError::TransformError`
//! with a structured reason so the gateway_logs row carries the
//! triggering rule (the existing OBS-05 error-type taxonomy already
//! has slots for this).

use serde::{Deserialize, Serialize};

use crate::providers::traits::{ChatCompletionResponse, GatewayError};

/// Inclusive upper bound on `MaxLength.max_chars`. Anything past this
/// is almost certainly a configuration mistake — even a 1M-char
/// completion is well beyond any model's context window — so we
/// reject it at admission rather than store a value the guardrail
/// could never trigger on.
pub const MAX_LENGTH_CAP_CEILING: usize = 1_000_000;

/// Single guardrail rule. New variants slot in here; the runtime
/// matches on them in `apply_output_guardrails`.
///
/// Serialized as `{"type": "max_length", "max_chars": N}` so the
/// `models.output_guardrails` JSONB column carries the discriminator
/// inline and future variants (JsonSchema, Toxicity — see module
/// docstring) land without breaking older rows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputGuardrail {
    /// Reject when the assistant message exceeds `max_chars`. Cheap
    /// to evaluate and protects rendering pipelines from runaway
    /// completions.
    MaxLength { max_chars: usize },
}

/// Apply every guardrail in order; first rejection short-circuits.
/// The error message names which rule fired so operators can chase
/// it back to the configuration row that produced it.
pub fn apply_output_guardrails(
    response: &ChatCompletionResponse,
    rules: &[OutputGuardrail],
) -> Result<(), GatewayError> {
    for rule in rules {
        match rule {
            OutputGuardrail::MaxLength { max_chars } => {
                let total: usize = response
                    .choices
                    .iter()
                    .map(|c| c.message.content.as_str().map(|s| s.len()).unwrap_or(0))
                    .sum();
                if total > *max_chars {
                    return Err(GatewayError::TransformError(format!(
                        "output guardrail max_length: response is {total} chars > {max_chars} cap"
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::traits::{ChatMessage, Choice};

    fn resp(content: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "id".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "m".into(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String(content.into()),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    #[test]
    fn max_length_passes_under_cap() {
        let r = resp("hello");
        let rules = [OutputGuardrail::MaxLength { max_chars: 100 }];
        assert!(apply_output_guardrails(&r, &rules).is_ok());
    }

    #[test]
    fn max_length_rejects_over_cap() {
        let r = resp(&"x".repeat(200));
        let rules = [OutputGuardrail::MaxLength { max_chars: 100 }];
        let err = apply_output_guardrails(&r, &rules).unwrap_err();
        assert!(matches!(err, GatewayError::TransformError(_)));
    }

    #[test]
    fn empty_rules_pass_any_response() {
        let r = resp(&"x".repeat(10_000));
        assert!(apply_output_guardrails(&r, &[]).is_ok());
    }

    #[test]
    fn exactly_at_cap_passes() {
        // `>` not `>=` — content of exactly max_chars must be allowed.
        // Lock this in so an over-cautious refactor to `>=` is caught.
        let r = resp(&"x".repeat(100));
        let rules = [OutputGuardrail::MaxLength { max_chars: 100 }];
        assert!(apply_output_guardrails(&r, &rules).is_ok());
    }

    #[test]
    fn one_char_over_cap_rejects() {
        let r = resp(&"x".repeat(101));
        let rules = [OutputGuardrail::MaxLength { max_chars: 100 }];
        assert!(apply_output_guardrails(&r, &rules).is_err());
    }

    #[test]
    fn non_string_content_counts_as_zero() {
        // Tool-call responses set content to a JSON array; the guardrail
        // shouldn't blow up there — it should just count those choices
        // as zero-length and let the rule decide.
        let r = ChatCompletionResponse {
            id: "id".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "m".into(),
            choices: vec![Choice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::json!([{"type": "tool_use"}]),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let rules = [OutputGuardrail::MaxLength { max_chars: 5 }];
        assert!(apply_output_guardrails(&r, &rules).is_ok());
    }

    #[test]
    fn multi_choice_content_sums_across_choices() {
        // n-best sampling: two choices, each 60 chars, summed = 120 > 100.
        let r = ChatCompletionResponse {
            id: "id".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "m".into(),
            choices: (0..2)
                .map(|i| Choice {
                    index: i,
                    message: ChatMessage {
                        role: "assistant".into(),
                        content: serde_json::Value::String("x".repeat(60)),
                        ..Default::default()
                    },
                    finish_reason: None,
                })
                .collect(),
            usage: None,
        };
        let rules = [OutputGuardrail::MaxLength { max_chars: 100 }];
        assert!(apply_output_guardrails(&r, &rules).is_err());
    }
}
