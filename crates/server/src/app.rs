use axum::{
    http::{header, HeaderValue, Method},
    routing::{delete, get, post},
    Router,
};
use fred::clients::Client;
use sqlx::PgPool;
use std::sync::Arc;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use agent_bastion_auth::jwt::JwtManager;
use agent_bastion_auth::oidc::OidcManager;
use agent_bastion_common::audit::AuditLogger;
use agent_bastion_common::config::AppConfig;
use agent_bastion_gateway::cache::ResponseCache;
use agent_bastion_gateway::content_filter::ContentFilter;
use agent_bastion_gateway::model_mapping::ModelMapper;
use agent_bastion_gateway::budget_alert::{BudgetAlertConfig, BudgetAlertManager};
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
    pub audit: AuditLogger,
    pub oidc: Option<OidcManager>,
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
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
}

// ---------------------------------------------------------------------------
// Gateway server (port 3000) — AI API + MCP, exposed to downstream clients
// ---------------------------------------------------------------------------

pub fn create_gateway_app(
    config: &AppConfig,
    state: AppState,
    jwt: Arc<JwtManager>,
) -> Router {
    // AI Gateway: /v1/*
    let model_router = Arc::new(ModelRouter::new());
    let gateway_state = GatewayState {
        router: model_router,
        model_mapper: Arc::new(ModelMapper::new()),
        content_filter: Arc::new(ContentFilter::new()),
        quota: Arc::new(QuotaManager::new(state.redis.clone())),
        cache: Arc::new(ResponseCache::new(state.redis.clone(), 3600)),
        pii_redactor: Arc::new(PiiRedactor::new()),
        budget_alert: Some(Arc::new(BudgetAlertManager::new(
            state.redis.clone(),
            BudgetAlertConfig {
                webhook_url: None, // Configured via admin API
                thresholds: vec![0.50, 0.80, 0.95],
            },
        ))),
    };
    let ai_routes = Router::new()
        .route("/v1/chat/completions", post(gateway_proxy::proxy_chat_completion))
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

    // Health check
    let health = Router::new()
        .route("/health", get(handlers::health::health_check));

    let app = Router::new()
        .merge(health)
        .merge(ai_routes)
        .merge(mcp_routes)
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
            .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::PATCH, Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
            .allow_credentials(true)
    };

    // Public auth routes
    let public_routes = Router::new()
        .route("/api/auth/login", post(handlers::auth::login))
        .route("/api/auth/register", post(handlers::auth::register))
        .route("/api/auth/refresh", post(handlers::auth::refresh))
        .route("/api/auth/sso/authorize", get(handlers::sso::sso_authorize))
        .route("/api/auth/sso/callback", get(handlers::sso::sso_callback));

    // User-level routes (any authenticated user)
    // Signature verification runs on POST/DELETE/PATCH (skipped for GET)
    let user_routes = Router::new()
        .route("/api/auth/me", get(handlers::auth::me))
        .route(
            "/api/keys",
            get(handlers::api_keys::list_keys).post(handlers::api_keys::create_key),
        )
        .route(
            "/api/keys/{id}",
            get(handlers::api_keys::get_key).delete(handlers::api_keys::revoke_key),
        )
        .route("/api/mcp/tools", get(handlers::mcp_tools::list_tools))
        .route("/api/audit/logs", get(handlers::audit::list_audit_logs))
        .route("/api/analytics/usage", get(handlers::analytics::get_usage))
        .route("/api/analytics/usage/stats", get(handlers::analytics::get_usage_stats))
        .route("/api/analytics/costs", get(handlers::analytics::get_costs))
        .route("/api/analytics/costs/stats", get(handlers::analytics::get_cost_stats))
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
            get(handlers::providers::get_provider).delete(handlers::providers::delete_provider),
        )
        .route(
            "/api/mcp/servers",
            get(handlers::mcp_servers::list_servers).post(handlers::mcp_servers::create_server),
        )
        .route(
            "/api/mcp/servers/{id}",
            get(handlers::mcp_servers::get_server).delete(handlers::mcp_servers::delete_server),
        )
        .route(
            "/api/mcp/servers/{id}/discover",
            post(handlers::mcp_tools::discover_tools),
        )
        .route(
            "/api/admin/users",
            get(handlers::admin::list_users).post(handlers::admin::create_user),
        )
        .route("/api/admin/settings/system", get(handlers::admin::get_system_settings))
        .route("/api/admin/settings/oidc", get(handlers::admin::get_oidc_settings))
        .route("/api/admin/settings/audit", get(handlers::admin::get_audit_settings))
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
        .route("/health", get(handlers::health::health_check));

    let app = Router::new()
        .merge(health)
        .merge(public_routes)
        .merge(user_routes)
        .merge(admin_routes)
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
        .layer(cors)
        .with_state(state);

    security_layers(app)
}
