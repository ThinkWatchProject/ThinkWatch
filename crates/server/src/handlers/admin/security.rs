//! The request guards in the console: every rule of each guard, and a
//! sample tried against them.
//!
//! Both answers are thinkwatch-core's (`tw_guard::view`, `tw_guard::trial`),
//! the same JSON the desktop app's control API returns for the same
//! questions. The view is lossless: the console rebuilds a guard's whole
//! policy from it and writes it back through `PATCH /api/admin/settings`
//! (one `security.*` key per guard, see `think_watch_common::guard_policy`).
//!
//! Permissions are the ones these features always had: `pii_redactor:*`
//! for outbound redaction, `content_filter:*` for the content filter and
//! tool-call inspection. Reading the policies is reading settings.

use axum::Json;
use axum::extract::{Path, State};
use tw_guard::policy::Guard;
use tw_guard::trial::{TrialRequest, TrialResult};
use tw_guard::view::SecurityDetail;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// The permission that changes a guard's policy.
pub(crate) fn write_permission(guard: Guard) -> &'static str {
    match guard {
        Guard::Redact => "pii_redactor:write",
        Guard::InspectTools | Guard::Content => "content_filter:write",
    }
}

/// The permission that tries a sample against a guard.
fn read_permission(guard: Guard) -> &'static str {
    match guard {
        Guard::Redact => "pii_redactor:read",
        Guard::InspectTools | Guard::Content => "content_filter:read",
    }
}

/// GET /api/admin/security — the three guards: mode and every rule
/// (built-in and custom) with whether it is on, what it does in the third
/// mode and what it does out of the box.
#[utoipa::path(
    get,
    path = "/api/admin/security",
    tag = "Settings",
    responses(
        (status = 200, description = "Each guard's mode and rules (thinkwatch-core's SecurityDetail)", body = serde_json::Value),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn get_security(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<SecurityDetail>, AppError> {
    auth_user
        .require_global_permission(&state.db, "settings:read")
        .await?;
    let policy = think_watch_common::guard_policy::read(&state.dynamic_config).await;
    Ok(Json(tw_guard::view::detail(&policy)))
}

/// POST /api/admin/security/{guard}/test — try a sample against one
/// guard: its enabled rules, one built-in rule (`rule`, switched off or
/// not), or a rule still being written (`pattern`). Answers every hit, and
/// what the third mode would send (`output`) or whether it would refuse.
#[utoipa::path(
    post,
    path = "/api/admin/security/{guard}/test",
    tag = "Settings",
    params(
        ("guard" = String, Path, description = "`redact`, `inspect_tools` or `content`"),
    ),
    request_body(
        content = inline(serde_json::Value),
        description = "sample: string; pattern?, match?, rule?, label?, action? (thinkwatch-core's SecurityTestRequest)",
    ),
    responses(
        (status = 200, description = "Hits, output and refusal (thinkwatch-core's SecurityTestResult)", body = serde_json::Value),
        (status = 400, description = "The rule tried cannot be used; the message says why"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "No such guard"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn test_security(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(guard): Path<String>,
    Json(req): Json<TrialRequest>,
) -> Result<Json<TrialResult>, AppError> {
    let guard = Guard::from_slug(&guard)
        .ok_or_else(|| AppError::NotFound(format!("There is no guard `{guard}`.")))?;
    auth_user
        .require_global_permission(&state.db, read_permission(guard))
        .await?;
    let policy = think_watch_common::guard_policy::read(&state.dynamic_config).await;
    tw_guard::trial::run(guard, &policy, &req)
        .map(Json)
        .map_err(|e| AppError::BadRequest(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_guard_keeps_the_permissions_it_had() {
        assert_eq!(write_permission(Guard::Redact), "pii_redactor:write");
        assert_eq!(write_permission(Guard::Content), "content_filter:write");
        assert_eq!(
            write_permission(Guard::InspectTools),
            "content_filter:write"
        );
        assert_eq!(read_permission(Guard::Redact), "pii_redactor:read");
        assert_eq!(read_permission(Guard::InspectTools), "content_filter:read");
        for &g in Guard::ALL {
            for p in [write_permission(g), read_permission(g)] {
                assert!(crate::handlers::roles::is_known_permission(p), "{p}");
            }
        }
    }
}
