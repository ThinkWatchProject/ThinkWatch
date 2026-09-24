//! Token counts for a request the upstream did not report usage for.
//!
//! The upstream's own usage is the truth, and it is what gets billed
//! whenever it arrives. It does not always arrive: a caller that leaves
//! mid-stream takes the upstream's final usage chunk with it, and some
//! upstreams never send one. Billing such a request as zero tokens would
//! make it free — no cost, no budget debit, no rate-limit weight — so the
//! count is estimated instead, and the audit row says so
//! (`usage_estimated`).
//!
//! The estimate is about four bytes of text per token. It leans high for
//! most text, which is the side to err on for limits and budgets. Images
//! and files are not counted: their base64 would turn one screenshot into
//! hundreds of thousands of "tokens".

use serde_json::Value;
use tw_dialect::ir::{Part, Request, ToolInput, ToolKind};

const BYTES_PER_TOKEN: u64 = 4;

/// The input a request carries, in tokens.
pub fn request_tokens(r: &Request) -> u64 {
    let mut bytes: usize = r.system.iter().map(String::len).sum();
    for part in r.messages.iter().flat_map(|m| &m.parts) {
        bytes += part_len(part);
    }
    for tool in &r.tools {
        bytes += tool.name.len() + tool.description.as_ref().map_or(0, String::len);
        if let ToolKind::Function { schema, .. } = &tool.kind {
            bytes += schema.to_string().len();
        }
    }
    to_tokens(bytes)
}

fn part_len(part: &Part) -> usize {
    match part {
        Part::Text(t) => t.len(),
        Part::Thinking(t) => t.text.len(),
        Part::ToolCall(c) => {
            c.name.len()
                + match &c.input {
                    ToolInput::Json(v) => v.to_string().len(),
                    ToolInput::Text(t) => t.len(),
                }
        }
        Part::ToolResult(t) => t.text().len(),
        Part::Image(_) | Part::File { .. } => 0,
    }
}

/// The output a whole answer carries, in tokens — whatever its format.
///
/// Every string in the answer counts except the bookkeeping around the
/// text: ids, names of things, and reasoning signatures, which are opaque
/// blobs rather than generated text.
pub fn answer_tokens(body: &[u8]) -> u64 {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return to_tokens(body.len());
    };
    to_tokens(text_len(&v))
}

fn text_len(v: &Value) -> usize {
    const NOT_TEXT: &[&str] = &[
        "id",
        "model",
        "object",
        "type",
        "role",
        "status",
        "finish_reason",
        "stop_reason",
        "system_fingerprint",
        "service_tier",
        "call_id",
        "tool_call_id",
        "signature",
        "encrypted_content",
    ];
    match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(text_len).sum(),
        Value::Object(o) => o
            .iter()
            .filter(|(k, _)| !NOT_TEXT.contains(&k.as_str()))
            .map(|(_, v)| text_len(v))
            .sum(),
        _ => 0,
    }
}

fn to_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(BYTES_PER_TOKEN)
}

/// Fill in what the upstream did not report.
///
/// `reported` is what was read off the upstream's bytes, if anything.
/// `finished` says whether the answer ran to its end: an upstream reports
/// its output count at the end, so one that was cut short has at most a
/// running count, and the text that did arrive is the better measure.
///
/// Returns the usage to bill and whether any of it is an estimate.
pub fn complete(
    reported: Option<tw_wire::Usage>,
    finished: bool,
    input_estimate: u64,
    answer: Option<&[u8]>,
) -> (tw_wire::Usage, bool) {
    let output_estimate = || answer.map(answer_tokens).unwrap_or(0);
    match reported {
        Some(mut u) => {
            let mut estimated = false;
            if u.input + u.cache_read + u.cache_write == 0 && input_estimate > 0 {
                u.input = input_estimate;
                estimated = true;
            }
            if !finished {
                let out = output_estimate();
                if out > u.output {
                    u.output = out;
                    estimated = true;
                }
            }
            (u, estimated)
        }
        None => (
            tw_wire::Usage {
                input: input_estimate,
                output: output_estimate(),
                ..Default::default()
            },
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_answer_counts_its_text_and_not_its_bookkeeping() {
        let body = json!({
            "id": "chatcmpl-a-very-long-identifier-that-is-not-text",
            "model": "gpt-4o-2024-08-06",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "12345678"}}],
        });
        assert_eq!(answer_tokens(body.to_string().as_bytes()), 2);
    }

    #[test]
    fn a_reasoning_signature_is_not_output() {
        let body = json!({
            "content": [
                {"type": "thinking", "thinking": "abcd", "signature": "x".repeat(4000)},
                {"type": "text", "text": "efgh"},
            ],
        });
        assert_eq!(answer_tokens(body.to_string().as_bytes()), 2);
    }

    #[test]
    fn nothing_reported_is_estimated_whole() {
        let answer = json!({"choices": [{"message": {"content": "x".repeat(40)}}]}).to_string();
        let (u, estimated) = complete(None, true, 100, Some(answer.as_bytes()));
        assert!(estimated);
        assert_eq!((u.input, u.output), (100, 10));
    }

    #[test]
    fn a_finished_answer_keeps_what_the_upstream_reported() {
        let reported = tw_wire::Usage {
            input: 7,
            output: 3,
            ..Default::default()
        };
        let answer = json!({"text": "x".repeat(400)}).to_string();
        let (u, estimated) = complete(Some(reported), true, 100, Some(answer.as_bytes()));
        assert!(!estimated);
        assert_eq!(u, reported);
    }

    #[test]
    fn a_cut_short_answer_bills_the_text_that_arrived() {
        // Anthropic reports input at the start and a running output
        // count; a stream cut off after it has only the first count.
        let reported = tw_wire::Usage {
            input: 7,
            output: 1,
            ..Default::default()
        };
        let answer = json!({"text": "x".repeat(400)}).to_string();
        let (u, estimated) = complete(Some(reported), false, 100, Some(answer.as_bytes()));
        assert!(estimated);
        assert_eq!((u.input, u.output), (7, 100));
    }
}
