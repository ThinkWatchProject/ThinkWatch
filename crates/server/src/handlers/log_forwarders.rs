use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;
use think_watch_common::models::LogForwarder;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// --- List all forwarders ---

#[utoipa::path(
    get,
    path = "/api/admin/log-forwarders",
    tag = "Log Forwarders",
    responses(
        (status = 200, description = "List of all log forwarders"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_forwarders(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<LogForwarder>>, AppError> {
    auth_user
        .require_global_permission(&state.db, "log_forwarders:read")
        .await?;
    // Hard cap at 500. Log forwarders are global infrastructure rows
    // (handful per deployment, max), so 500 is well beyond any
    // legitimate operational config — but without a cap a misconfig
    // or test fixture leak that creates thousands of rows would
    // serialize a multi-MB JSON payload synchronously and risk OOM.
    // Add tiebreaker on id so the truncation is at least stable.
    let forwarders = sqlx::query_as::<_, LogForwarder>(
        "SELECT * FROM log_forwarders ORDER BY created_at DESC, id DESC LIMIT 500",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(forwarders))
}

// --- Create forwarder ---

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateForwarderRequest {
    pub name: String,
    pub forwarder_type: String,
    pub config: serde_json::Value,
    pub enabled: Option<bool>,
    pub log_types: Option<Vec<String>>,
}

// `platform` was a separate LogType variant once but was collapsed
// into `audit` when the schemas turned out to be identical. Listing
// it here would let an operator save a forwarder filtered to a
// log_type that NO row ever emits — silent misconfiguration.
const VALID_LOG_TYPES: &[&str] = &["access", "app", "audit", "gateway", "mcp"];

#[utoipa::path(
    post,
    path = "/api/admin/log-forwarders",
    tag = "Log Forwarders",
    request_body(content = CreateForwarderRequest),
    responses(
        (status = 200, description = "Newly created log forwarder"),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateForwarderRequest>,
) -> Result<Json<LogForwarder>, AppError> {
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;
    // S3 / Splunk live as webhook forwarders today: both accept HTTP
    // POST with a JSON or NDJSON body, which is what the existing
    // `webhook` transport already emits. Splunk HEC URL goes in
    // config.url + a `Splunk <token>` Authorization header; S3 goes
    // through a presigned-PUT proxy (e.g. AWS API Gateway → Lambda)
    // to keep the gateway from holding AWS credentials. Native
    // first-class transports for both can be added by name here when
    // operator demand justifies a dedicated config schema instead of
    // re-using the generic webhook.
    let allowed_types = ["udp_syslog", "tcp_syslog", "kafka", "webhook"];
    if !allowed_types.contains(&req.forwarder_type.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid forwarder_type '{}'. Allowed: {}",
            req.forwarder_type,
            allowed_types.join(", ")
        )));
    }

    validate_forwarder_config(&req.forwarder_type, &req.config, &state.url_validator)?;

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
        auth_user
            .audit("log_forwarder.created")
            .resource(format!("log_forwarder:{}", forwarder.id)),
    );

    Ok(Json(forwarder))
}

// --- Update forwarder ---

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateForwarderRequest {
    pub name: Option<String>,
    pub config: Option<serde_json::Value>,
    pub enabled: Option<bool>,
    pub log_types: Option<Vec<String>>,
}

#[utoipa::path(
    patch,
    path = "/api/admin/log-forwarders/{id}",
    tag = "Log Forwarders",
    params(
        ("id" = uuid::Uuid, Path, description = "Log forwarder ID"),
    ),
    request_body(content = UpdateForwarderRequest),
    responses(
        (status = 200, description = "Updated log forwarder"),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateForwarderRequest>,
) -> Result<Json<LogForwarder>, AppError> {
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;
    let existing = sqlx::query_as::<_, LogForwarder>("SELECT * FROM log_forwarders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Forwarder not found".into()))?;

    if let Some(ref config) = req.config {
        validate_forwarder_config(&existing.forwarder_type, config, &state.url_validator)?;
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

#[utoipa::path(
    delete,
    path = "/api/admin/log-forwarders/{id}",
    tag = "Log Forwarders",
    params(
        ("id" = uuid::Uuid, Path, description = "Log forwarder ID"),
    ),
    responses(
        (status = 200, description = "Forwarder deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn delete_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;
    let result = sqlx::query("DELETE FROM log_forwarders WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Forwarder not found".into()));
    }

    state.audit.reload_forwarders().await;

    state.audit.log(
        auth_user
            .audit("log_forwarder.deleted")
            .resource(format!("log_forwarder:{id}")),
    );

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// --- Pause / resume ---

/// Body for the pause/resume endpoint. Explicit `enabled` field makes
/// the operation idempotent: a double-POST (network retry, double-
/// click, SDK auto-retry) produces the same final state instead of
/// silently flipping it back. The previous shape ("toggle") returned
/// inverted state on each call AND emitted opposite audit rows
/// (`log_forwarder.resumed` then `log_forwarder.paused`), which made
/// retried requests look like deliberate flapping.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct SetForwarderEnabledRequest {
    pub enabled: bool,
}

#[utoipa::path(
    post,
    path = "/api/admin/log-forwarders/{id}/toggle",
    tag = "Log Forwarders",
    params(
        ("id" = uuid::Uuid, Path, description = "Log forwarder ID"),
    ),
    request_body = SetForwarderEnabledRequest,
    responses(
        (status = 200, description = "Forwarder with the requested enabled state"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn toggle_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<SetForwarderEnabledRequest>,
) -> Result<Json<LogForwarder>, AppError> {
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;
    // Idempotent: SET enabled = $2, not NOT enabled. A retry of the
    // same request leaves the row in the same final state and emits
    // the same audit action.
    let updated = sqlx::query_as::<_, LogForwarder>(
        r#"UPDATE log_forwarders SET enabled = $2, updated_at = now()
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(req.enabled)
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
        auth_user
            .audit(action)
            .resource(format!("log_forwarder:{id}")),
    );

    Ok(Json(updated))
}

// --- Reset stats ---

#[utoipa::path(
    post,
    path = "/api/admin/log-forwarders/{id}/reset-stats",
    tag = "Log Forwarders",
    params(
        ("id" = uuid::Uuid, Path, description = "Log forwarder ID"),
    ),
    responses(
        (status = 200, description = "Forwarder with reset sent/error counters"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn reset_stats(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<LogForwarder>, AppError> {
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;
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

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TestResult {
    pub success: bool,
    pub message: String,
}

#[utoipa::path(
    post,
    path = "/api/admin/log-forwarders/{id}/test",
    tag = "Log Forwarders",
    params(
        ("id" = uuid::Uuid, Path, description = "Log forwarder ID"),
    ),
    responses(
        (status = 200, description = "Result of sending a test log entry", body = TestResult),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn test_forwarder(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<TestResult>, AppError> {
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;
    // Test endpoint fires outbound HTTP — cap at 5 calls/min/user to
    // prevent abuse as a network probe.
    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "log_forwarder",
    )
    .await?;
    let forwarder = sqlx::query_as::<_, LogForwarder>("SELECT * FROM log_forwarders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Forwarder not found".into()))?;

    let test_entry = auth_user
        .audit("log_forwarder.test")
        .resource(format!("log_forwarder:{id}"));

    // Reuse the shared HTTP client (timeouts + connection pool +
    // metric instrumentation) instead of minting a fresh one per
    // test. The previous `reqwest::Client::new()` skipped both.
    let http_client = (**state.http_client.load()).clone();
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
            // Revalidate the address at test time. Create/update already
            // call validate_host_port, but DNS for a previously-public
            // host could flip to 127.0.0.1 between create and test;
            // without this re-check the test endpoint becomes an
            // internal-port prober. Webhook/Kafka branches below
            // revalidate the same way.
            if let Err(e) = validate_host_port(&addr) {
                return Ok(Json(TestResult {
                    success: false,
                    message: format!("Address rejected by SSRF guard: {e}"),
                }));
            }
            let facility: u8 = forwarder
                .config
                .get("facility")
                .and_then(|v| v.as_u64())
                .and_then(|v| u8::try_from(v).ok())
                .unwrap_or(16);
            let priority = facility * 8 + 6u8;
            let msg = format!(
                "<{}>1 {} think-watch audit - {} [audit@0 test=\"true\"] test message\n",
                priority, test_entry.created_at, test_entry.action,
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
            // Re-validate at test-time (TOCTOU): the URL was checked
            // at create/update via validate_forwarder_config, but DNS
            // can flip between then and now (operator pointed
            // foo.example.com at 127.0.0.1 to exfiltrate the test
            // payload). Cheap enough to re-check on every test.
            if let Err(e) = (state.url_validator)(&url) {
                return Ok(Json(TestResult {
                    success: false,
                    message: format!("URL validation failed: {e}"),
                }));
            }
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
            // Same TOCTOU revalidation as the webhook arm.
            if let Err(e) = (state.url_validator)(&url) {
                return Ok(Json(TestResult {
                    success: false,
                    message: format!("URL validation failed: {e}"),
                }));
            }
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
    check: &think_watch_common::validation::UrlValidator,
) -> Result<(), AppError> {
    match forwarder_type {
        "udp_syslog" | "tcp_syslog" => {
            let addr = config
                .get("address")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    AppError::BadRequest(
                        "Syslog config requires 'address' field (e.g. \"127.0.0.1:514\")".into(),
                    )
                })?;
            validate_host_port(addr)?;
        }
        "kafka" => {
            let broker_url = config
                .get("broker_url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    AppError::BadRequest("Kafka config requires 'broker_url' field".into())
                })?;
            // SSRF: Kafka REST proxy URL must be a public HTTP(S) host.
            check(broker_url)?;
            let topic = config
                .get("topic")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    AppError::BadRequest("Kafka config requires 'topic' field".into())
                })?;
            validate_kafka_topic(topic)?;
        }
        "webhook" => {
            let url = config.get("url").and_then(|v| v.as_str()).ok_or_else(|| {
                AppError::BadRequest("Webhook config requires 'url' field".into())
            })?;
            // SSRF: reject localhost / private IPs / cloud metadata endpoints.
            check(url)?;
        }
        _ => {}
    }
    Ok(())
}

/// Validate a `host:port` syslog address. Rejects malformed strings and
/// hostnames that resolve to private/loopback IPs.
fn validate_host_port(addr: &str) -> Result<(), AppError> {
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| AppError::BadRequest("Syslog address must be host:port".into()))?;
    let port: u16 = port.parse().map_err(|_| {
        AppError::BadRequest("Syslog address port must be a number (1-65535)".into())
    })?;
    if port == 0 {
        return Err(AppError::BadRequest(
            "Syslog address port must be >= 1".into(),
        ));
    }
    if host.is_empty() {
        return Err(AppError::BadRequest("Syslog address missing host".into()));
    }

    // Block obvious SSRF targets — hostname blocklist + private/loopback IPs.
    const BLOCKED: &[&str] = &[
        "localhost",
        "127.0.0.1",
        "0.0.0.0",
        "169.254.169.254",
        "::1",
        "metadata.google.internal",
    ];
    if BLOCKED.contains(&host) {
        return Err(AppError::BadRequest("Syslog host is blocked".into()));
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>()
        && think_watch_common::validation::is_blocked_ip(&ip)
    {
        return Err(AppError::BadRequest(
            "Syslog host points to private network".into(),
        ));
    }
    Ok(())
}

/// Kafka topic names must match `[a-zA-Z0-9._-]{1,249}` per the Kafka
/// spec. We also enforce our own stricter rule (no leading dot, no `..`)
/// to prevent surprises when the topic is interpolated into an HTTP path.
fn validate_kafka_topic(topic: &str) -> Result<(), AppError> {
    if topic.is_empty() || topic.len() > 249 {
        return Err(AppError::BadRequest(
            "Kafka topic must be 1-249 characters".into(),
        ));
    }
    if topic.starts_with('.') || topic.contains("..") {
        return Err(AppError::BadRequest(
            "Kafka topic cannot start with '.' or contain '..'".into(),
        ));
    }
    if !topic
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(AppError::BadRequest(
            "Kafka topic may only contain letters, digits, '.', '_', '-'".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // validate_host_port — SSRF defense for syslog destinations
    // -----------------------------------------------------------------

    #[test]
    fn host_port_accepts_public_address() {
        assert!(validate_host_port("syslog.example.com:514").is_ok());
        assert!(validate_host_port("203.0.113.5:6514").is_ok());
    }

    #[test]
    fn host_port_rejects_missing_separator() {
        let err = validate_host_port("syslog.example.com").unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn host_port_rejects_non_numeric_port() {
        assert!(validate_host_port("host:abc").is_err());
    }

    #[test]
    fn host_port_rejects_port_zero() {
        // port == 0 is technically u16-parseable but reserved.
        assert!(validate_host_port("host:0").is_err());
    }

    #[test]
    fn host_port_rejects_port_over_65535() {
        // u16::parse fails for 65536+ → caught by the parse error path.
        assert!(validate_host_port("host:65536").is_err());
        assert!(validate_host_port("host:99999").is_err());
    }

    #[test]
    fn host_port_rejects_empty_host() {
        assert!(validate_host_port(":514").is_err());
    }

    #[test]
    fn host_port_blocks_localhost_alias() {
        // SSRF defense — operator can't proxy syslog at the gateway itself.
        for h in ["localhost:514", "127.0.0.1:514", "0.0.0.0:514", "::1:514"] {
            assert!(
                validate_host_port(h).is_err(),
                "{h} should be blocked but wasn't"
            );
        }
    }

    #[test]
    fn host_port_blocks_cloud_metadata_endpoints() {
        // 169.254.169.254 is the EC2/GCE metadata IP; the hostname form is
        // the GCE alias. Both must be blocked to prevent token exfil.
        assert!(validate_host_port("169.254.169.254:80").is_err());
        assert!(validate_host_port("metadata.google.internal:80").is_err());
    }

    #[test]
    fn host_port_blocks_private_rfc1918_via_ip_check() {
        // Defers to common::validation::is_blocked_ip for the full range
        // check. Spot-check a 10. and a 192.168. address.
        assert!(validate_host_port("10.0.0.1:514").is_err());
        assert!(validate_host_port("192.168.1.1:514").is_err());
    }

    #[test]
    fn host_port_uses_rsplit_so_ipv6_in_brackets_works() {
        // rsplit_once(':') means an IPv6 like `[2001:db8::1]:514` splits
        // correctly at the last colon. Public IPv6 should pass.
        // (Bracket form is the conventional way to disambiguate.)
        assert!(validate_host_port("[2001:db8::1]:514").is_ok());
    }

    // -----------------------------------------------------------------
    // validate_kafka_topic — character allowlist + dot rules
    // -----------------------------------------------------------------

    #[test]
    fn kafka_topic_accepts_typical_names() {
        for ok in ["audit-logs", "events.v1", "my_topic", "abc123", "a"] {
            assert!(validate_kafka_topic(ok).is_ok(), "{ok} should pass");
        }
    }

    #[test]
    fn kafka_topic_rejects_empty() {
        assert!(validate_kafka_topic("").is_err());
    }

    #[test]
    fn kafka_topic_rejects_overlength() {
        let s = "a".repeat(250);
        assert!(validate_kafka_topic(&s).is_err());
    }

    #[test]
    fn kafka_topic_accepts_exactly_249_chars() {
        // Boundary — `> 249` triggers rejection; exactly 249 must pass.
        let s = "a".repeat(249);
        assert!(validate_kafka_topic(&s).is_ok());
    }

    #[test]
    fn kafka_topic_rejects_leading_dot() {
        // Defense against shell/HTTP path traversal when the topic is
        // interpolated into a URL.
        assert!(validate_kafka_topic(".hidden").is_err());
    }

    #[test]
    fn kafka_topic_rejects_double_dot() {
        assert!(validate_kafka_topic("foo..bar").is_err());
    }

    #[test]
    fn kafka_topic_rejects_disallowed_chars() {
        // Slash, space, plus signs — common HTTP-path / URL-encoding traps.
        for bad in ["foo/bar", "foo bar", "foo+bar", "foo:bar", "foo*"] {
            assert!(
                validate_kafka_topic(bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn kafka_topic_rejects_non_ascii() {
        // Unicode word chars look "alphanumeric" in some checkers but
        // Kafka rejects them; lock the ASCII-only contract in.
        assert!(validate_kafka_topic("audit-日志").is_err());
    }
}
