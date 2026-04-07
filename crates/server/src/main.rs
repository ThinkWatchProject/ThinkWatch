use fred::prelude::*;
use fred::types::Builder;
use std::sync::Arc;
use think_watch_auth::oidc::OidcManager;
use think_watch_common::audit::{self, AuditLogger};
use think_watch_common::config::AppConfig;
use think_watch_common::db;
use think_watch_common::dynamic_config::{self, DynamicConfig};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

mod app;
mod error;
mod handlers;
mod middleware;
mod tasks;
mod tracing_ch;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Phase 1: stdout-only tracing (before ClickHouse is available).
    // The CH layer slot starts as None and gets swapped in after AuditLogger init.
    let (ch_layer, ch_layer_reload) =
        tracing_subscriber::reload::Layer::new(None::<tracing_ch::ClickHouseLayer>);
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().json())
        .with(ch_layer)
        .init();

    let config = AppConfig::from_env()?;
    if let Err(e) = config.validate() {
        tracing::error!("Configuration validation failed: {e}");
        std::process::exit(1);
    }
    tracing::info!("Starting ThinkWatch");

    // Startup dependency validation
    let pool = match db::create_pool(&config.database_url).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Failed to connect to PostgreSQL: {e}");
            tracing::error!("Check DATABASE_URL and ensure PostgreSQL is running");
            std::process::exit(1);
        }
    };
    if let Err(e) = db::run_migrations(&pool).await {
        tracing::error!("Database migration failed: {e}");
        std::process::exit(1);
    }

    let redis_config = Config::from_url(&config.redis_url).map_err(|e| {
        tracing::error!("Invalid REDIS_URL: {e}");
        anyhow::anyhow!("Invalid REDIS_URL")
    })?;
    let redis = Builder::from_config(redis_config).build()?;
    if let Err(e) = redis.init().await {
        tracing::error!("Failed to connect to Redis: {e}");
        tracing::error!("Check REDIS_URL and ensure Redis is running");
        std::process::exit(1);
    }
    tracing::info!("Redis connected");

    let ch_client = think_watch_common::clickhouse_client::create_client(&config.audit_config());
    if ch_client.is_some() {
        tracing::info!("ClickHouse client initialized");
    }

    // Initialize ClickHouse tables with retry (best-effort, non-blocking)
    {
        let ch = ch_client.clone();
        tokio::spawn(async move {
            audit::ensure_clickhouse_tables(&ch).await;
        });
    }

    // Load dynamic configuration from database
    let dynamic_config = Arc::new(DynamicConfig::load(pool.clone()).await?);
    tracing::info!("Dynamic configuration loaded");

    // Subscribe to config changes for multi-instance sync
    let sub_redis_config = Config::from_url(&config.redis_url)?;
    let sub_redis: fred::clients::SubscriberClient =
        Builder::from_config(sub_redis_config).build_subscriber_client()?;
    sub_redis.init().await?;
    dynamic_config::spawn_config_subscriber(sub_redis, dynamic_config.clone());

    let audit_logger =
        AuditLogger::new(config.audit_config(), Some(pool.clone()), ch_client.clone());

    // Phase 2: swap in ClickHouse tracing layer now that AuditLogger is ready
    let _ = ch_layer_reload
        .modify(|layer| *layer = Some(tracing_ch::ClickHouseLayer::new(audit_logger.clone())));
    tracing::info!("ClickHouse tracing layer activated");

    // Initialize OIDC — prefer dynamic config (database), fall back to env vars.
    // If OIDC env vars are set but no DB config exists yet, seed the DB from env.
    let oidc_manager = {
        let dc = &dynamic_config;
        let encryption_key =
            think_watch_common::crypto::parse_encryption_key(&config.encryption_key)?;

        // Check if OIDC is already configured in dynamic config (database)
        let db_issuer = dc.oidc_issuer_url().await;

        if db_issuer.is_none() && config.oidc_enabled() {
            // Seed database from env vars (first-time migration)
            tracing::info!("Seeding OIDC config from environment variables into database");
            let secret_plain = config.oidc_client_secret.as_deref().unwrap_or("");
            let secret_encrypted = hex::encode(think_watch_common::crypto::encrypt(
                secret_plain.as_bytes(),
                &encryption_key,
            )?);
            let default_redirect = format!(
                "http://{}:{}/api/auth/sso/callback",
                config.server_host, config.console_port
            );

            for (k, v, desc) in [
                ("oidc.enabled", serde_json::json!(true), "SSO enabled"),
                (
                    "oidc.issuer_url",
                    serde_json::json!(config.oidc_issuer_url.as_deref().unwrap_or("")),
                    "OIDC issuer URL",
                ),
                (
                    "oidc.client_id",
                    serde_json::json!(config.oidc_client_id.as_deref().unwrap_or("")),
                    "OIDC client ID",
                ),
                (
                    "oidc.client_secret_encrypted",
                    serde_json::json!(secret_encrypted),
                    "OIDC client secret (encrypted)",
                ),
                (
                    "oidc.redirect_url",
                    serde_json::json!(
                        config
                            .oidc_redirect_url
                            .as_deref()
                            .unwrap_or(&default_redirect)
                    ),
                    "OIDC redirect URL",
                ),
            ] {
                dc.upsert(k, &v, "oidc", Some(desc), None).await.ok();
            }
        }

        // Now load OIDC from dynamic config
        if dc.oidc_enabled().await {
            let issuer = dc.oidc_issuer_url().await.unwrap_or_default();
            let client_id = dc.oidc_client_id().await.unwrap_or_default();
            let secret_enc = dc.oidc_client_secret_encrypted().await.unwrap_or_default();
            let redirect = dc.oidc_redirect_url().await.unwrap_or_else(|| {
                format!(
                    "http://{}:{}/api/auth/sso/callback",
                    config.server_host, config.console_port
                )
            });

            // Decrypt client secret
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

            if !issuer.is_empty() && !client_id.is_empty() && !client_secret.is_empty() {
                match OidcManager::discover(&issuer, &client_id, &client_secret, &redirect).await {
                    Ok(mgr) => {
                        tracing::info!("OIDC provider discovered successfully");
                        Some(mgr)
                    }
                    Err(e) => {
                        tracing::warn!("OIDC discovery failed, SSO disabled: {e}");
                        None
                    }
                }
            } else {
                tracing::info!("OIDC configured but fields incomplete, SSO disabled");
                None
            }
        } else {
            tracing::info!("OIDC not configured, SSO disabled");
            None
        }
    };

    let jwt = Arc::new(think_watch_auth::jwt::JwtManager::new(&config.jwt_secret));

    // Build initial content filter and PII redactor from current dynamic config
    let initial_content_filter = app::load_content_filter(&dynamic_config).await;
    let initial_pii_redactor = app::load_pii_redactor(&dynamic_config).await;
    let content_filter = Arc::new(arc_swap::ArcSwap::from_pointee(initial_content_filter));
    let pii_redactor = Arc::new(arc_swap::ArcSwap::from_pointee(initial_pii_redactor));

    let state = app::AppState {
        db: pool,
        redis,
        jwt: jwt.clone(),
        config: config.clone(),
        dynamic_config,
        audit: audit_logger,
        oidc: Arc::new(tokio::sync::RwLock::new(oidc_manager)),
        started_at: chrono::Utc::now(),
        clickhouse: ch_client,
        content_filter,
        pii_redactor,
        // Shared MCP runtime — registry, circuit breakers, and connection
        // pool live in AppState so the console CRUD handlers and the
        // gateway proxy see the same view.
        mcp_registry: think_watch_mcp_gateway::registry::Registry::new(),
        mcp_circuit_breakers: think_watch_mcp_gateway::circuit_breaker::McpCircuitBreakers::new(),
        mcp_pool: think_watch_mcp_gateway::pool::ConnectionPool::new(),
        // Single shared HTTP client for outbound calls (tool discovery,
        // SSO, etc). 15s default timeout — individual handlers can wrap
        // calls in `tokio::time::timeout` for tighter limits.
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
    };

    // --- Hot reload of content filter / PII redactor on config change ---
    // Subscribes to the same `config:changed` channel so updates from any
    // instance trigger a reload of the in-memory rule sets.
    {
        let sub_redis_filters_config = Config::from_url(&config.redis_url)?;
        let sub_redis_filters: fred::clients::SubscriberClient =
            Builder::from_config(sub_redis_filters_config).build_subscriber_client()?;
        sub_redis_filters.init().await?;
        let dc_clone = state.dynamic_config.clone();
        let cf_clone = state.content_filter.clone();
        let pii_clone = state.pii_redactor.clone();
        tokio::spawn(async move {
            use fred::interfaces::{EventInterface, PubsubInterface};
            let mut rx = sub_redis_filters.message_rx();
            if let Err(e) = sub_redis_filters.subscribe("config:changed").await {
                tracing::warn!("Filter reload subscriber failed: {e}");
                return;
            }
            while let Ok(msg) = rx.recv().await {
                if msg.channel == "config:changed" {
                    // DynamicConfig has its own subscriber that reloads the cache
                    // first; we wait briefly so we read the fresh value.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let new_filter = app::load_content_filter(&dc_clone).await;
                    cf_clone.store(Arc::new(new_filter));
                    let new_pii = app::load_pii_redactor(&dc_clone).await;
                    pii_clone.store(Arc::new(new_pii));
                    tracing::info!("Content filter and PII redactor reloaded");
                }
            }
        });
    }

    // --- Start background tasks ---
    tasks::api_key_lifecycle::spawn_api_key_lifecycle_task(
        state.db.clone(),
        state.dynamic_config.clone(),
    );
    tasks::data_retention::spawn_data_retention_task(
        state.db.clone(),
        state.dynamic_config.clone(),
    );
    tracing::info!("Background tasks started");

    // Reconcile ClickHouse log table TTLs with the persisted settings on
    // startup. This ensures retention values configured via the admin UI
    // survive server restarts even if the table was created with different
    // defaults.
    handlers::admin::reconcile_clickhouse_ttls(&state).await;

    // --- Start Gateway server (AI API + MCP) ---
    let gateway_app = app::create_gateway_app(&config, state.clone(), jwt).await;
    let gateway_addr = config.gateway_addr();
    let gateway_listener = tokio::net::TcpListener::bind(&gateway_addr).await?;
    tracing::info!("Gateway listening on {gateway_addr} (AI API + MCP)");

    let gateway_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(
            gateway_listener,
            gateway_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        {
            tracing::error!("Gateway server crashed: {e}");
        }
    });

    // --- Start Console server (Web UI + management API) ---
    let console_app = app::create_console_app(&config, state);
    let console_addr = config.console_addr();
    let console_listener = tokio::net::TcpListener::bind(&console_addr).await?;
    tracing::info!("Console listening on {console_addr} (Web UI + Admin API)");

    let console_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(
            console_listener,
            console_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        {
            tracing::error!("Console server crashed: {e}");
        }
    });

    tokio::select! {
        _ = gateway_handle => tracing::error!("Gateway server exited unexpectedly"),
        _ = console_handle => tracing::error!("Console server exited unexpectedly"),
    }

    Ok(())
}
