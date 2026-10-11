//! `GET /api/gateway/info` — where clients reach the gateway, for any
//! signed-in console user.
//!
//! The Configuration Guide and the "Add to ThinkWatch Lite" button need
//! the gateway's public address. It used to be read from
//! `/api/admin/settings/system`, which needs `settings:read`: everyone
//! else got 403 and a guess built from the browser's host and port 3000,
//! whatever `general.public_*` said.
//!
//! The answer is what is configured, and the browser fills in the rest
//! from the address the console is open at — what the settings page means
//! by "auto-detect from the browser". The server does not guess from
//! request headers (`Host`, `X-Forwarded-*`): what a proxy in front of the
//! console sends says nothing certain about where the gateway is reached.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// The configured parts of the gateway's public address. A part that is
/// `null` is not set: the scheme and host are then those of the address
/// the console is open at, and the port is `gateway_port`. A default port
/// (80 for `http`, 443 for `https`) is left out of the URL.
#[derive(Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct GatewayInfo {
    /// `general.public_protocol`: `"http"` or `"https"`.
    #[schema(example = "https")]
    pub public_protocol: Option<String>,
    /// `general.public_host` as it goes into a URL: a domain name in lower
    /// case, an IPv4 address, or an IPv6 address in brackets. A stored
    /// value that is anything more than a host counts as not set.
    #[schema(example = "gateway.example.com")]
    pub public_host: Option<String>,
    /// `general.public_port`, 1–65535.
    #[schema(example = 443)]
    pub public_port: Option<u16>,
    /// The port the gateway listens on.
    #[schema(example = 3000)]
    pub gateway_port: u16,
}

#[utoipa::path(
    get,
    path = "/api/gateway/info",
    tag = "Gateway",
    responses(
        (status = 200, description = "The configured parts of the gateway's public address", body = GatewayInfo),
        (status = 401, description = "Not signed in"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn get_gateway_info(
    _auth_user: AuthUser,
    State(state): State<AppState>,
) -> Json<GatewayInfo> {
    let dc = &state.dynamic_config;
    let protocol = dc
        .get_string("general.public_protocol")
        .await
        .unwrap_or_default();
    let host = dc
        .get_string("general.public_host")
        .await
        .unwrap_or_default();
    let port = dc.get_i64("general.public_port").await.unwrap_or(0);
    Json(gateway_info(
        &protocol,
        &host,
        port,
        state.config.gateway_port,
    ))
}

/// The answer for `general.public_protocol`, `general.public_host` and
/// `general.public_port` as stored (empty and 0 mean "not set"). A value
/// the settings would refuse today counts as not set: a protocol other
/// than `http` or `https`, a host that is more than a host
/// ([`think_watch_common::validation::bare_host`]), a port outside
/// 1–65535.
fn gateway_info(protocol: &str, host: &str, port: i64, gateway_port: u16) -> GatewayInfo {
    GatewayInfo {
        public_protocol: matches!(protocol, "http" | "https").then(|| protocol.to_string()),
        public_host: think_watch_common::validation::bare_host(host),
        public_port: u16::try_from(port).ok().filter(|p| *p > 0),
        gateway_port,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_parts_are_given_as_set() {
        assert_eq!(
            gateway_info("https", "GW.Example.com", 443, 3000),
            GatewayInfo {
                public_protocol: Some("https".into()),
                public_host: Some("gw.example.com".into()),
                public_port: Some(443),
                gateway_port: 3000,
            }
        );
        assert_eq!(
            gateway_info("http", "::1", 8080, 3000)
                .public_host
                .as_deref(),
            Some("[::1]")
        );
    }

    #[test]
    fn what_is_not_set_is_null() {
        assert_eq!(
            gateway_info("", "", 0, 3100),
            GatewayInfo {
                public_protocol: None,
                public_host: None,
                public_port: None,
                gateway_port: 3100,
            }
        );
    }

    #[test]
    fn a_stored_value_the_settings_would_refuse_counts_as_not_set() {
        let info = gateway_info("ws", "gw.example.com:8443", 70_000, 3000);
        assert_eq!(info.public_protocol, None);
        assert_eq!(info.public_host, None);
        assert_eq!(info.public_port, None);
        assert_eq!(gateway_info("https", "", -1, 3000).public_port, None);
        for bad in [
            "user@gw.example.com",
            "gw.example.com/v1",
            "gw example",
            "${GATEWAY_HOST}",
        ] {
            assert_eq!(
                gateway_info("https", bad, 0, 3000).public_host,
                None,
                "{bad}"
            );
        }
    }

    #[test]
    fn the_answer_has_the_four_documented_fields() {
        let body = serde_json::to_value(gateway_info("https", "", 0, 3000)).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "public_protocol": "https",
                "public_host": null,
                "public_port": null,
                "gateway_port": 3000,
            })
        );
    }
}
