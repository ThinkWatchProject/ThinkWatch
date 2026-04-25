use fred::prelude::*;
use fred::types::Builder;
use think_watch_common::audit;
use think_watch_common::config::AppConfig;
use think_watch_common::db;
use think_watch_server::{app, handlers, init, tracing_ch};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

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

    let state = init::init_state(config.clone(), pool, redis, ch_client).await?;

    // Phase 2: swap in ClickHouse tracing layer now that AuditLogger is ready
    let _ = ch_layer_reload
        .modify(|layer| *layer = Some(tracing_ch::ClickHouseLayer::new(state.audit.clone())));
    tracing::info!("ClickHouse tracing layer activated");

    // Process-global side effects: only the binary installs them.
    init::install_cb_listener(&state);
    init::spawn_config_subscriber(&state).await?;
    init::spawn_background_tasks(&state);
    tracing::info!("Background tasks started");

    // Reconcile ClickHouse log table TTLs with the persisted settings on
    // startup. This ensures retention values configured via the admin UI
    // survive server restarts even if the table was created with different
    // defaults.
    handlers::admin::reconcile_clickhouse_ttls(&state).await;

    // --- Start Gateway server (AI API + MCP) ---
    let gateway_app = app::create_gateway_app(&config, state.clone()).await?;
    let gateway_addr = config.gateway_addr();
    let gateway_listener = tokio::net::TcpListener::bind(&gateway_addr).await?;
    tracing::info!("Gateway listening on {gateway_addr} (AI API + MCP)");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // First Ctrl+C / SIGTERM → graceful shutdown.
    // Second Ctrl+C → force exit (covers long-lived SSE / keep-alive connections).
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("Graceful shutdown started — press Ctrl+C again to force quit");
        let _ = shutdown_tx.send(true);

        wait_for_shutdown_signal().await;
        tracing::warn!("Forced shutdown");
        std::process::exit(1);
    });

    let mut gw_rx = shutdown_rx.clone();
    let gateway_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(
            gateway_listener,
            gateway_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = gw_rx.wait_for(|&v| v).await;
        })
        .await
        {
            tracing::error!("Gateway server crashed: {e}");
        }
    });

    // --- Start Console server (Web UI + management API) ---
    let console_app = app::create_console_app(&config, state)?;
    let console_addr = config.console_addr();
    let console_listener = tokio::net::TcpListener::bind(&console_addr).await?;
    tracing::info!("Console listening on {console_addr} (Web UI + Admin API)");

    let mut con_rx = shutdown_rx.clone();
    let console_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(
            console_listener,
            console_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = con_rx.wait_for(|&v| v).await;
        })
        .await
        {
            tracing::error!("Console server crashed: {e}");
        }
    });

    let _ = tokio::join!(gateway_handle, console_handle);
    tracing::info!("Both servers stopped, exiting");

    // Touch unused symbols imported by the binary face only — keeps a
    // single import list so the lib + bin share their `audit`
    // dependency without a "warning: unused import" in either.
    let _ = audit::AuditEntry::new("noop");

    Ok(())
}

/// Resolve when the process receives SIGTERM (Kubernetes rolling
/// restart, `kubectl delete pod`, container stop) or SIGINT
/// (operator Ctrl+C in a terminal). On non-unix platforms only the
/// ctrl_c future is wired up — SIGTERM is unix-specific.
async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to install ctrl_c handler: {e}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!("failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT received, starting graceful shutdown"),
        _ = terminate => tracing::info!("SIGTERM received, starting graceful shutdown"),
    }
}
