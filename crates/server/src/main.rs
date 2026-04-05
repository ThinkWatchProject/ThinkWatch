use agent_bastion_auth::oidc::OidcManager;
use agent_bastion_common::audit::{self, AuditLogger};
use agent_bastion_common::config::AppConfig;
use agent_bastion_common::db;
use agent_bastion_common::dynamic_config::{self, DynamicConfig};
use fred::prelude::*;
use fred::types::Builder;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

mod app;
mod error;
mod handlers;
mod middleware;
mod tasks;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .json()
        .init();

    let config = AppConfig::from_env()?;
    if let Err(e) = config.validate() {
        tracing::error!("Configuration validation failed: {e}");
        std::process::exit(1);
    }
    tracing::info!("Starting AgentBastion");

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

    // Initialize ClickHouse tables with retry (best-effort, non-blocking)
    if config.clickhouse_url.is_some() {
        let audit_config = config.audit_config();
        tokio::spawn(async move {
            audit::ensure_clickhouse_tables(&audit_config).await;
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

    let audit_logger = AuditLogger::new(config.audit_config(), Some(pool.clone()));

    // Initialize OIDC if configured (all fields validated by oidc_enabled())
    let oidc_manager = if config.oidc_enabled() {
        let issuer = config
            .oidc_issuer_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OIDC_ISSUER_URL required when OIDC is enabled"))?;
        let client_id = config
            .oidc_client_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OIDC_CLIENT_ID required when OIDC is enabled"))?;
        let client_secret = config
            .oidc_client_secret
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OIDC_CLIENT_SECRET required when OIDC is enabled"))?;
        let default_redirect = format!(
            "http://{}:{}/api/auth/sso/callback",
            config.server_host, config.console_port
        );
        let redirect = config
            .oidc_redirect_url
            .as_deref()
            .unwrap_or(&default_redirect);

        match OidcManager::discover(issuer, client_id, client_secret, redirect).await {
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
        tracing::info!("OIDC not configured, SSO disabled");
        None
    };

    let jwt = Arc::new(agent_bastion_auth::jwt::JwtManager::new(&config.jwt_secret));

    let ch_client =
        agent_bastion_common::clickhouse_client::create_client(&config.audit_config());
    if ch_client.is_some() {
        tracing::info!("ClickHouse client initialized");
    }

    let state = app::AppState {
        db: pool,
        redis,
        jwt: jwt.clone(),
        config: config.clone(),
        dynamic_config,
        audit: audit_logger,
        oidc: oidc_manager,
        started_at: chrono::Utc::now(),
        clickhouse: ch_client,
    };

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
