//! The request guards in the console — secrets and privacy (outbound
//! redaction), dangerous commands (tool-call inspection) and the content
//! filter, which checks requests and the text of answers: every rule of
//! each guard, and a sample tried against them.
//!
//! Both answers are thinkwatch-core's (`tw_guard::view`, `tw_guard::trial`),
//! the same JSON the desktop app's control API returns for the same
//! questions. The view is lossless: the console rebuilds a guard's whole
//! policy from it and writes it back through `PATCH /api/admin/settings`
//! (one `security.*` key per guard, see `think_watch_common::guard_policy`).
//!
//! Permissions are the ones these features always had: trying a sample
//! takes `pii_redactor:read` for outbound redaction and
//! `content_filter:read` for the content filter and tool-call inspection;
//! changing a policy takes `settings:write` and the guard's `:write`.
//! Reading the policies is reading settings.
//!
//! Redaction's exception values have endpoints of their own
//! (`/api/admin/security/redact/exceptions`, see
//! `think_watch_common::guard_policy::exceptions`): listing them is reading
//! settings, adding and removing them is changing the redaction policy.
//! A value pasted to add is hashed on arrival; only its SHA-256 and masked
//! form are stored, answered or written to the audit log.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tw_guard::policy::{EXCEPTIONS_MAX, Guard, RedactException, RedactPolicy};
use tw_guard::trial::{TrialRequest, TrialResult};
use tw_guard::view::SecurityDetail;

use think_watch_common::dynamic_config;
use think_watch_common::errors::AppError;
use think_watch_common::guard_policy;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// The permission that changes a guard's policy, on top of `settings:write`
/// (see `settings::update_settings`).
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

/// GET /api/admin/security — the three guards: mode (on, monitor, off) and
/// every rule (built-in and custom) with whether it is on, what it does
/// while the guard is on and what it does out of the box; a content rule
/// also says what it checks (requests, answers) and what a hit is replaced
/// with.
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
/// not), or a rule still being written (`pattern`). A content sample is
/// tried as a request or as an answer (`scope`). Answers every hit, and
/// what the guard would send or pass on while on (`output`), or whether it
/// would refuse.
#[utoipa::path(
    post,
    path = "/api/admin/security/{guard}/test",
    tag = "Settings",
    params(
        ("guard" = String, Path, description = "`redact`, `inspect_tools` or `content`"),
    ),
    request_body(
        content = inline(serde_json::Value),
        description = "sample: string; pattern?, match?, case_sensitive?, rule?, label?, action?, scope? (`request` or `response`), replacement? (thinkwatch-core's SecurityTestRequest)",
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
    // With the providers' credentials, as the gateway runs: a pasted
    // provider key is reported (by provider, never by value).
    let saved = state.saved_secrets.load().compiled();
    tw_guard::trial::run_with(guard, &policy, &req, saved)
        .map(Json)
        .map_err(|e| AppError::BadRequest(e.to_string()))
}

/// `GET /api/admin/security/redact/exceptions`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ExceptionList {
    /// `{ sha256, masked, rule?, added_at }`, in the order added.
    #[schema(value_type = Vec<Object>)]
    pub items: Vec<RedactException>,
    /// How many there may be. Adding is refused at this many.
    pub max: usize,
}

/// `POST /api/admin/security/redact/exceptions`: the value to except, as it
/// appears in requests. Surrounding whitespace is ignored.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct AddException {
    pub value: String,
}

/// Never the value: this type ends up in error reports.
impl std::fmt::Debug for AddException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddException").finish_non_exhaustive()
    }
}

/// GET /api/admin/security/redact/exceptions — redaction's exception
/// values: each one's SHA-256, masked form, the rule that first recognised
/// it and when it was added.
#[utoipa::path(
    get,
    path = "/api/admin/security/redact/exceptions",
    tag = "Settings",
    responses(
        (status = 200, description = "The exception values", body = ExceptionList),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_exceptions(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<ExceptionList>, AppError> {
    auth_user
        .require_global_permission(&state.db, "settings:read")
        .await?;
    let stored = state
        .dynamic_config
        .get(guard_policy::key(Guard::Redact))
        .await;
    Ok(Json(ExceptionList {
        items: guard_policy::exceptions::stored(stored.as_ref()),
        max: EXCEPTIONS_MAX,
    }))
}

/// POST /api/admin/security/redact/exceptions — add a value. It is hashed
/// here and never stored, logged or answered; the answer is the exception
/// as stored. A saved provider key cannot be an exception.
#[utoipa::path(
    post,
    path = "/api/admin/security/redact/exceptions",
    tag = "Settings",
    request_body = AddException,
    responses(
        (status = 201, description = "Added: the exception as stored", body = Object),
        (status = 400, description = "Empty, a saved provider key, or the list is full"),
        (status = 403, description = "Forbidden"),
        (status = 409, description = "Already an exception"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn add_exception(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<AddException>,
) -> Result<(StatusCode, Json<RedactException>), AppError> {
    require_write(&auth_user, &state).await?;
    let saved = state.saved_secrets.load().compiled();
    let added = edit_exceptions(&state, &auth_user, |policy, list| {
        let e = policy
            .exception_of(&req.value, saved, chrono::Utc::now().timestamp_millis())
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
        if list.iter().any(|x| x.sha256 == e.sha256) {
            return Err(AppError::Conflict("This value is already an exception.".into()));
        }
        if list.len() >= EXCEPTIONS_MAX {
            return Err(AppError::BadRequest(format!(
                "There are already {EXCEPTIONS_MAX} exceptions, the most there may be; remove one first."
            )));
        }
        list.push(e.clone());
        Ok(e)
    })
    .await?;
    state.audit.log(
        auth_user
            .audit("security.redact_exception_added")
            .resource("system_settings")
            .detail(serde_json::json!({"rule": added.rule, "masked": added.masked})),
    );
    Ok((StatusCode::CREATED, Json(added)))
}

/// DELETE /api/admin/security/redact/exceptions/{sha256} — remove one.
/// The answer is the exception removed.
#[utoipa::path(
    delete,
    path = "/api/admin/security/redact/exceptions/{sha256}",
    tag = "Settings",
    params(("sha256" = String, Path, description = "The exception's SHA-256, as listed")),
    responses(
        (status = 200, description = "Removed: the exception as it was stored", body = Object),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "No such exception"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn remove_exception(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(sha256): Path<String>,
) -> Result<Json<RedactException>, AppError> {
    require_write(&auth_user, &state).await?;
    let removed = edit_exceptions(&state, &auth_user, |_, list| {
        let at = list
            .iter()
            .position(|e| e.sha256 == sha256)
            .ok_or_else(|| AppError::NotFound("There is no such exception.".into()))?;
        Ok(list.remove(at))
    })
    .await?;
    state.audit.log(
        auth_user
            .audit("security.redact_exception_removed")
            .resource("system_settings")
            .detail(serde_json::json!({"rule": removed.rule, "masked": removed.masked})),
    );
    Ok(Json(removed))
}

/// Changing the exceptions is changing the redaction policy.
async fn require_write(auth_user: &AuthUser, state: &AppState) -> Result<(), AppError> {
    for p in ["settings:write", write_permission(Guard::Redact)] {
        auth_user.require_global_permission(&state.db, p).await?;
    }
    Ok(())
}

/// Change the stored exceptions with `f`, which sees the policy as stored
/// and its exception list, and swap the guards in on every instance. The
/// row is locked from read to write, so two admins adding at once on two
/// instances both land. Nothing is written when `f` refuses.
async fn edit_exceptions<R>(
    state: &AppState,
    auth_user: &AuthUser,
    f: impl FnOnce(&RedactPolicy, &mut Vec<RedactException>) -> Result<R, AppError>,
) -> Result<R, AppError> {
    let key = guard_policy::key(Guard::Redact);
    let mut tx = state.db.begin().await?;
    let stored: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM system_settings WHERE key = $1 FOR UPDATE")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
    let policy: RedactPolicy = match &stored {
        None => RedactPolicy::default(),
        Some(v) => serde_json::from_value(v.clone()).map_err(|_| {
            AppError::BadRequest(format!(
                "The stored {key} cannot be read; save the redaction settings again first."
            ))
        })?,
    };
    let mut list = policy.exceptions.clone();
    let out = f(&policy, &mut list)?;
    let value = guard_policy::exceptions::with_list(stored, &list);
    guard_policy::validate(Guard::Redact, &value).map_err(AppError::BadRequest)?;
    let written = sqlx::query(
        "UPDATE system_settings SET value = $1, updated_by = $2, updated_at = now() WHERE key = $3",
    )
    .bind(&value)
    .bind(auth_user.claims.sub)
    .bind(key)
    .execute(&mut *tx)
    .await?;
    if written.rows_affected() == 0 {
        return Err(AppError::Internal(anyhow::anyhow!(
            "system setting {key} is missing"
        )));
    }
    tx.commit().await?;
    state
        .dynamic_config
        .reload()
        .await
        .map_err(AppError::Internal)?;
    crate::app::reload_guards(state).await;
    dynamic_config::notify_config_changed(&state.redis).await;
    Ok(out)
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
