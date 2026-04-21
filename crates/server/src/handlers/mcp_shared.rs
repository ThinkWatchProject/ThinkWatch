//! Cross-handler MCP utilities.
//!
//! `probe_mcp_endpoint` (one-shot tools/list probe), namespace-prefix
//! normalisation, and auth-header shaping are called from every handler
//! that touches an MCP server: `mcp_servers` (CRUD + connectivity test),
//! `mcp_store` (install-from-template pre-flight), and future service-
//! layer callers. Previously they lived inside `mcp_servers.rs` and the
//! store handler reached in via `super::mcp_servers::*`, which coupled
//! the two handlers and made either file risky to split independently.
//!
//! Splitting the utilities out here removes the cross-handler reach and
//! anchors them as "MCP plumbing" rather than part of either handler's
//! public surface. The Rust error enum stays `AppError` because callers
//! need to propagate through axum responses.

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
pub struct McpProbeOutcome {
    pub success: bool,
    pub message: String,
    pub latency_ms: u64,
    pub tools: Vec<McpToolSummary>,
}

pub async fn probe_mcp_endpoint(
    http: &reqwest::Client,
    endpoint_url: &str,
    auth_type: Option<&str>,
    auth_secret: Option<&str>,
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

    if let Some((name, value)) = build_auth_probe_header(auth_type, auth_secret) {
        builder = builder.header(name, value);
    }

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
                message: format!("Request failed: {e}"),
                latency_ms,
                tools: vec![],
            };
        }
    };

    if !resp.status().is_success() {
        return McpProbeOutcome {
            success: false,
            message: format!("HTTP {}", resp.status()),
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
            message: "Invalid response: missing `result` field".into(),
            latency_ms,
            tools: vec![],
        };
    }

    let count = tools.len();
    McpProbeOutcome {
        success: true,
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

/// Build an `(header_name, header_value)` pair for auth probes.
pub fn build_auth_probe_header(
    auth_type: Option<&str>,
    auth_secret: Option<&str>,
) -> Option<(String, String)> {
    let secret = auth_secret.filter(|s| !s.is_empty())?;
    match auth_type? {
        "bearer" => Some(("Authorization".to_owned(), format!("Bearer {secret}"))),
        "api_key" => Some(("X-API-Key".to_owned(), secret.to_owned())),
        _ => None,
    }
}
