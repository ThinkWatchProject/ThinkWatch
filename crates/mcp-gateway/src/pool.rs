use std::collections::HashMap;
use std::sync::Arc;

use reqwest::Client;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::proxy::{JsonRpcRequest, JsonRpcResponse};
use crate::registry::RegisteredServer;

/// Caller identity passed per-request for template header resolution.
#[derive(Debug, Clone)]
pub struct CallerIdentity {
    pub user_id: String,
    pub user_email: String,
}

/// A single connection to an upstream MCP server.
///
/// Intentionally **stateless** with respect to per-user credentials —
/// the proxy resolves which `Authorization` header to attach via
/// [`crate::user_token::UserTokenResolver`] and passes the resolved
/// header into [`ConnectionPool::send_request`] per-call. The
/// connection only holds the bits that don't vary per caller (the
/// HTTP client, endpoint URL, and template-bearing custom headers).
#[derive(Debug, Clone)]
pub struct McpConnection {
    pub server_id: Uuid,
    pub endpoint_url: String,
    client: Client,
    /// Custom headers with optional template variables (`{{user_id}}`,
    /// `{{user_email}}`), resolved per-request.
    custom_headers: Vec<(String, String)>,
}

impl McpConnection {
    fn new(server: &RegisteredServer, client: Client) -> Self {
        Self {
            server_id: server.id,
            endpoint_url: server.endpoint_url.clone(),
            client,
            custom_headers: server.custom_headers.clone(),
        }
    }
}

/// Error type for connection-pool operations.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("HTTP request to upstream MCP server failed: {0}")]
    RequestFailed(#[from] reqwest::Error),

    #[error("Upstream MCP server returned non-success status {status}: {body}")]
    UpstreamError { status: u16, body: String },

    #[error("Failed to parse upstream JSON-RPC response: {0}")]
    ParseError(String),
}

/// Manages a pool of `McpConnection`s keyed by server ID.
#[derive(Clone)]
pub struct ConnectionPool {
    connections: Arc<RwLock<HashMap<Uuid, McpConnection>>>,
    client: Client,
}

impl ConnectionPool {
    /// Create a pool with the default 30s per-request timeout.
    pub fn new() -> Self {
        Self::with_timeout(30)
    }

    /// Create a pool with a custom per-request timeout (in seconds).
    /// Used by the server crate to wire `Timeouts.mcp_pool_secs` through
    /// from `AppConfig` so the timeout is operator-tunable.
    pub fn with_timeout(timeout_secs: u64) -> Self {
        // SSRF defense: don't auto-follow redirects on the MCP hot
        // path. An admin-registered MCP server's `validate_url` check
        // happens at create time — the runtime fetch then dials the
        // saved URL. Without an explicit redirect policy the default
        // 10-redirect follow lets a 302 from the upstream steer
        // traffic into internal infra (loopback, link-local
        // metadata, RFC1918 ranges) after the save-time check
        // passed. Pass 20 covered the AppState http_client; the
        // dedicated MCP pool client was missed.
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            client,
        }
    }

    /// Return an existing connection for the server, or create a new one.
    pub async fn get_or_create(&self, server: &RegisteredServer) -> McpConnection {
        // Fast path: read lock.
        {
            let conns = self.connections.read().await;
            if let Some(conn) = conns.get(&server.id) {
                return conn.clone();
            }
        }

        // Slow path: write lock.
        let mut conns = self.connections.write().await;
        // Double-check after acquiring write lock.
        if let Some(conn) = conns.get(&server.id) {
            return conn.clone();
        }

        let conn = McpConnection::new(server, self.client.clone());
        conns.insert(server.id, conn.clone());
        conn
    }

    /// Remove the cached connection for a server (e.g. after a health-check
    /// failure or server deregistration).
    pub async fn remove(&self, server_id: Uuid) {
        let mut conns = self.connections.write().await;
        conns.remove(&server_id);
    }

    /// Send a JSON-RPC request to an upstream MCP server and return the
    /// parsed response together with any upstream session ID the server
    /// sent back.  The caller is responsible for persisting the returned
    /// session ID (typically via [`crate::session::SessionManager`]) and
    /// passing it back on the next call.
    ///
    /// `auth_header` is the `(name, value)` pair the proxy resolved for
    /// this specific caller — typically `("Authorization", "Bearer …")`
    /// from `UserTokenResolver`. `None` means the upstream is anonymous
    /// (no Authorization header at all).
    ///
    /// ## Streaming responses
    ///
    /// MCP's Streamable HTTP transport lets an upstream tool emit
    /// `notifications/progress` events DURING tool execution and a
    /// final response event AT the end — all over the same SSE body.
    /// The third tuple element captures the complete raw event
    /// sequence (one envelope per `\n` delimiter) when the upstream
    /// used `text/event-stream`; the audit pipeline embeds it in
    /// `mcp_logs.tool_result` so an investigator can replay the whole
    /// tool execution timeline, not just the final result. `None`
    /// for plain `application/json` responses where the response
    /// envelope IS the entire payload and the proxy can serialize
    /// `response.result` directly.
    /// Shared HTTP request builder for both `send_request` (buffered)
    /// and `send_request_streaming` (pass-through). Carries all the
    /// auth + session + custom-header decoration once, so the two
    /// variants stay in lockstep when those rules change.
    #[allow(clippy::too_many_arguments)]
    fn build_upstream_request(
        conn: &McpConnection,
        request: &JsonRpcRequest,
        auth_header: Option<(&str, &str)>,
        caller: Option<&CallerIdentity>,
        upstream_session_id: Option<&str>,
        trace_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut builder = conn
            .client
            .post(&conn.endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some((name, value)) = auth_header {
            builder = builder.header(name, value);
        }
        if let Some(t) = trace_id {
            builder = builder.header("x-trace-id", t);
        }
        for (key, template) in &conn.custom_headers {
            let value = if let Some(c) = caller {
                template
                    .replace("{{user_id}}", &c.user_id)
                    .replace("{{user_email}}", &c.user_email)
            } else {
                template.clone()
            };
            builder = builder.header(key.as_str(), value);
        }
        if let Some(sid) = upstream_session_id {
            builder = builder.header("Mcp-Session-Id", sid);
        }
        builder.json(request)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn send_request(
        &self,
        conn: &McpConnection,
        request: &JsonRpcRequest,
        auth_header: Option<(&str, &str)>,
        caller: Option<&CallerIdentity>,
        upstream_session_id: Option<&str>,
        trace_id: Option<&str>,
    ) -> Result<(JsonRpcResponse, Option<String>, Option<String>), PoolError> {
        let builder = Self::build_upstream_request(
            conn,
            request,
            auth_header,
            caller,
            upstream_session_id,
            trace_id,
        );
        let resp = builder.send().await?;

        // Capture the upstream session ID from the response header so the
        // caller can persist it per-user.
        let new_session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(PoolError::UpstreamError {
                status: status.as_u16(),
                body,
            });
        }

        // The MCP Streamable HTTP spec allows the server to reply with
        // either `application/json` (plain JSON-RPC) or `text/event-stream`
        // (SSE wrapping a JSON-RPC message in a `data:` line).  We must
        // handle both.
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        let (json_resp, stream_audit_body): (JsonRpcResponse, Option<String>) =
            if content_type.contains("text/event-stream") {
                let text = resp
                    .text()
                    .await
                    .map_err(|e| PoolError::ParseError(e.to_string()))?;
                // Pick the response envelope by matching id against the
                // request id — without this, a tool that emits
                // `notifications/progress` before the final response
                // had its first progress notification returned AS the
                // response (notifications have id=None which deserializes
                // into JsonRpcResponse just fine, masking the real reply).
                let (matched, full_audit) = parse_sse_json_rpc(&text, request.id.as_ref())?;
                (matched, Some(full_audit))
            } else {
                let envelope: JsonRpcResponse = resp
                    .json()
                    .await
                    .map_err(|e| PoolError::ParseError(e.to_string()))?;
                (envelope, None)
            };

        // Validate JSON-RPC version
        if json_resp.jsonrpc != "2.0" {
            return Err(PoolError::ParseError("Invalid JSON-RPC version".into()));
        }

        Ok((json_resp, new_session_id, stream_audit_body))
    }

    /// Streaming counterpart of `send_request`: doesn't consume the
    /// response body, so the caller can forward upstream SSE chunks
    /// to the downstream client AS THEY ARRIVE instead of buffering
    /// the entire tool execution into memory first.
    ///
    /// Returns the raw `reqwest::Response` (caller pulls its
    /// `bytes_stream` for chunk-by-chunk forwarding) plus the
    /// upstream `Mcp-Session-Id` header (same persistence contract
    /// as `send_request`). Non-2xx upstream responses are still
    /// buffered into a `PoolError::UpstreamError` because once a
    /// status indicates failure, body content is small + we want
    /// the error message visible to the caller; only successful
    /// responses are streamed.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_request_streaming(
        &self,
        conn: &McpConnection,
        request: &JsonRpcRequest,
        auth_header: Option<(&str, &str)>,
        caller: Option<&CallerIdentity>,
        upstream_session_id: Option<&str>,
        trace_id: Option<&str>,
    ) -> Result<(reqwest::Response, Option<String>), PoolError> {
        let builder = Self::build_upstream_request(
            conn,
            request,
            auth_header,
            caller,
            upstream_session_id,
            trace_id,
        );
        let resp = builder.send().await?;
        let new_session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());
        let status = resp.status();
        if !status.is_success() {
            // Same error-surfacing contract as send_request — buffer
            // the small error body so the caller can see the upstream
            // diagnostic instead of an opaque "non-2xx".
            let body = resp.text().await.unwrap_or_default();
            return Err(PoolError::UpstreamError {
                status: status.as_u16(),
                body,
            });
        }
        Ok((resp, new_session_id))
    }
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a complete SSE body, returning:
///
///   * the JSON-RPC ENVELOPE that matches the request id (the
///     actual response — distinct from any `notifications/progress`
///     events the upstream may have emitted during tool execution);
///   * the full event sequence concatenated as a JSON array string,
///     suitable for embedding into `mcp_logs.tool_result` so the
///     audit trail captures the entire tool-execution timeline, not
///     just the final result.
///
/// SSE format is:
/// ```text
/// event: message
/// data: {"jsonrpc":"2.0", "method":"notifications/progress", "params":{...}}
///
/// event: message
/// data: {"jsonrpc":"2.0", "id":1, "result":{...}}
/// ```
///
/// Multi-line `data:` fields are concatenated per the SSE spec. We
/// match the response by id rather than "first envelope" because a
/// notification envelope shape (`{jsonrpc, method, params}`) ALSO
/// deserializes into `JsonRpcResponse` (id=None, result=None) — so
/// the previous "first-success" picker silently returned the first
/// notification AS the response when streaming was in play.
///
/// Falls back to "last envelope with a result or error" when no id
/// match is found, to handle older MCP servers that return
/// `id: null` on the final response.
fn parse_sse_json_rpc(
    text: &str,
    request_id: Option<&serde_json::Value>,
) -> Result<(JsonRpcResponse, String), PoolError> {
    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut data_buf = String::new();
    let flush = |buf: &mut String, events: &mut Vec<serde_json::Value>| {
        if buf.is_empty() {
            return;
        }
        // Best-effort parse — non-JSON events (rare) are recorded
        // verbatim as a string for audit so the full transcript is
        // preserved even on malformed envelopes.
        match serde_json::from_str::<serde_json::Value>(buf) {
            Ok(v) => events.push(v),
            Err(_) => events.push(serde_json::Value::String(buf.clone())),
        }
        buf.clear();
    };
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim_start();
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(payload);
        } else if line.is_empty() {
            flush(&mut data_buf, &mut events);
        }
        // event:/id:/retry: lines per the SSE spec are silently
        // dropped — we only care about the data payloads.
    }
    flush(&mut data_buf, &mut events);

    if events.is_empty() {
        return Err(PoolError::ParseError(
            "No SSE events found in stream".into(),
        ));
    }

    // Find the envelope that matches the request id. JSON-RPC
    // responses ALWAYS carry the request's id; notifications carry
    // no id or null id. Match by deep equality so numeric / string
    // / structured ids all work consistently.
    let response_event = if let Some(req_id) = request_id {
        events
            .iter()
            .rev()
            .find(|e| {
                e.get("id")
                    .map(|id| {
                        id == req_id && (e.get("result").is_some() || e.get("error").is_some())
                    })
                    .unwrap_or(false)
            })
            .cloned()
            // Fallback: last envelope with result or error, in case the
            // upstream replied with id=null (some older MCP impls do).
            .or_else(|| {
                events
                    .iter()
                    .rev()
                    .find(|e| e.get("result").is_some() || e.get("error").is_some())
                    .cloned()
            })
    } else {
        // No request id (notification-style send — rare) — pick the
        // last response-shaped envelope.
        events
            .iter()
            .rev()
            .find(|e| e.get("result").is_some() || e.get("error").is_some())
            .cloned()
    };

    let response_event = response_event.ok_or_else(|| {
        PoolError::ParseError(format!(
            "SSE stream had {} event(s) but none carried a result or error",
            events.len()
        ))
    })?;

    let json_resp: JsonRpcResponse = serde_json::from_value(response_event).map_err(|e| {
        PoolError::ParseError(format!("matched SSE event failed to deserialize: {e}"))
    })?;

    // Serialize the full event sequence for audit. JSON array of the
    // raw envelopes — auditors can scan progress notifications + the
    // final response in one pretty-print. Failure here would mean
    // serde_json::Value -> String is broken; treat as a hard error
    // because the transport already accepted these as parseable JSON.
    let audit_body = serde_json::to_string(&events)
        .map_err(|e| PoolError::ParseError(format!("audit serialization: {e}")))?;

    Ok((json_resp, audit_body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_id(n: u64) -> serde_json::Value {
        json!(n)
    }

    #[test]
    fn parses_single_envelope_response() {
        let sse = "event: message\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\
                   \n";
        let req_id = make_id(1);
        let (resp, audit) = parse_sse_json_rpc(sse, Some(&req_id)).unwrap();
        assert_eq!(resp.id, Some(json!(1)));
        assert_eq!(resp.result, Some(json!({"ok": true})));
        // Audit body is a JSON array of all events.
        let events: Vec<serde_json::Value> = serde_json::from_str(&audit).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn returns_response_not_intermediate_progress_notification() {
        // This is the regression: pre-fix code returned the FIRST
        // parseable envelope, which for streaming tools is the
        // progress notification (id=null) — silently dropping the
        // real response.
        let sse = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"pct\":30}}\n\
                   \n\
                   data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"pct\":90}}\n\
                   \n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":42,\"result\":{\"content\":[{\"text\":\"done\"}]}}\n\
                   \n";
        let req_id = make_id(42);
        let (resp, audit) = parse_sse_json_rpc(sse, Some(&req_id)).unwrap();
        assert_eq!(resp.id, Some(json!(42)), "response must match request id");
        assert_eq!(
            resp.result,
            Some(json!({"content": [{"text": "done"}]})),
            "response.result is the actual reply, not a progress notification"
        );
        // Audit body captures the FULL stream so an investigator can
        // replay all three events.
        let events: Vec<serde_json::Value> = serde_json::from_str(&audit).unwrap();
        assert_eq!(
            events.len(),
            3,
            "audit body preserves the full event timeline"
        );
        assert!(audit.contains("notifications/progress"));
        assert!(audit.contains("\"pct\":30"));
    }

    #[test]
    fn falls_back_to_last_response_shaped_envelope_when_id_does_not_match() {
        // Some older MCP impls return id=null on the final response;
        // we should still pick up the envelope with `result` rather
        // than erroring.
        let sse = "data: {\"jsonrpc\":\"2.0\",\"id\":null,\"result\":{\"ok\":1}}\n\n";
        let req_id = make_id(7);
        let (resp, _audit) = parse_sse_json_rpc(sse, Some(&req_id)).unwrap();
        assert_eq!(resp.result, Some(json!({"ok": 1})));
    }

    #[test]
    fn errors_when_no_event_carries_result_or_error() {
        let sse =
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n";
        let req_id = make_id(1);
        let err = parse_sse_json_rpc(sse, Some(&req_id)).unwrap_err();
        assert!(matches!(err, PoolError::ParseError(_)));
    }

    #[test]
    fn handles_multiline_data_payload() {
        // Per the SSE spec, repeated `data:` lines within one event
        // are concatenated with '\n'. JSON-RPC envelopes don't usually
        // span lines but the parser must handle it for spec
        // conformance.
        let sse = "data: {\"jsonrpc\":\"2.0\",\n\
                   data: \"id\":1,\n\
                   data: \"result\":{\"ok\":true}}\n\
                   \n";
        let req_id = make_id(1);
        let (resp, _audit) = parse_sse_json_rpc(sse, Some(&req_id)).unwrap();
        assert_eq!(resp.result, Some(json!({"ok": true})));
    }

    #[test]
    fn handles_stream_without_trailing_blank_line() {
        // Stream ends with a payload but no terminating blank line —
        // the flush at end-of-stream should still pick it up.
        let sse = "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":\"x\"}";
        let req_id = make_id(3);
        let (resp, _audit) = parse_sse_json_rpc(sse, Some(&req_id)).unwrap();
        assert_eq!(resp.result, Some(json!("x")));
    }

    #[test]
    fn errors_on_empty_stream() {
        let req_id = make_id(1);
        let err = parse_sse_json_rpc("", Some(&req_id)).unwrap_err();
        assert!(matches!(err, PoolError::ParseError(_)));
    }
}
