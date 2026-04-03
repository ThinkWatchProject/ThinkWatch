use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use agent_bastion_common::crypto;
use agent_bastion_common::dto::CreateProviderRequest;
use agent_bastion_common::errors::AppError;
use agent_bastion_common::models::Provider;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

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

    // Resolve DNS and check all resolved IPs to prevent DNS rebinding
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&(host, 80)) {
        for addr in addrs {
            let ip = addr.ip();
            if ip.is_loopback() || ip.is_unspecified() || is_private_ip(&ip) || is_link_local(&ip) {
                return Err(AppError::BadRequest(
                    "URL resolves to a private or loopback address".into(),
                ));
            }
        }
    }

    // Also block obviously private hostnames even if DNS fails
    if let Ok(ip) = host.parse::<std::net::IpAddr>()
        && (ip.is_loopback() || ip.is_unspecified() || is_private_ip(&ip) || is_link_local(&ip))
    {
        return Err(AppError::BadRequest("URL points to private network".into()));
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
    _auth_user: AuthUser,
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

    let provider = sqlx::query_as::<_, Provider>(
        r#"INSERT INTO providers (name, display_name, provider_type, base_url, api_key_encrypted, config_json)
           VALUES ($1, $2, $3, $4, $5, $6) RETURNING *"#,
    )
    .bind(&req.name)
    .bind(&req.display_name)
    .bind(&req.provider_type)
    .bind(&req.base_url)
    .bind(&encrypted_key)
    .bind(req.config.unwrap_or(serde_json::json!({})))
    .fetch_one(&state.db)
    .await?;

    Ok(Json(provider))
}

#[derive(Debug, serde::Deserialize)]
pub struct UpdateProviderRequest {
    pub display_name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

pub async fn update_provider(
    _auth_user: AuthUser,
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

    let updated = sqlx::query_as::<_, Provider>(
        r#"UPDATE providers SET display_name = $2, base_url = $3, api_key_encrypted = $4
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(display_name)
    .bind(base_url)
    .bind(&api_key_encrypted)
    .fetch_one(&state.db)
    .await?;

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
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    sqlx::query("UPDATE providers SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .execute(&state.db)
        .await?;

    Ok(Json(serde_json::json!({"status": "deleted"})))
}
