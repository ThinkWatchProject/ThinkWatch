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

pub(super) async fn resolve_dashboard_user_filter(
    pool: &sqlx::PgPool,
    caller_id: uuid::Uuid,
) -> Result<Option<Vec<String>>, AppError> {
    // Global analytics:read_all → no filter.
    let has_global_all: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM rbac_role_assignments ra
               JOIN rbac_roles r ON r.id = ra.role_id
              WHERE ra.user_id = $1
                AND ra.scope_kind = 'global'
                AND EXISTS (
                    SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                    WHERE stmt->>'Effect' = 'Allow'
                      AND (stmt->>'Action' = '*' OR stmt->>'Action' = 'analytics:read_all'
                           OR (stmt->'Action' @> '\"analytics:read_all\"'::jsonb))
                )
         )",
    )
    .bind(caller_id)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("dashboard scope check failed: {e}")))?;
    if has_global_all {
        return Ok(None);
    }

    // Otherwise build the visible-user set: caller themself + every
    // team member of any team the caller holds analytics:read_team
    // (or analytics:read_all) for at team scope.
    let user_id_strs: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT u.id::text
           FROM users u
          WHERE u.deleted_at IS NULL
            AND (u.id = $1
             OR EXISTS (
                 SELECT 1 FROM team_members tm
                   JOIN rbac_role_assignments ra ON ra.scope_kind = 'team'
                                                 AND ra.scope_id = tm.team_id
                   JOIN rbac_roles r ON r.id = ra.role_id
                  WHERE tm.user_id = u.id
                    AND ra.user_id = $1
                    AND EXISTS (
                        SELECT 1 FROM jsonb_array_elements(r.policy_document->'Statement') AS stmt
                        WHERE stmt->>'Effect' = 'Allow'
                          AND (stmt->>'Action' = '*'
                               OR stmt->>'Action' = 'analytics:read_team'
                               OR stmt->>'Action' = 'analytics:read_all'
                               OR (stmt->'Action' @> '\"analytics:read_team\"'::jsonb)
                               OR (stmt->'Action' @> '\"analytics:read_all\"'::jsonb))
                    )
             ))",
    )
    .bind(caller_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("dashboard scope query failed: {e}")))?;
    Ok(Some(user_id_strs.into_iter().map(|(s,)| s).collect()))
}
