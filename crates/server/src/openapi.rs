use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::handlers::{
    access_logs::{AccessLogEntry, AccessLogsResponse},
    admin::UpdateSettingsRequest,
    analytics::{CostRow, CostStats, UsageRow, UsageStats},
    api_keys::UpdateKeyRequest,
    app_logs::{AppLogEntryResponse, AppLogsResponse},
    auth::{ChangePasswordRequest, TotpSetupResponse, TotpVerifyRequest},
    dashboard::{
        DashboardLive, DashboardStats, LiveLogRow, ProviderHealth, RpmBucket, WsTicketResponse,
    },
    gateway_logs::{GatewayLogEntry, GatewayLogsResponse},
    limits::{
        CapListResponse, CapRow, CapUsage, RuleListResponse, RuleRow, RuleUsage, UpsertCapRequest,
        UpsertRuleRequest, UsageResponse,
    },
    log_forwarders::{CreateForwarderRequest, TestResult, UpdateForwarderRequest},
    mcp_logs::{McpLogEntry, McpLogsResponse},
    mcp_servers::UpdateMcpServerRequest,
    models::{CreateModelRequest, ModelRow, UpdateModelRequest},
    platform_logs::{PlatformLogEntryResponse, PlatformLogsResponse},
    providers::{TestProviderRequest, TestProviderResponse, UpdateProviderRequest},
    roles::{
        CreateRoleRequest, PermissionDef, RoleHistoryResponse, RoleMember, RoleMembersResponse,
        RoleResponse, RolesListResponse, UpdateRoleRequest,
    },
    setup::{AdminSetup, ProviderSetup, SetupInitRequest, SetupInitResponse, SetupStatusResponse},
    teams::{
        AddMemberRequest, CreateTeamRequest, Team, TeamMemberRow, TeamWithCount, UpdateTeamRequest,
    },
};

/// OpenAPI document covering the ThinkWatch console API (port 3001).
///
/// Authentication: all protected endpoints accept either:
/// - `Authorization: Bearer <jwt>` — issued by `/api/auth/login` or `/api/auth/refresh`
/// - `Authorization: Bearer tw-<key>` — a `tw-` prefixed API key with the `console` surface
///
/// JWT tokens are also delivered / consumed via httpOnly cookies (`access_token`,
/// `refresh_token`) for the browser flow; the Bearer header scheme covers the
/// programmatic / CLI case and is what Swagger UI uses.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "ThinkWatch Console API",
        version = "1.0.0",
        description = "Management API for the ThinkWatch AI gateway platform. Runs on port 3001.",
        contact(name = "ThinkWatch", url = "https://github.com/thinkwatch"),
    ),
    paths(
        // Auth
        crate::handlers::auth::login,
        crate::handlers::auth::register,
        crate::handlers::auth::refresh,
        crate::handlers::auth::logout,
        crate::handlers::auth::me,
        crate::handlers::auth::change_password,
        crate::handlers::auth::delete_account,
        crate::handlers::auth::revoke_sessions,
        crate::handlers::auth::totp_setup,
        crate::handlers::auth::totp_verify_setup,
        crate::handlers::auth::totp_disable,
        crate::handlers::auth::totp_status,
        // API Keys
        crate::handlers::api_keys::list_keys,
        crate::handlers::api_keys::create_key,
        crate::handlers::api_keys::get_key,
        crate::handlers::api_keys::update_key,
        crate::handlers::api_keys::revoke_key,
        crate::handlers::api_keys::rotate_key,
        crate::handlers::api_keys::list_expiring_keys,
        // Setup
        crate::handlers::setup::setup_status,
        crate::handlers::setup::setup_initialize,
        // Users
        crate::handlers::admin::list_users,
        crate::handlers::admin::create_user,
        crate::handlers::admin::update_user,
        crate::handlers::admin::delete_user,
        crate::handlers::admin::force_logout_user,
        crate::handlers::admin::reset_user_password,
        // Settings
        crate::handlers::admin::get_system_settings,
        crate::handlers::admin::get_oidc_settings,
        crate::handlers::admin::update_oidc_settings,
        crate::handlers::admin::get_audit_settings,
        crate::handlers::admin::get_all_settings,
        crate::handlers::admin::get_settings_by_category,
        crate::handlers::admin::update_settings,
        crate::handlers::admin::test_content_filter,
        crate::handlers::admin::list_content_filter_presets,
        crate::handlers::admin::test_pii_redactor,
        // Teams
        crate::handlers::teams::list_teams,
        crate::handlers::teams::get_team,
        crate::handlers::teams::create_team,
        crate::handlers::teams::update_team,
        crate::handlers::teams::delete_team,
        crate::handlers::teams::list_members,
        crate::handlers::teams::add_member,
        crate::handlers::teams::remove_member,
        // Roles & Permissions
        crate::handlers::roles::list_roles,
        crate::handlers::roles::create_role,
        crate::handlers::roles::update_role,
        crate::handlers::roles::reset_role,
        crate::handlers::roles::delete_role,
        crate::handlers::roles::list_role_members,
        crate::handlers::roles::list_permissions,
        crate::handlers::roles::list_role_history,
        // Providers
        crate::handlers::providers::list_providers,
        crate::handlers::providers::create_provider,
        crate::handlers::providers::get_provider,
        crate::handlers::providers::update_provider,
        crate::handlers::providers::delete_provider,
        crate::handlers::providers::test_provider,
        // Models
        crate::handlers::models::list_models,
        crate::handlers::models::create_model,
        crate::handlers::models::update_model,
        crate::handlers::models::delete_model,
        // Rate Limits & Budgets
        crate::handlers::limits::list_rules,
        crate::handlers::limits::upsert_rule,
        crate::handlers::limits::delete_rule,
        crate::handlers::limits::list_caps,
        crate::handlers::limits::upsert_cap,
        crate::handlers::limits::delete_cap,
        crate::handlers::limits::get_usage,
        // Analytics
        crate::handlers::analytics::get_usage_stats,
        crate::handlers::analytics::get_usage,
        crate::handlers::analytics::get_cost_stats,
        crate::handlers::analytics::get_costs,
        // Dashboard
        crate::handlers::dashboard::get_dashboard_stats,
        crate::handlers::dashboard::get_dashboard_live,
        crate::handlers::dashboard::create_dashboard_ws_ticket,
        // Audit & Logs
        crate::handlers::audit::list_audit_logs,
        crate::handlers::gateway_logs::list_gateway_logs,
        crate::handlers::mcp_logs::list_mcp_logs,
        crate::handlers::platform_logs::list_platform_logs,
        crate::handlers::access_logs::list_access_logs,
        crate::handlers::app_logs::list_app_logs,
        // MCP
        crate::handlers::mcp_servers::list_servers,
        crate::handlers::mcp_servers::create_server,
        crate::handlers::mcp_servers::get_server,
        crate::handlers::mcp_servers::update_server,
        crate::handlers::mcp_servers::delete_server,
        crate::handlers::mcp_tools::list_tools,
        crate::handlers::mcp_tools::discover_tools,
        // Log Forwarders
        crate::handlers::log_forwarders::list_forwarders,
        crate::handlers::log_forwarders::create_forwarder,
        crate::handlers::log_forwarders::update_forwarder,
        crate::handlers::log_forwarders::delete_forwarder,
        crate::handlers::log_forwarders::toggle_forwarder,
        crate::handlers::log_forwarders::reset_stats,
        crate::handlers::log_forwarders::test_forwarder,
    ),
    components(
        schemas(
            // Auth
            ChangePasswordRequest, TotpSetupResponse, TotpVerifyRequest,
            // API Keys
            UpdateKeyRequest,
            // Setup
            AdminSetup, ProviderSetup, SetupInitRequest, SetupInitResponse, SetupStatusResponse,
            // Teams
            Team, TeamWithCount, CreateTeamRequest, UpdateTeamRequest, TeamMemberRow, AddMemberRequest,
            // Roles
            RoleResponse, RolesListResponse, CreateRoleRequest, UpdateRoleRequest,
            PermissionDef, RoleMember, RoleMembersResponse, RoleHistoryResponse,
            // Providers
            UpdateProviderRequest, TestProviderRequest, TestProviderResponse,
            // Models
            ModelRow, CreateModelRequest, UpdateModelRequest,
            // Limits
            RuleRow, RuleListResponse, UpsertRuleRequest, RuleUsage,
            CapRow, CapListResponse, UpsertCapRequest, CapUsage, UsageResponse,
            // Analytics
            UsageStats, UsageRow, CostStats, CostRow,
            // Dashboard
            DashboardStats, ProviderHealth, RpmBucket, LiveLogRow, DashboardLive, WsTicketResponse,
            // Logs
            GatewayLogEntry, GatewayLogsResponse,
            McpLogEntry, McpLogsResponse,
            PlatformLogEntryResponse, PlatformLogsResponse,
            AccessLogEntry, AccessLogsResponse,
            AppLogEntryResponse, AppLogsResponse,
            // MCP
            UpdateMcpServerRequest,
            // Log Forwarders
            CreateForwarderRequest, UpdateForwarderRequest, TestResult,
            // Settings
            UpdateSettingsRequest,
        )
    ),
    tags(
        (name = "Auth",          description = "Login, registration, refresh, logout, TOTP 2FA"),
        (name = "API Keys",      description = "API key lifecycle: create, list, rotate, revoke"),
        (name = "Setup",         description = "One-time platform initialization"),
        (name = "Users",         description = "User management (admin only)"),
        (name = "Settings",      description = "System, OIDC, audit, and content-filter settings"),
        (name = "Teams",         description = "Team management and membership"),
        (name = "Roles",         description = "RBAC role definitions and assignments"),
        (name = "Providers",     description = "AI provider configuration (OpenAI, Anthropic, …)"),
        (name = "Models",        description = "Model catalog and pricing configuration"),
        (name = "Limits",        description = "Per-key / per-team rate limits and spend budgets"),
        (name = "Analytics",     description = "Usage and cost analytics"),
        (name = "Dashboard",     description = "Real-time dashboard stats and live log stream"),
        (name = "Audit Logs",    description = "Platform audit trail"),
        (name = "Gateway Logs",  description = "AI gateway request logs"),
        (name = "MCP Logs",      description = "MCP tool call logs"),
        (name = "System Logs",   description = "Platform, access, and application logs"),
        (name = "MCP Servers",   description = "MCP server registration and tool discovery"),
        (name = "MCP Tools",     description = "MCP tool listing"),
        (name = "Log Forwarders", description = "External log forwarding destinations"),
    ),
    security(
        ("bearerAuth" = [])
    ),
    modifiers(&SecurityAddon),
)]
pub struct ApiDoc;

/// Registers the global `bearerAuth` HTTP security scheme.
/// Both JWT access tokens and `tw-` console API keys use the same
/// `Authorization: Bearer <credential>` header.
struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
            components.add_security_scheme(
                "bearerAuth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("JWT or tw- API key")
                        .build(),
                ),
            );
        }
    }
}

/// Returns an Axum router that exposes:
/// - `GET /api/openapi.json` — raw OpenAPI 3.1 spec (machine-readable)
/// - `GET /api/docs`         — Swagger UI (browser-friendly)
pub fn openapi_router() -> axum::Router<crate::app::AppState> {
    let spec = ApiDoc::openapi();
    axum::Router::new().merge(SwaggerUi::new("/api/docs").url("/api/openapi.json", spec))
}
