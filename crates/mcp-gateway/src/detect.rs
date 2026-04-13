//! Auto-detect the MCP transport type for a remote endpoint.
//!
//! 1. POST an `initialize` JSON-RPC request → success means **Streamable HTTP**.
//! 2. If POST gets 404/405, try GET with `Accept: text/event-stream` → SSE.
//! 3. Both fail → return an error.

use std::time::Duration;

use reqwest::Client;

/// Detected transport type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedTransport {
    StreamableHttp,
    Sse,
}

impl DetectedTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StreamableHttp => "streamable_http",
            Self::Sse => "sse",
        }
    }
}

/// Try to auto-detect the transport type of a remote MCP endpoint.
///
/// `auth_header` is an optional `(name, value)` tuple attached to every probe
/// request (e.g. `("Authorization", "Bearer xxx")`).
pub async fn detect_transport(
    client: &Client,
    url: &str,
    auth_header: Option<(&str, &str)>,
) -> Result<DetectedTransport, String> {
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "transport-detect",
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "ThinkWatch TransportDetector",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    });

    // --- Probe 1: Streamable HTTP (POST) ---
    let mut post_req = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .timeout(Duration::from_secs(10))
        .json(&init_body);

    if let Some((name, value)) = auth_header {
        post_req = post_req.header(name, value);
    }

    match post_req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            // 2xx, 401, 403, 400 all indicate the endpoint exists and accepts POST
            if status < 500 && status != 404 && status != 405 {
                return Ok(DetectedTransport::StreamableHttp);
            }
            // 404 / 405 → fall through to SSE probe
        }
        Err(_) => {
            // Connection error → still try SSE in case it's a different path
        }
    }

    // --- Probe 2: SSE (GET) ---
    let mut get_req = client
        .get(url)
        .header("Accept", "text/event-stream")
        .timeout(Duration::from_secs(10));

    if let Some((name, value)) = auth_header {
        get_req = get_req.header(name, value);
    }

    match get_req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_lowercase();

            // SSE endpoint typically returns 200 with text/event-stream,
            // or 401/403 if auth is required
            if content_type.contains("text/event-stream") {
                return Ok(DetectedTransport::Sse);
            }
            if (status == 401 || status == 403) && status != 404 {
                // Auth rejected on GET — likely SSE since POST already failed
                return Ok(DetectedTransport::Sse);
            }

            Err(format!(
                "Endpoint returned HTTP {status} on both POST and GET — cannot determine transport type"
            ))
        }
        Err(e) => Err(format!("Cannot reach endpoint: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_as_str() {
        assert_eq!(
            DetectedTransport::StreamableHttp.as_str(),
            "streamable_http"
        );
        assert_eq!(DetectedTransport::Sse.as_str(), "sse");
    }
}
