use agent_bastion_auth::oidc::OidcManager;
use agent_bastion_common::audit::{self, AuditLogger};
use agent_bastion_common::config::AppConfig;
use agent_bastion_common::db;
use fred::prelude::*;
use fred::types::Builder;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

mod app;
mod error;
mod handlers;
mod middleware;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .json()
        .init();

    let config = AppConfig::from_env()?;
    config.validate();
    tracing::info!("Starting AgentBastion");

    let pool = db::create_pool(&config.database_url).await?;
    db::run_migrations(&pool).await?;

    let redis_config = Config::from_url(&config.redis_url)?;
    let redis = Builder::from_config(redis_config).build()?;
    redis.init().await?;
    tracing::info!("Redis connected");

    // Initialize Quickwit audit index (best-effort, non-blocking)
    if let Some(ref qw_url) = config.quickwit_url {
        let qw_url = qw_url.clone();
        let qw_index = config.quickwit_index.clone();
        tokio::spawn(async move {
            audit::ensure_quickwit_index(&qw_url, &qw_index).await;
        });
    }

    let audit_logger = AuditLogger::new(config.audit_config(), Some(pool.clone()));

    // Initialize OIDC if configured (all fields validated by oidc_enabled())
    let oidc_manager = if config.oidc_enabled() {
        let issuer = config.oidc_issuer_url.as_deref()
            .ok_or_else(|| anyhow::anyhow!("OIDC_ISSUER_URL required when OIDC is enabled"))?;
        let client_id = config.oidc_client_id.as_deref()
            .ok_or_else(|| anyhow::anyhow!("OIDC_CLIENT_ID required when OIDC is enabled"))?;
        let client_secret = config.oidc_client_secret.as_deref()
            .ok_or_else(|| anyhow::anyhow!("OIDC_CLIENT_SECRET required when OIDC is enabled"))?;
        let redirect = config.oidc_redirect_url.as_deref()
            .unwrap_or("http://localhost:3001/api/auth/sso/callback");

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

    let state = app::AppState {
        db: pool,
        redis,
        jwt: jwt.clone(),
        config: config.clone(),
        audit: audit_logger,
        oidc: oidc_manager,
    };

    // --- Start Gateway server (AI API + MCP) ---
    let gateway_app = app::create_gateway_app(&config, state.clone(), jwt);
    let gateway_addr = config.gateway_addr();
    let gateway_listener = tokio::net::TcpListener::bind(&gateway_addr).await?;
    tracing::info!("Gateway listening on {gateway_addr} (AI API + MCP)");

    let gateway_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(gateway_listener, gateway_app).await {
            tracing::error!("Gateway server crashed: {e}");
        }
    });

    // --- Start Console server (Web UI + management API) ---
    let console_app = app::create_console_app(&config, state);
    let console_addr = config.console_addr();
    let console_listener = tokio::net::TcpListener::bind(&console_addr).await?;
    tracing::info!("Console listening on {console_addr} (Web UI + Admin API)");

    let console_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(console_listener, console_app).await {
            tracing::error!("Console server crashed: {e}");
        }
    });

    tokio::select! {
        _ = gateway_handle => tracing::error!("Gateway server exited unexpectedly"),
        _ = console_handle => tracing::error!("Console server exited unexpectedly"),
    }

    Ok(())
}
