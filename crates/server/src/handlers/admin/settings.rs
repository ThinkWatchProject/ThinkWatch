//! System info + audit config + generic settings CRUD + the
//! `update_settings` PATCH handler that drives the dynamic-config
//! store. The retention-related helpers it depends on
//! (`apply_clickhouse_ttls`, `MAX_RETENTION_DAYS`) live next door
//! in `super::retention`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use think_watch_common::dynamic_config::{self, SettingEntry};
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::retention::{MAX_RETENTION_DAYS, apply_blob_lifecycle, apply_clickhouse_ttls};

// --- System settings ---

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SystemInfo {
    pub version: String,
    pub uptime: String,
    pub rust_version: String,
    pub server_host: String,
    pub gateway_port: u16,
    pub console_port: u16,
    /// Configured public protocol ("", "http", or "https"). Empty means auto-detect.
    pub public_protocol: String,
    /// Configured public host. Empty means auto-detect from browser.
    pub public_host: String,
    /// Configured public port. 0 means use the gateway listening port.
    pub public_port: i64,
}

fn format_uptime(dur: chrono::TimeDelta) -> String {
    let secs = dur.num_seconds();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

#[utoipa::path(
    get,
    path = "/api/admin/settings/system",
    tag = "Settings",
    responses(
        (status = 200, description = "System info", body = SystemInfo),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn get_system_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<SystemInfo>, AppError> {
    auth_user
        .require_global_permission(&state.db, "settings:read")
        .await?;
    let uptime = chrono::Utc::now() - state.started_at;
    let dc = &state.dynamic_config;
    Ok(Json(SystemInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime: format_uptime(uptime),
        rust_version: env!("RUSTC_VERSION").to_string(),
        server_host: state.config.server_host.clone(),
        gateway_port: state.config.gateway_port,
        console_port: state.config.console_port,
        public_protocol: dc
            .get_string("general.public_protocol")
            .await
            .unwrap_or_default(),
        public_host: dc
            .get_string("general.public_host")
            .await
            .unwrap_or_default(),
        public_port: dc.get_i64("general.public_port").await.unwrap_or(0),
    }))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AuditConfigResponse {
    pub clickhouse_url: Option<String>,
    pub clickhouse_db: String,
    pub connected: bool,
}

#[utoipa::path(
    get,
    path = "/api/admin/settings/audit",
    tag = "Settings",
    responses(
        (status = 200, description = "Audit/ClickHouse configuration", body = AuditConfigResponse),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn get_audit_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<AuditConfigResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "settings:read")
        .await?;
    let connected = if let Some(ref ch) = state.clickhouse {
        ch.query("SELECT 1").fetch_one::<u8>().await.is_ok()
    } else {
        false
    };
    Ok(Json(AuditConfigResponse {
        clickhouse_url: state
            .config
            .clickhouse_url
            .as_deref()
            .and_then(redact_url_userinfo),
        clickhouse_db: state.config.clickhouse_db.clone(),
        connected,
    }))
}

/// Strip `user[:password]@` from a URL so admins viewing audit config
/// don't see embedded ClickHouse credentials (which leak through browser
/// history + screenshots).
///
/// - Returns the original verbatim when the URL has no userinfo to strip,
///   or doesn't parse as a URL at all (the alternative — dropping the
///   field — is worse for diagnostics).
/// - Returns `None` when the URL DOES have userinfo but the `url` crate
///   refuses to strip it (e.g. cannot-be-base schemes like `mailto:` —
///   not a realistic ClickHouse URL, but if it ever appears, dropping
///   the field is strictly safer than returning a half-redacted URL
///   that still leaks the password).
fn redact_url_userinfo(raw: &str) -> Option<String> {
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return Some(raw.to_string());
    };
    if parsed.username().is_empty() && parsed.password().is_none() {
        return Some(raw.to_string());
    }
    if parsed.set_username("").is_err() || parsed.set_password(None).is_err() {
        tracing::warn!(
            "clickhouse_url contains credentials but its scheme refuses userinfo \
             mutation; dropping the field from the audit-settings response"
        );
        return None;
    }
    Some(parsed.to_string())
}

#[cfg(test)]
mod redact_url_tests {
    use super::redact_url_userinfo;

    #[test]
    fn strips_user_and_password() {
        assert_eq!(
            redact_url_userinfo("http://user:pw@host:9000/db").as_deref(),
            Some("http://host:9000/db"),
        );
    }

    #[test]
    fn passes_through_clean_url() {
        assert_eq!(
            redact_url_userinfo("http://host:9000/db").as_deref(),
            Some("http://host:9000/db"),
        );
    }

    #[test]
    fn passes_through_unparseable() {
        assert_eq!(
            redact_url_userinfo("not a url").as_deref(),
            Some("not a url"),
        );
    }
}

// --- Dynamic settings CRUD ---

/// GET /api/admin/settings — return all settings grouped by category.
#[utoipa::path(
    get,
    path = "/api/admin/settings",
    tag = "Settings",
    responses(
        (status = 200, description = "All settings grouped by category"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn get_all_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<HashMap<String, Vec<SettingEntry>>>, AppError> {
    auth_user
        .require_global_permission(&state.db, "settings:read")
        .await?;
    let grouped = state.dynamic_config.get_all_grouped().await;
    Ok(Json(grouped))
}

/// GET /api/admin/settings/category/{category} — return settings for a specific category.
#[utoipa::path(
    get,
    path = "/api/admin/settings/category/{category}",
    tag = "Settings",
    params(
        ("category" = String, Path, description = "Settings category name"),
    ),
    responses(
        (status = 200, description = "Settings for the given category"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn get_settings_by_category(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(category): Path<String>,
) -> Result<Json<Vec<SettingEntry>>, AppError> {
    auth_user
        .require_global_permission(&state.db, "settings:read")
        .await?;
    let settings = state.dynamic_config.get_by_category(&category).await;
    Ok(Json(settings))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateSettingsRequest {
    pub settings: HashMap<String, serde_json::Value>,
}

/// PATCH /api/admin/settings — update one or more settings.
#[utoipa::path(
    patch,
    path = "/api/admin/settings",
    tag = "Settings",
    request_body = UpdateSettingsRequest,
    responses(
        (status = 200, description = "Settings updated"),
        (status = 400, description = "Validation error"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn update_settings(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user
        .require_global_permission(&state.db, "settings:write")
        .await?;
    // Validate each setting
    for (key, value) in &req.settings {
        validate_setting(key, value)?;
    }

    // DB-level validation for settings that reference other entities
    if let Some(role_val) = req.settings.get("auth.default_role") {
        let role_name = role_val.as_str().unwrap_or("");
        if !role_name.is_empty() {
            let exists: Option<(String,)> =
                sqlx::query_as("SELECT name FROM rbac_roles WHERE name = $1")
                    .bind(role_name)
                    .fetch_optional(&state.db)
                    .await?;
            if exists.is_none() {
                return Err(AppError::BadRequest(format!(
                    "Role '{role_name}' does not exist"
                )));
            }
        }
    }

    state
        .dynamic_config
        .update(&req.settings, Some(auth_user.claims.sub))
        .await
        .map_err(AppError::Internal)?;

    // Hot-reload content filter / PII redactor immediately on this instance
    // (other instances pick it up via the Redis Pub/Sub subscriber).
    if req
        .settings
        .contains_key("security.content_filter_patterns")
    {
        let cf = crate::app::load_content_filter(&state.dynamic_config).await;
        state.content_filter.store(std::sync::Arc::new(cf));
    }
    if req.settings.contains_key("security.pii_redactor_patterns") {
        let pii = crate::app::load_pii_redactor(&state.dynamic_config).await;
        state.pii_redactor.store(std::sync::Arc::new(pii));
    }
    if req.settings.contains_key("security.tool_inspection") {
        let tools = crate::app::load_tool_inspection(&state.dynamic_config).await;
        state.tool_inspection.store(std::sync::Arc::new(tools));
    }

    // Apply ClickHouse TTL changes for any retention setting that was updated.
    // ClickHouse runs the cleanup asynchronously in its merge worker, so this
    // returns immediately.
    apply_clickhouse_ttls(&state, &req.settings).await;
    // Push the bucket lifecycle rule to the blob store when the
    // operator touched it. No-op when the setting isn't in `req` and
    // when blob-store offload isn't configured.
    apply_blob_lifecycle(&state, &req.settings).await;

    // Notify other instances via Redis Pub/Sub
    dynamic_config::notify_config_changed(&state.redis).await;

    state.audit.log(
        auth_user
            .audit("settings.update")
            .resource("system_settings")
            .detail(serde_json::json!({
                "keys": req.settings.keys().collect::<Vec<_>>(),
            })),
    );

    Ok(Json(
        serde_json::json!({"status": "updated", "count": req.settings.len()}),
    ))
}
/// Validate a setting value based on its key.
fn validate_setting(key: &str, value: &serde_json::Value) -> Result<(), AppError> {
    // Per-key reasonable upper bounds for positive integer settings.
    // Without these, an admin (or a script with admin creds) can store
    // `i64::MAX` and cause downstream allocation explosions
    // (`audit.channel_capacity` is a Vec preallocation), absurd
    // session lifetimes, or denial-of-service via "infinite" timeouts
    // that pin server threads. Maxima are chosen to be larger than any
    // legitimate operational value while bounded enough that overflow
    // and OOM are impossible. If a real workload bumps against one,
    // raise it deliberately rather than removing the cap.
    fn positive_int_bound(key: &str) -> Option<i64> {
        Some(match key {
            "auth.jwt_access_ttl_secs" => 86_400,            // 1 day
            "auth.jwt_refresh_ttl_days" => 365,              // 1 year
            "gateway.cache_ttl_secs" => 86_400,              // 1 day
            "gateway.request_timeout_secs" => 600,           // 10 min
            "gateway.body_limit_bytes" => 100 * 1024 * 1024, // 100 MiB
            "console.request_timeout_secs" => 600,
            "console.body_limit_bytes" => 100 * 1024 * 1024,
            "security.signature_nonce_ttl_secs" => 3600, // 1 hour
            "audit.batch_size" => 10_000,                // CH insert sweet spot
            "audit.flush_interval_secs" => 300,          // 5 min
            "audit.channel_capacity" => 1_000_000,       // ~GB-scale memory budget
            // Body-column TTL (ClickHouse) AND its companion bucket
            // lifecycle (S3 / RustFS / MinIO). The PATCH handler in
            // this file previously rejected both with "Unknown
            // setting" because neither had a validation arm — fix
            // them together with the new lifecycle knob. Both share
            // MAX_RETENTION_DAYS as the upper bound for the same
            // reason the data.retention_days_* settings do.
            "audit.body_retention_days" => MAX_RETENTION_DAYS,
            "audit.body_s3_lifecycle_days" => MAX_RETENTION_DAYS,
            "api_keys.rotation_grace_period_hours" => 24 * 30, // 30 days
            _ => return None,
        })
    }

    if let Some(max) = positive_int_bound(key) {
        let v = value
            .as_i64()
            .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
        if !(1..=max).contains(&v) {
            return Err(AppError::BadRequest(format!(
                "{key} must be between 1 and {max}"
            )));
        }
        return Ok(());
    }

    match key {
        // Audit sampling fraction. Lives in the same dynamic-config
        // path as other audit knobs; 0.0 keeps no events (a knob worth
        // having for emergency CH offload), 1.0 keeps everything.
        // The runtime reads this value through audit_sample_rate(),
        // which clamps too — this is just the UI-side reject.
        "audit.sample_rate" => {
            let v = value
                .as_f64()
                .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
                .ok_or_else(|| {
                    AppError::BadRequest(format!("{key} must be a number between 0 and 1"))
                })?;
            if !(0.0..=1.0).contains(&v) {
                return Err(AppError::BadRequest(
                    "audit.sample_rate must be between 0.0 and 1.0".into(),
                ));
            }
        }

        // Request-signature drift tolerance. Upper bound is hard-coded
        // in common::dynamic_config::SIGNATURE_DRIFT_MAX_SECS; the
        // middleware clamps at read time too, but we reject out-of-range
        // writes here so the admin UI surfaces the error immediately.
        "security.signature_drift_secs" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            let max = think_watch_common::dynamic_config::SIGNATURE_DRIFT_MAX_SECS;
            if !(1..=max).contains(&v) {
                return Err(AppError::BadRequest(format!(
                    "security.signature_drift_secs must be between 1 and {max}"
                )));
            }
        }

        // MCP background health-check cadence. Lower bound (5s) prevents
        // a typo from DOSing every registered upstream; upper bound is
        // a day, anything beyond that is effectively "never" — admin
        // should disable the loop entirely if they want that.
        "mcp.health_interval_secs" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(5..=86400).contains(&v) {
                return Err(AppError::BadRequest(
                    "mcp.health_interval_secs must be between 5 and 86400".into(),
                ));
            }
        }

        // MCP client-session TTL.  Lower bound 60s keeps sessions from
        // expiring mid-conversation; upper bound 86400 (1 day) limits
        // stale upstream session accumulation.
        "mcp.session_ttl_secs" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(60..=86400).contains(&v) {
                return Err(AppError::BadRequest(
                    "mcp.session_ttl_secs must be between 60 and 86400".into(),
                ));
            }
        }

        // MCP global response cache TTL. 0 = disabled, up to 86400 (1 day).
        "mcp.cache_ttl_secs" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(0..=86400).contains(&v) {
                return Err(AppError::BadRequest(
                    "mcp.cache_ttl_secs must be between 0 and 86400".into(),
                ));
            }
        }

        // Integer settings that can be 0 (0 = disabled)
        "api_keys.default_expiry_days"
        | "api_keys.inactivity_timeout_days"
        | "api_keys.rotation_period_days"
        | "data.retention_days_audit"
        | "data.retention_days_gateway"
        | "data.retention_days_mcp"
        | "data.retention_days_access"
        | "data.retention_days_app" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(0..=MAX_RETENTION_DAYS).contains(&v) {
                return Err(AppError::BadRequest(format!(
                    "{key} must be between 0 and {MAX_RETENTION_DAYS}"
                )));
            }
        }

        // Boolean settings
        "setup.initialized" => {
            // Only allow setting to true — prevent resetting initialization
            let v = value
                .as_bool()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a boolean")))?;
            if !v {
                return Err(AppError::BadRequest(
                    "Cannot reset setup.initialized to false".into(),
                ));
            }
        }

        "auth.allow_registration" | "security.rate_limit_fail_closed" => {
            if !value.is_boolean() {
                return Err(AppError::BadRequest(format!("{key} must be a boolean")));
            }
        }

        "auth.default_role" => {
            value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
        }

        "mcp_store.registry_url" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            // Block save-time SSRF in addition to the fetch-time
            // check in `sync_registry`: catching it here gives the
            // admin an immediate error instead of a "saved but
            // sync fails" mystery.
            if !s.is_empty() {
                think_watch_common::validation::validate_url(s)?;
            }
        }

        // Client IP resolution
        "security.client_ip_source" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if !["connection", "xff", "x-real-ip"].contains(&s) {
                return Err(AppError::BadRequest(
                    "client_ip_source must be \"connection\", \"xff\", or \"x-real-ip\"".into(),
                ));
            }
        }
        "security.client_ip_xff_position" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if !["left", "right"].contains(&s) {
                return Err(AppError::BadRequest(
                    "client_ip_xff_position must be \"left\" or \"right\"".into(),
                ));
            }
        }
        "security.client_ip_xff_depth" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(1..=20).contains(&v) {
                return Err(AppError::BadRequest(
                    "client_ip_xff_depth must be between 1 and 20".into(),
                ));
            }
        }

        // General — public gateway URL components
        "general.public_protocol" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if !s.is_empty() && s != "http" && s != "https" {
                return Err(AppError::BadRequest(
                    "public_protocol must be \"http\", \"https\", or empty".into(),
                ));
            }
        }
        "general.public_host" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if s.len() > 253 {
                return Err(AppError::BadRequest("public_host too long".into()));
            }
            if s.contains("://") || s.contains('/') {
                return Err(AppError::BadRequest(
                    "public_host must be a hostname only (no scheme or path)".into(),
                ));
            }
        }
        "general.public_port" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(0..=65535).contains(&v) {
                return Err(AppError::BadRequest(
                    "public_port must be between 0 and 65535".into(),
                ));
            }
        }

        // String settings
        "setup.site_name" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if s.is_empty() || s.len() > 100 {
                return Err(AppError::BadRequest(
                    "Site name must be 1-100 characters".into(),
                ));
            }
        }

        // Content filter rules: each rule requires pattern, match_type, action, and name.
        "security.content_filter_patterns" => {
            let arr = value
                .as_array()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a JSON array")))?;
            if arr.len() > 500 {
                return Err(AppError::BadRequest(
                    "Content filter rules: max 500 rules".into(),
                ));
            }
            for (i, item) in arr.iter().enumerate() {
                let pattern = item
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AppError::BadRequest(format!("Rule {i}: missing 'pattern' string"))
                    })?;
                if pattern.len() > 500 {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: pattern max 500 characters"
                    )));
                }
                let match_type =
                    item.get("match_type")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            AppError::BadRequest(format!("Rule {i}: missing 'match_type' field"))
                        })?;
                if !["contains", "regex"].contains(&match_type) {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: match_type must be 'contains' or 'regex'"
                    )));
                }
                let action = item.get("action").and_then(|v| v.as_str()).ok_or_else(|| {
                    AppError::BadRequest(format!("Rule {i}: missing 'action' field"))
                })?;
                if !["block", "warn", "log"].contains(&action) {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: action must be 'block', 'warn', or 'log'"
                    )));
                }
                let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: missing 'name' field"
                    )));
                };
                // The same compile the gateway runs: an empty pattern, a bad
                // or oversized regex is refused here rather than skipped there.
                use tw_guard::content::{Action, Match, Rule, RuleInput};
                if let (Some(matching), Some(action)) =
                    (Match::from_slug(match_type), Action::from_slug(action))
                    && let Err(e) = Rule::new(RuleInput {
                        id: name,
                        name,
                        custom: true,
                        pattern,
                        matching,
                        action,
                    })
                {
                    return Err(AppError::BadRequest(format!("Rule {i}: {}", e.detail)));
                }
            }
        }

        "security.pii_redactor_patterns" => {
            let arr = value
                .as_array()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a JSON array")))?;
            if arr.len() > 100 {
                return Err(AppError::BadRequest(
                    "PII redactor patterns: max 100 rules".into(),
                ));
            }
            for (i, item) in arr.iter().enumerate() {
                let regex_str = item.get("regex").and_then(|v| v.as_str()).ok_or_else(|| {
                    AppError::BadRequest(format!("PII pattern {i}: missing 'regex' string"))
                })?;
                if regex_str.len() > 1000 {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: regex max 1000 characters"
                    )));
                }
                // Compiled exactly as the redactor will compile it, bounds
                // included, so what is saved is what runs.
                if tw_guard::redact::rules::compile("", regex_str).is_err() {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: invalid or oversized regex"
                    )));
                }
                // The prefix lands inside the placeholder (`{{EMAIL_1}}`);
                // a brace or a space there would make one that can never be
                // told apart from ordinary text.
                let prefix = item
                    .get("placeholder_prefix")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AppError::BadRequest(format!(
                            "PII pattern {i}: missing 'placeholder_prefix'"
                        ))
                    })?;
                if prefix.is_empty()
                    || prefix.len() > 32
                    || !prefix
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: 'placeholder_prefix' must be 1-32 letters, digits or underscores"
                    )));
                }
                if item.get("name").and_then(|v| v.as_str()).is_none() {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: missing 'name'"
                    )));
                }
            }
        }

        "security.hidden_text" => {
            serde_json::from_value::<think_watch_gateway::hidden_text::Action>(value.clone())
                .map_err(|_| {
                    AppError::BadRequest(format!(
                        "{key} must be one of \"off\", \"log\", \"warn\", \"block\""
                    ))
                })?;
        }

        "security.tool_inspection" => {
            let cfg: think_watch_gateway::tool_inspection::ToolInspectionConfig =
                serde_json::from_value(value.clone())
                    .map_err(|e| AppError::BadRequest(format!("{key}: {e}")))?;
            if let Some(problem) = cfg.problem() {
                return Err(AppError::BadRequest(format!("{key}: {problem}")));
            }
        }

        "security.budget_alert_webhook_url" => {
            let url = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if !url.is_empty() {
                think_watch_common::validation::validate_url(url)?;
            }
        }

        // ---- Routing strategy + circuit-breaker defaults ----
        "gateway.default_routing_strategy" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if !["weighted", "latency", "health", "latency_health"].contains(&s) {
                return Err(AppError::BadRequest(
                    "default_routing_strategy must be one of: weighted, latency, health, latency_health".into(),
                ));
            }
        }
        "gateway.default_affinity_mode" => {
            let s = value
                .as_str()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a string")))?;
            if !["none", "provider", "route"].contains(&s) {
                return Err(AppError::BadRequest(
                    "default_affinity_mode must be one of: none, provider, route".into(),
                ));
            }
        }
        "gateway.default_affinity_ttl_secs" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(0..=86400).contains(&v) {
                return Err(AppError::BadRequest(
                    "default_affinity_ttl_secs must be between 0 and 86400".into(),
                ));
            }
        }
        "gateway.latency_strategy_k" => {
            let v = value
                .as_f64()
                .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be a number")))?;
            if !(0.5..=8.0).contains(&v) {
                return Err(AppError::BadRequest(
                    "latency_strategy_k must be between 0.5 and 8.0".into(),
                ));
            }
        }
        "gateway.cb_enabled" => {
            if !value.is_boolean() {
                return Err(AppError::BadRequest(format!("{key} must be a boolean")));
            }
        }
        "gateway.cb_error_pct" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(1..=100).contains(&v) {
                return Err(AppError::BadRequest(
                    "cb_error_pct must be between 1 and 100".into(),
                ));
            }
        }
        "gateway.cb_min_samples" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(1..=100_000).contains(&v) {
                return Err(AppError::BadRequest(
                    "cb_min_samples must be between 1 and 100000".into(),
                ));
            }
        }
        "gateway.cb_window_secs" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(5..=3600).contains(&v) {
                return Err(AppError::BadRequest(
                    "cb_window_secs must be between 5 and 3600".into(),
                ));
            }
        }
        "gateway.cb_open_secs" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if !(1..=3600).contains(&v) {
                return Err(AppError::BadRequest(
                    "cb_open_secs must be between 1 and 3600".into(),
                ));
            }
        }

        _ => {
            return Err(AppError::BadRequest(format!("Unknown setting: {key}")));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_positive_integer_settings() {
        assert!(validate_setting("auth.jwt_access_ttl_secs", &json!(900)).is_ok());
        assert!(validate_setting("auth.jwt_access_ttl_secs", &json!(0)).is_err());
        assert!(validate_setting("auth.jwt_access_ttl_secs", &json!(-1)).is_err());
    }

    #[test]
    fn rejects_positive_integer_above_upper_bound() {
        // Upper bounds defeat the i64::MAX → OOM/overflow DoS class.
        // audit.channel_capacity max = 1_000_000, audit.batch_size max =
        // 10_000 — values above are rejected at save time.
        assert!(validate_setting("audit.channel_capacity", &json!(i64::MAX)).is_err());
        assert!(validate_setting("audit.batch_size", &json!(10_001)).is_err());
        assert!(validate_setting("auth.jwt_refresh_ttl_days", &json!(366)).is_err());
        assert!(validate_setting("gateway.request_timeout_secs", &json!(601)).is_err());
    }

    #[test]
    fn validates_zero_allowed_settings() {
        assert!(validate_setting("api_keys.default_expiry_days", &json!(0)).is_ok());
        assert!(validate_setting("api_keys.default_expiry_days", &json!(30)).is_ok());
        assert!(validate_setting("api_keys.default_expiry_days", &json!(-1)).is_err());
    }

    #[test]
    fn validates_site_name() {
        assert!(validate_setting("setup.site_name", &json!("My Site")).is_ok());
        assert!(validate_setting("setup.site_name", &json!("")).is_err());
        let long_name = "x".repeat(101);
        assert!(validate_setting("setup.site_name", &json!(long_name)).is_err());
    }

    #[test]
    fn validates_content_filter_patterns() {
        // Empty array is valid
        assert!(validate_setting("security.content_filter_patterns", &json!([])).is_ok());
        // Valid rule
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "action": "block", "name": "Test", "match_type": "contains"}])
            )
            .is_ok()
        );
        // Regex match_type with valid pattern
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "\\d{4}", "action": "warn", "name": "Test", "match_type": "regex"}])
            )
            .is_ok()
        );
        // Regex match_type with invalid regex → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "[invalid((", "action": "block", "name": "T", "match_type": "regex"}])
            )
            .is_err()
        );
        // Missing match_type → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "action": "block", "name": "T"}])
            )
            .is_err()
        );
        // Missing action → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "name": "T", "match_type": "contains"}])
            )
            .is_err()
        );
        // Missing name → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "action": "block", "match_type": "contains"}])
            )
            .is_err()
        );
        // Not an array → rejected
        assert!(validate_setting("security.content_filter_patterns", &json!("not array")).is_err());
        // Invalid action value → rejected
        assert!(
            validate_setting(
                "security.content_filter_patterns",
                &json!([{"pattern": "test", "action": "invalid", "name": "x", "match_type": "contains"}])
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_settings() {
        assert!(validate_setting("unknown.key", &json!("anything")).is_err());
    }

    #[test]
    fn validates_boolean_settings() {
        assert!(validate_setting("setup.initialized", &json!(true)).is_ok());
        assert!(validate_setting("setup.initialized", &json!(false)).is_err());
        assert!(validate_setting("setup.initialized", &json!("yes")).is_err());
    }
}
