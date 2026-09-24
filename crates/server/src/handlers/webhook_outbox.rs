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
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;
use crate::services::webhook_outbox_repository::{self as repo, WebhookOutboxRow};

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
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;

    let items = repo::list(&state.db, q.forwarder_id).await?;
    let total = repo::count(&state.db, q.forwarder_id).await?;

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
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;
    // Cap at 500 forwarder rows so a deployment with hundreds of dead
    // endpoints can't return a multi-megabyte JSON body. Sorted by
    // backlog desc so operators see the biggest offenders first; the
    // tail (rare in practice) is dropped silently.
    let rows = repo::counts_by_forwarder(&state.db).await?;
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
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;

    if repo::delete(&state.db, id).await? == 0 {
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
    auth_user
        .require_global_permission(&state.db, "log_forwarders:write")
        .await?;

    if repo::retry_now(&state.db, id).await? == 0 {
        return Err(AppError::NotFound("Outbox row not found".into()));
    }

    state.audit.log(
        auth_user
            .audit("webhook_outbox.retried")
            .resource(format!("webhook_outbox:{id}")),
    );

    Ok(Json(serde_json::json!({ "status": "rescheduled" })))
}
