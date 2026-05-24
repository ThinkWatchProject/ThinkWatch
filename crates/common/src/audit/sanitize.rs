//! Recursive secret-key redaction for audit detail blobs.
//!
//! Walks JSON objects and arrays, replacing any value whose KEY name
//! contains a credential-shaped substring with `[REDACTED]`. Used by
//! every `detail_str` call site so a developer who stuffs a token
//! into `entry.detail()` doesn't accidentally write it to ClickHouse.

/// Sanitize an `Option<serde_json::Value>` in place. No-op when None.
pub(super) fn sanitize_detail(detail: &mut Option<serde_json::Value>) {
    if let Some(value) = detail {
        sanitize_value(value);
    }
}

/// Sanitize a captured request/response body string. The gateway and
/// MCP proxy serialise their JSON payloads to a `String` before
/// stashing them in `AuditEntry::request_body` / `response_body`, so
/// the same secret-key matcher that runs on `detail` doesn't reach
/// them — a prompt like `{"api_key":"sk-…","messages":[…]}` would
/// otherwise land in `gateway_logs.request_body` verbatim. If the body
/// parses as JSON we re-walk it through the same matcher and
/// re-serialise; non-JSON bodies pass through (no known shape to
/// redact against). The cost is one parse + re-serialise per audit
/// row, only at flush time on the batched worker.
pub(super) fn sanitize_body_if_json(body: Option<String>) -> Option<String> {
    let body = body?;
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&body) else {
        return Some(body);
    };
    sanitize_value(&mut value);
    Some(value.to_string())
}

/// Recursively redact values whose keys look secret-shaped, anywhere
/// in the JSON tree. Top-level-only redaction (the previous version)
/// missed MCP `arguments.{password,api_key,token,…}` and any other
/// payload that nests sensitive fields under a benign key. Walks
/// objects and arrays; leaves primitives alone.
fn sanitize_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            // Collect candidate keys first so we don't borrow the map
            // mutably twice in the same step.
            let keys_to_redact: Vec<String> = map
                .keys()
                .filter(|k| is_secret_key_name(k))
                .cloned()
                .collect();
            for key in keys_to_redact {
                map.insert(key, serde_json::Value::String("[REDACTED]".to_string()));
            }
            // Recurse into the surviving children — sub-objects can
            // hold their own secret-named fields.
            for v in map.values_mut() {
                sanitize_value(v);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sanitize_value(item);
            }
        }
        _ => {}
    }
}

fn is_secret_key_name(key: &str) -> bool {
    let lower = key.to_lowercase();

    // Escape hatch: field names that contain "token"/"key" as a
    // unit-of-measure or lookup-key rather than a credential. The
    // straight substring check below is intentionally aggressive on
    // unfamiliar names, so this list only needs to grow when a
    // concrete observability complaint surfaces. Pricing was the
    // first such complaint: `input_price_per_token` was being
    // emitted as `[REDACTED]` in `platform_pricing.updated` audit
    // rows, which broke billing-change traceability — the price IS
    // the auditable fact.
    const SAFE_SUFFIXES: &[&str] = &[
        "_per_token", // input_price_per_token, output_price_per_token
        "_tokens",    // input_tokens, output_tokens, total_tokens, prompt_tokens
    ];
    if SAFE_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        return false;
    }

    lower.contains("password")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("key")
        || lower.contains("authorization")
        || lower.contains("credential")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_sanitizer_redacts_obvious_credential_field_names() {
        // The matcher is deliberately substring-based and aggressive
        // on unfamiliar names — better to over-redact than miss a
        // real secret. Pin the canonical positives so a future
        // refactor that swaps to e.g. exact-match matching is caught.
        let mut detail = Some(serde_json::json!({
            "password": "hunter2",
            "api_key": "sk-live-xxx",
            "auth_token": "eyJ…",
            "client_secret": "zzz",
            "Authorization": "Bearer …",
            "credential": {"aws": "AKIA…"},
            "nested": {"refresh_token": "rt-…"},
        }));
        sanitize_detail(&mut detail);
        let v = detail.unwrap();
        assert_eq!(v["password"], "[REDACTED]");
        assert_eq!(v["api_key"], "[REDACTED]");
        assert_eq!(v["auth_token"], "[REDACTED]");
        assert_eq!(v["client_secret"], "[REDACTED]");
        assert_eq!(v["Authorization"], "[REDACTED]");
        assert_eq!(v["credential"], "[REDACTED]");
        // Recursive: nested credential-shaped key also redacted.
        assert_eq!(v["nested"]["refresh_token"], "[REDACTED]");
    }

    #[test]
    fn detail_sanitizer_keeps_pricing_and_token_count_fields_visible() {
        // `_per_token` and `_tokens` show up in pricing + usage rows
        // that operators need to see verbatim. Without the
        // SAFE_SUFFIXES escape hatch, the audit row for
        // `platform_pricing.updated` came out as
        // `{"input_price_per_token":"[REDACTED]"}` and the operator
        // couldn't tell what the new rate was. Pin the carve-out so
        // tightening the matcher later doesn't silently re-redact.
        let mut detail = Some(serde_json::json!({
            "input_price_per_token": "0.00000015",
            "output_price_per_token": "0.00000060",
            "input_tokens": 12,
            "output_tokens": 5,
            "total_tokens": 17,
            "prompt_tokens": 12,
            "completion_tokens": 5,
        }));
        sanitize_detail(&mut detail);
        let v = detail.unwrap();
        assert_eq!(v["input_price_per_token"], "0.00000015");
        assert_eq!(v["output_price_per_token"], "0.00000060");
        assert_eq!(v["input_tokens"], 12);
        assert_eq!(v["total_tokens"], 17);
        assert_eq!(v["completion_tokens"], 5);
    }

    #[test]
    fn body_sanitizer_redacts_secrets_in_json_body_strings() {
        // The gateway captures the full request payload as a JSON
        // string. Without body-level sanitisation, a user prompt that
        // embeds `{"api_key":"…"}` lands in CH verbatim. Pin the
        // contract: parse → walk → re-serialise produces the same
        // redacted shape as `sanitize_detail` would on the Value
        // directly.
        let body = r#"{"api_key":"sk-live-xxx","messages":[{"role":"user","content":"hi"}]}"#;
        let out = sanitize_body_if_json(Some(body.to_string())).unwrap();
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("sk-live-xxx"));
        assert!(out.contains("hi")); // benign content preserved
    }

    #[test]
    fn body_sanitizer_passes_through_non_json() {
        let body = "not actually json {{{".to_string();
        assert_eq!(sanitize_body_if_json(Some(body.clone())), Some(body),);
    }
}
