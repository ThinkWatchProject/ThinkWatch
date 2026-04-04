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

use agent_bastion_auth::jwt::JwtManager;
use agent_bastion_auth::oidc::OidcManager;
use agent_bastion_common::audit::AuditLogger;
use agent_bastion_common::config::AppConfig;
use agent_bastion_common::dynamic_config::DynamicConfig;
use agent_bastion_gateway::budget_alert::{BudgetAlertConfig, BudgetAlertManager};
use agent_bastion_gateway::cache::ResponseCache;
use agent_bastion_gateway::content_filter::ContentFilter;
use agent_bastion_gateway::model_mapping::ModelMapper;
use agent_bastion_gateway::pii_redactor::PiiRedactor;
use agent_bastion_gateway::proxy::{self as gateway_proxy, GatewayState};
use agent_bastion_gateway::quota::QuotaManager;
use agent_bastion_gateway::router::ModelRouter;
use agent_bastion_mcp_gateway::access_control::AccessController;
use agent_bastion_mcp_gateway::pool::ConnectionPool;
use agent_bastion_mcp_gateway::proxy::McpProxy;
use agent_bastion_mcp_gateway::registry::Registry;
use agent_bastion_mcp_gateway::session::SessionManager;
use agent_bastion_mcp_gateway::transport::streamable_http::{self, McpGatewayState};

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
    pub oidc: Option<OidcManager>,
    pub started_at: chrono::DateTime<chrono::Utc>,
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
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ))
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
}

// ---------------------------------------------------------------------------
// Gateway server (port 3000) — AI API + MCP, exposed to downstream clients
// ---------------------------------------------------------------------------

pub async fn create_gateway_app(
    _config: &AppConfig,
    state: AppState,
    jwt: Arc<JwtManager>,
) -> Router {
    // Load dynamic config values for gateway initialization
    let dc = &state.dynamic_config;
    let cache_ttl = dc.cache_ttl_secs().await;
    let budget_thresholds = dc.budget_alert_thresholds().await;
    let budget_webhook = dc.budget_webhook_url().await;

    // Load content filter patterns from dynamic config
    let content_filter = match dc.get_json("security.content_filter_patterns").await {
        Some(v) => {
            if let Ok(patterns) = serde_json::from_value::<
                Vec<agent_bastion_gateway::content_filter::DenyPatternConfig>,
            >(v)
            {
                ContentFilter::from_config(&patterns)
            } else {
                ContentFilter::new()
            }
        }
        None => ContentFilter::new(),
    };

    // Load PII redactor patterns from dynamic config
    let pii_redactor = match dc.get_json("security.pii_redactor_patterns").await {
        Some(v) => {
            if let Ok(patterns) = serde_json::from_value::<
                Vec<agent_bastion_gateway::pii_redactor::PiiPatternConfig>,
            >(v)
            {
                PiiRedactor::from_config(&patterns)
            } else {
                PiiRedactor::new()
            }
        }
        None => PiiRedactor::new(),
    };

    // AI Gateway: /v1/*
    // Load providers from database and register them in the model router
    let mut model_router = ModelRouter::new();
    if let Err(e) = load_providers_into_router(&state, &mut model_router).await {
        tracing::error!("Failed to load providers: {e}");
    }
    let model_router = Arc::new(model_router);
    let gateway_state = GatewayState {
        router: model_router,
        model_mapper: Arc::new(ModelMapper::new()),
        content_filter: Arc::new(content_filter),
        quota: Arc::new(QuotaManager::new(state.redis.clone())),
        cache: Arc::new(ResponseCache::new(state.redis.clone(), cache_ttl)),
        pii_redactor: Arc::new(pii_redactor),
        budget_alert: Some(Arc::new(BudgetAlertManager::new(
            state.redis.clone(),
            BudgetAlertConfig {
                webhook_url: budget_webhook,
                thresholds: budget_thresholds,
            },
        ))),
        cost_tracker: Arc::new(agent_bastion_gateway::cost_tracker::CostTracker::new()),
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
            crate::middleware::api_key_auth::require_api_key_or_jwt,
        ))
        .with_state(gateway_state);

    // MCP Gateway: /mcp
    let registry = Registry::new();
    let access_controller = AccessController::new();
    let pool = ConnectionPool::new();
    let mcp_proxy = McpProxy::new(registry, access_controller, pool);
    let session_manager = SessionManager::new();
    let mcp_state = Arc::new(McpGatewayState {
        proxy: mcp_proxy,
        sessions: session_manager,
        jwt_manager: jwt,
    });
    let mcp_routes = Router::new()
        .route("/mcp", post(streamable_http::handle_post))
        .route("/mcp", delete(streamable_http::handle_delete))
        .with_state(mcp_state);

    // Health check routes
    let health = Router::new()
        .route("/health", get(handlers::health::health_check))
        .route("/health/live", get(handlers::health::liveness))
        .route(
            "/health/ready",
            get(handlers::health::readiness).with_state(state.clone()),
        );

    // Prometheus metrics endpoint
    let prom_handle = handlers::metrics::install_prometheus_recorder();
    let metrics_route = Router::new()
        .route("/metrics", get(handlers::metrics::prometheus_metrics))
        .with_state(prom_handle);

    let app = Router::new()
        .merge(health)
        .merge(metrics_route)
        .merge(ai_routes)
        .merge(mcp_routes)
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024)) // 10MB for large prompts
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(120), // longer timeout for streaming
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
        );

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
            "/api/admin/providers/{id}",
            get(handlers::providers::get_provider)
                .patch(handlers::providers::update_provider)
                .delete(handlers::providers::delete_provider),
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
            "/api/admin/users/{id}/force-logout",
            post(handlers::admin::force_logout_user),
        )
        .route(
            "/api/admin/settings/system",
            get(handlers::admin::get_system_settings),
        )
        .route(
            "/api/admin/settings/oidc",
            get(handlers::admin::get_oidc_settings),
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
            "/api/admin/permissions",
            get(handlers::roles::list_permissions),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::verify_signature::verify_signature,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::require_role::require_admin,
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
            std::time::Duration::from_secs(30),
        ))
        .layer(cors)
        // CSP header for console
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'",
            ),
        ))
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
    use agent_bastion_gateway::providers::{
        anthropic::AnthropicProvider, azure_openai::AzureOpenAiProvider, bedrock::BedrockProvider,
        custom::CustomProvider, google::GoogleProvider, openai::OpenAiProvider,
    };

    let providers = sqlx::query_as::<_, agent_bastion_common::models::Provider>(
        "SELECT * FROM providers WHERE is_active = true AND deleted_at IS NULL",
    )
    .fetch_all(&state.db)
    .await?;

    let encryption_key =
        agent_bastion_common::crypto::parse_encryption_key(&state.config.encryption_key)?;

    for provider in &providers {
        let api_key_bytes =
            agent_bastion_common::crypto::decrypt(&provider.api_key_encrypted, &encryption_key)?;
        let api_key = String::from_utf8(api_key_bytes)?;

        let dyn_provider: Arc<dyn agent_bastion_gateway::providers::DynAiProvider> =
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
                router.register_provider(prefix, Arc::clone(&dyn_provider));
            }
        } else {
            for model_id in &models {
                router.register_provider(model_id, Arc::clone(&dyn_provider));
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
