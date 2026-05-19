use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use think_watch_common::dynamic_config::{self, SettingEntry};
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// =========================================================================
// Submodule split — see crates/server/src/handlers/admin/ for per-resource
// files. Re-exports below preserve the existing `handlers::admin::*` call
// shape across the codebase (52 sites), so consumers don't have to know
// which submodule owns which handler.
// =========================================================================

mod oidc;
mod users;

pub use oidc::{
    DisableOidcRequest, OidcActiveSnapshot, OidcDraftSnapshot, OidcSettingsResponse,
    OidcTestResult, StartOidcTestLoginResponse, UpdateOidcDraftRequest, activate_oidc_draft,
    delete_oidc_draft, discover_oidc_draft, get_oidc_settings, start_oidc_test_login,
    toggle_oidc_active, update_oidc_draft,
};
pub use users::{
    CreateUserByAdminRequest, CreateUserByAdminResponse, ListUsersQuery, SuperAdminIds,
    UpdateUserRequest, create_user, delete_user, force_logout_user, list_super_admin_ids,
    list_users, reset_user_password, update_user,
};

// utoipa's `#[utoipa::path]` macro generates a `__path_<fn>` companion
// type for each annotated handler. When the openapi spec registers
// handlers via `crate::handlers::admin::<fn>`, the macro lookup looks
// for `__path_<fn>` in the SAME module — so we must re-export those
// too, otherwise spec generation fails to find them.
#[allow(unused_imports)]
pub use oidc::{
    __path_activate_oidc_draft, __path_delete_oidc_draft, __path_discover_oidc_draft,
    __path_get_oidc_settings, __path_start_oidc_test_login, __path_toggle_oidc_active,
    __path_update_oidc_draft,
};
#[allow(unused_imports)]
pub use users::{
    __path_create_user, __path_delete_user, __path_force_logout_user, __path_list_super_admin_ids,
    __path_list_users, __path_reset_user_password, __path_update_user,
};

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
        clickhouse_url: state.config.clickhouse_url.clone(),
        clickhouse_db: state.config.clickhouse_db.clone(),
        connected,
    }))
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

/// Map of `data.retention_days_*` setting keys to their ClickHouse table name.
const RETENTION_TABLES: &[(&str, &str)] = &[
    ("data.retention_days_audit", "audit_logs"),
    ("data.retention_days_gateway", "gateway_logs"),
    ("data.retention_days_mcp", "mcp_logs"),
    ("data.retention_days_access", "access_logs"),
    ("data.retention_days_app", "app_logs"),
];

/// Maximum retention window we will accept, in days. Anything bigger is
/// almost certainly a typo and risks accidentally turning the window off.
const MAX_RETENTION_DAYS: i64 = 36500; // 100 years

/// Whitelist of valid ClickHouse log table identifiers. Used as a
/// belt-and-braces guard so we never inject anything we don't already
/// know about into a `ALTER TABLE ...` statement, even though all call
/// sites today only pass &'static str literals from RETENTION_TABLES.
const VALID_LOG_TABLES: &[&str] = &[
    "audit_logs",
    "gateway_logs",
    "mcp_logs",
    "access_logs",
    "app_logs",
];

/// Issue a single `ALTER TABLE ... MODIFY TTL` against ClickHouse.
/// Validates `table` against an explicit whitelist and clamps `days`
/// into a sane range. Returns `false` if the call was skipped or failed.
async fn apply_single_ttl(ch: &clickhouse::Client, table: &str, days: i64) -> bool {
    if !VALID_LOG_TABLES.contains(&table) {
        tracing::error!(table, "refusing TTL update for unknown table");
        return false;
    }
    if !(1..=MAX_RETENTION_DAYS).contains(&days) {
        tracing::error!(table, days, "refusing TTL update: days out of range");
        return false;
    }
    let sql =
        format!("ALTER TABLE {table} MODIFY TTL toDateTime(created_at) + INTERVAL {days} DAY");
    match ch.query(&sql).execute().await {
        Ok(()) => {
            tracing::info!(table, days, "ClickHouse TTL updated");
            true
        }
        Err(e) => {
            tracing::error!(table, days, "Failed to update ClickHouse TTL: {e}");
            false
        }
    }
}

/// Issue `ALTER TABLE ... MODIFY TTL` for every retention setting included in
/// the update. Failures are logged but not surfaced — the setting is already
/// persisted, and ClickHouse may be temporarily unavailable.
async fn apply_clickhouse_ttls(state: &AppState, settings: &HashMap<String, serde_json::Value>) {
    let Some(ch) = state.clickhouse.as_ref() else {
        return;
    };
    for (key, table) in RETENTION_TABLES {
        let Some(value) = settings.get(*key) else {
            continue;
        };
        let Some(days) = value.as_i64() else { continue };
        if days <= 0 {
            continue;
        }
        apply_single_ttl(ch, table, days).await;
    }
    // Body-column TTL is administered through a separate setting that
    // shortens the lifetime of the heavy payload columns without
    // touching the row TTL. Only apply on the PATCH path when the
    // operator actually included it in the request, so unrelated edits
    // (e.g. a single bump to access-log retention) don't churn the
    // body-column metadata.
    if let Some(value) = settings.get("audit.body_retention_days")
        && let Some(days) = value.as_i64()
    {
        apply_body_column_ttls(ch, days).await;
        // Re-check the bucket lifecycle horizon — if the operator
        // just raised retention above the bucket's GC, surface the
        // mismatch in their PATCH-response log line rather than
        // waiting for an auditor to hit a 404.
        check_body_retention_vs_lifecycle(state).await;
    }
}

/// Apply current persisted retention settings to all ClickHouse log tables.
/// Called once at server startup so settings survive restarts. Silently no-ops
/// if ClickHouse is not configured.
pub async fn reconcile_clickhouse_ttls(state: &AppState) {
    let Some(ch) = state.clickhouse.as_ref() else {
        return;
    };
    let dc = &state.dynamic_config;
    let pairs: [(i64, &str); 5] = [
        (dc.data_retention_days_audit().await, "audit_logs"),
        (dc.data_retention_days_gateway().await, "gateway_logs"),
        (dc.data_retention_days_mcp().await, "mcp_logs"),
        (dc.data_retention_days_access().await, "access_logs"),
        (dc.data_retention_days_app().await, "app_logs"),
    ];
    for (days, table) in pairs {
        if days <= 0 {
            continue;
        }
        apply_single_ttl(ch, table, days).await;
    }
    apply_body_column_ttls(ch, dc.audit_body_retention_days().await).await;
}

/// Detect mismatches between `audit.body_retention_days` (the CH
/// column TTL operators tune via dynamic_config) and the bucket's
/// own lifecycle horizon (administered out-of-band — RustFS init
/// script, `mc ilm`, the AWS console, etc.). When the CH retention
/// is set ABOVE the bucket horizon, every `s3://bucket/key` URL
/// stored in CH between (bucket_days, ch_days] will 404 on read —
/// the audit row outlives its referenced object. Silent in
/// production until an auditor hits a "body fetch failed" 502.
///
/// Called at startup and after PATCH /api/admin/settings. Fail-OPEN:
/// blob-store backends that can't report a lifecycle (`InlineStore`,
/// or an S3 backend without a configured rule) skip the check
/// cleanly. Transport errors are logged but don't fail startup —
/// we WANT the server up even if the lifecycle query is flaky.
pub async fn check_body_retention_vs_lifecycle(state: &AppState) {
    if !state.blob_store.can_offload() {
        return;
    }
    let configured_days = state.dynamic_config.audit_body_retention_days().await;
    if configured_days <= 0 {
        return;
    }
    let bucket_days = match state.blob_store.lifecycle_days().await {
        Ok(Some(days)) => days,
        Ok(None) => {
            // No rule configured on the bucket — operator has to
            // own the cleanup themselves. Log info so the audit
            // posture is visible but don't warn.
            tracing::info!(
                "blob-store bucket has no lifecycle rule covering `bodies/`; \
                 audit.body_retention_days={configured_days} relies on operator-driven cleanup"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "blob-store lifecycle query failed; skipping retention cross-check"
            );
            return;
        }
    };
    if (configured_days as u32) > bucket_days {
        tracing::warn!(
            audit_body_retention_days = configured_days,
            bucket_lifecycle_days = bucket_days,
            "audit.body_retention_days is ABOVE the bucket lifecycle horizon — \
             offloaded body URLs between {bucket_days}d and {configured_days}d will 404 on \
             read; raise the bucket lifecycle (`mc ilm`) OR lower audit.body_retention_days"
        );
        metrics::counter!("audit_body_retention_above_bucket_lifecycle_total").increment(1);
    } else {
        tracing::info!(
            audit_body_retention_days = configured_days,
            bucket_lifecycle_days = bucket_days,
            "audit.body_retention_days vs bucket lifecycle check passed"
        );
    }
}

/// `(table, column)` pairs that hold captured request/response bodies and
/// therefore deserve their own (shorter) TTL — auditors typically need
/// recent replay, but holding terabytes of week-old prompts wastes
/// storage. The byte-count + status columns are tiny and stay on the
/// row's normal TTL.
const BODY_COLUMNS: &[(&str, &str)] = &[
    ("gateway_logs", "request_body"),
    ("gateway_logs", "response_body"),
    ("mcp_logs", "tool_arguments"),
    ("mcp_logs", "tool_result"),
];

/// Issue per-column `ALTER TABLE ... MODIFY COLUMN <col> TTL ...` against
/// each body column. Column-level TTL is independent of the row TTL: when
/// it expires, ClickHouse merges the column to its default (NULL for our
/// Nullable(String) columns) while leaving the row in place until the
/// table-level TTL kicks in. Failures are logged but not surfaced —
/// startup races and intermittent CH availability shouldn't prevent the
/// server from coming up.
async fn apply_body_column_ttls(ch: &clickhouse::Client, days: i64) {
    if !(1..=MAX_RETENTION_DAYS).contains(&days) {
        tracing::error!(days, "refusing body TTL update: days out of range");
        return;
    }
    for (table, column) in BODY_COLUMNS {
        if !VALID_LOG_TABLES.contains(table) {
            // Guard against future drift even though the const is
            // hand-curated — if someone adds a body column on an
            // unaudited table, refuse to ALTER it rather than letting
            // a typo through.
            tracing::error!(table, "body column points at non-whitelisted table");
            continue;
        }
        let sql = format!(
            "ALTER TABLE {table} MODIFY COLUMN {column} \
             TTL toDateTime(created_at) + INTERVAL {days} DAY"
        );
        match ch.query(&sql).execute().await {
            Ok(()) => tracing::info!(table, column, days, "ClickHouse body-column TTL updated"),
            Err(e) => {
                tracing::error!(table, column, days, "Failed to update body-column TTL: {e}")
            }
        }
    }
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

    // Apply ClickHouse TTL changes for any retention setting that was updated.
    // ClickHouse runs the cleanup asynchronously in its merge worker, so this
    // returns immediately.
    apply_clickhouse_ttls(&state, &req.settings).await;

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

// ---------------------------------------------------------------------------
// Content filter — test sandbox & presets
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ContentFilterTestRequest {
    /// User text to test against the supplied rules.
    pub text: String,
    /// Rules to test (the unsaved rules currently in the UI).
    pub rules: Vec<think_watch_gateway::content_filter::DenyRuleConfig>,
}

#[derive(Debug, Serialize)]
pub struct ContentFilterTestMatch {
    pub name: String,
    pub pattern: String,
    pub match_type: String,
    pub action: String,
    pub matched_snippet: String,
}

#[derive(Debug, Serialize)]
pub struct ContentFilterTestResponse {
    pub matches: Vec<ContentFilterTestMatch>,
}

/// POST /api/admin/settings/content-filter/test — try the supplied rules
/// against a sample of user text and return every rule that fires.
#[utoipa::path(
    post,
    path = "/api/admin/settings/content-filter/test",
    tag = "Settings",
    request_body(
        content = inline(serde_json::Value),
        description = "text: string, rules: DenyRuleConfig[]",
    ),
    responses(
        (status = 200, description = "Rules that matched the input text"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn test_content_filter(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<ContentFilterTestRequest>,
) -> Result<Json<ContentFilterTestResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "content_filter:read")
        .await?;
    use think_watch_gateway::content_filter::ContentFilter;
    let filter = ContentFilter::from_config(&req.rules);
    let matches = filter
        .check_text_all(&req.text)
        .into_iter()
        .map(|m| ContentFilterTestMatch {
            name: m.name,
            pattern: m.pattern,
            match_type: m.match_type.to_string(),
            action: m.action.to_string(),
            matched_snippet: m.matched_snippet,
        })
        .collect();
    Ok(Json(ContentFilterTestResponse { matches }))
}

#[derive(Debug, Serialize)]
pub struct ContentFilterPreset {
    pub id: String,
    pub rules: Vec<think_watch_gateway::content_filter::DenyRuleConfig>,
}

/// GET /api/admin/settings/content-filter/presets — return built-in rule groups
/// (basic / strict / chinese). UI labels are localized on the frontend.
#[utoipa::path(
    get,
    path = "/api/admin/settings/content-filter/presets",
    tag = "Settings",
    responses(
        (status = 200, description = "Built-in content filter preset groups"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_content_filter_presets(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<ContentFilterPreset>>, AppError> {
    auth_user
        .require_global_permission(&state.db, "content_filter:read")
        .await?;
    let groups = think_watch_gateway::content_filter::presets()
        .into_iter()
        .map(|g| ContentFilterPreset {
            id: g.id.to_string(),
            rules: g.rules,
        })
        .collect();
    Ok(Json(groups))
}

// ---------------------------------------------------------------------------
// PII redactor — test sandbox
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PiiRedactorTestRequest {
    pub text: String,
    pub patterns: Vec<think_watch_gateway::pii_redactor::PiiPatternConfig>,
}

#[derive(Debug, Serialize)]
pub struct PiiRedactorTestMatch {
    pub name: String,
    pub original: String,
    pub placeholder: String,
}

#[derive(Debug, Serialize)]
pub struct PiiRedactorTestResponse {
    pub redacted_text: String,
    pub matches: Vec<PiiRedactorTestMatch>,
}

/// POST /api/admin/settings/pii-redactor/test — apply the supplied PII patterns
/// to a text sample and return the redacted version with the substitution map.
#[utoipa::path(
    post,
    path = "/api/admin/settings/pii-redactor/test",
    tag = "Settings",
    request_body(
        content = inline(serde_json::Value),
        description = "text: string, patterns: PiiPatternConfig[]",
    ),
    responses(
        (status = 200, description = "Redacted text and substitution map"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn test_pii_redactor(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<PiiRedactorTestRequest>,
) -> Result<Json<PiiRedactorTestResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "pii_redactor:read")
        .await?;
    use think_watch_gateway::pii_redactor::PiiRedactor;
    use think_watch_gateway::providers::traits::ChatMessage;

    let redactor = PiiRedactor::from_config(&req.patterns);
    let messages = vec![ChatMessage {
        role: "user".to_string(),
        content: serde_json::Value::String(req.text.clone()),
        ..Default::default()
    }];
    let (redacted, ctx) = redactor.redact_messages(&messages);

    let redacted_text = redacted
        .first()
        .and_then(|m| m.content.as_str())
        .unwrap_or("")
        .to_string();

    let matches = ctx
        .replacements
        .into_iter()
        .map(|(placeholder, original)| {
            // Extract pattern name from placeholder format "{{NAME_salt_n}}"
            let name = placeholder
                .trim_start_matches("{{")
                .trim_end_matches("}}")
                .split('_')
                .next()
                .unwrap_or("")
                .to_string();
            PiiRedactorTestMatch {
                name,
                original,
                placeholder,
            }
        })
        .collect();

    Ok(Json(PiiRedactorTestResponse {
        redacted_text,
        matches,
    }))
}

/// Validate a setting value based on its key.
fn validate_setting(key: &str, value: &serde_json::Value) -> Result<(), AppError> {
    match key {
        // Integer settings that must be > 0
        "auth.jwt_access_ttl_secs"
        | "auth.jwt_refresh_ttl_days"
        | "gateway.cache_ttl_secs"
        | "gateway.request_timeout_secs"
        | "gateway.body_limit_bytes"
        | "console.request_timeout_secs"
        | "console.body_limit_bytes"
        | "security.signature_nonce_ttl_secs"
        | "audit.batch_size"
        | "audit.flush_interval_secs"
        | "audit.channel_capacity"
        | "api_keys.rotation_grace_period_hours" => {
            let v = value
                .as_i64()
                .ok_or_else(|| AppError::BadRequest(format!("{key} must be an integer")))?;
            if v <= 0 {
                return Err(AppError::BadRequest(format!("{key} must be > 0")));
            }
        }

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
                if match_type == "regex"
                    && think_watch_common::regex_util::compile_bounded(pattern).is_err()
                {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: invalid or oversized regex pattern"
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
                if item.get("name").and_then(|v| v.as_str()).is_none() {
                    return Err(AppError::BadRequest(format!(
                        "Rule {i}: missing 'name' field"
                    )));
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
                // Validate regex compiles AND fits the bounded size budget.
                // Bare `regex::Regex::new` accepts 10 MiB NFA + 2 MiB DFA
                // by default — large enough to ReDoS the gateway at
                // request time. Use the shared bounded helper so save-time
                // rejection matches what the runtime would accept.
                if think_watch_common::regex_util::compile_bounded(regex_str).is_err() {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: invalid or oversized regex"
                    )));
                }
                if item
                    .get("placeholder_prefix")
                    .and_then(|v| v.as_str())
                    .is_none()
                {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: missing 'placeholder_prefix'"
                    )));
                }
                if item.get("name").and_then(|v| v.as_str()).is_none() {
                    return Err(AppError::BadRequest(format!(
                        "PII pattern {i}: missing 'name'"
                    )));
                }
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
