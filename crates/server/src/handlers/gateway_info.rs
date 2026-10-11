//! `GET /api/gateway/info` — where clients reach the gateway, for any
//! signed-in console user.
//!
//! The Configuration Guide and the "Add to ThinkWatch Lite" button need
//! the gateway's public address. It used to be read from
//! `/api/admin/settings/system`, which needs `settings:read`: everyone
//! else got 403 and a guess built from the browser's host and port 3000,
//! whatever `general.public_*` said.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::Serialize;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GatewayInfo {
    /// The gateway's root as clients reach it: scheme, host and port
    /// (left out when it is the scheme's default), no path and no
    /// trailing slash — `https://gateway.example.com`,
    /// `http://10.0.0.5:3000`. The API paths (`/v1/...`, `/mcp`) go
    /// after it.
    pub base_url: String,
}

#[utoipa::path(
    get,
    path = "/api/gateway/info",
    tag = "Gateway",
    responses(
        (status = 200, description = "The gateway's public base URL", body = GatewayInfo),
        (status = 401, description = "Not signed in"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn get_gateway_info(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<GatewayInfo> {
    let dc = &state.dynamic_config;
    let configured = PublicAddress {
        protocol: dc
            .get_string("general.public_protocol")
            .await
            .unwrap_or_default(),
        host: dc
            .get_string("general.public_host")
            .await
            .unwrap_or_default(),
        port: dc.get_i64("general.public_port").await.unwrap_or(0),
    };
    Json(GatewayInfo {
        base_url: public_base_url(&configured, &headers, state.config.gateway_port),
    })
}

/// `general.public_protocol`, `general.public_host`, `general.public_port`
/// as stored: empty and 0 mean "not set".
pub(crate) struct PublicAddress {
    pub protocol: String,
    pub host: String,
    pub port: i64,
}

/// The gateway's base URL from the `general.public_*` settings. A part
/// that is not set comes from the address the console was opened at —
/// what the settings page means by "auto-detect from the browser" — and
/// the port from the gateway's own listening port:
///
/// - scheme: the setting, else the request's `X-Forwarded-Proto` (the
///   bundled nginx sets it to the scheme the browser used), else `http`;
/// - host: the setting, else `X-Forwarded-Host`, else `Host`, without its
///   port, else `localhost`;
/// - port: the setting when it is 1–65535, else the gateway port; left out
///   when it is the scheme's default.
///
/// The request headers only fill in what an administrator left unset, and
/// the answer goes back to the same caller, so a forged header misleads no
/// one but the client that sent it. A configured host that is not a bare
/// host name or IP address (a port, a path or anything else that would
/// change the URL's shape) is passed over like an unset one.
pub(crate) fn public_base_url(
    configured: &PublicAddress,
    headers: &HeaderMap,
    gateway_port: u16,
) -> String {
    let scheme = match configured.protocol.as_str() {
        p @ ("http" | "https") => p,
        _ => first_value(headers, "x-forwarded-proto")
            .map(|p| p.to_ascii_lowercase())
            .and_then(|p| match p.as_str() {
                "https" => Some("https"),
                "http" => Some("http"),
                _ => None,
            })
            .unwrap_or("http"),
    };
    let host = bare_host(&configured.host)
        .or_else(|| first_value(headers, "x-forwarded-host").and_then(host_of_authority))
        .or_else(|| first_value(headers, "host").and_then(host_of_authority))
        .unwrap_or_else(|| "localhost".to_string());
    let port = u16::try_from(configured.port)
        .ok()
        .filter(|p| *p > 0)
        .unwrap_or(gateway_port);
    let default_port = matches!((scheme, port), ("http", 80) | ("https", 443));
    if default_port {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    }
}

/// The first of a header's comma-separated values, trimmed; `None` when
/// absent or empty.
fn first_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// The host of a `Host`-style authority (`example.com:8080`,
/// `[::1]:3001`), checked like a configured host.
fn host_of_authority(authority: &str) -> Option<String> {
    let parsed: axum::http::uri::Authority = authority.parse().ok()?;
    bare_host(parsed.host())
}

/// `host` as it goes into a URL — a domain name in lower case, an IPv4
/// address, or an IPv6 address in brackets (given with or without them)
/// — or `None` when it is empty or is anything more than a host.
fn bare_host(host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    let bracketed;
    let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        bracketed = format!("[{host}]");
        bracketed.as_str()
    } else {
        host
    };
    let parsed = url::Host::parse(host).ok()?.to_string();
    parsed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
        .then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn configured(protocol: &str, host: &str, port: i64) -> PublicAddress {
        PublicAddress {
            protocol: protocol.into(),
            host: host.into(),
            port,
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn the_settings_make_the_url_and_a_default_port_is_left_out() {
        let none = HeaderMap::new();
        assert_eq!(
            public_base_url(&configured("https", "gw.example.com", 443), &none, 3000),
            "https://gw.example.com"
        );
        assert_eq!(
            public_base_url(&configured("https", "gw.example.com", 8443), &none, 3000),
            "https://gw.example.com:8443"
        );
        assert_eq!(
            public_base_url(&configured("http", "10.0.0.5", 0), &none, 3000),
            "http://10.0.0.5:3000"
        );
        assert_eq!(
            public_base_url(&configured("http", "gw.internal", 80), &none, 3000),
            "http://gw.internal"
        );
        // A configured host wins over the request's.
        assert_eq!(
            public_base_url(
                &configured("https", "GW.Example.com", 0),
                &headers(&[("host", "console.example.com")]),
                443
            ),
            "https://gw.example.com"
        );
    }

    #[test]
    fn unset_parts_come_from_the_address_the_console_was_opened_at() {
        assert_eq!(
            public_base_url(
                &configured("", "", 0),
                &headers(&[
                    ("host", "console.example.com:8080"),
                    ("x-forwarded-proto", "https"),
                ]),
                3000
            ),
            "https://console.example.com:3000"
        );
        assert_eq!(
            public_base_url(
                &configured("", "", 443),
                &headers(&[
                    ("host", "internal:3001"),
                    ("x-forwarded-host", "tw.example.com, proxy.internal"),
                    ("x-forwarded-proto", "HTTPS"),
                ]),
                3000
            ),
            "https://tw.example.com"
        );
        // No forwarded scheme: plain http, as the console port serves.
        assert_eq!(
            public_base_url(
                &configured("", "", 0),
                &headers(&[("host", "localhost:5173")]),
                3000
            ),
            "http://localhost:3000"
        );
        // Nothing to go on.
        assert_eq!(
            public_base_url(&configured("", "", 0), &HeaderMap::new(), 3000),
            "http://localhost:3000"
        );
        // A scheme that is neither is ignored.
        assert_eq!(
            public_base_url(
                &configured("", "gw", 0),
                &headers(&[("x-forwarded-proto", "ws")]),
                3000
            ),
            "http://gw:3000"
        );
    }

    #[test]
    fn ipv6_hosts_are_bracketed() {
        let none = HeaderMap::new();
        assert_eq!(
            public_base_url(&configured("http", "::1", 0), &none, 3000),
            "http://[::1]:3000"
        );
        assert_eq!(
            public_base_url(&configured("http", "[fd00::5]", 0), &none, 3000),
            "http://[fd00::5]:3000"
        );
        assert_eq!(
            public_base_url(
                &configured("", "", 0),
                &headers(&[("host", "[::1]:3001")]),
                3000
            ),
            "http://[::1]:3000"
        );
    }

    #[test]
    fn a_configured_host_that_is_more_than_a_host_is_passed_over() {
        let from_request = headers(&[("host", "console.example.com")]);
        for bad in [
            "gw.example.com:8443",
            "user@gw.example.com",
            "gw.example.com?x=1",
            "gw example",
            "gw.example.com#top",
            "${GATEWAY_HOST}",
        ] {
            assert_eq!(
                public_base_url(&configured("https", bad, 0), &from_request, 443),
                "https://console.example.com",
                "{bad}"
            );
        }
    }
}
