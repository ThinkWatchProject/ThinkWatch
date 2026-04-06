use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Database-backed dynamic configuration with in-memory cache.
///
/// Settings are loaded from `system_settings` at startup and cached in memory.
/// Changes are persisted to DB and the cache is refreshed. Multi-instance
/// deployments stay in sync via Redis Pub/Sub on the `config:changed` channel.
#[derive(Clone)]
pub struct DynamicConfig {
    cache: Arc<RwLock<HashMap<String, Value>>>,
    db: PgPool,
}

impl DynamicConfig {
    /// Load all settings from the database into a new `DynamicConfig`.
    pub async fn load(db: PgPool) -> anyhow::Result<Self> {
        let rows: Vec<(String, Value)> = sqlx::query_as("SELECT key, value FROM system_settings")
            .fetch_all(&db)
            .await?;

        let mut map = HashMap::with_capacity(rows.len());
        for (key, value) in rows {
            map.insert(key, value);
        }

        Ok(Self {
            cache: Arc::new(RwLock::new(map)),
            db,
        })
    }

    /// Reload all settings from the database.
    pub async fn reload(&self) -> anyhow::Result<()> {
        let rows: Vec<(String, Value)> = sqlx::query_as("SELECT key, value FROM system_settings")
            .fetch_all(&self.db)
            .await?;

        let mut map = HashMap::with_capacity(rows.len());
        for (key, value) in rows {
            map.insert(key, value);
        }

        let mut cache = self.cache.write().await;
        *cache = map;
        Ok(())
    }

    /// Update one or more settings in the database and refresh the cache.
    /// Validates values before persisting.
    pub async fn update(
        &self,
        updates: &HashMap<String, Value>,
        updated_by: Option<uuid::Uuid>,
    ) -> anyhow::Result<()> {
        // Validate values before persisting
        for (key, value) in updates {
            validate_setting(key, value)?;
        }

        let mut tx = self.db.begin().await?;

        for (key, value) in updates {
            sqlx::query(
                r#"UPDATE system_settings
                   SET value = $1, updated_by = $2, updated_at = now()
                   WHERE key = $3"#,
            )
            .bind(value)
            .bind(updated_by)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        self.reload().await?;
        Ok(())
    }

    // --- Typed getters ---

    /// Get a raw JSON value.
    pub async fn get(&self, key: &str) -> Option<Value> {
        self.cache.read().await.get(key).cloned()
    }

    /// Get as i64.
    pub async fn get_i64(&self, key: &str) -> Option<i64> {
        self.cache.read().await.get(key).and_then(|v| v.as_i64())
    }

    /// Get as f64.
    pub async fn get_f64(&self, key: &str) -> Option<f64> {
        self.cache.read().await.get(key).and_then(|v| v.as_f64())
    }

    /// Get as String.
    pub async fn get_string(&self, key: &str) -> Option<String> {
        self.cache
            .read()
            .await
            .get(key)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
    }

    /// Get as bool.
    pub async fn get_bool(&self, key: &str) -> Option<bool> {
        self.cache.read().await.get(key).and_then(|v| v.as_bool())
    }

    /// Get as Vec<f64>.
    pub async fn get_f64_vec(&self, key: &str) -> Option<Vec<f64>> {
        self.cache.read().await.get(key).and_then(|v| {
            v.as_array()
                .map(|arr| arr.iter().filter_map(|x| x.as_f64()).collect())
        })
    }

    /// Get the raw JSON value (for complex structures like pattern arrays).
    pub async fn get_json(&self, key: &str) -> Option<Value> {
        self.cache.read().await.get(key).cloned()
    }

    /// Get all settings grouped by category.
    pub async fn get_all_grouped(&self) -> HashMap<String, Vec<SettingEntry>> {
        let rows: Vec<SettingRow> = sqlx::query_as(
            "SELECT key, value, category, description, updated_at FROM system_settings ORDER BY key",
        )
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();

        let mut grouped: HashMap<String, Vec<SettingEntry>> = HashMap::new();
        for row in rows {
            grouped
                .entry(row.category.clone())
                .or_default()
                .push(SettingEntry {
                    key: row.key,
                    value: row.value,
                    category: row.category,
                    description: row.description,
                    updated_at: row.updated_at,
                });
        }
        grouped
    }

    /// Get settings for a specific category.
    pub async fn get_by_category(&self, category: &str) -> Vec<SettingEntry> {
        let rows: Vec<SettingRow> = sqlx::query_as(
            "SELECT key, value, category, description, updated_at FROM system_settings WHERE category = $1 ORDER BY key",
        )
        .bind(category)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .map(|row| SettingEntry {
                key: row.key,
                value: row.value,
                category: row.category,
                description: row.description,
                updated_at: row.updated_at,
            })
            .collect()
    }

    // --- Convenience getters with defaults ---

    pub async fn jwt_access_ttl_secs(&self) -> i64 {
        self.get_i64("auth.jwt_access_ttl_secs")
            .await
            .unwrap_or(900)
    }

    pub async fn jwt_refresh_ttl_days(&self) -> i64 {
        self.get_i64("auth.jwt_refresh_ttl_days").await.unwrap_or(7)
    }

    pub async fn cache_ttl_secs(&self) -> u64 {
        self.get_i64("gateway.cache_ttl_secs").await.unwrap_or(3600) as u64
    }

    pub async fn signature_drift_secs(&self) -> i64 {
        self.get_i64("security.signature_drift_secs")
            .await
            .unwrap_or(300)
    }

    pub async fn signature_nonce_ttl_secs(&self) -> i64 {
        self.get_i64("security.signature_nonce_ttl_secs")
            .await
            .unwrap_or(600)
    }

    pub async fn budget_alert_thresholds(&self) -> Vec<f64> {
        self.get_f64_vec("budget.alert_thresholds")
            .await
            .unwrap_or_else(|| vec![0.50, 0.80, 0.95])
    }

    pub async fn budget_webhook_url(&self) -> Option<String> {
        self.get_string("budget.webhook_url").await
    }

    pub async fn is_initialized(&self) -> bool {
        self.get_bool("setup.initialized").await.unwrap_or(false)
    }

    pub async fn site_name(&self) -> String {
        self.get_string("setup.site_name")
            .await
            .unwrap_or_else(|| "ThinkWatch".to_string())
    }

    pub async fn api_keys_default_expiry_days(&self) -> i64 {
        self.get_i64("api_keys.default_expiry_days")
            .await
            .unwrap_or(90)
    }

    pub async fn api_keys_inactivity_timeout_days(&self) -> i64 {
        self.get_i64("api_keys.inactivity_timeout_days")
            .await
            .unwrap_or(0)
    }

    pub async fn api_keys_rotation_period_days(&self) -> i64 {
        self.get_i64("api_keys.rotation_period_days")
            .await
            .unwrap_or(0)
    }

    pub async fn api_keys_rotation_grace_period_hours(&self) -> i64 {
        self.get_i64("api_keys.rotation_grace_period_hours")
            .await
            .unwrap_or(24)
    }

    pub async fn data_retention_days_usage(&self) -> i64 {
        self.get_i64("data.retention_days_usage")
            .await
            .unwrap_or(90)
    }

    pub async fn data_retention_days_audit(&self) -> i64 {
        self.get_i64("data.retention_days_audit")
            .await
            .unwrap_or(365)
    }

    pub async fn client_ip_source(&self) -> String {
        self.get_string("security.client_ip_source")
            .await
            .unwrap_or_else(|| "connection".to_string())
    }

    pub async fn client_ip_xff_position(&self) -> String {
        self.get_string("security.client_ip_xff_position")
            .await
            .unwrap_or_else(|| "left".to_string())
    }

    pub async fn client_ip_xff_depth(&self) -> i64 {
        self.get_i64("security.client_ip_xff_depth")
            .await
            .unwrap_or(1)
    }

    pub async fn allow_registration(&self) -> bool {
        self.get_bool("auth.allow_registration")
            .await
            .unwrap_or(false)
    }

    // --- OIDC / SSO ---

    pub async fn oidc_enabled(&self) -> bool {
        self.get_bool("oidc.enabled").await.unwrap_or(false)
    }

    pub async fn oidc_issuer_url(&self) -> Option<String> {
        self.get_string("oidc.issuer_url").await
    }

    pub async fn oidc_client_id(&self) -> Option<String> {
        self.get_string("oidc.client_id").await
    }

    /// Returns the encrypted client secret (hex). Caller must decrypt.
    pub async fn oidc_client_secret_encrypted(&self) -> Option<String> {
        self.get_string("oidc.client_secret_encrypted").await
    }

    pub async fn oidc_redirect_url(&self) -> Option<String> {
        self.get_string("oidc.redirect_url").await
    }

    /// Insert or update a setting, creating the row if it doesn't exist.
    pub async fn upsert(
        &self,
        key: &str,
        value: &Value,
        category: &str,
        description: Option<&str>,
        updated_by: Option<uuid::Uuid>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO system_settings (key, value, category, description, updated_by, updated_at)
               VALUES ($1, $2, $3, $4, $5, now())
               ON CONFLICT (key) DO UPDATE
               SET value = $2, updated_by = $5, updated_at = now()"#,
        )
        .bind(key)
        .bind(value)
        .bind(category)
        .bind(description)
        .bind(updated_by)
        .execute(&self.db)
        .await?;

        self.reload().await?;
        Ok(())
    }
}

/// Publish a config change notification via Redis Pub/Sub.
pub async fn notify_config_changed(redis: &fred::clients::Client) {
    use fred::interfaces::PubsubInterface;
    let _: Result<(), _> = redis.publish("config:changed", "reload").await;
}

/// Subscribe to config change notifications and reload when received.
pub fn spawn_config_subscriber(redis: fred::clients::SubscriberClient, config: Arc<DynamicConfig>) {
    tokio::spawn(async move {
        use fred::interfaces::{EventInterface, PubsubInterface};
        let mut rx = redis.message_rx();

        if let Err(e) = redis.subscribe("config:changed").await {
            tracing::warn!("Failed to subscribe to config:changed: {e}");
            return;
        }

        while let Ok(msg) = rx.recv().await {
            if msg.channel == "config:changed" {
                tracing::info!("Config change notification received, reloading");
                if let Err(e) = config.reload().await {
                    tracing::error!("Failed to reload config: {e}");
                }
            }
        }
    });
}

// --- Internal types ---

#[derive(Debug, sqlx::FromRow)]
struct SettingRow {
    key: String,
    value: Value,
    category: String,
    description: Option<String>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SettingEntry {
    pub key: String,
    pub value: Value,
    pub category: String,
    pub description: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Validate a setting value before persisting.
/// Returns an error if the value is invalid for the given key.
fn validate_setting(key: &str, value: &Value) -> anyhow::Result<()> {
    match key {
        // Integer settings with minimum bounds
        k if k.ends_with("_secs") || k.ends_with("_days") || k.ends_with("_hours") => {
            let n = value
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected an integer value"))?;
            if n < 0 {
                anyhow::bail!("{key}: value must be non-negative, got {n}");
            }
        }

        // JWT TTLs need reasonable bounds
        "auth.jwt_access_ttl_secs" => {
            let n = value
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected integer"))?;
            if !(60..=86400).contains(&n) {
                anyhow::bail!("{key}: must be between 60 and 86400 seconds");
            }
        }
        "auth.jwt_refresh_ttl_days" => {
            let n = value
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected integer"))?;
            if !(1..=365).contains(&n) {
                anyhow::bail!("{key}: must be between 1 and 365 days");
            }
        }

        // Budget thresholds must be between 0.0 and 1.0
        "budget.alert_thresholds" => {
            let arr = value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected an array of numbers"))?;
            for v in arr {
                let n = v
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("{key}: array elements must be numbers"))?;
                if !(0.0..=1.0).contains(&n) {
                    anyhow::bail!("{key}: threshold values must be between 0.0 and 1.0, got {n}");
                }
            }
        }

        // Webhook URL validation
        "budget.webhook_url" => {
            if let Some(url) = value.as_str()
                && !url.is_empty()
                && !url.starts_with("http://")
                && !url.starts_with("https://")
            {
                anyhow::bail!("{key}: must be a valid http/https URL");
            }
        }

        // Boolean settings
        "setup.initialized" | "auth.allow_registration" => {
            value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a boolean value"))?;
        }

        // Client IP settings
        "security.client_ip_source" => {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a string"))?;
            if !["connection", "xff", "x-real-ip"].contains(&s) {
                anyhow::bail!("{key}: must be \"connection\", \"xff\", or \"x-real-ip\"");
            }
        }
        "security.client_ip_xff_position" => {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a string"))?;
            if !["left", "right"].contains(&s) {
                anyhow::bail!("{key}: must be \"left\" or \"right\"");
            }
        }
        "security.client_ip_xff_depth" => {
            let n = value
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected an integer"))?;
            if !(1..=20).contains(&n) {
                anyhow::bail!("{key}: must be between 1 and 20");
            }
        }

        // General — public gateway URL components (used by configuration guide)
        "general.public_protocol" => {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a string"))?;
            if !s.is_empty() && s != "http" && s != "https" {
                anyhow::bail!("{key}: must be \"http\", \"https\", or empty for auto-detect");
            }
        }
        "general.public_host" => {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a string"))?;
            if s.len() > 253 {
                anyhow::bail!("{key}: host too long");
            }
            // Reject schemes / paths — host is just a hostname or IP
            if s.contains("://") || s.contains('/') {
                anyhow::bail!("{key}: must be a hostname only (no scheme or path)");
            }
        }
        "general.public_port" => {
            let n = value
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected an integer"))?;
            if !(0..=65535).contains(&n) {
                anyhow::bail!("{key}: must be between 0 and 65535 (0 = auto)");
            }
        }

        // String settings — basic non-empty check for critical ones
        "setup.site_name" => {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a string value"))?;
            if s.is_empty() || s.len() > 200 {
                anyhow::bail!("{key}: must be between 1 and 200 characters");
            }
        }

        // PII patterns: basic validation (regex compilation checked at gateway layer)
        "gateway.pii_patterns" => {
            if let Some(arr) = value.as_array() {
                for item in arr {
                    if item.get("regex").and_then(|v| v.as_str()).is_none() {
                        anyhow::bail!("{key}: each pattern must have a 'regex' string field");
                    }
                    if item
                        .get("placeholder_prefix")
                        .and_then(|v| v.as_str())
                        .is_none()
                    {
                        anyhow::bail!(
                            "{key}: each pattern must have a 'placeholder_prefix' string field"
                        );
                    }
                }
            }
        }

        // OIDC settings
        "oidc.enabled" => {
            value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a boolean value"))?;
        }
        "oidc.issuer_url" => {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a string"))?;
            if !s.is_empty() && !s.starts_with("https://") && !s.starts_with("http://") {
                anyhow::bail!("{key}: must be a valid http/https URL");
            }
        }
        "oidc.client_id" | "oidc.redirect_url" => {
            value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a string"))?;
        }
        "oidc.client_secret_encrypted" => {
            // Encrypted value — just check it's a string
            value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{key}: expected a string"))?;
        }

        _ => {} // Unknown keys pass through — forward-compatible
    }
    Ok(())
}
