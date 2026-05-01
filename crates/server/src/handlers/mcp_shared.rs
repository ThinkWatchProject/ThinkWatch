//! Cross-handler MCP utilities.
//!
//! `probe_mcp_endpoint` (one-shot tools/list probe) and namespace-prefix
//! normalisation. Called from every handler that touches an MCP server
//! catalog: `mcp_servers` (CRUD + connectivity test) and `mcp_store`
//! (install-from-template pre-flight).
//!
//! Probes are **anonymous** — they don't carry user credentials. Auth
//! validation for upstreams that require a Bearer token / PAT happens
//! when an end user authorizes through `/connections`. The admin "Test"
//! button verifies endpoint reachability + JSON-RPC compliance only.

use think_watch_common::errors::AppError;

/// Summary of a tool as returned by the MCP `tools/list` probe. Narrow
/// shape — callers that need schemas should use the proxy handler, not
/// this probe.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct McpToolSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Result of a one-shot JSON-RPC `tools/list` probe. Never persists
/// anything — returned straight back to the caller.
///
/// `success` means HTTP 2xx with a valid `result.tools` array.
/// `requires_auth` means HTTP 401/403 — the server is reachable but the
/// anonymous probe wasn't allowed to enumerate tools. Callers that probe
/// anonymously (admin "test" button, store install pre-flight) typically
/// treat `requires_auth` as a soft success — per-user auth happens later
/// at /connections. Callers that probe with real credentials
/// (`mcp_oauth::test_connection`) treat it as failure: the user's token
/// was rejected.
pub struct McpProbeOutcome {
    pub success: bool,
    pub requires_auth: bool,
    pub message: String,
    pub latency_ms: u64,
    pub tools: Vec<McpToolSummary>,
}

pub async fn probe_mcp_endpoint(
    http: &reqwest::Client,
    endpoint_url: &str,
    custom_headers: Option<&std::collections::HashMap<String, String>>,
) -> McpProbeOutcome {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    let mut builder = http
        .post(endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body);

    if let Some(headers) = custom_headers {
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
    }

    let started = std::time::Instant::now();
    let result = builder.send().await;
    let latency_ms = started.elapsed().as_millis() as u64;

    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            return McpProbeOutcome {
                success: false,
                requires_auth: false,
                message: format!("Request failed: {e}"),
                latency_ms,
                tools: vec![],
            };
        }
    };

    let status = resp.status();
    if !status.is_success() {
        // 401/403 from an anonymous probe is the *expected* response for
        // OAuth- or static-token-gated MCPs — the server is alive, the
        // probe just wasn't allowed in. Surface as a soft outcome so
        // anonymous callers can still proceed.
        let requires_auth =
            status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN;
        let message = if requires_auth {
            format!("Reachable — requires user auth (HTTP {})", status.as_u16())
        } else {
            format!("HTTP {status}")
        };
        return McpProbeOutcome {
            success: false,
            requires_auth,
            message,
            latency_ms,
            tools: vec![],
        };
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let json: serde_json::Value = if content_type.contains("text/event-stream") {
        match resp.text().await {
            Ok(text) => crate::mcp_runtime::parse_sse_json(&text).unwrap_or_else(|e| {
                tracing::warn!(
                    endpoint = %endpoint_url,
                    error = %e,
                    "MCP probe: failed to parse SSE JSON response"
                );
                serde_json::Value::Null
            }),
            Err(e) => {
                tracing::warn!(
                    endpoint = %endpoint_url,
                    error = %e,
                    "MCP probe: failed to read SSE response body"
                );
                serde_json::Value::Null
            }
        }
    } else {
        resp.json().await.unwrap_or_else(|e| {
            tracing::warn!(
                endpoint = %endpoint_url,
                error = %e,
                "MCP probe: failed to parse JSON response"
            );
            serde_json::Value::Null
        })
    };

    let result_field = json.get("result");
    let tools: Vec<McpToolSummary> = result_field
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(|n| n.as_str())
                        .map(|name| McpToolSummary {
                            name: name.to_string(),
                            description: t
                                .get("description")
                                .and_then(|d| d.as_str())
                                .map(|s| s.to_string()),
                        })
                })
                .collect()
        })
        .unwrap_or_default();

    if tools.is_empty() && result_field.is_none() {
        return McpProbeOutcome {
            success: false,
            requires_auth: false,
            message: "Invalid response: missing `result` field".into(),
            latency_ms,
            tools: vec![],
        };
    }

    let count = tools.len();
    McpProbeOutcome {
        success: true,
        requires_auth: false,
        message: format!("Connected — {count} tools available"),
        latency_ms,
        tools,
    }
}

/// Normalise / derive a namespace prefix. Explicit values must match
/// `[a-z0-9_]{1,32}`; a missing value is derived from the server name
/// by lowercasing, replacing non-alphanum with `_`, trimming leading /
/// trailing `_`, and clipping to 32 chars.
pub fn normalize_namespace_prefix(
    explicit: Option<&str>,
    fallback_name: &str,
) -> Result<String, AppError> {
    if let Some(p) = explicit {
        if !p
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            || p.is_empty()
            || p.len() > 32
        {
            return Err(AppError::BadRequest(
                "namespace_prefix must match [a-z0-9_]{1,32}".into(),
            ));
        }
        return Ok(p.to_owned());
    }
    let derived: String = fallback_name
        .chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .chars()
        .take(32)
        .collect();
    if derived.is_empty() {
        return Err(AppError::BadRequest(
            "Could not derive namespace_prefix from name; please provide one explicitly ([a-z0-9_]{1,32})".into(),
        ));
    }
    Ok(derived)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn probe(server: &MockServer) -> McpProbeOutcome {
        probe_mcp_endpoint(
            &reqwest::Client::new(),
            &format!("{}/mcp", server.uri()),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn probe_succeeds_on_2xx_with_tools() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "tools": [{ "name": "echo", "description": "Echo input" }] }
            })))
            .mount(&server)
            .await;

        let outcome = probe(&server).await;
        assert!(outcome.success);
        assert!(!outcome.requires_auth);
        assert_eq!(outcome.tools.len(), 1);
        assert_eq!(outcome.tools[0].name, "echo");
    }

    #[tokio::test]
    async fn probe_treats_401_as_requires_auth_not_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let outcome = probe(&server).await;
        // Anonymous probe shouldn't claim success — but `requires_auth`
        // tells the caller this is reachable, just gated.
        assert!(!outcome.success);
        assert!(outcome.requires_auth);
        assert!(
            outcome.message.contains("requires user auth"),
            "msg={}",
            outcome.message
        );
        assert!(outcome.tools.is_empty());
    }

    #[tokio::test]
    async fn probe_treats_403_as_requires_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let outcome = probe(&server).await;
        assert!(!outcome.success);
        assert!(outcome.requires_auth);
    }

    #[tokio::test]
    async fn probe_treats_500_as_hard_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let outcome = probe(&server).await;
        assert!(!outcome.success);
        assert!(!outcome.requires_auth);
        assert!(outcome.message.starts_with("HTTP 500"));
    }

    #[tokio::test]
    async fn probe_fails_on_2xx_with_missing_result_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": { "code": -32601, "message": "Method not found" }
            })))
            .mount(&server)
            .await;

        let outcome = probe(&server).await;
        assert!(!outcome.success);
        assert!(!outcome.requires_auth);
    }
}
