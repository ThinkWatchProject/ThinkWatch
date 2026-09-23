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

use tw_types::GatewayError;

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
    body: &[u8],
    client: tw_dialect::ir::Dialect,
    rules: &[OutputGuardrail],
) -> Result<(), GatewayError> {
    if rules.is_empty() {
        return Ok(());
    }
    let text = assistant_text(body, client);
    for rule in rules {
        match rule {
            OutputGuardrail::MaxLength { max_chars } => {
                // Counts bytes, as it always has — for CJK text that is
                // about three per character. Changing it to characters
                // would quietly loosen every configured cap, so it stays
                // until that is decided on its own.
                let total = text.len();
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

/// The assistant's text in a whole response, in whichever format the
/// caller asked for. The conversion layer already knows where each
/// format keeps it.
fn assistant_text(body: &[u8], client: tw_dialect::ir::Dialect) -> String {
    use tw_dialect::ir::{Block, Dialect};
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::new();
    };
    let r = match client {
        Dialect::Chat => tw_dialect::chat::decode_response(&v),
        Dialect::Anthropic => tw_dialect::anthropic::decode_response(&v),
        Dialect::Responses => tw_dialect::responses::decode_response(&v),
        Dialect::Gemini => tw_dialect::gemini::decode_response(&v),
        Dialect::Bedrock => tw_dialect::bedrock::decode_response(&v),
    };
    r.blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tw_dialect::ir::Dialect;

    fn chat(content: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "id", "object": "chat.completion", "created": 0, "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                         "finish_reason": "stop"}]
        }))
        .unwrap()
    }

    fn anthropic(text: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "msg", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": text}], "stop_reason": "end_turn"
        }))
        .unwrap()
    }

    #[test]
    fn max_length_allows_a_response_within_the_cap() {
        let rules = [OutputGuardrail::MaxLength { max_chars: 10 }];
        assert!(apply_output_guardrails(&chat("short"), Dialect::Chat, &rules).is_ok());
    }

    #[test]
    fn max_length_rejects_a_response_over_the_cap() {
        let rules = [OutputGuardrail::MaxLength { max_chars: 3 }];
        assert!(apply_output_guardrails(&chat("too long"), Dialect::Chat, &rules).is_err());
    }

    #[test]
    fn max_length_reads_the_text_in_whichever_format_the_caller_asked_for() {
        // The cap used to read `choices[].message.content` only, so an
        // Anthropic-shaped answer would have measured as empty.
        let rules = [OutputGuardrail::MaxLength { max_chars: 3 }];
        assert!(
            apply_output_guardrails(&anthropic("too long"), Dialect::Anthropic, &rules).is_err()
        );
    }

    #[test]
    fn no_rules_means_no_parsing_at_all() {
        assert!(apply_output_guardrails(b"not json", Dialect::Chat, &[]).is_ok());
    }
}
