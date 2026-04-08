use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use think_watch_common::crypto;
use think_watch_common::dto::CreateProviderRequest;
use think_watch_common::errors::AppError;
use think_watch_common::models::Provider;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// Headers that must not be overridden by custom_headers.
const BLOCKED_HEADERS: &[&str] = &[
    "host",
    "authorization",
    "cookie",
    "set-cookie",
    "transfer-encoding",
    "content-length",
    "connection",
    "proxy-authorization",
    "te",
    "upgrade",
];
const MAX_CUSTOM_HEADERS: usize = 20;
const MAX_HEADER_NAME_LEN: usize = 128;
const MAX_HEADER_VALUE_LEN: usize = 4096;

pub fn validate_custom_headers(
    headers: &std::collections::HashMap<String, String>,
) -> Result<(), AppError> {
    if headers.len() > MAX_CUSTOM_HEADERS {
        return Err(AppError::BadRequest(format!(
            "Too many custom headers (max {MAX_CUSTOM_HEADERS})"
        )));
    }
    for (name, value) in headers {
        if name.len() > MAX_HEADER_NAME_LEN || value.len() > MAX_HEADER_VALUE_LEN {
            return Err(AppError::BadRequest(format!(
                "Header '{name}' exceeds size limit (name max {MAX_HEADER_NAME_LEN}, value max {MAX_HEADER_VALUE_LEN})"
            )));
        }
        if BLOCKED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(AppError::BadRequest(format!(
                "Header '{name}' is not allowed as a custom header"
            )));
        }
        // Validate header name contains only valid characters (RFC 7230)
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
        {
            return Err(AppError::BadRequest(format!(
                "Header name '{name}' contains invalid characters"
            )));
        }
        // Reject header values with CR/LF (HTTP header injection)
        if value.bytes().any(|b| b == b'\r' || b == b'\n') {
            return Err(AppError::BadRequest(format!(
                "Header '{name}' value contains invalid characters"
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_url(url_str: &str) -> Result<(), AppError> {
    let parsed =
        url::Url::parse(url_str).map_err(|_| AppError::BadRequest("Invalid URL".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::BadRequest("URL must use http or https".into()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| AppError::BadRequest("URL must contain a host".into()))?;

    // Block well-known loopback / metadata hostnames
    let blocked_hosts = [
        "localhost",
        "127.0.0.1",
        "0.0.0.0",
        "169.254.169.254",
        "[::1]",
        "metadata.google.internal",
    ];
    if blocked_hosts.contains(&host) {
        return Err(AppError::BadRequest("URL points to blocked address".into()));
    }

    // Block obviously private hostnames (IP literals)
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(AppError::BadRequest("URL points to private network".into()));
        }
        // IP literal — no DNS rebinding risk
        return Ok(());
    }

    // For hostnames, do a single blocking DNS resolution. The function is
    // currently sync so the caller must run it from `spawn_blocking` if it
    // can't tolerate the brief block. The previous double-resolve with
    // std::thread::sleep was strictly worse: it blocked the *async*
    // executor for 50ms on every successful validation.
    //
    // The double-resolve was meant to catch DNS rebinding, but rebinding
    // is a TOFU attack against the actual TCP connection, not the
    // validator — defending properly requires a custom DNS-pinning
    // resolver in reqwest, which is out of scope here. We keep the
    // single-shot resolve as a baseline guard.
    let resolved: Vec<std::net::IpAddr> = std::net::ToSocketAddrs::to_socket_addrs(&(host, 80))
        .map(|addrs| addrs.map(|a| a.ip()).collect())
        .unwrap_or_default();

    if resolved.is_empty() {
        return Err(AppError::BadRequest(
            "URL host could not be resolved".into(),
        ));
    }
    for ip in &resolved {
        if is_blocked_ip(ip) {
            return Err(AppError::BadRequest(
                "URL resolves to a private or loopback address".into(),
            ));
        }
    }

    Ok(())
}

/// Aggregate "do not connect to" check covering loopback, unspecified
/// (0.0.0.0 / ::), private ranges, link-local, ULA, IPv4-mapped IPv6,
/// 6to4, and IPv6 multicast.
fn is_blocked_ip(ip: &std::net::IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || is_private_ip(ip) || is_link_local(ip) {
        return true;
    }
    if let std::net::IpAddr::V6(v6) = ip {
        // IPv4-mapped IPv6 (::ffff:0:0/96): unwrap and re-check the v4
        // address to catch e.g. ::ffff:127.0.0.1.
        if let Some(v4) = v6.to_ipv4_mapped() {
            let inner = std::net::IpAddr::V4(v4);
            if inner.is_loopback()
                || inner.is_unspecified()
                || is_private_ip(&inner)
                || is_link_local(&inner)
            {
                return true;
            }
        }
        // 6to4 (2002::/16) — embeds an IPv4 address; reject conservatively.
        let segs = v6.segments();
        if segs[0] == 0x2002 {
            return true;
        }
        // IPv6 multicast ff00::/8
        if (segs[0] & 0xff00) == 0xff00 {
            return true;
        }
    }
    false
}

/// Check if an IP is in a private range (RFC 1918 + RFC 6598).
fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 10.0.0.0/8
            octets[0] == 10
            // 172.16.0.0/12
            || (octets[0] == 172 && (16..=31).contains(&octets[1]))
            // 192.168.0.0/16
            || (octets[0] == 192 && octets[1] == 168)
            // 100.64.0.0/10 (CGNAT / RFC 6598)
            || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        }
        std::net::IpAddr::V6(v6) => {
            // Unique local addresses fc00::/7
            let segments = v6.segments();
            (segments[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Check if an IP is link-local (169.254.0.0/16 or fe80::/10).
fn is_link_local(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 169 && octets[1] == 254
        }
        std::net::IpAddr::V6(v6) => {
            let segments = v6.segments();
            (segments[0] & 0xffc0) == 0xfe80
        }
    }
}

fn encryption_key(state: &AppState) -> Result<[u8; 32], AppError> {
    crypto::parse_encryption_key(&state.config.encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))
}

pub async fn list_providers(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Provider>>, AppError> {
    auth_user.require_permission("providers:read")?;
    let providers = sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE deleted_at IS NULL ORDER BY created_at DESC",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(providers))
}

pub async fn create_provider(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateProviderRequest>,
) -> Result<Json<Provider>, AppError> {
    auth_user.require_permission("providers:create")?;
    if req.name.is_empty() || req.base_url.is_empty() || req.api_key.is_empty() {
        return Err(AppError::BadRequest(
            "name, base_url, and api_key are required".into(),
        ));
    }

    // SSRF prevention: validate base_url
    validate_url(&req.base_url)?;

    let key = encryption_key(&state)?;
    let encrypted_key = crypto::encrypt(req.api_key.as_bytes(), &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption failed: {e}")))?;

    // Merge custom_headers into config_json
    let mut config = req.config.unwrap_or(serde_json::json!({}));
    if let Some(headers) = &req.custom_headers {
        validate_custom_headers(headers)?;
        config["custom_headers"] = serde_json::to_value(headers)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to serialize headers: {e}")))?;
    }

    let provider = sqlx::query_as::<_, Provider>(
        r#"INSERT INTO providers (name, display_name, provider_type, base_url, api_key_encrypted, config_json)
           VALUES ($1, $2, $3, $4, $5, $6) RETURNING *"#,
    )
    .bind(&req.name)
    .bind(&req.display_name)
    .bind(&req.provider_type)
    .bind(&req.base_url)
    .bind(&encrypted_key)
    .bind(config)
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("provider.created")
            .resource("provider")
            .resource_id(provider.id.to_string())
            .detail(serde_json::json!({ "name": &req.name })),
    );

    Ok(Json(provider))
}

#[derive(Debug, serde::Deserialize)]
pub struct UpdateProviderRequest {
    pub display_name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// Custom HTTP headers forwarded when proxying requests to this provider.
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

pub async fn update_provider(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateProviderRequest>,
) -> Result<Json<Provider>, AppError> {
    auth_user.require_permission("providers:update")?;
    if req.api_key.is_some() {
        auth_user.require_permission("providers:rotate_key")?;
    }
    let existing = sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("Provider not found".into()))?;

    let display_name = req
        .display_name
        .as_deref()
        .unwrap_or(&existing.display_name);
    let base_url = req.base_url.as_deref().unwrap_or(&existing.base_url);

    if req.base_url.is_some() {
        validate_url(base_url)?;
    }

    let api_key_encrypted = if let Some(ref new_key) = req.api_key {
        let key = encryption_key(&state)?;
        crypto::encrypt(new_key.as_bytes(), &key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption failed: {e}")))?
    } else {
        existing.api_key_encrypted.clone()
    };

    // Merge custom_headers into existing config_json
    let config_json = if let Some(ref headers) = req.custom_headers {
        validate_custom_headers(headers)?;
        let mut config = existing.config_json.clone();
        config["custom_headers"] = serde_json::to_value(headers)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to serialize headers: {e}")))?;
        config
    } else {
        existing.config_json.clone()
    };

    let updated = sqlx::query_as::<_, Provider>(
        r#"UPDATE providers SET display_name = $2, base_url = $3, api_key_encrypted = $4, config_json = $5
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(display_name)
    .bind(base_url)
    .bind(&api_key_encrypted)
    .bind(&config_json)
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("provider.updated")
            .resource("provider")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": existing.name })),
    );

    Ok(Json(updated))
}

pub async fn get_provider(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Provider>, AppError> {
    auth_user.require_permission("providers:read")?;
    let provider = sqlx::query_as::<_, Provider>(
        "SELECT * FROM providers WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("Provider not found".into()))?;

    Ok(Json(provider))
}

pub async fn delete_provider(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("providers:delete")?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM providers WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;

    sqlx::query("UPDATE providers SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .execute(&state.db)
        .await?;

    state.audit.log(
        auth_user
            .audit("provider.deleted")
            .resource("provider")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": name })),
    );

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// ---------------------------------------------------------------------------
// Test connection — used by the setup wizard and Add Provider dialog so
// admins can verify base URL + API key + custom headers without persisting.
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct TestProviderRequest {
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, serde::Serialize)]
pub struct TestProviderResponse {
    pub success: bool,
    pub message: String,
    /// HTTP status code returned by upstream, if a response was received.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    /// Round-trip latency in milliseconds.
    pub latency_ms: u64,
    /// Number of models returned by the upstream `/v1/models` (where applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_count: Option<usize>,
}

/// Authenticated route — used by the providers admin page.
pub async fn test_provider(
    auth_user: AuthUser,
    State(_state): State<AppState>,
    Json(req): Json<TestProviderRequest>,
) -> Result<Json<TestProviderResponse>, AppError> {
    auth_user.require_permission("providers:create")?;
    run_provider_test(req).await
}

/// Unauthenticated route — used by the setup wizard before any user exists.
/// Gated by an extra check that setup is not yet complete, so anonymous
/// callers can't probe arbitrary URLs against an installed instance.
pub async fn test_provider_unauthenticated(
    State(state): State<AppState>,
    Json(req): Json<TestProviderRequest>,
) -> Result<Json<TestProviderResponse>, AppError> {
    if state.dynamic_config.is_initialized().await {
        return Err(AppError::Forbidden(
            "Setup already completed — use the authenticated endpoint".into(),
        ));
    }
    run_provider_test(req).await
}

async fn run_provider_test(
    req: TestProviderRequest,
) -> Result<Json<TestProviderResponse>, AppError> {
    if req.base_url.is_empty() || req.api_key.is_empty() {
        return Err(AppError::BadRequest(
            "base_url and api_key are required".into(),
        ));
    }
    validate_url(&req.base_url)?;
    if let Some(headers) = &req.custom_headers {
        validate_custom_headers(headers)?;
    }

    // Provider-specific probe URL + auth header. We always hit a cheap,
    // read-only endpoint that requires auth so a wrong key is detected too.
    let url = match req.provider_type.as_str() {
        "anthropic" => format!("{}/v1/models", req.base_url.trim_end_matches('/')),
        "google" => format!(
            "{}/v1beta/models?key={}",
            req.base_url.trim_end_matches('/'),
            urlencoding::encode(&req.api_key)
        ),
        // openai / azure / custom — all OpenAI-compatible /v1/models
        _ => format!("{}/v1/models", req.base_url.trim_end_matches('/')),
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to build HTTP client: {e}")))?;

    let mut builder = client.get(&url);
    match req.provider_type.as_str() {
        "anthropic" => {
            builder = builder
                .header("x-api-key", &req.api_key)
                .header("anthropic-version", "2023-06-01");
        }
        "google" => {
            // key is in the query string already
        }
        _ => {
            builder = builder.bearer_auth(&req.api_key);
        }
    }
    if let Some(headers) = req.custom_headers.as_ref() {
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
    }

    let started = std::time::Instant::now();
    let result = builder.send().await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(resp) => {
            let status = resp.status();
            let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            if status.is_success() {
                // Try to count models from the standard shape: { "data": [...] }
                // or anthropic's { "data": [...] } / google's { "models": [...] }.
                let model_count = body
                    .get("data")
                    .and_then(|v| v.as_array().map(|a| a.len()))
                    .or_else(|| {
                        body.get("models")
                            .and_then(|v| v.as_array().map(|a| a.len()))
                    });
                Ok(Json(TestProviderResponse {
                    success: true,
                    message: match model_count {
                        Some(n) => format!("Connected successfully — {n} models available"),
                        None => "Connected successfully".to_string(),
                    },
                    status_code: Some(status.as_u16()),
                    latency_ms,
                    model_count,
                }))
            } else {
                let upstream_err = body
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_string());
                Ok(Json(TestProviderResponse {
                    success: false,
                    message: format!("HTTP {}: {upstream_err}", status.as_u16()),
                    status_code: Some(status.as_u16()),
                    latency_ms,
                    model_count: None,
                }))
            }
        }
        Err(e) => Ok(Json(TestProviderResponse {
            success: false,
            message: format!("Request failed: {e}"),
            status_code: None,
            latency_ms,
            model_count: None,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn validate_url_accepts_public_https() {
        assert!(validate_url("https://api.openai.com/v1").is_ok());
        assert!(validate_url("https://generativelanguage.googleapis.com").is_ok());
    }

    #[test]
    fn validate_url_rejects_private_ips() {
        assert!(validate_url("http://127.0.0.1:8080").is_err());
        assert!(validate_url("http://localhost:3000").is_err());
        assert!(validate_url("http://0.0.0.0").is_err());
        assert!(validate_url("http://169.254.169.254/metadata").is_err());
        assert!(validate_url("http://[::1]:8080").is_err());
        assert!(validate_url("http://10.0.0.1").is_err());
        assert!(validate_url("http://192.168.1.1").is_err());
        assert!(validate_url("http://172.16.0.1").is_err());
        assert!(validate_url("http://100.64.1.1").is_err()); // CGNAT
    }

    #[test]
    fn validate_url_rejects_ipv4_mapped_ipv6_loopback() {
        // ::ffff:127.0.0.1 — IPv4-mapped IPv6 form of loopback
        assert!(validate_url("http://[::ffff:127.0.0.1]").is_err());
        assert!(validate_url("http://[::ffff:10.0.0.1]").is_err());
    }

    #[test]
    fn validate_url_rejects_6to4_and_ula() {
        // 6to4 prefix 2002::/16
        assert!(validate_url("http://[2002::1]").is_err());
        // Unique local fc00::/7
        assert!(validate_url("http://[fc00::1]").is_err());
        assert!(validate_url("http://[fd12:3456::1]").is_err());
        // Link-local fe80::/10
        assert!(validate_url("http://[fe80::1]").is_err());
        // IPv6 multicast
        assert!(validate_url("http://[ff02::1]").is_err());
    }

    #[test]
    fn validate_url_rejects_non_http() {
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn validate_url_rejects_no_host() {
        assert!(validate_url("http://").is_err());
    }

    #[test]
    fn validate_custom_headers_accepts_valid() {
        let mut headers = HashMap::new();
        headers.insert("X-Custom".to_string(), "value".to_string());
        assert!(validate_custom_headers(&headers).is_ok());
    }

    #[test]
    fn validate_custom_headers_rejects_blocked() {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer xxx".to_string());
        assert!(validate_custom_headers(&headers).is_err());
    }

    #[test]
    fn validate_custom_headers_rejects_crlf_injection() {
        let mut headers = HashMap::new();
        headers.insert(
            "X-Custom".to_string(),
            "value\r\nInjected: true".to_string(),
        );
        assert!(validate_custom_headers(&headers).is_err());
    }

    #[test]
    fn validate_custom_headers_rejects_too_many() {
        let headers: HashMap<String, String> = (0..25)
            .map(|i| (format!("X-H{i}"), "v".to_string()))
            .collect();
        assert!(validate_custom_headers(&headers).is_err());
    }

    #[test]
    fn is_private_ip_detects_rfc1918() {
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.1.1".parse().unwrap()));
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_detects_cgnat() {
        assert!(is_private_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_private_ip(&"100.127.255.255".parse().unwrap()));
        assert!(!is_private_ip(&"100.63.255.255".parse().unwrap()));
    }

    #[test]
    fn is_link_local_ipv4() {
        assert!(is_link_local(&"169.254.1.1".parse().unwrap()));
        assert!(!is_link_local(&"169.255.1.1".parse().unwrap()));
    }
}
