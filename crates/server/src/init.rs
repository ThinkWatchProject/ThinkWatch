//! Server bootstrap helpers shared by the production binary
//! (`main.rs`) and integration tests (`crates/test-support`).
//!
//! `init_state` builds the full `AppState` from already-constructed
//! infra clients (Postgres, Redis, optional ClickHouse). Side effects
//! that are unsuitable for tests — installing the global circuit-
//! breaker listener, registering the Prometheus recorder, spawning
//! the long-lived background loops — are split into separate
//! `install_*` / `spawn_*` helpers so each caller can opt in.

use std::sync::Arc;

use anyhow::Context;
use fred::clients::Client as RedisClient;
use fred::interfaces::ClientLike;
use fred::types::Builder;
use sqlx::PgPool;
use think_watch_auth::oidc::OidcManager;
use think_watch_common::audit::{self, AuditConfig, AuditLogger};
use think_watch_common::config::AppConfig;
use think_watch_common::dynamic_config::{self, DynamicConfig};

use crate::app::{self, AppState};
use crate::handlers;
use crate::tasks;

/// Boot the dynamic config + audit pipeline + OIDC + every shared
/// arc-swap handle that lives on `AppState`. Caller is expected to
/// have already built the connection pool, redis client, and
/// (optionally) the ClickHouse client.
pub async fn init_state(
    config: AppConfig,
    pool: PgPool,
    redis: RedisClient,
    ch_client: Option<clickhouse::Client>,
) -> anyhow::Result<AppState> {
    // RBAC catalog + persisted limit invariants.
    handlers::roles::validate_seeded_roles(&pool)
        .await
        .context("seeded RBAC roles reference unknown permissions")?;
    think_watch_common::limits::validate_persisted(&pool)
        .await
        .context("persisted rate-limit / weight rows fail validation")?;

    // ClickHouse tables. Same bounded retry as production but without
    // the metrics counter (recorder is not installed in tests).
    if ch_client.is_some() {
        let mut attempt = 0u32;
        loop {
            match audit::ensure_clickhouse_tables(&ch_client).await {
                Ok(()) => break,
                Err(e) if attempt < 4 => {
                    let backoff_ms = 1_500u64 * 2u64.pow(attempt);
                    tracing::warn!(
                        attempt = attempt + 1,
                        backoff_ms,
                        "ClickHouse table init failed, retrying: {e}"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    attempt += 1;
                }
                Err(e) => {
                    tracing::error!(
                        "ClickHouse table init failed after {} attempts: {e}",
                        attempt + 1
                    );
                    break;
                }
            }
        }
    }

    let dynamic_config = Arc::new(DynamicConfig::load(pool.clone()).await?);

    let audit_logger = AuditLogger::new(
        audit_config(&config),
        Some(pool.clone()),
        ch_client.clone(),
        Some(dynamic_config.clone()),
    )
    .await;

    let oidc_manager = build_oidc(&config, &dynamic_config).await;

    let jwt = Arc::new(think_watch_auth::jwt::JwtManager::new(&config.jwt_secret));

    let initial_content_filter = app::load_content_filter(&dynamic_config).await;
    let initial_pii_redactor = app::load_pii_redactor(&dynamic_config).await;
    let content_filter = Arc::new(arc_swap::ArcSwap::from_pointee(initial_content_filter));
    let pii_redactor = Arc::new(arc_swap::ArcSwap::from_pointee(initial_pii_redactor));

    let init_http_secs = dynamic_config.perf_http_client_secs().await as u64;
    let init_mcp_pool_secs = dynamic_config.perf_mcp_pool_secs().await as u64;

    let gateway_router = Arc::new(arc_swap::ArcSwap::from_pointee(
        think_watch_gateway::router::ModelRouter::new(),
    ));

    let state = AppState {
        db: pool,
        redis,
        jwt,
        config,
        dynamic_config,
        audit: audit_logger,
        oidc: Arc::new(tokio::sync::RwLock::new(oidc_manager)),
        started_at: chrono::Utc::now(),
        clickhouse: ch_client,
        content_filter,
        pii_redactor,
        mcp_registry: think_watch_mcp_gateway::registry::Registry::new(),
        mcp_circuit_breakers: think_watch_mcp_gateway::circuit_breaker::McpCircuitBreakers::new(),
        mcp_pool: Arc::new(arc_swap::ArcSwap::from_pointee(
            think_watch_mcp_gateway::pool::ConnectionPool::with_timeout(init_mcp_pool_secs),
        )),
        http_client: Arc::new(arc_swap::ArcSwap::from_pointee(
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(init_http_secs))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        )),
        gateway_router,
    };

    Ok(state)
}

fn audit_config(config: &AppConfig) -> AuditConfig {
    config.audit_config()
}

async fn build_oidc(config: &AppConfig, dc: &DynamicConfig) -> Option<OidcManager> {
    if !dc.oidc_enabled().await {
        return None;
    }
    let issuer = dc.oidc_issuer_url().await.unwrap_or_default();
    let client_id = dc.oidc_client_id().await.unwrap_or_default();
    let secret_enc = dc.oidc_client_secret_encrypted().await.unwrap_or_default();
    let redirect = dc.oidc_redirect_url().await.unwrap_or_else(|| {
        format!(
            "http://{}:{}/api/auth/sso/callback",
            config.server_host, config.console_port
        )
    });

    let encryption_key =
        match think_watch_common::crypto::parse_encryption_key(&config.encryption_key) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!("Failed to parse encryption key for OIDC: {e}");
                return None;
            }
        };

    let client_secret = if secret_enc.is_empty() {
        String::new()
    } else {
        match hex::decode(&secret_enc)
            .map_err(|e| format!("hex decode: {e}"))
            .and_then(|bytes| {
                think_watch_common::crypto::decrypt(&bytes, &encryption_key)
                    .map_err(|e| format!("decrypt: {e}"))
            })
            .and_then(|plain| String::from_utf8(plain).map_err(|e| format!("utf8: {e}")))
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to decrypt OIDC client secret at startup: {e}");
                String::new()
            }
        }
    };

    if issuer.is_empty() || client_id.is_empty() || client_secret.is_empty() {
        return None;
    }

    match OidcManager::discover(&issuer, &client_id, &client_secret, &redirect).await {
        Ok(mgr) => Some(mgr),
        Err(e) => {
            tracing::error!(issuer = %issuer, "OIDC discovery failed; SSO disabled: {e}");
            None
        }
    }
}

/// Install the **process-global** circuit-breaker open listener so
/// CB transitions get recorded as audit events. Production calls this
/// exactly once. Integration tests that spawn multiple in-process
/// instances must NOT call it (the inner registry is a `OnceLock`).
pub fn install_cb_listener(state: &AppState) {
    let audit_for_cb = state.audit.clone();
    think_watch_common::cb_registry::set_open_listener(move |key, kind| {
        audit_for_cb.log(
            think_watch_common::audit::AuditEntry::new("provider.circuit_open")
                .resource(format!("{kind}_provider:{key}"))
                .detail(serde_json::json!({
                    "kind": kind,
                    "provider": key,
                })),
        );
    });
}

/// Subscribe to Redis `config:changed` and hot-reload the in-memory
/// dynamic config / content filter / PII redactor / HTTP client / MCP
/// pool whenever any instance flips a setting.
pub async fn spawn_config_subscriber(state: &AppState) -> anyhow::Result<()> {
    // Multi-instance config sync (`system_settings.value` updates → Pub/Sub).
    let sub_main = fred::types::config::Config::from_url(&state.config.redis_url)?;
    let sub_main_redis: fred::clients::SubscriberClient =
        Builder::from_config(sub_main).build_subscriber_client()?;
    sub_main_redis.init().await?;
    dynamic_config::spawn_config_subscriber(sub_main_redis, state.dynamic_config.clone());

    // Hot-reload the per-state arc-swap handles on the same channel.
    let sub_filters_cfg = fred::types::config::Config::from_url(&state.config.redis_url)?;
    let sub_filters: fred::clients::SubscriberClient =
        Builder::from_config(sub_filters_cfg).build_subscriber_client()?;
    sub_filters.init().await?;
    let dc_clone = state.dynamic_config.clone();
    let cf_clone = state.content_filter.clone();
    let pii_clone = state.pii_redactor.clone();
    let http_clone = state.http_client.clone();
    let pool_clone = state.mcp_pool.clone();
    tokio::spawn(async move {
        use fred::interfaces::{EventInterface, PubsubInterface};
        let mut rx = sub_filters.message_rx();
        if let Err(e) = sub_filters.subscribe("config:changed").await {
            tracing::warn!("Filter reload subscriber failed: {e}");
            return;
        }
        while let Ok(msg) = rx.recv().await {
            if msg.channel == "config:changed" {
                if let Err(e) = dc_clone.reload().await {
                    tracing::warn!("Failed to reload dynamic config: {e}");
                    continue;
                }
                let new_filter = app::load_content_filter(&dc_clone).await;
                cf_clone.store(Arc::new(new_filter));
                let new_pii = app::load_pii_redactor(&dc_clone).await;
                pii_clone.store(Arc::new(new_pii));

                let http_secs = dc_clone.perf_http_client_secs().await as u64;
                let new_http = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(http_secs))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new());
                http_clone.store(Arc::new(new_http));

                let pool_secs = dc_clone.perf_mcp_pool_secs().await as u64;
                let new_pool =
                    think_watch_mcp_gateway::pool::ConnectionPool::with_timeout(pool_secs);
                pool_clone.store(Arc::new(new_pool));

                tracing::info!("Hot-reloaded filters, HTTP client, and MCP pool");
            }
        }
    });
    Ok(())
}

/// Spawn the long-running periodic background workers: API-key
/// lifecycle (expiry sweep) and data retention (log purge). Production
/// calls this once after `init_state`. Tests usually skip it and
/// invoke the underlying functions directly when they want
/// deterministic timing.
pub fn spawn_background_tasks(state: &AppState) {
    tasks::api_key_lifecycle::spawn_api_key_lifecycle_task(
        state.db.clone(),
        state.dynamic_config.clone(),
        state.audit.clone(),
    );
    tasks::data_retention::spawn_data_retention_task(
        state.db.clone(),
        state.dynamic_config.clone(),
        state.audit.clone(),
    );
}
