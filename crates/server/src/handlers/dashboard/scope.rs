//! RBAC-shaped "which user_ids should be visible" resolver, shared
//! by every dashboard endpoint that scans ClickHouse.
//!
//! Returns:
//!   - `None` — caller has `analytics:read_all` at global scope and
//!     should see every gateway log (no SQL filter)
//!   - `Some(user_ids)` — caller has either `analytics:read_team` at
//!     team scope, or no analytics perm at all. The set always
//!     contains the caller's own id, plus every team member of any
//!     team the caller has `analytics:read_team` for. The list is
//!     stringified because the ClickHouse `user_id` column is
//!     `LowCardinality(Nullable(String))`.
//!
//! Sits in its own module (not on `AuthUser`) because the WebSocket
//! loop only has a user_id from a ticket — it never sees the JWT —
//! so it can't lean on the `AuthUser` helpers from auth_guard.

use think_watch_common::errors::AppError;

use crate::services::observability_repository as repo;

pub(super) async fn resolve_dashboard_user_filter(
    pool: &sqlx::PgPool,
    caller_id: uuid::Uuid,
) -> Result<Option<Vec<String>>, AppError> {
    // Global analytics:read_all → no filter.
    let has_global_all = repo::has_global_analytics_read_all(pool, caller_id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("dashboard scope check failed: {e}")))?;
    if has_global_all {
        return Ok(None);
    }

    // Otherwise build the visible-user set: caller themself + every
    // team member of any team the caller holds analytics:read_team
    // (or analytics:read_all) for at team scope.
    let user_id_strs = repo::analytics_team_scope_user_ids(pool, caller_id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("dashboard scope query failed: {e}")))?;
    Ok(Some(user_id_strs.into_iter().map(|(s,)| s).collect()))
}
