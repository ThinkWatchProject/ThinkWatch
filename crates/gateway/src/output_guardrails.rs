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

use crate::providers::traits::{ChatCompletionResponse, GatewayError};

/// Single guardrail rule. New variants slot in here; the runtime
/// matches on them in `apply_output_guardrails`.
#[derive(Debug, Clone)]
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
}
