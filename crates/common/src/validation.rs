use crate::errors::AppError;

/// Validate password complexity:
/// - at least 8 characters
/// - contains at least one uppercase letter
/// - contains at least one lowercase letter
/// - contains at least one digit
pub fn validate_password(password: &str) -> Result<(), AppError> {
    if password.len() < 8 {
        return Err(AppError::BadRequest(
            "Password must be at least 8 characters".into(),
        ));
    }
    let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = password.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = password.chars().any(|c| c.is_ascii_digit());
    if !has_upper || !has_lower || !has_digit {
        return Err(AppError::BadRequest(
            "Password must contain at least one uppercase letter, one lowercase letter, and one digit".into(),
        ));
    }
    Ok(())
}

// --- Outbound URL + header validation (shared SSRF + injection guards) ---
//
// Used by every handler that configures an upstream endpoint:
// providers, MCP servers, MCP store install, log forwarders, OIDC
// issuer, dynamic settings. Lives here rather than in any one
// handler so the cross-module super:: calls collapse.

/// Headers a tenant admin is never allowed to supply as a "custom header"
/// on an upstream call — overriding these would let a key's role forge
/// auth, smuggle cookies, or break HTTP framing.
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
        // Valid header-name characters per RFC 7230.
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
        {
            return Err(AppError::BadRequest(format!(
                "Header name '{name}' contains invalid characters"
            )));
        }
        // Reject CR/LF in values (HTTP header injection).
        if value.bytes().any(|b| b == b'\r' || b == b'\n') {
            return Err(AppError::BadRequest(format!(
                "Header '{name}' value contains invalid characters"
            )));
        }
    }
    Ok(())
}

pub fn validate_url(url_str: &str) -> Result<(), AppError> {
    let parsed =
        url::Url::parse(url_str).map_err(|_| AppError::BadRequest("Invalid URL".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::BadRequest("URL must use http or https".into()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| AppError::BadRequest("URL must contain a host".into()))?;

    // Block well-known loopback / metadata hostnames.
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

    // Block obviously private hostnames (IP literals).
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(AppError::BadRequest("URL points to private network".into()));
        }
        // IP literal — no DNS rebinding risk.
        return Ok(());
    }

    // For hostnames, do a single blocking DNS resolution. Caller must
    // run from `spawn_blocking` if it can't tolerate the brief block.
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
pub fn is_blocked_ip(ip: &std::net::IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || is_private_ip(ip) || is_link_local(ip) {
        return true;
    }
    if let std::net::IpAddr::V6(v6) = ip {
        // IPv4-mapped IPv6 (::ffff:0:0/96) — re-check the v4 payload.
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

fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 10
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        }
        std::net::IpAddr::V6(v6) => {
            let segments = v6.segments();
            (segments[0] & 0xfe00) == 0xfc00
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_password() {
        assert!(validate_password("Abcdef1x").is_ok());
    }

    #[test]
    fn too_short() {
        assert!(validate_password("Ab1").is_err());
    }

    #[test]
    fn no_uppercase() {
        assert!(validate_password("abcdef12").is_err());
    }

    #[test]
    fn no_lowercase() {
        assert!(validate_password("ABCDEF12").is_err());
    }

    #[test]
    fn no_digit() {
        assert!(validate_password("Abcdefgh").is_err());
    }

    // ---------------------------------------------------------------
    // URL + header validation tests (moved from handlers::providers)
    // ---------------------------------------------------------------

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
        assert!(validate_url("http://[::ffff:127.0.0.1]").is_err());
        assert!(validate_url("http://[::ffff:10.0.0.1]").is_err());
    }

    #[test]
    fn validate_url_rejects_6to4_and_ula() {
        assert!(validate_url("http://[2002::1]").is_err());
        assert!(validate_url("http://[fc00::1]").is_err());
        assert!(validate_url("http://[fd12:3456::1]").is_err());
        assert!(validate_url("http://[fe80::1]").is_err());
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
