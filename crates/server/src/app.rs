use axum::{
    Router,
    http::{HeaderValue, Method, header},
    routing::{delete, get, patch, post},
};
use fred::clients::Client;
use sqlx::PgPool;
use std::sync::Arc;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use think_watch_auth::jwt::JwtManager;
use think_watch_auth::oidc::OidcManager;
use think_watch_common::audit::AuditLogger;
use think_watch_common::config::AppConfig;
use think_watch_common::dynamic_config::DynamicConfig;
use think_watch_gateway::cache::ResponseCache;
use think_watch_gateway::content_filter::ContentFilter;
use think_watch_gateway::model_mapping::ModelMapper;
use think_watch_gateway::pii_redactor::PiiRedactor;
use think_watch_gateway::proxy::{self as gateway_proxy, GatewayState};
use think_watch_gateway::quota::QuotaManager;
use think_watch_gateway::router::ModelRouter;
use think_watch_mcp_gateway::access_control::AccessController;
use think_watch_mcp_gateway::proxy::McpProxy;
use think_watch_mcp_gateway::session::SessionManager;
use think_watch_mcp_gateway::transport::streamable_http::{self, McpGatewayState};

use crate::handlers;

/// Shared state accessible by both gateway and console servers.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub redis: Client,
    pub jwt: Arc<JwtManager>,
    pub config: AppConfig,
    pub dynamic_config: Arc<DynamicConfig>,
    pub audit: AuditLogger,
    /// OIDC manager — wrapped in Arc<RwLock> so it can be reloaded when
    /// SSO settings change via the admin UI without restarting the server.
    pub oidc: Arc<tokio::sync::RwLock<Option<OidcManager>>>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// ClickHouse client for log queries. `None` if ClickHouse is not configured.
    pub clickhouse: Option<clickhouse::Client>,
    /// Hot-swappable content filter — admin updates trigger a reload via
    /// `reload_content_filter()` without restarting the server.
    pub content_filter: Arc<arc_swap::ArcSwap<ContentFilter>>,
    /// Hot-swappable PII redactor.
    pub pii_redactor: Arc<arc_swap::ArcSwap<PiiRedactor>>,
    /// In-memory registry of upstream MCP servers. Shared between the MCP
    /// gateway runtime and the console CRUD handlers so that adding/removing
    /// a server in the admin UI is reflected immediately, without restart.
    pub mcp_registry: think_watch_mcp_gateway::registry::Registry,
    /// Per-MCP-server circuit breakers. Lives in AppState so the console
    /// CRUD handlers can pre-register newly added servers.
    pub mcp_circuit_breakers: think_watch_mcp_gateway::circuit_breaker::McpCircuitBreakers,
    /// Connection pool for upstream MCP servers. Shared so the update/delete
    /// CRUD handlers can evict stale connections when an endpoint changes —
    /// the pool keys by server id, so a renamed endpoint would otherwise
    /// keep using the old URL.
    pub mcp_pool: think_watch_mcp_gateway::pool::ConnectionPool,
    /// Shared HTTP client used by tool discovery and any future
    /// outbound calls. Reusing one client preserves the connection pool
    /// (TCP + TLS) instead of paying TCP/TLS handshake on every call.
    pub http_client: reqwest::Client,
}

/// Build a `ContentFilter` from the current `system_settings` value.
pub async fn load_content_filter(dc: &DynamicConfig) -> ContentFilter {
    let configs: Vec<think_watch_gateway::content_filter::DenyRuleConfig> = dc
        .get_json("security.content_filter_patterns")
        .await
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    ContentFilter::from_config(&configs)
}

/// Build a `PiiRedactor` from the current `system_settings` value.
pub async fn load_pii_redactor(dc: &DynamicConfig) -> PiiRedactor {
    let configs: Vec<think_watch_gateway::pii_redactor::PiiPatternConfig> = dc
        .get_json("security.pii_redactor_patterns")
        .await
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    PiiRedactor::from_config(&configs)
}

/// Common security layers applied to both servers.
fn security_layers<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("strict-transport-security"),
            HeaderValue::from_static("max-age=63072000; includeSubDomains; preload"),
        ))
        .layer(CatchPanicLayer::new())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<_>| {
                    tracing::info_span!(
                        "http",
                        method = %request.method(),
                        path = %request.uri().path(),
                    )
                })
                .on_failure(
                    |error: tower_http::classify::ServerErrorsFailureClass,
                     latency: std::time::Duration,
                     _span: &tracing::Span| {
                        tracing::error!(%error, latency_ms = latency.as_millis(), "request failed");
                    },
                ),
        )
}

// ---------------------------------------------------------------------------
// Gateway server (port 3000) — AI API + MCP, exposed to downstream clients
// ---------------------------------------------------------------------------

pub async fn create_gateway_app(_config: &AppConfig, state: AppState) -> Router {
    // Load dynamic config values for gateway initialization
    let dc = &state.dynamic_config;
    let cache_ttl = dc.cache_ttl_secs().await;

    // AI Gateway: /v1/*
    // Load providers from database. Failure is logged loudly but
    // does NOT abort startup — that would block the chicken-and-egg
    // case of a fresh deployment that has zero providers and needs
    // the console (port 3001) to be reachable so the admin can add
    // the first one. The `/health/ready` probe checks the providers
    // table directly, so K8s won't route AI traffic to a pod with
    // an empty router.
    let mut model_router = ModelRouter::new();
    if let Err(e) = load_providers_into_router(&state, &mut model_router).await {
        metrics::counter!("gateway_provider_load_failed_total").increment(1);
        tracing::error!(
            "Failed to load providers from database; gateway will start with empty router \
             and /health/ready will return 503 until providers are configured: {e}"
        );
    }
    let model_router = Arc::new(model_router);
    let gateway_state = GatewayState {
        router: model_router,
        model_mapper: Arc::new(ModelMapper::new()),
        // Share the hot-swappable filter handles with the gateway state.
        content_filter: state.content_filter.clone(),
        quota: Arc::new(QuotaManager::new(state.redis.clone())),
        cache: Arc::new(ResponseCache::new(state.redis.clone(), cache_ttl)),
        pii_redactor: state.pii_redactor.clone(),
        cost_tracker: Arc::new(think_watch_gateway::cost_tracker::CostTracker::new()),
        rate_limiter: Arc::new(think_watch_gateway::rate_limiter::RateLimiter::new(
            state.redis.clone(),
        )),
        db: state.db.clone(),
        redis: state.redis.clone(),
        weight_cache: think_watch_common::limits::weight::WeightCache::new(),
        dynamic_config: state.dynamic_config.clone(),
    };
    let ai_routes = Router::new()
        .route(
            "/v1/chat/completions",
            post(gateway_proxy::proxy_chat_completion),
        )
        .route(
            "/v1/messages",
            post(gateway_proxy::proxy_anthropic_messages),
        )
        .route("/v1/responses", post(gateway_proxy::proxy_responses))
        .route("/v1/models", get(gateway_proxy::list_models_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::api_key_auth::require_api_key("ai_gateway"),
        ))
        .with_state(gateway_state);

    // MCP Gateway: /mcp
    //
    // Use the shared registry + circuit breakers from AppState so that the
    // console CRUD handlers can sync runtime state when admins add/remove
    // servers, and so the dashboard can reflect real CB state.
    let registry = state.mcp_registry.clone();
    let access_controller = AccessController::new();
    let pool = state.mcp_pool.clone();

    // Initial load: hydrate the in-memory registry from Postgres so the
    // gateway can actually proxy to configured upstream servers without a
    // manual restart-time setup. CRUD handlers keep it in sync afterwards.
    if let Err(e) = crate::mcp_runtime::load_mcp_servers_into_registry(&state, &registry).await {
        tracing::error!("Failed to load MCP servers: {e}");
    }

    // Pre-register a CB for every loaded server so the dashboard upstream
    // health panel shows them as `Closed` immediately on first paint.
    for server in registry.list().await {
        state.mcp_circuit_breakers.register(&server.name).await;
    }

    // Background health check loop — keeps the in-memory registry status
    // fresh AND mirrors the result back to `mcp_servers.status` so the
    // admin UI sees real liveness without a manual probe.
    crate::mcp_runtime::spawn_mcp_health_loop(
        state.clone(),
        registry.clone(),
        pool.clone(),
        state.config.timeouts.mcp_health_interval_secs,
    );

    let mut mcp_proxy = McpProxy::new(
        registry,
        access_controller,
        pool,
        state.db.clone(),
        state.redis.clone(),
        state.dynamic_config.clone(),
    );
    // Wire the shared CB registry into the proxy so per-server breakers
    // are visible to the dashboard handler.
    mcp_proxy.circuit_breakers = state.mcp_circuit_breakers.clone();
    let session_manager = SessionManager::with_redis(state.redis.clone());
    let mcp_state = Arc::new(McpGatewayState {
        proxy: mcp_proxy,
        sessions: session_manager,
    });
    let mcp_routes = Router::new()
        .route("/mcp", post(streamable_http::handle_post))
        .route("/mcp", delete(streamable_http::handle_delete))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::api_key_auth::require_api_key("mcp_gateway"),
        ))
        .with_state(mcp_state);

    // Health check routes
    let health = Router::new()
        .route("/health", get(handlers::health::health_check))
        .route("/health/live", get(handlers::health::liveness))
        .route(
            "/health/ready",
            get(handlers::health::readiness).with_state(state.clone()),
        );

    // Prometheus `/metrics` endpoint — opt-in via env.
    //
    // The Prometheus recorder is always installed (so metrics are
    // still collected and available for any future scraping path),
    // but the HTTP route is only mounted when `METRICS_BEARER_TOKEN`
    // is set. Rationale: /metrics on the public gateway port leaks
    // cost / token-usage / error signals, so the safe default is
    // "endpoint does not exist" rather than "endpoint exists with
    // no auth". Operators who want scraping set the token (the
    // `deploy/generate-secrets.sh` script does this automatically),
    // and Prometheus passes it via `Authorization: Bearer <value>`.
    //
    // Failing to set the token does not affect any other startup
    // path — only the /metrics route is skipped.
    let prom_handle = handlers::metrics::install_prometheus_recorder();
    let metrics_route = match std::env::var("METRICS_BEARER_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(token) => {
            tracing::info!("/metrics endpoint enabled (bearer auth required)");
            let metrics_state = handlers::metrics::MetricsState {
                handle: prom_handle,
                bearer_token: token,
            };
            Some(
                Router::new()
                    .route("/metrics", get(handlers::metrics::prometheus_metrics))
                    .with_state(metrics_state),
            )
        }
        None => {
            tracing::info!(
                "/metrics endpoint disabled — set METRICS_BEARER_TOKEN to enable Prometheus scraping"
            );
            None
        }
    };

    // CORS for gateway — allow configured origins (same as console)
    let gateway_cors = {
        let origins: Vec<HeaderValue> = _config
            .cors_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();

        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
            .allow_credentials(true)
    };

    let mut app = Router::new().merge(health);
    if let Some(mr) = metrics_route {
        app = app.merge(mr);
    }
    let app = app
        .merge(ai_routes)
        .merge(mcp_routes)
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024)) // 10MB for large prompts
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(120), // longer timeout for streaming
        ))
        .layer(gateway_cors)
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
        ))
        .layer(crate::middleware::access_log::AccessLogLayer::new(
            state.audit.clone(),
            state.dynamic_config.clone(),
            _config.gateway_port,
        ))
        .with_state(state.clone());

    security_layers(app)
}

// ---------------------------------------------------------------------------
// Console server (port 3001) — Web UI + management API, internal only
// ---------------------------------------------------------------------------

pub fn create_console_app(config: &AppConfig, state: AppState) -> Router {
    let cors = {
        let origins: Vec<HeaderValue> = config
            .cors_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();

        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::DELETE,
                Method::PATCH,
                Method::OPTIONS,
            ])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
            .allow_credentials(true)
    };

    // Public auth routes
    let public_routes = Router::new()
        .route("/api/auth/login", post(handlers::auth::login))
        .route("/api/auth/register", post(handlers::auth::register))
        .route("/api/auth/refresh", post(handlers::auth::refresh))
        .route("/api/auth/sso/authorize", get(handlers::sso::sso_authorize))
        .route("/api/auth/sso/callback", get(handlers::sso::sso_callback))
        .route("/api/auth/sso/status", get(handlers::health::sso_status))
        // Setup routes (public, guarded by initialization check)
        .route("/api/setup/status", get(handlers::setup::setup_status))
        .route(
            "/api/setup/initialize",
            post(handlers::setup::setup_initialize),
        )
        .route(
            "/api/setup/test-provider",
            post(handlers::providers::test_provider_unauthenticated),
        )
        // Dashboard live WebSocket — auth is performed by atomically
        // consuming a single-use ticket that the client minted via
        // POST /api/dashboard/ws-ticket (which IS authenticated). The
        // ticket lives for 30s and is bound to the user_id.
        .route("/api/dashboard/ws", get(handlers::dashboard::dashboard_ws));

    // User-level routes (any authenticated user)
    // Signature verification runs on POST/DELETE/PATCH (skipped for GET)
    let user_routes = Router::new()
        .route("/api/auth/me", get(handlers::auth::me))
        .route("/api/auth/password", post(handlers::auth::change_password))
        .route("/api/auth/account", delete(handlers::auth::delete_account))
        .route(
            "/api/auth/revoke-sessions",
            post(handlers::auth::revoke_sessions),
        )
        // TOTP management
        .route("/api/auth/totp/status", get(handlers::auth::totp_status))
        .route("/api/auth/totp/setup", post(handlers::auth::totp_setup))
        .route(
            "/api/auth/totp/verify-setup",
            post(handlers::auth::totp_verify_setup),
        )
        .route("/api/auth/totp/disable", post(handlers::auth::totp_disable))
        .route(
            "/api/keys",
            get(handlers::api_keys::list_keys).post(handlers::api_keys::create_key),
        )
        .route(
            "/api/keys/expiring",
            get(handlers::api_keys::list_expiring_keys),
        )
        .route(
            "/api/keys/{id}",
            get(handlers::api_keys::get_key)
                .patch(handlers::api_keys::update_key)
                .delete(handlers::api_keys::revoke_key),
        )
        .route(
            "/api/keys/{id}/rotate",
            post(handlers::api_keys::rotate_key),
        )
        .route(
            "/api/dashboard/stats",
            get(handlers::dashboard::get_dashboard_stats),
        )
        .route(
            "/api/dashboard/live",
            get(handlers::dashboard::get_dashboard_live),
        )
        .route(
            "/api/dashboard/ws-ticket",
            post(handlers::dashboard::create_dashboard_ws_ticket),
        )
        .route("/api/mcp/tools", get(handlers::mcp_tools::list_tools))
        .route("/api/mcp/logs", get(handlers::mcp_logs::list_mcp_logs))
        .route(
            "/api/gateway/logs",
            get(handlers::gateway_logs::list_gateway_logs),
        )
        .route("/api/audit/logs", get(handlers::audit::list_audit_logs))
        .route("/api/analytics/usage", get(handlers::analytics::get_usage))
        .route(
            "/api/analytics/usage/stats",
            get(handlers::analytics::get_usage_stats),
        )
        .route("/api/analytics/costs", get(handlers::analytics::get_costs))
        .route(
            "/api/analytics/costs/stats",
            get(handlers::analytics::get_cost_stats),
        )
        .route("/api/health", get(handlers::health::api_health_check))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::verify_signature::verify_signature,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::auth_guard::require_auth,
        ));

    // Admin routes (admin/super_admin role required)
    let admin_routes = Router::new()
        .route(
            "/api/admin/providers",
            get(handlers::providers::list_providers).post(handlers::providers::create_provider),
        )
        .route(
            "/api/admin/providers/test",
            post(handlers::providers::test_provider),
        )
        .route(
            "/api/admin/providers/{id}",
            get(handlers::providers::get_provider)
                .patch(handlers::providers::update_provider)
                .delete(handlers::providers::delete_provider),
        )
        .route(
            "/api/admin/models",
            get(handlers::models::list_models).post(handlers::models::create_model),
        )
        .route(
            "/api/admin/models/{id}",
            patch(handlers::models::update_model).delete(handlers::models::delete_model),
        )
        .route(
            "/api/mcp/servers",
            get(handlers::mcp_servers::list_servers).post(handlers::mcp_servers::create_server),
        )
        .route(
            "/api/mcp/servers/{id}",
            get(handlers::mcp_servers::get_server)
                .patch(handlers::mcp_servers::update_server)
                .delete(handlers::mcp_servers::delete_server),
        )
        .route(
            "/api/mcp/servers/{id}/discover",
            post(handlers::mcp_tools::discover_tools),
        )
        .route(
            "/api/admin/users",
            get(handlers::admin::list_users).post(handlers::admin::create_user),
        )
        .route(
            "/api/admin/users/{id}",
            patch(handlers::admin::update_user).delete(handlers::admin::delete_user),
        )
        .route(
            "/api/admin/users/{id}/force-logout",
            post(handlers::admin::force_logout_user),
        )
        .route(
            "/api/admin/users/{id}/reset-password",
            post(handlers::admin::reset_user_password),
        )
        .route(
            "/api/admin/settings/system",
            get(handlers::admin::get_system_settings),
        )
        .route(
            "/api/admin/settings/oidc",
            get(handlers::admin::get_oidc_settings).patch(handlers::admin::update_oidc_settings),
        )
        .route(
            "/api/admin/settings/audit",
            get(handlers::admin::get_audit_settings),
        )
        // Dynamic settings CRUD
        .route(
            "/api/admin/settings",
            get(handlers::admin::get_all_settings).patch(handlers::admin::update_settings),
        )
        .route(
            "/api/admin/settings/category/{category}",
            get(handlers::admin::get_settings_by_category),
        )
        // Content filter sandbox & presets
        .route(
            "/api/admin/settings/content-filter/test",
            post(handlers::admin::test_content_filter),
        )
        .route(
            "/api/admin/settings/content-filter/presets",
            get(handlers::admin::list_content_filter_presets),
        )
        // PII redactor sandbox
        .route(
            "/api/admin/settings/pii-redactor/test",
            post(handlers::admin::test_pii_redactor),
        )
        // Log forwarders CRUD
        .route(
            "/api/admin/log-forwarders",
            get(handlers::log_forwarders::list_forwarders)
                .post(handlers::log_forwarders::create_forwarder),
        )
        .route(
            "/api/admin/log-forwarders/{id}",
            patch(handlers::log_forwarders::update_forwarder)
                .delete(handlers::log_forwarders::delete_forwarder),
        )
        .route(
            "/api/admin/log-forwarders/{id}/toggle",
            post(handlers::log_forwarders::toggle_forwarder),
        )
        .route(
            "/api/admin/log-forwarders/{id}/test",
            post(handlers::log_forwarders::test_forwarder),
        )
        .route(
            "/api/admin/log-forwarders/{id}/reset-stats",
            post(handlers::log_forwarders::reset_stats),
        )
        // Platform operation logs
        .route(
            "/api/admin/platform-logs",
            get(handlers::platform_logs::list_platform_logs),
        )
        // Access logs
        .route(
            "/api/admin/access-logs",
            get(handlers::access_logs::list_access_logs),
        )
        // Application runtime logs
        .route(
            "/api/admin/app-logs",
            get(handlers::app_logs::list_app_logs),
        )
        // Custom roles CRUD
        .route(
            "/api/admin/roles",
            get(handlers::roles::list_roles).post(handlers::roles::create_role),
        )
        .route(
            "/api/admin/roles/{id}",
            patch(handlers::roles::update_role).delete(handlers::roles::delete_role),
        )
        .route(
            "/api/admin/roles/{id}/members",
            get(handlers::roles::list_role_members),
        )
        .route(
            "/api/admin/roles/{id}/history",
            get(handlers::roles::list_role_history),
        )
        .route(
            "/api/admin/roles/{id}/reset",
            post(handlers::roles::reset_role),
        )
        .route(
            "/api/admin/permissions",
            get(handlers::roles::list_permissions),
        )
        // Limits CRUD: rate-limit rules + budget caps + current usage,
        // keyed by (subject_kind, subject_id). All gated by
        // `rate_limits:read` / `rate_limits:write` inside the handler.
        .route(
            "/api/admin/limits/{kind}/{id}/rules",
            get(handlers::limits::list_rules).post(handlers::limits::upsert_rule),
        )
        .route(
            "/api/admin/limits/{kind}/{id}/rules/{rule_id}",
            delete(handlers::limits::delete_rule),
        )
        .route(
            "/api/admin/limits/{kind}/{id}/budgets",
            get(handlers::limits::list_caps).post(handlers::limits::upsert_cap),
        )
        .route(
            "/api/admin/limits/{kind}/{id}/budgets/{cap_id}",
            delete(handlers::limits::delete_cap),
        )
        .route(
            "/api/admin/limits/{kind}/{id}/usage",
            get(handlers::limits::get_usage),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::verify_signature::verify_signature,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::auth_guard::require_auth,
        ));

    // Public health check (no internal details)
    let health = Router::new()
        .route("/health", get(handlers::health::health_check))
        .route("/health/live", get(handlers::health::liveness))
        .route(
            "/health/ready",
            get(handlers::health::readiness).with_state(state.clone()),
        );

    let app = Router::new()
        .merge(health)
        .merge(public_routes)
        .merge(user_routes)
        .merge(admin_routes)
        .layer(RequestBodyLimitLayer::new(1024 * 1024)) // 1MB for console API
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(state.config.timeouts.console_request_secs),
        ))
        .layer(cors)
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'",
            ),
        ))
        .layer(crate::middleware::access_log::AccessLogLayer::new(state.audit.clone(), state.dynamic_config.clone(), config.console_port))
        .with_state(state);

    security_layers(app)
}

// ---------------------------------------------------------------------------
// Provider loading from database
// ---------------------------------------------------------------------------

/// Default model prefix patterns for known provider types.
fn default_model_prefixes(provider_type: &str) -> Vec<&'static str> {
    match provider_type {
        "openai" => vec!["gpt-", "o1-", "o3-", "o4-", "chatgpt-"],
        "anthropic" => vec!["claude-"],
        "google" => vec!["gemini-"],
        "azure_openai" => vec![], // Azure uses deployment names, must register specific models
        "bedrock" => vec![], // Bedrock uses full model IDs like "anthropic.claude-3-5-sonnet-20241022-v2:0"
        _ => vec![],
    }
}

/// Load all active providers from the database, instantiate the appropriate
/// provider implementation, and register them in the model router.
async fn load_providers_into_router(
    state: &AppState,
    router: &mut ModelRouter,
) -> anyhow::Result<()> {
    use think_watch_gateway::providers::{
        anthropic::AnthropicProvider, azure_openai::AzureOpenAiProvider, bedrock::BedrockProvider,
        custom::CustomProvider, google::GoogleProvider, openai::OpenAiProvider,
    };

    let providers = sqlx::query_as::<_, think_watch_common::models::Provider>(
        "SELECT * FROM providers WHERE is_active = true AND deleted_at IS NULL",
    )
    .fetch_all(&state.db)
    .await?;

    let encryption_key =
        think_watch_common::crypto::parse_encryption_key(&state.config.encryption_key)?;

    for provider in &providers {
        let api_key_bytes =
            think_watch_common::crypto::decrypt(&provider.api_key_encrypted, &encryption_key)?;
        let api_key = String::from_utf8(api_key_bytes)?;

        let dyn_provider: Arc<dyn think_watch_gateway::providers::DynAiProvider> =
            match provider.provider_type.as_str() {
                "openai" => Arc::new(OpenAiProvider::new(provider.base_url.clone(), api_key)),
                "anthropic" => Arc::new(AnthropicProvider::new(provider.base_url.clone(), api_key)),
                "google" => Arc::new(GoogleProvider::new(provider.base_url.clone(), api_key)),
                "azure_openai" => {
                    // Azure: base_url is the resource endpoint, api_key is the Azure API key
                    // api_version from config_json or default
                    let api_version = provider
                        .config_json
                        .get("api_version")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    Arc::new(AzureOpenAiProvider::new(
                        provider.base_url.clone(),
                        api_key,
                        api_version,
                    ))
                }
                "bedrock" => {
                    // Bedrock: base_url stores the AWS region, api_key stores "access_key_id:secret_access_key"
                    Arc::new(BedrockProvider::new(provider.base_url.clone(), api_key))
                }
                _ => Arc::new(CustomProvider::new(
                    provider.name.clone(),
                    provider.base_url.clone(),
                    api_key,
                )),
            };

        // Register models from the models table
        let models = sqlx::query_scalar::<_, String>(
            "SELECT model_id FROM models WHERE provider_id = $1 AND is_active = true",
        )
        .bind(provider.id)
        .fetch_all(&state.db)
        .await?;

        if models.is_empty() {
            // No specific models registered — use default prefixes
            for prefix in default_model_prefixes(&provider.provider_type) {
                router.register_provider(prefix, Arc::clone(&dyn_provider), provider.id);
            }
        } else {
            for model_id in &models {
                router.register_provider(model_id, Arc::clone(&dyn_provider), provider.id);
            }
        }

        tracing::info!(
            provider = %provider.name,
            provider_type = %provider.provider_type,
            model_count = if models.is_empty() {
                default_model_prefixes(&provider.provider_type).len()
            } else {
                models.len()
            },
            "Provider loaded"
        );
    }

    tracing::info!(
        total_providers = providers.len(),
        total_routes = router.list_models().len(),
        "All providers loaded into model router"
    );

    Ok(())
}
