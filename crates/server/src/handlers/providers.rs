use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use agent_bastion_common::crypto;
use agent_bastion_common::dto::CreateProviderRequest;
use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::Provider;

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
        if ip.is_loopback() || ip.is_unspecified() || is_private_ip(&ip) || is_link_local(&ip) {
            return Err(AppError::BadRequest("URL points to private network".into()));
        }
        // IP literal — no DNS rebinding risk
        return Ok(());
    }

    // Resolve DNS and check all resolved IPs to prevent DNS rebinding.
    // We resolve twice with a short delay to detect rebinding attacks where
    // the first resolution returns a public IP and the second returns private.
    let resolve = |host: &str| -> Vec<std::net::IpAddr> {
        std::net::ToSocketAddrs::to_socket_addrs(&(host, 80))
            .map(|addrs| addrs.map(|a| a.ip()).collect())
            .unwrap_or_default()
    };

    let first_ips = resolve(host);
    for ip in &first_ips {
        if ip.is_loopback() || ip.is_unspecified() || is_private_ip(ip) || is_link_local(ip) {
            return Err(AppError::BadRequest(
                "URL resolves to a private or loopback address".into(),
            ));
        }
    }

    // Second resolution after a short delay to catch DNS rebinding
    std::thread::sleep(std::time::Duration::from_millis(50));
    let second_ips = resolve(host);
    for ip in &second_ips {
        if ip.is_loopback() || ip.is_unspecified() || is_private_ip(ip) || is_link_local(ip) {
            return Err(AppError::BadRequest(
                "URL resolves to a private address (possible DNS rebinding)".into(),
            ));
        }
    }

    if first_ips.is_empty() && second_ips.is_empty() {
        return Err(AppError::BadRequest("URL host could not be resolved".into()));
    }

    Ok(())
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
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Provider>>, AppError> {
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
            .resource_id(id.to_string()),
    );

    Ok(Json(updated))
}

pub async fn get_provider(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Provider>, AppError> {
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
    sqlx::query("UPDATE providers SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .execute(&state.db)
        .await?;

    state.audit.log(
        auth_user
            .audit("provider.deleted")
            .resource("provider")
            .resource_id(id.to_string()),
    );

    Ok(Json(serde_json::json!({"status": "deleted"})))
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
        headers.insert("X-Custom".to_string(), "value\r\nInjected: true".to_string());
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
