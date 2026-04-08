use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use think_watch_auth::{api_key, password};
use think_watch_common::audit::AuditEntry;
use think_watch_common::dynamic_config;
use think_watch_common::errors::AppError;
use think_watch_common::validation::validate_password;

use crate::app::AppState;

#[derive(Debug, Serialize)]
pub struct SetupStatusResponse {
    pub initialized: bool,
    pub needs_setup: bool,
}

/// GET /api/setup/status — public, returns initialization status.
pub async fn setup_status(
    State(state): State<AppState>,
) -> Result<Json<SetupStatusResponse>, AppError> {
    let initialized = state.dynamic_config.is_initialized().await;
    Ok(Json(SetupStatusResponse {
        initialized,
        needs_setup: !initialized,
    }))
}

#[derive(Debug, Deserialize)]
pub struct SetupInitRequest {
    pub admin: AdminSetup,
    pub site_name: Option<String>,
    pub provider: Option<ProviderSetup>,
}

#[derive(Debug, Deserialize)]
pub struct AdminSetup {
    pub email: String,
    pub display_name: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct ProviderSetup {
    pub name: String,
    pub display_name: String,
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
}

#[derive(Debug, Serialize)]
pub struct SetupInitResponse {
    pub admin_id: uuid::Uuid,
    pub admin_email: String,
    pub api_key: Option<String>,
    pub provider_id: Option<uuid::Uuid>,
    pub message: String,
}

/// POST /api/setup/initialize — public (only works if not initialized).
/// Protected by:
/// 1. DB-level check (setup.initialized = true rejects)
/// 2. Redis rate limiting (max 5 attempts per minute per IP)
/// 3. Database advisory lock to prevent race conditions
pub async fn setup_initialize(
    State(state): State<AppState>,
    Json(req): Json<SetupInitRequest>,
) -> Result<Json<SetupInitResponse>, AppError> {
    // Check if already initialized (fast path from cache)
    if state.dynamic_config.is_initialized().await {
        return Err(AppError::Forbidden("Setup already completed".into()));
    }

    // Rate limit: max 5 setup attempts per minute (global, not per-user since no auth)
    let rate_key = "setup_rate_limit";
    let count: u64 = fred::interfaces::KeysInterface::incr_by(&state.redis, rate_key, 1)
        .await
        .unwrap_or(1);
    if count == 1 {
        let _: () = fred::interfaces::KeysInterface::expire(&state.redis, rate_key, 60, None)
            .await
            .unwrap_or(());
    }
    if count > 5 {
        return Err(AppError::BadRequest(
            "Too many setup attempts. Please try again later.".into(),
        ));
    }

    // Double-check from DB (not cache) to prevent race condition,
    // using a PostgreSQL advisory lock to serialize concurrent attempts.
    let mut tx = state.db.begin().await?;

    // Acquire an advisory lock (key = 1 for setup). This blocks concurrent setup attempts.
    sqlx::query("SELECT pg_advisory_xact_lock(1)")
        .execute(&mut *tx)
        .await?;

    let db_initialized: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM system_settings WHERE key = 'setup.initialized'")
            .fetch_optional(&mut *tx)
            .await?;

    if db_initialized
        .as_ref()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err(AppError::Forbidden("Setup already completed".into()));
    }

    // Validate inputs
    validate_password(&req.admin.password)?;
    if !req.admin.email.contains('@') || !req.admin.email.contains('.') {
        return Err(AppError::BadRequest("Invalid email format".into()));
    }

    // 1. Create super_admin user
    let password_hash = password::hash_password(&req.admin.password)?;
    let admin_user = sqlx::query_as::<_, (uuid::Uuid, String)>(
        r#"INSERT INTO users (email, display_name, password_hash)
           VALUES ($1, $2, $3) RETURNING id, email"#,
    )
    .bind(&req.admin.email)
    .bind(&req.admin.display_name)
    .bind(&password_hash)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate") || e.to_string().contains("unique") {
            AppError::Conflict("Email already exists".into())
        } else {
            AppError::Internal(e.into())
        }
    })?;

    // Assign super_admin role.
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
           SELECT $1, id, 'global', $1 FROM rbac_roles WHERE name = 'super_admin'"#,
    )
    .bind(admin_user.0)
    .execute(&mut *tx)
    .await?;

    // 2. Create first provider (optional)
    let mut provider_id = None;
    if let Some(ref provider) = req.provider {
        let encryption_key =
            think_watch_common::crypto::parse_encryption_key(&state.config.encryption_key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
        let encrypted_api_key =
            think_watch_common::crypto::encrypt(provider.api_key.as_bytes(), &encryption_key)
                .map_err(|e| {
                    AppError::Internal(anyhow::anyhow!("Failed to encrypt API key: {e}"))
                })?;

        let pid = sqlx::query_scalar::<_, uuid::Uuid>(
            r#"INSERT INTO providers (name, display_name, provider_type, base_url, api_key_encrypted)
               VALUES ($1, $2, $3, $4, $5) RETURNING id"#,
        )
        .bind(&provider.name)
        .bind(&provider.display_name)
        .bind(&provider.provider_type)
        .bind(&provider.base_url)
        .bind(&encrypted_api_key)
        .fetch_one(&mut *tx)
        .await?;

        provider_id = Some(pid);
    }

    // 3. Generate first API key for admin user
    let generated = api_key::generate_api_key();
    sqlx::query(
        r#"INSERT INTO api_keys (key_prefix, key_hash, name, user_id)
           VALUES ($1, $2, $3, $4)"#,
    )
    .bind(&generated.prefix)
    .bind(&generated.hash)
    .bind("Default Admin Key")
    .bind(admin_user.0)
    .execute(&mut *tx)
    .await?;

    // 4. Mark as initialized
    let site_name = req.site_name.as_deref().unwrap_or("ThinkWatch");
    sqlx::query(
        "UPDATE system_settings SET value = $1, updated_at = now() WHERE key = 'setup.initialized'",
    )
    .bind(serde_json::json!(true))
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE system_settings SET value = $1, updated_at = now() WHERE key = 'setup.site_name'",
    )
    .bind(serde_json::json!(site_name))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Reload dynamic config
    let _ = state.dynamic_config.reload().await;
    dynamic_config::notify_config_changed(&state.redis).await;

    state.audit.log(
        AuditEntry::new("setup.initialize")
            .user_id(admin_user.0)
            .resource("system")
            .detail(serde_json::json!({
                "admin_email": req.admin.email,
                "provider_created": provider_id.is_some(),
            })),
    );

    Ok(Json(SetupInitResponse {
        admin_id: admin_user.0,
        admin_email: admin_user.1,
        api_key: Some(generated.plaintext),
        provider_id,
        message: "Setup completed successfully. Please log in with your admin credentials.".into(),
    }))
}
