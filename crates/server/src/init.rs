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
use think_watch_common::tasks::supervise;

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

    // Idempotent at-rest encryption backfill: re-wrap any provider
    // header values or AWS secrets still stored in plaintext under
    // `providers.config_json`. Runs once per boot; no-op once every
    // row is in the `{"$enc": "<b64>"}` shape.
    if let Err(e) = app::backfill_provider_secrets(&pool, &config.encryption_key).await {
        // Don't block startup if backfill fails — the read path still
        // handles legacy plaintext (with a warn). The next admin
        // re-save will encrypt the row.
        tracing::error!("Provider secret backfill failed (continuing): {e}");
    }

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

    let crypto_key = think_watch_common::crypto::parse_encryption_key(&config.encryption_key)
        .map_err(|e| anyhow::anyhow!("invalid ENCRYPTION_KEY: {e}"))?;
    // `redirect::Policy::none()` is the SSRF defense — without it
    // reqwest follows up to 10 redirects, which silently bypasses
    // any validate_url call we did at save time: admin saves a
    // safe-looking https://attacker.example.com URL, attacker
    // returns `302 Location: http://169.254.169.254/...`, we
    // follow into the AWS metadata endpoint. Force callers to
    // handle redirects explicitly (and re-validate each hop) by
    // disabling automatic follow.
    let init_http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(init_http_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let user_token_resolver = think_watch_mcp_gateway::user_token::UserTokenResolver::new(
        pool.clone(),
        crypto_key,
        init_http_client.clone(),
        think_watch_mcp_gateway::cache::McpResponseCache::new(redis.clone()),
    );

    // Build the cost tracker once and share it between AppState
    // (so the platform-pricing PATCH handler can invalidate the
    // baseline cache on edit) and the GatewayState assembled in
    // `build_gateway_state` (the actual cost-attribution hot path).
    let weight_cache = think_watch_common::limits::weight::WeightCache::new();
    let cost_tracker = Arc::new(think_watch_gateway::cost_tracker::CostTracker::new(
        pool.clone(),
        weight_cache.clone(),
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
        http_client: Arc::new(arc_swap::ArcSwap::from_pointee(init_http_client)),
        gateway_router,
        weight_cache,
        user_token_resolver,
        url_validator: crate::app::production_url_validator(),
        cost_tracker,
    };

    Ok(state)
}

fn audit_config(config: &AppConfig) -> AuditConfig {
    config.audit_config()
}

async fn build_oidc(config: &AppConfig, dc: &DynamicConfig) -> Option<OidcManager> {
    let oidc_config = match crate::oidc_helpers::active_config(dc, config).await {
        Ok(Some(c)) => c,
        Ok(None) => return None,
        Err(e) => {
            tracing::error!("Failed to assemble OIDC config at startup: {e}");
            return None;
        }
    };
    match OidcManager::discover(&oidc_config).await {
        Ok(mgr) => Some(mgr),
        Err(e) => {
            tracing::error!(issuer = %oidc_config.issuer_url, "OIDC discovery failed; SSO disabled: {e}");
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
        use think_watch_common::audit::{AuditActor, SystemActor};
        audit_for_cb.log(
            SystemActor
                .audit("provider.circuit_open")
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
    // Wrap in `supervise()` so a panic inside the reload (e.g.
    // load_content_filter blowing up on a malformed
    // system_settings.value blob) emits a metric +
    // `supervised_task_panics_total{task=…}` instead of silently
    // killing multi-instance config sync until the pod restarts.
    // Full re-spawn isn't trivial here because the closure moves
    // ownership of the SubscriberClient — the supervisor would need
    // a fresh subscription per attempt — so this is panic-observability
    // only, not auto-recovery. Restart is still the operator's job.
    supervise("config_filter_reload_subscriber", async move {
        use fred::interfaces::{EventInterface, PubsubInterface};
        let mut rx = sub_filters.message_rx();
        if let Err(e) = sub_filters.subscribe("config:changed").await {
            tracing::warn!("Filter reload subscriber failed: {e}");
            return;
        }
        // `while let Ok(...)` would silently kill this task on the
        // first broadcast `Lagged` or transient `Closed` — filters /
        // HTTP / MCP-pool hot-reload would silently break until
        // restart. Loop forever, treat Lagged as "reload now to catch
        // up", exit cleanly only on a final Closed.
        let do_reload = async |dc: &Arc<DynamicConfig>,
                               cf: &arc_swap::ArcSwap<
            think_watch_gateway::content_filter::ContentFilter,
        >,
                               pii: &arc_swap::ArcSwap<
            think_watch_gateway::pii_redactor::PiiRedactor,
        >,
                               http: &arc_swap::ArcSwap<reqwest::Client>,
                               pool: &arc_swap::ArcSwap<
            think_watch_mcp_gateway::pool::ConnectionPool,
        >| {
            if let Err(e) = dc.reload().await {
                tracing::warn!("Failed to reload dynamic config: {e}");
                return;
            }
            let new_filter = app::load_content_filter(dc).await;
            cf.store(Arc::new(new_filter));
            let new_pii = app::load_pii_redactor(dc).await;
            pii.store(Arc::new(new_pii));
            let http_secs = dc.perf_http_client_secs().await as u64;
            let new_http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(http_secs))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());
            http.store(Arc::new(new_http));
            let pool_secs = dc.perf_mcp_pool_secs().await as u64;
            let new_pool = think_watch_mcp_gateway::pool::ConnectionPool::with_timeout(pool_secs);
            pool.store(Arc::new(new_pool));
            tracing::info!("Hot-reloaded filters, HTTP client, and MCP pool");
        };

        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if msg.channel == "config:changed" {
                        do_reload(&dc_clone, &cf_clone, &pii_clone, &http_clone, &pool_clone).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("filter reload subscriber lagged by {n} messages; reloading");
                    do_reload(&dc_clone, &cf_clone, &pii_clone, &http_clone, &pool_clone).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("filter reload subscriber channel closed; exiting task");
                    return;
                }
            }
        }
    });

    // Gateway router cross-instance reload. Provider / model / route
    // CRUD on any replica publishes on this channel; every other
    // replica's subscriber rebuilds its local `ArcSwap<ModelRouter>`.
    // Without this, multi-instance deployments had per-replica stale
    // routers between CRUD time and the next process restart.
    let sub_router_cfg = fred::types::config::Config::from_url(&state.config.redis_url)?;
    let sub_router: fred::clients::SubscriberClient =
        Builder::from_config(sub_router_cfg).build_subscriber_client()?;
    sub_router.init().await?;
    let router_state = state.clone();
    // Same `supervise()` rationale as the filter subscriber above: a
    // panic inside the router rebuild (malformed provider row,
    // bad ModelRouter::insert input) would otherwise silently kill
    // cross-instance routing sync.
    supervise("config_router_reload_subscriber", async move {
        use fred::interfaces::{EventInterface, PubsubInterface};
        let mut rx = sub_router.message_rx();
        if let Err(e) = sub_router
            .subscribe(app::GATEWAY_ROUTER_CHANGED_CHANNEL)
            .await
        {
            tracing::warn!("Gateway-router reload subscriber failed: {e}");
            return;
        }
        loop {
            match rx.recv().await {
                Ok(msg) if msg.channel == app::GATEWAY_ROUTER_CHANGED_CHANNEL => {
                    // Build a new router locally; don't re-publish so a
                    // single CRUD doesn't fan out into a publish storm.
                    let mut new_router = think_watch_gateway::router::ModelRouter::new();
                    if let Err(e) =
                        app::load_providers_into_router(&router_state, &mut new_router).await
                    {
                        tracing::error!(
                            "Failed to rebuild gateway router after pub/sub notify: {e}"
                        );
                        continue;
                    }
                    router_state.gateway_router.store(Arc::new(new_router));
                    tracing::info!("Gateway router hot-reloaded via pub/sub");
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        "gateway_router:changed subscriber lagged by {n}; reloading anyway"
                    );
                    let mut new_router = think_watch_gateway::router::ModelRouter::new();
                    if let Err(e) =
                        app::load_providers_into_router(&router_state, &mut new_router).await
                    {
                        tracing::error!("Failed to rebuild gateway router on lag: {e}");
                        continue;
                    }
                    router_state.gateway_router.store(Arc::new(new_router));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!(
                        "gateway_router:changed subscriber channel closed; exiting task"
                    );
                    return;
                }
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
