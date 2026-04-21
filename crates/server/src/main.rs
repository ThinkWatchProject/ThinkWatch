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
mod handlers;
mod mcp_runtime;
mod middleware;
mod openapi;
mod services;
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

    // Verify every seeded role only references permissions that exist
    // in the static PERMISSION_CATALOG. A mismatch means either the
    // catalog or the seed drifted and authorization would silently
    // misbehave.
    if let Err(e) = handlers::roles::validate_seeded_roles(&pool).await {
        tracing::error!("RBAC catalog validation failed: {e}");
        std::process::exit(1);
    }

    // Verify every persisted rate-limit rule has an allowed window
    // length and every model row has positive weights. Catches
    // hand-edits to the DB before they reach the gateway hot path.
    if let Err(e) = think_watch_common::limits::validate_persisted(&pool).await {
        tracing::error!("Limits validation failed: {e}");
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

    // Initialize ClickHouse tables — block startup with bounded
    // retry (up to 5 attempts with exponential backoff, ~22s total).
    // If all retries fail we log an error and continue rather than
    // refuse to start, but bump `audit_clickhouse_init_failed_total`
    // so dashboards see it.
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
                    metrics::counter!("audit_clickhouse_init_failed_total").increment(1);
                    tracing::error!(
                        "ClickHouse table init failed after {} attempts: {e}. \
                         Audit log inserts will fail until the schema is present.",
                        attempt + 1
                    );
                    break;
                }
            }
        }
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

    // Install the circuit-breaker OPEN listener. Both gateways
    // (AI + MCP) write into the shared cb_registry; this listener
    // emits a single `provider.circuit_open` audit event per
    // Closed→Open / HalfOpen→Open transition, tagged with the
    // caller's `kind` so downstream subscribers can distinguish AI
    // provider outages from MCP server outages.
    {
        let audit_for_cb = audit_logger.clone();
        think_watch_common::cb_registry::set_open_listener(move |key, kind| {
            audit_for_cb.log(
                audit::AuditEntry::new("provider.circuit_open")
                    .resource(format!("{kind}_provider:{key}"))
                    .detail(serde_json::json!({
                        "kind": kind,
                        "provider": key,
                    })),
            );
        });
    }

    // Initialize OIDC from dynamic config (database).
    let oidc_manager = {
        let dc = &dynamic_config;
        let encryption_key =
            think_watch_common::crypto::parse_encryption_key(&config.encryption_key)?;

        // Load OIDC from dynamic config
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
                // OIDC config lives in `system_settings` and is
                // edited by admins through the Web UI. We CANNOT
                // fail-fast on discovery errors here: doing so
                // would lock the admin out (the only place to fix
                // a broken OIDC config is the very console the
                // failing startup prevents from coming up).
                //
                // Instead: log loudly, leave SSO disabled, and
                // bump a metric. The console-side
                // `/api/auth/sso/status` endpoint reads the live
                // OidcManager state, so the UI can render a
                // "configured but unreachable" banner pointing the
                // admin at the misconfig.
                match OidcManager::discover(&issuer, &client_id, &client_secret, &redirect).await {
                    Ok(mgr) => {
                        tracing::info!("OIDC provider discovered successfully");
                        Some(mgr)
                    }
                    Err(e) => {
                        metrics::counter!("auth_oidc_discovery_failed_total").increment(1);
                        tracing::error!(
                            issuer = %issuer,
                            "OIDC discovery failed; SSO disabled until fixed in Admin > Settings: {e}"
                        );
                        None
                    }
                }
            } else {
                tracing::warn!(
                    "OIDC enabled but issuer/client_id/client_secret incomplete; SSO disabled"
                );
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

    // Read perf tunables from DynamicConfig before moving it into AppState.
    let init_http_secs = dynamic_config.perf_http_client_secs().await as u64;
    let init_mcp_pool_secs = dynamic_config.perf_mcp_pool_secs().await as u64;

    // Pre-create the gateway router handle. `create_gateway_app` will
    // load providers and swap in the real router; CRUD handlers can
    // later call `rebuild_gateway_router` to hot-reload.
    let gateway_router = Arc::new(arc_swap::ArcSwap::from_pointee(
        think_watch_gateway::router::ModelRouter::new(),
    ));

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
        mcp_pool: Arc::new(arc_swap::ArcSwap::from_pointee(
            think_watch_mcp_gateway::pool::ConnectionPool::with_timeout(init_mcp_pool_secs),
        )),
        http_client: Arc::new(arc_swap::ArcSwap::from_pointee(
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(init_http_secs))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        )),
        gateway_router: gateway_router.clone(),
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
        let http_clone = state.http_client.clone();
        let pool_clone = state.mcp_pool.clone();
        tokio::spawn(async move {
            use fred::interfaces::{EventInterface, PubsubInterface};
            let mut rx = sub_redis_filters.message_rx();
            if let Err(e) = sub_redis_filters.subscribe("config:changed").await {
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

                    // Rebuild HTTP client / MCP pool if their timeouts changed.
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
    }

    // --- Start background tasks ---
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
    tracing::info!("Background tasks started");

    // Reconcile ClickHouse log table TTLs with the persisted settings on
    // startup. This ensures retention values configured via the admin UI
    // survive server restarts even if the table was created with different
    // defaults.
    handlers::admin::reconcile_clickhouse_ttls(&state).await;

    // --- Start Gateway server (AI API + MCP) ---
    let gateway_app = app::create_gateway_app(&config, state.clone()).await;
    let gateway_addr = config.gateway_addr();
    let gateway_listener = tokio::net::TcpListener::bind(&gateway_addr).await?;
    tracing::info!("Gateway listening on {gateway_addr} (AI API + MCP)");

    // Graceful shutdown:
    //   1. wait_for_shutdown_signal() resolves when the process gets
    //      SIGTERM (k8s rolling upgrade) or SIGINT (operator Ctrl+C).
    //   2. Both axum servers are wired with `with_graceful_shutdown`
    //      so they stop accepting new connections AND drain in-flight
    //      ones before returning.
    //   3. We wait on both server JoinHandles so the process doesn't
    //      exit until they've actually finished draining — if a long
    //      streaming response is in-flight when SIGTERM arrives, it
    //      keeps running until the response completes (or until the
    //      orchestrator's grace period kills the pod).
    //
    // Background tasks (audit worker, retention sweep, config
    // subscriber) are still detached `tokio::spawn`s — they don't
    // hold critical state past the request boundary, and once the
    // tokio runtime is dropped at the end of `main` they get torn
    // down anyway. The win here is the in-flight requests, which is
    // what k8s actually cares about.
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
    let console_app = app::create_console_app(&config, state);
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
