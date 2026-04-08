use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;
use think_watch_common::models::LogForwarder;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// --- List all forwarders ---

pub async fn list_forwarders(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<LogForwarder>>, AppError> {
    auth_user.require_permission("log_forwarders:read")?;
    let forwarders =
        sqlx::query_as::<_, LogForwarder>("SELECT * FROM log_forwarders ORDER BY created_at DESC")
            .fetch_all(&state.db)
            .await?;

    Ok(Json(forwarders))
}

// --- Create forwarder ---

#[derive(Debug, Deserialize)]
pub struct CreateForwarderRequest {
    pub name: String,
    pub forwarder_type: String,
    pub config: serde_json::Value,
    pub enabled: Option<bool>,
    pub log_types: Option<Vec<String>>,
}

const VALID_LOG_TYPES: &[&str] = &["access", "app", "audit", "gateway", "mcp", "platform"];

pub async fn create_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateForwarderRequest>,
) -> Result<Json<LogForwarder>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    let allowed_types = ["udp_syslog", "tcp_syslog", "kafka", "webhook"];
    if !allowed_types.contains(&req.forwarder_type.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid forwarder_type '{}'. Allowed: {}",
            req.forwarder_type,
            allowed_types.join(", ")
        )));
    }

    validate_forwarder_config(&req.forwarder_type, &req.config)?;

    let log_types = req.log_types.unwrap_or_else(|| vec!["audit".into()]);
    for lt in &log_types {
        if !VALID_LOG_TYPES.contains(&lt.as_str()) {
            return Err(AppError::BadRequest(format!(
                "Invalid log_type '{}'. Allowed: {}",
                lt,
                VALID_LOG_TYPES.join(", ")
            )));
        }
    }

    let enabled = req.enabled.unwrap_or(true);
    let forwarder = sqlx::query_as::<_, LogForwarder>(
        r#"INSERT INTO log_forwarders (name, forwarder_type, config, enabled, log_types)
           VALUES ($1, $2, $3, $4, $5) RETURNING *"#,
    )
    .bind(&req.name)
    .bind(&req.forwarder_type)
    .bind(&req.config)
    .bind(enabled)
    .bind(&log_types)
    .fetch_one(&state.db)
    .await?;

    state.audit.reload_forwarders().await;

    state.audit.log(
        think_watch_common::audit::AuditEntry::new("log_forwarder.created")
            .user_id(auth_user.claims.sub)
            .resource(format!("log_forwarder:{}", forwarder.id)),
    );

    Ok(Json(forwarder))
}

// --- Update forwarder ---

#[derive(Debug, Deserialize)]
pub struct UpdateForwarderRequest {
    pub name: Option<String>,
    pub config: Option<serde_json::Value>,
    pub enabled: Option<bool>,
    pub log_types: Option<Vec<String>>,
}

pub async fn update_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateForwarderRequest>,
) -> Result<Json<LogForwarder>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    let existing = sqlx::query_as::<_, LogForwarder>("SELECT * FROM log_forwarders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Forwarder not found".into()))?;

    if let Some(ref config) = req.config {
        validate_forwarder_config(&existing.forwarder_type, config)?;
    }

    let log_types = if let Some(ref lts) = req.log_types {
        for lt in lts {
            if !VALID_LOG_TYPES.contains(&lt.as_str()) {
                return Err(AppError::BadRequest(format!(
                    "Invalid log_type '{}'. Allowed: {}",
                    lt,
                    VALID_LOG_TYPES.join(", ")
                )));
            }
        }
        lts.clone()
    } else {
        existing.log_types.clone()
    };

    let name = req.name.as_deref().unwrap_or(&existing.name);
    let config = req.config.as_ref().unwrap_or(&existing.config);
    let enabled = req.enabled.unwrap_or(existing.enabled);

    let updated = sqlx::query_as::<_, LogForwarder>(
        r#"UPDATE log_forwarders SET name = $2, config = $3, enabled = $4, log_types = $5, updated_at = now()
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(name)
    .bind(config)
    .bind(enabled)
    .bind(&log_types)
    .fetch_one(&state.db)
    .await?;

    state.audit.reload_forwarders().await;

    Ok(Json(updated))
}

// --- Delete forwarder ---

pub async fn delete_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    let result = sqlx::query("DELETE FROM log_forwarders WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Forwarder not found".into()));
    }

    state.audit.reload_forwarders().await;

    state.audit.log(
        think_watch_common::audit::AuditEntry::new("log_forwarder.deleted")
            .user_id(auth_user.claims.sub)
            .resource(format!("log_forwarder:{id}")),
    );

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// --- Toggle (pause / resume) ---

pub async fn toggle_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<LogForwarder>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    let updated = sqlx::query_as::<_, LogForwarder>(
        r#"UPDATE log_forwarders SET enabled = NOT enabled, updated_at = now()
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Forwarder not found".into()))?;

    state.audit.reload_forwarders().await;

    let action = if updated.enabled {
        "log_forwarder.resumed"
    } else {
        "log_forwarder.paused"
    };
    state.audit.log(
        think_watch_common::audit::AuditEntry::new(action)
            .user_id(auth_user.claims.sub)
            .resource(format!("log_forwarder:{id}")),
    );

    Ok(Json(updated))
}

// --- Reset stats ---

pub async fn reset_stats(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<LogForwarder>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    let updated = sqlx::query_as::<_, LogForwarder>(
        r#"UPDATE log_forwarders SET sent_count = 0, error_count = 0, last_error = NULL, updated_at = now()
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Forwarder not found".into()))?;

    Ok(Json(updated))
}

// --- Test forwarder (send a test entry) ---

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub success: bool,
    pub message: String,
}

pub async fn test_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<TestResult>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    let forwarder = sqlx::query_as::<_, LogForwarder>("SELECT * FROM log_forwarders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Forwarder not found".into()))?;

    let test_entry = think_watch_common::audit::AuditEntry::new("log_forwarder.test")
        .user_id(auth_user.claims.sub)
        .resource(format!("log_forwarder:{id}"));

    // Send test message and report result
    let http_client = reqwest::Client::new();
    let result: Result<(), String> = match forwarder.forwarder_type.as_str() {
        "udp_syslog" | "tcp_syslog" => {
            let addr = match forwarder.config.get("address").and_then(|v| v.as_str()) {
                Some(a) => a.to_string(),
                None => {
                    return Ok(Json(TestResult {
                        success: false,
                        message: "Missing 'address' in config".into(),
                    }));
                }
            };
            let facility: u8 = forwarder
                .config
                .get("facility")
                .and_then(|v| v.as_u64())
                .and_then(|v| u8::try_from(v).ok())
                .unwrap_or(16);
            let priority = facility * 8 + 6u8;
            let msg = format!(
                "<{}>1 {} think-watch audit - {} [audit@0 test=\"true\"] test message\n",
                priority, &test_entry.created_at, test_entry.action,
            );
            if forwarder.forwarder_type == "udp_syslog" {
                match std::net::UdpSocket::bind("0.0.0.0:0") {
                    Ok(socket) => socket
                        .send_to(msg.as_bytes(), &addr)
                        .map(|_| ())
                        .map_err(|e| format!("UDP send failed: {e}")),
                    Err(e) => Err(format!("Failed to bind UDP socket: {e}")),
                }
            } else {
                match tokio::net::TcpStream::connect(&addr).await {
                    Ok(mut stream) => {
                        tokio::io::AsyncWriteExt::write_all(&mut stream, msg.as_bytes())
                            .await
                            .map_err(|e| format!("TCP write failed: {e}"))
                    }
                    Err(e) => Err(format!("TCP connect failed: {e}")),
                }
            }
        }
        "webhook" => {
            let url = match forwarder.config.get("url").and_then(|v| v.as_str()) {
                Some(u) => u.to_string(),
                None => {
                    return Ok(Json(TestResult {
                        success: false,
                        message: "Missing 'url' in config".into(),
                    }));
                }
            };
            let mut req = http_client
                .post(&url)
                .header("Content-Type", "application/json")
                .json(&test_entry);
            if let Some(token) = forwarder.config.get("auth_header").and_then(|v| v.as_str()) {
                req = req.header("Authorization", token);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => Ok(()),
                Ok(resp) => Err(format!("HTTP {}", resp.status())),
                Err(e) => Err(format!("{e}")),
            }
        }
        "kafka" => {
            let broker_url = match forwarder.config.get("broker_url").and_then(|v| v.as_str()) {
                Some(u) => u.to_string(),
                None => {
                    return Ok(Json(TestResult {
                        success: false,
                        message: "Missing 'broker_url' in config".into(),
                    }));
                }
            };
            let topic = match forwarder.config.get("topic").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => {
                    return Ok(Json(TestResult {
                        success: false,
                        message: "Missing 'topic' in config".into(),
                    }));
                }
            };
            let payload = serde_json::json!({"records": [{"value": &test_entry}]});
            let url = format!("{}/topics/{}", broker_url.trim_end_matches('/'), topic);
            match http_client
                .post(&url)
                .header("Content-Type", "application/vnd.kafka.json.v2+json")
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => Ok(()),
                Ok(resp) => Err(format!("HTTP {}", resp.status())),
                Err(e) => Err(format!("{e}")),
            }
        }
        _ => Err("Unknown forwarder type".into()),
    };

    match result {
        Ok(()) => Ok(Json(TestResult {
            success: true,
            message: "Test message sent successfully".into(),
        })),
        Err(msg) => Ok(Json(TestResult {
            success: false,
            message: msg,
        })),
    }
}

// --- Validation ---

fn validate_forwarder_config(
    forwarder_type: &str,
    config: &serde_json::Value,
) -> Result<(), AppError> {
    match forwarder_type {
        "udp_syslog" | "tcp_syslog" => {
            if config.get("address").and_then(|v| v.as_str()).is_none() {
                return Err(AppError::BadRequest(
                    "Syslog config requires 'address' field (e.g. \"127.0.0.1:514\")".into(),
                ));
            }
        }
        "kafka" => {
            if config.get("broker_url").and_then(|v| v.as_str()).is_none() {
                return Err(AppError::BadRequest(
                    "Kafka config requires 'broker_url' field".into(),
                ));
            }
            if config.get("topic").and_then(|v| v.as_str()).is_none() {
                return Err(AppError::BadRequest(
                    "Kafka config requires 'topic' field".into(),
                ));
            }
        }
        "webhook" => {
            if config.get("url").and_then(|v| v.as_str()).is_none() {
                return Err(AppError::BadRequest(
                    "Webhook config requires 'url' field".into(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}
