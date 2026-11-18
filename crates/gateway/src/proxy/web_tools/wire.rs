// From ThinkWatch-Core v0.98.1. Copyright (c) 2026 ThinkWatch (MIT).
// The upstream license is in LICENSE.core-web-tools.

//! Encode complete native responses as valid native SSE. Only hosted-tool requests
//! use this path; ordinary model traffic retains its original streaming behavior.

use serde_json::{Value, json};
use tw_dialect::ir::Dialect;

pub(crate) fn stream(dialect: Dialect, response: &Value) -> Vec<u8> {
    let mut out = String::new();
    let mut sequence = 0;
    let mut event = |kind: &str, mut value: Value| {
        value["type"] = json!(kind);
        if dialect == Dialect::Responses {
            value["sequence_number"] = json!(sequence);
            sequence += 1;
        }
        out.push_str(&format!("event: {kind}\ndata: {value}\n\n"));
    };
    if dialect == Dialect::Anthropic {
        let mut start = response.clone();
        start["content"] = json!([]);
        start["stop_reason"] = Value::Null;
        start["stop_sequence"] = Value::Null;
        start["usage"]["output_tokens"] = json!(0);
        event("message_start", json!({"message":start}));
        for (index, block) in response["content"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let mut start = block.clone();
            let delta = match block["type"].as_str() {
                Some("text") => {
                    start["text"] = json!("");
                    Some(json!({"type":"text_delta","text":block["text"]}))
                }
                Some("thinking") => {
                    start["thinking"] = json!("");
                    start["signature"] = json!("");
                    Some(json!({"type":"thinking_delta","thinking":block["thinking"]}))
                }
                Some("tool_use" | "server_tool_use") => {
                    start["input"] = json!({});
                    Some(
                        json!({"type":"input_json_delta","partial_json":block["input"].to_string()}),
                    )
                }
                _ => None,
            };
            event(
                "content_block_start",
                json!({"index":index,"content_block":start}),
            );
            if let Some(delta) = delta {
                event("content_block_delta", json!({"index":index,"delta":delta}));
            }
            if block["type"] == "thinking" && block["signature"].is_string() {
                event(
                    "content_block_delta",
                    json!({"index":index,"delta":{"type":"signature_delta","signature":block["signature"]}}),
                );
            }
            event("content_block_stop", json!({"index":index}));
        }
        let mut delta = json!({"stop_reason":response["stop_reason"],"stop_sequence":response["stop_sequence"]});
        if let Some(results) = response.get("safeguard_results") {
            delta["safeguard_results"] = results.clone();
        }
        event(
            "message_delta",
            json!({"delta":delta,"usage":response["usage"]}),
        );
        event("message_stop", json!({}));
    } else {
        let mut start = response.clone();
        start["output"] = json!([]);
        start["status"] = json!("in_progress");
        event("response.created", json!({"response":start}));
        event("response.in_progress", json!({"response":start}));
        for (index, item) in response["output"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let mut initial = item.clone();
            if item["type"] == "message" {
                initial["content"] = json!([]);
                initial["status"] = json!("in_progress");
            }
            if item["type"] == "function_call" {
                initial["arguments"] = json!("");
            }
            if item["type"] == "reasoning" {
                initial["summary"] = json!([]);
            }
            if item["type"] == "web_search_call" {
                initial["status"] = json!("in_progress");
            }
            event(
                "response.output_item.added",
                json!({"output_index":index,"item":initial}),
            );
            if item["type"] == "web_search_call" {
                for kind in [
                    "response.web_search_call.in_progress",
                    "response.web_search_call.searching",
                    "response.web_search_call.completed",
                ] {
                    event(kind, json!({"output_index":index,"item_id":item["id"]}));
                }
            }
            for (part_index, part) in item["content"].as_array().into_iter().flatten().enumerate() {
                let mut initial = part.clone();
                if part["type"] == "output_text" {
                    initial["text"] = json!("");
                    initial["annotations"] = json!([]);
                }
                event(
                    "response.content_part.added",
                    json!({"output_index":index,"content_index":part_index,"item_id":item["id"],"part":initial}),
                );
                if part["type"] == "output_text" {
                    event(
                        "response.output_text.delta",
                        json!({"output_index":index,"content_index":part_index,"item_id":item["id"],"delta":part["text"]}),
                    );
                    for (annotation_index, annotation) in part["annotations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .enumerate()
                    {
                        event(
                            "response.output_text.annotation.added",
                            json!({"output_index":index,"content_index":part_index,"item_id":item["id"],"annotation_index":annotation_index,"annotation":annotation}),
                        );
                    }
                    event(
                        "response.output_text.done",
                        json!({"output_index":index,"content_index":part_index,"item_id":item["id"],"text":part["text"]}),
                    );
                }
                event(
                    "response.content_part.done",
                    json!({"output_index":index,"content_index":part_index,"item_id":item["id"],"part":part}),
                );
            }
            for (summary_index, part) in
                item["summary"].as_array().into_iter().flatten().enumerate()
            {
                event(
                    "response.reasoning_summary_part.added",
                    json!({"output_index":index,"item_id":item["id"],"summary_index":summary_index,"part":{"type":"summary_text","text":""}}),
                );
                event(
                    "response.reasoning_summary_text.delta",
                    json!({"output_index":index,"item_id":item["id"],"summary_index":summary_index,"delta":part["text"]}),
                );
                event(
                    "response.reasoning_summary_text.done",
                    json!({"output_index":index,"item_id":item["id"],"summary_index":summary_index,"text":part["text"]}),
                );
                event(
                    "response.reasoning_summary_part.done",
                    json!({"output_index":index,"item_id":item["id"],"summary_index":summary_index,"part":part}),
                );
            }
            if item["type"] == "function_call" {
                event(
                    "response.function_call_arguments.delta",
                    json!({"output_index":index,"item_id":item["id"],"delta":item["arguments"]}),
                );
                event(
                    "response.function_call_arguments.done",
                    json!({"output_index":index,"item_id":item["id"],"arguments":item["arguments"]}),
                );
            }
            event(
                "response.output_item.done",
                json!({"output_index":index,"item":item}),
            );
        }
        let kind = match response["status"].as_str() {
            Some("incomplete") => "response.incomplete",
            Some("failed") => "response.failed",
            _ => "response.completed",
        };
        event(kind, json!({"response":response}));
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn events(dialect: Dialect, response: Value) -> Vec<Value> {
        let bytes = stream(dialect, &response);
        let mut decoder = tw_dialect::frame::Decoder::default();
        decoder
            .feed(&bytes)
            .into_iter()
            .map(|f| serde_json::from_str(&f.data).unwrap())
            .collect()
    }
    #[test]
    fn preserves_anthropic_signatures_and_safeguard_results() {
        let response = json!({"type":"message","role":"assistant","id":"msg_test","model":"model","content":[{"type":"thinking","thinking":"Consider the sources","signature":"signed-thinking"},{"type":"text","text":"Answer"}],"stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0},"safeguard_results":[{"type":"dangerous_tool_use","status":{"type":"available","tool_uses":{}}}]});
        let events = events(Dialect::Anthropic, response);
        assert!(
            events
                .iter()
                .any(|e| e["delta"]["signature"] == "signed-thinking")
        );
        assert_eq!(
            events
                .iter()
                .find(|e| e["type"] == "message_delta")
                .unwrap()["delta"]["safeguard_results"][0]["status"]["type"],
            "available"
        );
        assert_eq!(events.last().unwrap()["type"], "message_stop");
    }
    #[test]
    fn responses_failure_is_never_reported_as_completed_and_annotations_are_emitted() {
        let response = json!({"id":"resp_test","status":"failed","output":[{"type":"message","id":"msg_test","content":[{"type":"output_text","text":"A source","annotations":[{"type":"url_citation","url":"https://example.org","title":"Example","start_index":0,"end_index":8}]}]}],"error":{"code":"server_error","message":"Failed"}});
        let events = events(Dialect::Responses, response);
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "response.output_text.annotation.added")
        );
        assert_eq!(events.last().unwrap()["type"], "response.failed");
        for (n, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], n);
        }
    }
}
