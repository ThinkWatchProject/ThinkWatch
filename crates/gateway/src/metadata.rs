use crate::providers::traits::ChatCompletionRequest;
use axum::http::HeaderMap;
use std::collections::HashMap;

/// Per-request metadata extracted from client headers and request body.
///
/// Metadata can be used for cost attribution, audit logging, and analytics.
#[derive(Debug, Clone)]
pub struct RequestMetadata {
    /// Custom key-value tags attached by the client.
    pub tags: HashMap<String, String>,
    /// Model name from the request.
    pub model: String,
    /// Unique request identifier.
    pub request_id: String,
    /// ISO 8601 timestamp when the request was received.
    pub timestamp: String,
}

/// Validation limits for metadata tags.
const MAX_TAGS: usize = 10;
const MAX_KEY_LEN: usize = 64;
const MAX_VALUE_LEN: usize = 256;

impl RequestMetadata {
    /// Extract metadata from request headers and body.
    ///
    /// Headers with the `X-Metadata-` prefix are extracted as tags (prefix stripped,
    /// lowercased). A `metadata` object in the request body JSON is also merged in
    /// (header values take precedence over body values for the same key).
    ///
    /// Validation: max 10 tags, max 64 chars per key, max 256 chars per value.
    /// Tags that exceed limits are silently dropped.
    pub fn extract(headers: &HeaderMap, body: &ChatCompletionRequest) -> Self {
        let mut tags = HashMap::new();

        // Extract from request body `extra` field — look for a "metadata" object
        if let Some(metadata_obj) = body.extra.get("metadata")
            && let Some(map) = metadata_obj.as_object()
        {
            for (k, v) in map {
                if let Some(v_str) = v.as_str() {
                    let key = k.to_lowercase();
                    if key.len() <= MAX_KEY_LEN && v_str.len() <= MAX_VALUE_LEN {
                        tags.insert(key, v_str.to_string());
                    }
                }
            }
        }

        // Extract from headers — these override body values
        let prefix = "x-metadata-";
        for (name, value) in headers {
            let header_name = name.as_str().to_lowercase();
            if let Some(tag_key) = header_name.strip_prefix(prefix) {
                if tag_key.is_empty() {
                    continue;
                }
                if let Ok(value_str) = value.to_str()
                    && tag_key.len() <= MAX_KEY_LEN
                    && value_str.len() <= MAX_VALUE_LEN
                {
                    tags.insert(tag_key.to_string(), value_str.to_string());
                }
            }
        }

        // Enforce max tags limit — keep first MAX_TAGS entries
        if tags.len() > MAX_TAGS {
            let keys: Vec<String> = tags.keys().cloned().collect();
            for key in keys.into_iter().skip(MAX_TAGS) {
                tags.remove(&key);
            }
        }

        // Honor a caller-supplied `x-trace-id` so the request_id used
        // for logging / metadata matches the trace_id the client pinned
        // — letting one client correlate the AI call with its
        // follow-on MCP tools/call under a single trace. Validation
        // mirrors the access_log middleware and the MCP transport.
        // Restrict to printable ASCII (0x20-0x7E) so the resulting string
        // is always safe to round-trip through `HeaderValue`. Anything
        // outside that range is rejected and we fall back to a UUID.
        let request_id = headers
            .get("x-trace-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| {
                !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| (0x20..=0x7E).contains(&b))
            })
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let timestamp = chrono::Utc::now().to_rfc3339();

        Self {
            tags,
            model: body.model.clone(),
            request_id,
            timestamp,
        }
    }

    /// Serialize metadata to a JSON value suitable for audit log detail fields.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "request_id": self.request_id,
            "model": self.model,
            "timestamp": self.timestamp,
            "tags": self.tags,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn make_request(model: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            stream: None,
            extra: serde_json::json!({}),
            caller_user_id: None,
            caller_user_email: None,
        }
    }

    fn make_request_with_metadata(
        model: &str,
        metadata: serde_json::Value,
    ) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            stream: None,
            extra: serde_json::json!({ "metadata": metadata }),
            caller_user_id: None,
            caller_user_email: None,
        }
    }

    #[test]
    fn extract_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Metadata-Department",
            HeaderValue::from_static("engineering"),
        );
        headers.insert("X-Metadata-Project", HeaderValue::from_static("chatbot-v2"));

        let req = make_request("gpt-4o");
        let meta = RequestMetadata::extract(&headers, &req);

        assert_eq!(meta.tags.get("department").unwrap(), "engineering");
        assert_eq!(meta.tags.get("project").unwrap(), "chatbot-v2");
        assert_eq!(meta.model, "gpt-4o");
        assert!(!meta.request_id.is_empty());
    }

    #[test]
    fn extract_from_body() {
        let headers = HeaderMap::new();
        let req = make_request_with_metadata(
            "gpt-4o",
            serde_json::json!({"team": "platform", "env": "prod"}),
        );
        let meta = RequestMetadata::extract(&headers, &req);

        assert_eq!(meta.tags.get("team").unwrap(), "platform");
        assert_eq!(meta.tags.get("env").unwrap(), "prod");
    }

    #[test]
    fn headers_override_body() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Metadata-Team", HeaderValue::from_static("infra"));

        let req = make_request_with_metadata("gpt-4o", serde_json::json!({"team": "platform"}));
        let meta = RequestMetadata::extract(&headers, &req);

        assert_eq!(meta.tags.get("team").unwrap(), "infra");
    }

    #[test]
    fn max_tags_enforced() {
        let mut headers = HeaderMap::new();
        for i in 0..15 {
            let name = format!("X-Metadata-Key{i}");
            // HeaderName requires lowercase; axum normalizes but for test we use valid names
            headers.insert(
                axum::http::HeaderName::from_bytes(name.to_lowercase().as_bytes()).unwrap(),
                HeaderValue::from_str(&format!("val{i}")).unwrap(),
            );
        }

        let req = make_request("gpt-4o");
        let meta = RequestMetadata::extract(&headers, &req);

        assert!(meta.tags.len() <= MAX_TAGS);
    }

    #[test]
    fn to_json_includes_all_fields() {
        let headers = HeaderMap::new();
        let req = make_request_with_metadata("gpt-4o", serde_json::json!({"dept": "eng"}));
        let meta = RequestMetadata::extract(&headers, &req);
        let json = meta.to_json();

        assert!(json.get("request_id").is_some());
        assert!(json.get("model").is_some());
        assert!(json.get("timestamp").is_some());
        assert!(json.get("tags").is_some());
        assert_eq!(json["tags"]["dept"], "eng");
    }
}
