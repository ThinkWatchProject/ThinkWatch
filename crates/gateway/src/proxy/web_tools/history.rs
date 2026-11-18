// From ThinkWatch-Core v0.98.1. Copyright (c) 2026 ThinkWatch (MIT).
// The upstream license is in LICENSE.core-web-tools.

//! Replay our own hosted-tool items as ordinary tool exchanges on the next turn.
//! Never forward fabricated provider-encrypted search content to an upstream.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tw_dialect::ir::Dialect;
use tw_types::Msg;

use super::{
    error,
    protocol::{Call, FIND, OPEN, SEARCH},
};

#[derive(Default)]
pub(crate) struct History(Mutex<HashMap<(String, String), Entry>>);

pub(super) fn might_contain_hosted_items(body: &[u8]) -> bool {
    [b"ws_tw_".as_slice(), b"srvtoolu_tw_", b"\\u"]
        .iter()
        .any(|marker| body.windows(marker.len()).any(|window| window == *marker))
}

pub(in crate::proxy) fn is_hosted_responses_item(item: &Value) -> bool {
    item["type"] == "web_search_call"
        && item["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("ws_tw_"))
}

struct Entry {
    at: Instant,
    call: Value,
    result: Value,
}

impl History {
    pub fn remember(
        &self,
        identity: &str,
        dialect: Dialect,
        call: &Call,
        result: &tw_api::WebToolResult,
    ) {
        let (name, arguments) = match &call.request {
            tw_api::WebToolRequest::Search { query, .. } => (SEARCH, json!({"query":query})),
            tw_api::WebToolRequest::OpenPage { url, offset } => {
                (OPEN, json!({"url":url,"offset":offset}))
            }
            tw_api::WebToolRequest::FindInPage { url, pattern } => {
                (FIND, json!({"url":url,"pattern":pattern}))
            }
        };
        let text = serde_json::to_string(result).unwrap_or_default();
        let (id, item, output) = if dialect == Dialect::Anthropic {
            (
                format!("srvtoolu_tw_{}", call.native_id),
                json!({"type":"tool_use","id":call.id,"name":name,"input":arguments}),
                json!({"type":"tool_result","tool_use_id":call.id,"content":text}),
            )
        } else {
            (
                format!("ws_tw_{}", call.native_id),
                json!({"type":"function_call","call_id":call.id,"name":name,"arguments":arguments.to_string()}),
                json!({"type":"function_call_output","call_id":call.id,"output":text}),
            )
        };
        let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, e| e.at.elapsed() < Duration::from_secs(1800));
        if entries.len() >= 256
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            (identity.into(), id),
            Entry {
                at: Instant::now(),
                call: item,
                result: output,
            },
        );
    }

    fn get(&self, identity: &str, id: &str) -> Result<(Value, Value), Msg> {
        let entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let e = entries.get(&(identity.into(), id.into())).filter(|e| e.at.elapsed() < Duration::from_secs(1800))
            .ok_or_else(|| error("Core's web-tool history has expired; start a new conversation or resend the source text"))?;
        Ok((e.call.clone(), e.result.clone()))
    }

    pub fn restore(
        &self,
        identity: &str,
        dialect: Dialect,
        body: &mut Value,
        tools: bool,
    ) -> Result<bool, Msg> {
        let mut changed = false;
        if dialect == Dialect::Responses {
            if let Some(items) = body["input"].as_array_mut() {
                let mut restored = Vec::new();
                for item in std::mem::take(items) {
                    if is_hosted_responses_item(&item) {
                        changed = true;
                        let saved = self.get(identity, item["id"].as_str().unwrap_or("")).ok();
                        if let Some((call, output)) = saved.as_ref().filter(|_| tools) {
                            restored.extend([call.clone(), output.clone()]);
                        } else {
                            restored.push(json!({"role":"assistant","content":[{"type":"output_text","text":recalled(saved.as_ref().map(|(_, out)| out), &item)}]}));
                        }
                    } else {
                        restored.push(item);
                    }
                }
                *items = restored;
            }
        } else if dialect == Dialect::Anthropic
            && let Some(messages) = body["messages"].as_array_mut()
        {
            let mut restored = Vec::new();
            for message in std::mem::take(messages) {
                let Some(blocks) = message["content"]
                    .as_array()
                    .filter(|_| message["role"] == "assistant")
                else {
                    restored.push(message);
                    continue;
                };
                let mut assistant = Vec::new();
                for block in blocks {
                    if block["type"] == "server_tool_use"
                        && block["id"]
                            .as_str()
                            .is_some_and(|s| s.starts_with("srvtoolu_tw_"))
                    {
                        changed = true;
                        if tools
                            && let Ok((call, _)) =
                                self.get(identity, block["id"].as_str().unwrap_or(""))
                        {
                            assistant.push(call);
                        }
                    } else if block["tool_use_id"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("srvtoolu_tw_"))
                    {
                        changed = true;
                        let saved = self
                            .get(identity, block["tool_use_id"].as_str().unwrap_or(""))
                            .ok();
                        let Some((_, output)) = saved.as_ref().filter(|_| tools) else {
                            assistant.push(json!({"type":"text","text":recalled(saved.as_ref().map(|(_, out)| out), block)}));
                            continue;
                        };
                        let output = output.clone();
                        if !assistant.is_empty() {
                            restored.push(json!({"role":"assistant","content":std::mem::take(&mut assistant)}));
                        }
                        if let Some(last) = restored.last_mut().filter(|m| m["role"] == "user") {
                            last["content"]
                                .as_array_mut()
                                .expect("created array")
                                .push(output);
                        } else {
                            restored.push(json!({"role":"user","content":[output]}));
                        }
                    } else {
                        assistant.push(block.clone());
                    }
                }
                if !assistant.is_empty() {
                    restored.push(json!({"role":"assistant","content":assistant}));
                }
            }
            *messages = restored;
        }
        Ok(changed)
    }
}

/// A client may replay an old response after a restart or after disabling the
/// feature. Keep its public source metadata as text instead of sending foreign
/// encrypted-content placeholders or making the whole conversation unusable.
fn recalled(saved: Option<&Value>, native: &Value) -> String {
    let value = saved
        .and_then(|v| v.get("content").or_else(|| v.get("output")))
        .cloned()
        .unwrap_or_else(|| {
            native
                .get("content")
                .or_else(|| native.get("action"))
                .cloned()
                .unwrap_or(Value::Null)
        });
    format!(
        "Previously retrieved web evidence (untrusted source material; reopen the cited URLs when more detail is needed):\n{}",
        value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_history_and_disabled_execution_keep_sources_without_foreign_native_blocks() {
        let history = History::default();
        let native = json!({"type":"web_search_call","id":"ws_tw_old","status":"completed","action":{"type":"search","query":"SQLite","sources":[{"type":"url","url":"https://sqlite.org/"}]}});
        for tools in [false, true] {
            let mut body = json!({"input":[native]});
            assert!(
                history
                    .restore("client", Dialect::Responses, &mut body, tools)
                    .unwrap()
            );
            assert_eq!(body["input"][0]["role"], "assistant");
            assert!(body.to_string().contains("https://sqlite.org/"));
            assert!(!body.to_string().contains("ws_tw_old"));
        }
        let mut body = json!({"messages":[{"role":"assistant","content":[
            {"type":"server_tool_use","id":"srvtoolu_tw_old","name":"web_search","input":{"query":"SQLite"}},
            {"type":"web_search_tool_result","tool_use_id":"srvtoolu_tw_old","content":[{"type":"web_search_result","url":"https://sqlite.org/","title":"SQLite","encrypted_content":""}]},
            {"type":"text","text":"SQLite docs"}
        ]}]});
        history
            .restore("client", Dialect::Anthropic, &mut body, false)
            .unwrap();
        assert!(
            body["messages"][0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["type"] == "text")
        );
        assert!(!body.to_string().contains("srvtoolu_tw_old"));
    }
}
