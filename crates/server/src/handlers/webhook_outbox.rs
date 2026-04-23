//! Admin endpoints for the durable webhook outbox.
//!
//! The drain worker handles the happy path. These endpoints exist for
//! the case where the operator needs to *see* what's stuck (capacity
//! planning, dead-receiver investigation) or *act* on a stuck row
//! (manual delete after fixing the receiver out-of-band).
//!
//! All three endpoints sit behind `log_forwarders:write` since they
//! peek at delivery payloads — the same scope the rest of the
//! forwarder admin UI uses.

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct WebhookOutboxRow {
    pub id: Uuid,
    pub forwarder_id: Uuid,
    /// Looked up at list time so the UI can render a name without a
    /// second round-trip. `None` means the forwarder was deleted —
    /// the FK CASCADE should normally clean those up but a row could
    /// linger if the worker is mid-iteration.
    pub forwarder_name: Option<String>,
    /// URL the delivery is targeting, extracted from the forwarder
    /// config. Lets the operator debug a stuck row without jumping
    /// to the forwarder-admin page to cross-reference. `None` when
    /// the forwarder was deleted or the config is somehow missing
    /// the `url` field (defensive).
    pub forwarder_url: Option<String>,
    pub attempts: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct WebhookOutboxListResponse {
    pub items: Vec<WebhookOutboxRow>,
    pub total: i64,
}

#[derive(Debug, Deserialize)]
pub struct ListOutboxQuery {
    /// When set, only rows targeting this forwarder are returned.
    /// The log-forwarders admin page uses this to render a
    /// per-forwarder backlog drawer inline.
    pub forwarder_id: Option<Uuid>,
}

/// `GET /api/admin/webhook-outbox` — list pending deliveries oldest-first.
///
/// Capped at 200 rows; an operator with a backlog larger than that
/// has bigger problems than pagination. `total` is returned separately
/// so the UI can show "showing 200 of 1,453 — drain is behind".
///
/// With `?forwarder_id=<uuid>`, the result is narrowed to that
/// forwarder — mounted directly under each row on the log-forwarders
/// admin page, so operators don't have to bounce to a separate
/// outbox view to see which of their destinations is backing up.
#[utoipa::path(
    get,
    path = "/api/admin/webhook-outbox",
    tag = "Admin",
    params(
        ("forwarder_id" = Option<String>, Query, description = "Narrow to one forwarder"),
    ),
    responses(
        (status = 200, description = "Pending webhook deliveries", body = WebhookOutboxListResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_outbox(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<ListOutboxQuery>,
) -> Result<Json<WebhookOutboxListResponse>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    auth_user
        .assert_scope_global(&state.db, "log_forwarders:write")
        .await?;

    // `$1::uuid IS NULL OR o.forwarder_id = $1` lets one prepared
    // statement serve both the "show everything" and "only this
    // forwarder" calls. `->>` returns TEXT for the URL column —
    // safer than a second materialised column that'd drift from the
    // forwarder's canonical config.
    let items: Vec<WebhookOutboxRow> = sqlx::query_as(
        "SELECT o.id, o.forwarder_id, f.name AS forwarder_name, \
                (f.config->>'url')::text AS forwarder_url, \
                o.attempts, o.next_attempt_at, o.last_error, o.created_at \
           FROM webhook_outbox o \
           LEFT JOIN log_forwarders f ON f.id = o.forwarder_id \
          WHERE $1::uuid IS NULL OR o.forwarder_id = $1 \
          ORDER BY o.next_attempt_at ASC \
          LIMIT 200",
    )
    .bind(q.forwarder_id)
    .fetch_all(&state.db)
    .await?;

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM webhook_outbox \
          WHERE $1::uuid IS NULL OR forwarder_id = $1",
    )
    .bind(q.forwarder_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(WebhookOutboxListResponse { items, total }))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct WebhookOutboxCount {
    #[schema(value_type = String, format = Uuid)]
    pub forwarder_id: Uuid,
    pub count: i64,
}

/// `GET /api/admin/webhook-outbox/counts` — backlog size per forwarder.
///
/// Feeds the "backlog" column on the log-forwarders admin table so
/// operators see at-a-glance which destinations are stuck. Returns
/// only forwarders with `count > 0` — the table joins by id and
/// defaults missing rows to zero.
#[utoipa::path(
    get,
    path = "/api/admin/webhook-outbox/counts",
    tag = "Admin",
    responses(
        (status = 200, description = "Per-forwarder backlog counts", body = Vec<WebhookOutboxCount>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn outbox_counts(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<WebhookOutboxCount>>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    auth_user
        .assert_scope_global(&state.db, "log_forwarders:write")
        .await?;
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT forwarder_id, COUNT(*) AS count \
           FROM webhook_outbox \
          GROUP BY forwarder_id",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(forwarder_id, count)| WebhookOutboxCount {
                forwarder_id,
                count,
            })
            .collect(),
    ))
}

/// `DELETE /api/admin/webhook-outbox/{id}` — drop a single stuck row.
///
/// Used when the operator has confirmed the receiver is permanently
/// gone (decommissioned endpoint, etc.) and wants to free the
/// outbox without waiting for the 24-attempt natural expiry.
#[utoipa::path(
    delete,
    path = "/api/admin/webhook-outbox/{id}",
    tag = "Admin",
    params(("id" = Uuid, Path, description = "Outbox row id")),
    responses(
        (status = 200, description = "Row deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Row not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn delete_outbox_row(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    auth_user
        .assert_scope_global(&state.db, "log_forwarders:write")
        .await?;

    let result = sqlx::query("DELETE FROM webhook_outbox WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Outbox row not found".into()));
    }

    state.audit.log(
        auth_user
            .audit("webhook_outbox.deleted")
            .resource(format!("webhook_outbox:{id}")),
    );

    Ok(Json(serde_json::json!({ "status": "deleted" })))
}

/// `POST /api/admin/webhook-outbox/{id}/retry` — schedule an immediate
/// re-attempt.
///
/// Bumps `next_attempt_at` to `now()` so the next drain tick (≤ 10s)
/// picks the row up. Doesn't reset `attempts` so the 24-cap still
/// applies — operators who really want a fresh count delete + re-emit.
#[utoipa::path(
    post,
    path = "/api/admin/webhook-outbox/{id}/retry",
    tag = "Admin",
    params(("id" = Uuid, Path, description = "Outbox row id")),
    responses(
        (status = 200, description = "Row scheduled for immediate retry"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Row not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn retry_outbox_row(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("log_forwarders:write")?;
    auth_user
        .assert_scope_global(&state.db, "log_forwarders:write")
        .await?;

    let result = sqlx::query("UPDATE webhook_outbox SET next_attempt_at = now() WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Outbox row not found".into()));
    }

    state.audit.log(
        auth_user
            .audit("webhook_outbox.retried")
            .resource(format!("webhook_outbox:{id}")),
    );

    Ok(Json(serde_json::json!({ "status": "rescheduled" })))
}
