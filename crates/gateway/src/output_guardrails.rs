//! Output guardrails — per-model limits on what the model returns.
//!
//! Stored per model in `models.output_guardrails`. Today there is one
//! kind, `max_length`, and the engine is thinkwatch-core's
//! (`tw_guard::output`), shared with the desktop gateway:
//!
//! - a whole answer is measured before any of it goes out, and replaced
//!   by an error when it is over ([`apply_output_guardrails`]);
//! - a stream is measured frame by frame as it goes ([`StreamLimit`]);
//!   the frame that crosses the cap is not sent, and the stream is closed
//!   with an error in the caller's format.
//!
//! Only the answer's text counts — not thinking, not tool-call arguments.
//! It is measured in the caller's format, after any conversion, before
//! PII is painted back, so a placeholder cannot push an answer over.

use serde::{Deserialize, Serialize};
use tw_guard::output::{Limit, Meter, Unit};

use crate::error::GatewayError;

/// Inclusive upper bound on `MaxLength.max_chars`. Anything past this
/// is almost certainly a configuration mistake — even a 1M-char
/// completion is well beyond any model's context window — so we
/// reject it at admission rather than store a value the guardrail
/// could never trigger on.
pub const MAX_LENGTH_CAP_CEILING: usize = 1_000_000;

/// Single guardrail rule.
///
/// Serialized as `{"type": "max_length", "max_chars": N}` so the
/// `models.output_guardrails` JSONB column carries the discriminator
/// inline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputGuardrail {
    /// Refuse an answer whose text is longer than `max_chars`. Counted
    /// in **bytes**, as it always has been — for CJK text that is about
    /// three per character. Counting characters would quietly loosen
    /// every configured cap, so that stays its own decision.
    MaxLength { max_chars: usize },
}

/// The tightest length cap among `rules`, if any.
pub fn length_limit(rules: &[OutputGuardrail]) -> Option<Limit> {
    rules
        .iter()
        .map(|r| match r {
            OutputGuardrail::MaxLength { max_chars } => *max_chars,
        })
        .min()
        .map(|max| Limit {
            max,
            unit: Unit::Bytes,
        })
}

/// Check a whole answer, in the caller's format, against `rules`.
pub fn apply_output_guardrails(
    body: &[u8],
    client: tw_dialect::ir::Dialect,
    rules: &[OutputGuardrail],
) -> Result<(), GatewayError> {
    let Some(limit) = length_limit(rules) else {
        return Ok(());
    };
    match limit.check_whole(body, client) {
        Some(total) => Err(too_long(total, limit.max)),
        None => Ok(()),
    }
}

/// The length cap on a stream the caller reads in `client`'s format.
pub struct StreamLimit {
    meter: Meter,
    max: usize,
}

impl StreamLimit {
    /// `None` when the model has no length cap. The gateway's streams are
    /// SSE inside, whatever the caller asked for (see
    /// `proxy::generate::GEMINI_SSE`), so this reads SSE.
    pub fn new(rules: &[OutputGuardrail], client: tw_dialect::ir::Dialect) -> Option<Self> {
        length_limit(rules).map(|limit| Self {
            meter: Meter::sse(limit, client),
            max: limit.max,
        })
    }

    /// Feed the next client-format bytes. When they take the answer over
    /// the cap: the error to end the stream with, and how many leading
    /// bytes of `chunk` still go out (the whole frames before the one that
    /// crossed).
    pub fn check(&mut self, chunk: &[u8]) -> Option<(GatewayError, usize)> {
        let trip = self.meter.feed(chunk)?;
        Some((too_long(trip.seen, self.max), trip.safe_prefix))
    }
}

/// The error an answer over the cap becomes. The message names the rule
/// so operators can trace it back to the model's configuration.
pub fn too_long(total: usize, max: usize) -> GatewayError {
    GatewayError::TransformError(format!(
        "output guardrail max_length: response is {total} chars > {max} cap"
    ))
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
    fn max_length_counts_bytes() {
        // Three characters, nine bytes.
        let rules = [OutputGuardrail::MaxLength { max_chars: 8 }];
        assert!(apply_output_guardrails(&chat("你好吗"), Dialect::Chat, &rules).is_err());
    }

    #[test]
    fn the_tightest_cap_wins() {
        let rules = [
            OutputGuardrail::MaxLength { max_chars: 100 },
            OutputGuardrail::MaxLength { max_chars: 3 },
        ];
        assert_eq!(length_limit(&rules).unwrap().max, 3);
        assert!(StreamLimit::new(&[], Dialect::Chat).is_none());
    }

    #[test]
    fn a_stream_trips_on_the_frame_that_crosses_the_cap() {
        let rules = [OutputGuardrail::MaxLength { max_chars: 5 }];
        let mut m = StreamLimit::new(&rules, Dialect::Chat).unwrap();
        let chunk = |t: &str| {
            format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"index":0,"delta":{"content":t}}]})
            )
        };
        assert!(m.check(chunk("abc").as_bytes()).is_none());
        let first = chunk("de");
        let both = format!("{first}{}", chunk("fgh"));
        let (err, safe) = m.check(both.as_bytes()).expect("over the cap");
        assert!(err.to_string().contains("8 chars > 5 cap"), "{err}");
        assert_eq!(safe, first.len());
        // Reported once.
        assert!(m.check(chunk("more").as_bytes()).is_none());
    }

    #[test]
    fn no_rules_means_no_parsing_at_all() {
        assert!(apply_output_guardrails(b"not json", Dialect::Chat, &[]).is_ok());
    }
}
