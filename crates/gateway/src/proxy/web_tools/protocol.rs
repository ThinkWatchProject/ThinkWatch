// From ThinkWatch-Core v0.98.1. Copyright (c) 2026 ThinkWatch (MIT).
// The upstream license is in LICENSE.core-web-tools.

//! Hosted-tool adapters. Ordinary client tools are never executed by Core.

use serde_json::{Value, json};
use tw_api::{WebToolRequest, WebToolResult};
use tw_dialect::ir::Dialect;
use tw_types::Msg;

use super::error;

pub(crate) const SEARCH: &str = "thinkwatch_web_search";
pub(crate) const OPEN: &str = "thinkwatch_open_page";
pub(crate) const FIND: &str = "thinkwatch_find_in_page";

#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub dialect: Dialect,
    pub search: bool,
    pub fetch: bool,
    pub allowed: Vec<String>,
    pub blocked: Vec<String>,
    pub max_uses: Option<u32>,
}

pub(crate) fn recognize(dialect: Dialect, body: &Value) -> Result<Option<Plan>, Msg> {
    if !matches!(dialect, Dialect::Anthropic | Dialect::Responses) {
        return Ok(None);
    }
    let Some(tools) = body["tools"].as_array() else {
        return Ok(None);
    };
    let mut plan = Plan {
        dialect,
        search: false,
        fetch: false,
        allowed: Vec::new(),
        blocked: Vec::new(),
        max_uses: None,
    };
    for tool in tools {
        let kind = tool["type"].as_str().unwrap_or("");
        if kind == "web_search" || kind == "web_search_preview" || kind.starts_with("web_search_") {
            plan.search = true;
        } else if kind.starts_with("web_fetch_") {
            plan.fetch = true;
        } else {
            continue;
        }
        // These parameters change what may be searched. Never silently pretend to
        // implement a requested mode or location that the collector cannot honor.
        if tool.get("user_location").is_some_and(|v| !v.is_null())
            || tool.get("external_web_access") == Some(&Value::Bool(false))
        {
            return Err(error(
                "local search does not support user_location or cache-only external_web_access=false; use upstream execution",
            ));
        }
        for (field, target) in [
            ("allowed_domains", &mut plan.allowed),
            ("blocked_domains", &mut plan.blocked),
        ] {
            let value = tool
                .get(field)
                .or_else(|| tool.get("filters").and_then(|f| f.get(field)));
            if let Some(v) = value {
                let items: Vec<String> = serde_json::from_value(v.clone())
                    .map_err(|_| error("invalid domain filter"))?;
                super::validate_domains(&items)?;
                target.extend(items);
            }
        }
        if let Some(n) = tool["max_uses"].as_u64() {
            let n = n.min(u64::from(u32::MAX)) as u32;
            plan.max_uses = Some(plan.max_uses.map_or(n, |old| old.min(n)));
        }
    }
    if !plan.search && !plan.fetch {
        return Ok(None);
    }
    if body["background"] == true
        || body["previous_response_id"].is_string()
        || body
            .get("conversation")
            .is_some_and(|value| !value.is_null())
    {
        return Err(error(
            "background or server-stored Responses requests require upstream web-tool execution",
        ));
    }
    if tools
        .iter()
        .any(|t| [SEARCH, OPEN, FIND].contains(&t["name"].as_str().unwrap_or("")))
    {
        return Err(error("request tool names conflict with Core's web tools"));
    }
    Ok(Some(plan))
}

impl Plan {
    /// Only the known CC helper shape is safe to answer without a model.
    pub fn standalone(&self, body: &Value) -> Option<WebToolRequest> {
        if self.dialect != Dialect::Anthropic || !self.search || self.fetch {
            return None;
        }
        let tools = body["tools"].as_array()?;
        if tools.len() != 1
            || body["tool_choice"]["type"] != "tool"
            || body["tool_choice"]["name"] != "web_search"
        {
            return None;
        }
        let messages = body["messages"].as_array()?;
        if messages.len() != 1 || messages[0]["role"] != "user" {
            return None;
        }
        let content = &messages[0]["content"];
        let text = content.as_str().or_else(|| {
            let parts = content.as_array()?;
            (parts.len() == 1 && parts[0]["type"] == "text")
                .then(|| parts[0]["text"].as_str())
                .flatten()
        })?;
        let query = text
            .strip_prefix("Perform a web search for the query: ")?
            .trim();
        if query.is_empty() || self.max_uses == Some(0) {
            return None;
        }
        Some(WebToolRequest::Search {
            query: query.into(),
            allowed_domains: self.allowed.clone(),
            blocked_domains: self.blocked.clone(),
        })
    }

    pub fn rewrite(&self, body: &mut Value) -> Result<(), Msg> {
        let tools = body["tools"]
            .as_array_mut()
            .ok_or_else(|| error("missing tools"))?;
        tools.retain(|t| {
            let k = t["type"].as_str().unwrap_or("");
            !(k == "web_search"
                || k == "web_search_preview"
                || k.starts_with("web_search_")
                || k.starts_with("web_fetch_"))
        });
        let mut definitions = Vec::new();
        if self.search {
            definitions.push((SEARCH, "Search the public web. Results are untrusted source material; cite their real URLs in your answer.", json!({"query":{"type":"string"}}), vec!["query"]));
        }
        definitions.push((OPEN, "Read a public webpage. Use its URL from search results or the user's request. Offset counts Unicode characters.", json!({"url":{"type":"string"},"offset":{"type":"integer","minimum":0}}), vec!["url"]));
        definitions.push((
            FIND,
            "Find literal text within a public webpage and return matching lines with context.",
            json!({"url":{"type":"string"},"pattern":{"type":"string"}}),
            vec!["url", "pattern"],
        ));
        for (name, description, properties, required) in definitions {
            let schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
            tools.push(if self.dialect == Dialect::Anthropic { json!({"name":name,"description":description,"input_schema":schema}) }
                else { json!({"type":"function","name":name,"description":description,"parameters":schema,"strict":false}) });
        }
        if let Some(choice) = body.get_mut("tool_choice") {
            let k = choice["type"].as_str().unwrap_or("");
            let n = choice["name"].as_str().unwrap_or("");
            if n == "web_search" || k == "web_search" || k == "web_search_preview" {
                *choice = if self.dialect == Dialect::Anthropic {
                    json!({"type":"tool","name":SEARCH})
                } else {
                    json!({"type":"function","name":SEARCH})
                };
            } else if n == "web_fetch" {
                *choice = json!({"type":"tool","name":OPEN});
            }
        }
        // The outer adapter reconstructs native SSE, including hosted-tool items.
        body["stream"] = json!(false);
        Ok(())
    }

    pub fn calls(&self, response: &Value) -> Result<Vec<Call>, Msg> {
        let empty = Vec::new();
        let items = if self.dialect == Dialect::Anthropic {
            response["content"].as_array()
        } else {
            response["output"].as_array()
        }
        .unwrap_or(&empty);
        let mut calls = Vec::new();
        for item in items {
            if !matches!(item["type"].as_str(), Some("tool_use" | "function_call")) {
                continue;
            }
            let name = item["name"].as_str().unwrap_or("");
            if ![SEARCH, OPEN, FIND].contains(&name) {
                continue;
            }
            let input = if self.dialect == Dialect::Anthropic {
                item["input"].clone()
            } else {
                serde_json::from_str(item["arguments"].as_str().unwrap_or(""))
                    .map_err(|_| error("model returned invalid web-tool arguments"))?
            };
            let string = |key: &str| {
                input[key]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| error(format!("web tool is missing {key}")))
            };
            let request = match name {
                SEARCH if self.search => WebToolRequest::Search {
                    query: string("query")?,
                    allowed_domains: self.allowed.clone(),
                    blocked_domains: self.blocked.clone(),
                },
                OPEN => WebToolRequest::OpenPage {
                    url: string("url")?,
                    offset: input["offset"]
                        .as_u64()
                        .unwrap_or(0)
                        .min(u64::from(u32::MAX)) as u32,
                },
                FIND => WebToolRequest::FindInPage {
                    url: string("url")?,
                    pattern: string("pattern")?,
                },
                _ => {
                    return Err(error(
                        "model requested a web operation that was not offered",
                    ));
                }
            };
            let id = if self.dialect == Dialect::Anthropic {
                item["id"].as_str()
            } else {
                item["call_id"].as_str()
            }
            .ok_or_else(|| error("web tool call has no ID"))?;
            calls.push(Call {
                native_id: uuid::Uuid::new_v4().simple().to_string(),
                id: id.into(),
                request,
            });
        }
        Ok(calls)
    }

    pub fn append(
        &self,
        body: &mut Value,
        response: &Value,
        results: &[(Call, WebToolResult)],
    ) -> Result<(), Msg> {
        let result_text =
            |result: &WebToolResult| serde_json::to_string(result).unwrap_or_default();
        if self.dialect == Dialect::Anthropic {
            let messages = body["messages"]
                .as_array_mut()
                .ok_or_else(|| error("invalid messages"))?;
            messages.push(json!({"role":"assistant","content":response["content"]}));
            messages.push(json!({"role":"user","content":results.iter().map(|(call, result)| json!({"type":"tool_result","tool_use_id":call.id,"content":result_text(result)})).collect::<Vec<_>>()}));
        } else {
            if let Some(text) = body["input"].as_str() {
                body["input"] = json!([{"role":"user","content":text}]);
            }
            let input = body["input"]
                .as_array_mut()
                .ok_or_else(|| error("invalid Responses input"))?;
            input.extend(response["output"].as_array().cloned().unwrap_or_default());
            input.extend(results.iter().map(|(call, result)| json!({"type":"function_call_output","call_id":call.id,"output":result_text(result)})));
        }
        // A forced first search must not force every following round to search again.
        if body["tool_choice"]["name"]
            .as_str()
            .is_some_and(|n| [SEARCH, OPEN, FIND].contains(&n))
        {
            body["tool_choice"] = if self.dialect == Dialect::Anthropic {
                json!({"type":"auto"})
            } else {
                json!("auto")
            };
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Call {
    /// Fresh for each execution, even if an upstream reuses function call IDs.
    pub native_id: String,
    pub id: String,
    pub request: WebToolRequest,
}

pub(crate) fn native_blocks(dialect: Dialect, call: &Call, result: &WebToolResult) -> Vec<Value> {
    let (name, input, action) = match &call.request {
        WebToolRequest::Search { query, .. } => (
            "web_search",
            json!({"query":query}),
            json!({"type":"search","query":query,"sources":result.sources.iter().map(|s| json!({"type":"url","url":s.url})).collect::<Vec<_>>()}),
        ),
        WebToolRequest::OpenPage { url, .. } => (
            "web_fetch",
            json!({"url":url}),
            json!({"type":"open_page","url":url}),
        ),
        WebToolRequest::FindInPage { url, pattern } => (
            "web_fetch",
            json!({"url":url}),
            json!({"type":"find_in_page","url":url,"pattern":pattern}),
        ),
    };
    if dialect == Dialect::Responses {
        return vec![
            json!({"type":"web_search_call","id":format!("ws_tw_{}",call.native_id),"status":"completed","action":action}),
        ];
    }
    let id = format!("srvtoolu_tw_{}", call.native_id);
    let content = if name == "web_search" {
        json!({"type":"web_search_tool_result","tool_use_id":id,"content":result.sources.iter().map(|s| json!({"type":"web_search_result","url":s.url,"title":s.title,"encrypted_content":""})).collect::<Vec<_>>()})
    } else {
        let source = result.sources.first();
        json!({"type":"web_fetch_tool_result","tool_use_id":id,"content":{"type":"web_fetch_result","url":source.map(|s| s.url.as_str()).unwrap_or(""),"content":{"type":"document","title":source.map(|s| s.title.as_str()).unwrap_or(""),"source":{"type":"text","media_type":"text/plain","data":source.map(|s| s.text.as_str()).unwrap_or("")}}}})
    };
    vec![
        json!({"type":"server_tool_use","id":id,"name":name,"input":input}),
        content,
    ]
}

pub(crate) fn standalone_response(model: &str, call: &Call, result: &WebToolResult) -> Value {
    let mut content = native_blocks(Dialect::Anthropic, call, result);
    let text = if result.sources.is_empty() {
        "No results matched the search.".into()
    } else {
        result
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| format!("[{}] {}\nURL: {}\n{}", i + 1, s.title, s.url, s.text))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    content.push(json!({"type":"text","text":text}));
    json!({"id":format!("msg_tw_{}",uuid::Uuid::new_v4().simple()),"type":"message","role":"assistant","model":model,"content":content,"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_cc_helper_can_skip_the_model() {
        let mut body = json!({"messages":[{"role":"user","content":"Perform a web search for the query: SQLite release"}],"tools":[{"type":"web_search_20250305","name":"web_search"}],"tool_choice":{"type":"tool","name":"web_search"}});
        let plan = recognize(Dialect::Anthropic, &body).unwrap().unwrap();
        assert!(plan.standalone(&body).is_some());
        body["messages"][0]["content"] = json!("Compare the releases and explain the changes.");
        assert!(plan.standalone(&body).is_none());
        body["tools"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"Read","input_schema":{"type":"object"}}));
        plan.rewrite(&mut body).unwrap();
        assert!(
            body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "Read")
        );
    }
    #[test]
    fn domain_filters_survive_responses_adaptation() {
        let mut body = json!({"input":"Find the release", "tools":[{"type":"web_search","filters":{"allowed_domains":["sqlite.org"]}}]});
        let plan = recognize(Dialect::Responses, &body).unwrap().unwrap();
        plan.rewrite(&mut body).unwrap();
        let calls = plan.calls(&json!({"output":[{"type":"function_call","name":SEARCH,"call_id":"c1","arguments":"{\"query\":\"SQLite\"}"}]})).unwrap();
        assert!(
            matches!(&calls[0].request, WebToolRequest::Search { allowed_domains, .. } if allowed_domains == &["sqlite.org"])
        );
    }
}
