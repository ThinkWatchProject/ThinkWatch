//! DB seed helpers used by integration tests.
//!
//! Each helper writes directly into the per-test Postgres database so
//! a test can ask for "an admin user", "a registered openai provider",
//! "an api key with these scopes" without spinning the full HTTP
//! flow first. They're plain async functions that take a [`PgPool`]
//! reference; tests get the pool from `TestApp::db`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use think_watch_auth::{api_key, password};
use think_watch_common::models::{ApiKey, Provider, User};
use uuid::Uuid;

/// Outcome of [`create_user`].
#[derive(Debug, Clone)]
pub struct SeededUser {
    pub user: User,
    pub plaintext_password: String,
}

/// Insert a fresh user row with the given email and the given password.
/// Returns the row plus the plaintext so the test can call /login.
pub async fn create_user(
    db: &PgPool,
    email: &str,
    display_name: &str,
    plaintext_password: &str,
) -> Result<SeededUser> {
    let hash = password::hash_password(plaintext_password)?;
    let user = sqlx::query_as::<_, User>(
        r#"INSERT INTO users (email, display_name, password_hash)
           VALUES ($1, $2, $3) RETURNING *"#,
    )
    .bind(email)
    .bind(display_name)
    .bind(&hash)
    .fetch_one(db)
    .await
    .context("INSERT users")?;
    Ok(SeededUser {
        user,
        plaintext_password: plaintext_password.to_string(),
    })
}

/// Convenience — generate a unique email and create a developer user.
pub async fn create_random_user(db: &PgPool) -> Result<SeededUser> {
    let email = format!("dev-{}@example.com", Uuid::new_v4().simple());
    let user = create_user(db, &email, "Developer", "DevPwd_1234567!").await?;
    assign_role_global(db, user.user.id, "developer").await?;
    Ok(user)
}

/// Convenience — admin user with super_admin role at global scope.
pub async fn create_admin_user(db: &PgPool) -> Result<SeededUser> {
    let email = format!("admin-{}@example.com", Uuid::new_v4().simple());
    let user = create_user(db, &email, "Admin", "AdminPwd_1234567!").await?;
    assign_role_global(db, user.user.id, "super_admin").await?;
    Ok(user)
}

/// Attach a system role to the user at `scope_kind = 'global'`.
pub async fn assign_role_global(db: &PgPool, user_id: Uuid, role_name: &str) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, assigned_by)
           SELECT $1, id, 'global', $1 FROM rbac_roles WHERE name = $2"#,
    )
    .bind(user_id)
    .bind(role_name)
    .execute(db)
    .await
    .with_context(|| format!("assign role {role_name}"))?;
    Ok(())
}

/// Mark `setup.initialized = true` so the public `/api/setup/*`
/// endpoints stop returning 200 and the system behaves like a real
/// running deployment.
pub async fn mark_setup_complete(db: &PgPool) -> Result<()> {
    sqlx::query(
        r#"UPDATE system_settings
           SET value = 'true'::jsonb, updated_at = now()
           WHERE key = 'setup.initialized'"#,
    )
    .execute(db)
    .await
    .context("UPDATE system_settings setup.initialized")?;
    Ok(())
}

/// Set an arbitrary system setting. JSON value as-is.
pub async fn set_setting(db: &PgPool, key: &str, value: Value) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO system_settings (key, value, category, description)
           VALUES ($1, $2, 'test', 'set by integration test')
           ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()"#,
    )
    .bind(key)
    .bind(value)
    .execute(db)
    .await
    .with_context(|| format!("set_setting {key}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Providers / models / routes
// ---------------------------------------------------------------------------

/// Insert a provider row with the given `provider_type` (`openai`,
/// `anthropic`, `google`, `azure_openai`, `bedrock`, …) pointing at
/// the supplied base URL (typically the test wiremock server).
pub async fn create_provider(
    db: &PgPool,
    name: &str,
    provider_type: &str,
    base_url: &str,
    extra_config: Option<Value>,
) -> Result<Provider> {
    let config = extra_config.unwrap_or_else(|| serde_json::json!({}));
    sqlx::query_as::<_, Provider>(
        r#"INSERT INTO providers (name, display_name, provider_type, base_url, is_active, config_json)
           VALUES ($1, $1, $2, $3, true, $4)
           RETURNING *"#,
    )
    .bind(name)
    .bind(provider_type)
    .bind(base_url)
    .bind(config)
    .fetch_one(db)
    .await
    .context("INSERT providers")
}

/// Register a model + a default route to `provider_id` so the gateway
/// can dispatch to it. Use a unique `model_id` per test.
pub async fn create_model_and_route(db: &PgPool, provider_id: Uuid, model_id: &str) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO models (model_id, display_name)
           VALUES ($1, $1)
           ON CONFLICT (model_id) DO NOTHING"#,
    )
    .bind(model_id)
    .execute(db)
    .await
    .context("INSERT models")?;
    sqlx::query(
        r#"INSERT INTO model_routes (model_id, provider_id, upstream_model, weight, priority, enabled)
           VALUES ($1, $2, NULL, 100, 0, true)"#,
    )
    .bind(model_id)
    .bind(provider_id)
    .execute(db)
    .await
    .context("INSERT model_routes")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SeededApiKey {
    pub row: ApiKey,
    pub plaintext: String,
}

/// Mint an API key for `user_id` valid on the given gateway surfaces
/// (e.g. `["ai_gateway"]`). The plaintext value is the `tw-…` secret
/// the caller must use as `Authorization: Bearer …`.
pub async fn create_api_key(
    db: &PgPool,
    user_id: Uuid,
    name: &str,
    surfaces: &[&str],
    allowed_models: Option<&[&str]>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<SeededApiKey> {
    let generated = api_key::generate_api_key();
    let surfaces_vec: Vec<String> = surfaces.iter().map(|s| (*s).to_string()).collect();
    let models_vec: Option<Vec<String>> =
        allowed_models.map(|m| m.iter().map(|s| (*s).to_string()).collect());

    let row = sqlx::query_as::<_, ApiKey>(
        r#"INSERT INTO api_keys (key_prefix, key_hash, name, user_id, surfaces, allowed_models, expires_at, is_active)
           VALUES ($1, $2, $3, $4, $5, $6, $7, true)
           RETURNING *"#,
    )
    .bind(&generated.prefix)
    .bind(&generated.hash)
    .bind(name)
    .bind(user_id)
    .bind(&surfaces_vec)
    .bind(models_vec.as_ref())
    .bind(expires_at)
    .fetch_one(db)
    .await
    .context("INSERT api_keys")?;

    Ok(SeededApiKey {
        row,
        plaintext: generated.plaintext,
    })
}

// ---------------------------------------------------------------------------
// MCP servers
// ---------------------------------------------------------------------------

pub async fn create_mcp_server(
    db: &PgPool,
    name: &str,
    namespace_prefix: &str,
    endpoint_url: &str,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO mcp_servers (id, name, namespace_prefix, endpoint_url, transport_type, status)
           VALUES ($1, $2, $3, $4, 'streamable_http', 'active')"#,
    )
    .bind(id)
    .bind(name)
    .bind(namespace_prefix)
    .bind(endpoint_url)
    .execute(db)
    .await
    .context("INSERT mcp_servers")?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Insert a budget cap row directly. `subject_kind` is one of `user`
/// or `api_key` (DB CHECK rejects others). `period` is `daily` /
/// `weekly` / `monthly`. The cap is in **weighted tokens**, matching
/// the schema — costs are computed against gateway_logs separately.
pub async fn create_budget_cap(
    db: &PgPool,
    subject_kind: &str,
    subject_id: Uuid,
    period: &str,
    limit_tokens: i64,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO budget_caps
            (id, subject_kind, subject_id, period, limit_tokens, enabled)
           VALUES ($1, $2, $3, $4, $5, true)"#,
    )
    .bind(id)
    .bind(subject_kind)
    .bind(subject_id)
    .bind(period)
    .bind(limit_tokens)
    .execute(db)
    .await
    .context("INSERT budget_caps")?;
    Ok(id)
}

/// Insert a rate-limit rule. `surface` is `ai_gateway` or
/// `mcp_gateway`; `metric` is `requests` or `tokens`. `window_secs`
/// must be ≥ 60 (validated by `limits::validate_persisted` at
/// startup).
pub async fn create_rate_limit_rule(
    db: &PgPool,
    subject_kind: &str,
    subject_id: Uuid,
    surface: &str,
    metric: &str,
    window_secs: i32,
    max_count: i64,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO rate_limit_rules
            (id, subject_kind, subject_id, surface, metric, window_secs, max_count, enabled)
           VALUES ($1, $2, $3, $4, $5, $6, $7, true)"#,
    )
    .bind(id)
    .bind(subject_kind)
    .bind(subject_id)
    .bind(surface)
    .bind(metric)
    .bind(window_secs)
    .bind(max_count)
    .execute(db)
    .await
    .context("INSERT rate_limit_rules")?;
    Ok(id)
}
