//! Admin handlers grouped by resource. Each submodule owns its own
//! types + Axum handlers + private helpers; this parent re-exports
//! every public symbol so the existing `crate::handlers::admin::*`
//! call shape (52 sites across app routes, openapi, main) keeps
//! working without a sweep.

mod content_filter;
mod oidc;
mod retention;
mod settings;
mod users;

pub use content_filter::{
    ContentFilterPreset, ContentFilterTestMatch, ContentFilterTestRequest,
    ContentFilterTestResponse, PiiRedactorTestMatch, PiiRedactorTestRequest,
    PiiRedactorTestResponse, list_content_filter_presets, test_content_filter, test_pii_redactor,
};
pub use oidc::{
    DisableOidcRequest, OidcActiveSnapshot, OidcDraftSnapshot, OidcSettingsResponse,
    OidcTestResult, StartOidcTestLoginResponse, UpdateOidcDraftRequest, activate_oidc_draft,
    delete_oidc_draft, discover_oidc_draft, get_oidc_settings, start_oidc_test_login,
    toggle_oidc_active, update_oidc_draft,
};
pub use retention::{check_body_retention_vs_lifecycle, reconcile_clickhouse_ttls};
pub use settings::{
    AuditConfigResponse, SystemInfo, UpdateSettingsRequest, get_all_settings, get_audit_settings,
    get_settings_by_category, get_system_settings, update_settings,
};
pub use users::{
    CreateUserByAdminRequest, CreateUserByAdminResponse, ListUsersQuery, SuperAdminIds,
    UpdateUserRequest, create_user, delete_user, force_logout_user, list_super_admin_ids,
    list_users, reset_user_password, update_user,
};

// utoipa generates a `__path_<fn>` companion type per `#[utoipa::path]`
// annotation, looked up in the SAME module the openapi spec
// references. Re-exporting them here keeps the spec entries (which
// point at `crate::handlers::admin::<fn>`) finding their companion
// types after the submodule split.
#[allow(unused_imports)]
pub use content_filter::{
    __path_list_content_filter_presets, __path_test_content_filter, __path_test_pii_redactor,
};
#[allow(unused_imports)]
pub use oidc::{
    __path_activate_oidc_draft, __path_delete_oidc_draft, __path_discover_oidc_draft,
    __path_get_oidc_settings, __path_start_oidc_test_login, __path_toggle_oidc_active,
    __path_update_oidc_draft,
};
#[allow(unused_imports)]
pub use settings::{
    __path_get_all_settings, __path_get_audit_settings, __path_get_settings_by_category,
    __path_get_system_settings, __path_update_settings,
};
#[allow(unused_imports)]
pub use users::{
    __path_create_user, __path_delete_user, __path_force_logout_user, __path_list_super_admin_ids,
    __path_list_users, __path_reset_user_password, __path_update_user,
};
