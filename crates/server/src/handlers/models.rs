// ============================================================================
// Admin model CRUD
//
// Manages rows in the `models` table — the exposed catalog clients see
// via `/v1/models`. Each row carries `input_weight` / `output_weight`
// (relative factors against `platform_pricing` for cost reporting +
// weighted-token quota accounting). Routing to providers is handled by
// the `model_routes` table.
//
// Permissions: `models:read` for GET, `models:write` for POST/PATCH/DELETE.
// ============================================================================

use axum::Json;
use axum::extract::{Path, Query, State};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use think_watch_common::dto::ProviderHeader;
use think_watch_common::errors::AppError;
use think_watch_common::models::Model;

use super::serde_util::deserialize_some;
use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// Row shape returned by `GET /api/admin/models`. Route counts are
/// joined in so the UI can show "active / draft / unrouted" status
/// without a second round-trip.
#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ModelRow {
    pub id: Uuid,
    pub model_id: String,
    pub display_name: String,
    #[schema(value_type = f64)]
    pub input_weight: Decimal,
    #[schema(value_type = f64)]
    pub output_weight: Decimal,
    pub route_count: i64,
    pub enabled_route_count: i64,
    /// Provider display names (or `name` if display_name is null) for
    /// every route attached to the model, ordered by weight DESC. Lets
    /// the list table show "who serves this?" without an extra fetch.
    pub providers: Vec<String>,
}

/// `status` filter accepted by `GET /api/admin/models`:
///
/// * `active`    — at least one enabled route (appears in `/v1/models`)
/// * `draft`     — has routes, all disabled (user imported but not exposed)
/// * `unrouted`  — no routes at all (orphan catalog entry)
/// * anything else or missing = no filter
#[derive(Debug, Deserialize)]
pub struct ModelListQuery {
    pub q: Option<String>,
    pub status: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ModelListResponse {
    pub items: Vec<ModelRow>,
    pub total: i64,
}

#[utoipa::path(
    get,
    path = "/api/admin/models",
    tag = "Models",
    params(
        ("q" = Option<String>, Query, description = "Search model_id or display_name"),
        ("page" = Option<i64>, Query, description = "Page number (1-based)"),
        ("page_size" = Option<i64>, Query, description = "Items per page (default 50)"),
    ),
    security(("bearer_token" = [])),
    responses(
        (status = 200, description = "Paginated list of models", body = ModelListResponse),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn list_models(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<ModelListQuery>,
) -> Result<Json<ModelListResponse>, AppError> {
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;

    let page_size = query.page_size.unwrap_or(50).clamp(1, 200);
    let page = query.page.unwrap_or(1).max(1);
    let offset = (page - 1) * page_size;
    let search = query.q.as_deref().unwrap_or("").trim();
    let search_pattern = format!("%{search}%");
    let status = query.status.as_deref().unwrap_or("");

    // Unified query with `$1='' OR ...` to combine optional search +
    // status filter. `status_filter`:
    //   'active'    — enabled_route_count > 0
    //   'disabled'  — route_count > 0 AND enabled_route_count = 0
    //   'unrouted'  — route_count = 0
    //   otherwise   — no filter
    //
    // We compute `route_count` / `enabled_route_count` via `LATERAL`
    // subquery so the filter happens on the joined shape; PG rewrites
    // this to a HashAggregate over `model_routes`.
    let status_filter_sql = match status {
        "active" => "AND rc.enabled_route_count > 0",
        "disabled" => "AND rc.route_count > 0 AND rc.enabled_route_count = 0",
        "unrouted" => "AND rc.route_count = 0",
        _ => "",
    };

    let total_sql = format!(
        r#"SELECT COUNT(*) FROM models m
           LEFT JOIN LATERAL (
             SELECT COUNT(*)                                 AS route_count,
                    COUNT(*) FILTER (WHERE mr.enabled = true) AS enabled_route_count
             FROM model_routes mr
             JOIN providers p ON p.id = mr.provider_id AND p.deleted_at IS NULL
             WHERE mr.model_id = m.model_id
           ) rc ON true
           WHERE ($1 = '' OR m.model_id ILIKE $2 OR m.display_name ILIKE $2)
             {status_filter_sql}"#,
    );
    let list_sql = format!(
        r#"SELECT m.id, m.model_id, m.display_name,
                  m.input_weight, m.output_weight,
                  COALESCE(rc.route_count, 0)         AS route_count,
                  COALESCE(rc.enabled_route_count, 0) AS enabled_route_count,
                  COALESCE(rc.providers, '{{}}'::text[]) AS providers
           FROM models m
           LEFT JOIN LATERAL (
             SELECT COUNT(*)                                 AS route_count,
                    COUNT(*) FILTER (WHERE mr.enabled = true) AS enabled_route_count,
                    array_agg(COALESCE(p.display_name, p.name)
                              ORDER BY mr.weight DESC, p.name) AS providers
             FROM model_routes mr
             JOIN providers p ON p.id = mr.provider_id AND p.deleted_at IS NULL
             WHERE mr.model_id = m.model_id
           ) rc ON true
           WHERE ($1 = '' OR m.model_id ILIKE $2 OR m.display_name ILIKE $2)
             {status_filter_sql}
           ORDER BY m.model_id
           LIMIT $3 OFFSET $4"#,
    );

    let total: Option<i64> = sqlx::query_scalar(&total_sql)
        .bind(search)
        .bind(&search_pattern)
        .fetch_one(&state.db)
        .await?;
    let rows = sqlx::query_as::<_, ModelRow>(&list_sql)
        .bind(search)
        .bind(&search_pattern)
        .bind(page_size)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;

    Ok(Json(ModelListResponse {
        items: rows,
        total: total.unwrap_or(0),
    }))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateModelRequest {
    pub model_id: String,
    pub display_name: String,
    /// Relative input-token cost factor. Defaults to 1.0.
    #[schema(value_type = Option<f64>)]
    pub input_weight: Option<Decimal>,
    /// Relative output-token cost factor. Defaults to 1.0.
    #[schema(value_type = Option<f64>)]
    pub output_weight: Option<Decimal>,
    /// Override the gateway-wide default routing strategy. NULL ⇒
    /// inherit. One of weighted/latency/cost/latency_cost.
    #[serde(default)]
    pub routing_strategy: Option<String>,
    /// Override session affinity mode. NULL ⇒ inherit.
    /// One of none/provider/route.
    #[serde(default)]
    pub affinity_mode: Option<String>,
    /// Override the affinity key TTL (seconds, 0–86400). NULL ⇒ inherit.
    #[serde(default)]
    pub affinity_ttl_secs: Option<i32>,
    /// Free-form admin tags. NULL = no tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[utoipa::path(
    post,
    path = "/api/admin/models",
    tag = "Models",
    security(("bearer_token" = [])),
    request_body = CreateModelRequest,
    responses(
        (status = 200, description = "Model created"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn create_model(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateModelRequest>,
) -> Result<Json<Model>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;
    if req.model_id.trim().is_empty() || req.display_name.trim().is_empty() {
        return Err(AppError::BadRequest(
            "model_id and display_name are required".into(),
        ));
    }
    let in_w = req.input_weight.unwrap_or(Decimal::ONE);
    let out_w = req.output_weight.unwrap_or(Decimal::ONE);
    if in_w <= Decimal::ZERO || out_w <= Decimal::ZERO {
        return Err(AppError::BadRequest(
            "weights must be greater than zero".into(),
        ));
    }
    validate_routing_overrides(
        req.routing_strategy.as_deref(),
        req.affinity_mode.as_deref(),
        req.affinity_ttl_secs,
    )?;

    let model = sqlx::query_as::<_, Model>(
        r#"INSERT INTO models
              (model_id, display_name, input_weight, output_weight,
               routing_strategy, affinity_mode, affinity_ttl_secs, tags)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           RETURNING id, model_id, display_name, input_weight, output_weight,
                     routing_strategy, affinity_mode, affinity_ttl_secs, tags"#,
    )
    .bind(&req.model_id)
    .bind(&req.display_name)
    .bind(in_w)
    .bind(out_w)
    .bind(&req.routing_strategy)
    .bind(&req.affinity_mode)
    .bind(req.affinity_ttl_secs)
    .bind(req.tags.as_deref())
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("model.created")
            .resource("model")
            .resource_id(model.id.to_string())
            .detail(serde_json::json!({ "model_id": &req.model_id })),
    );

    Ok(Json(model))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateModelRequest {
    pub display_name: Option<String>,
    #[schema(value_type = Option<f64>)]
    pub input_weight: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub output_weight: Option<Decimal>,
    /// PATCH semantics: absent = unchanged, JSON `null` = clear (revert
    /// to global default), string = override. One of
    /// weighted / latency / cost / latency_cost.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub routing_strategy: Option<Option<String>>,
    /// PATCH-clearable affinity mode. One of none / provider / route.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub affinity_mode: Option<Option<String>>,
    /// PATCH-clearable affinity TTL (0–86400 seconds).
    #[serde(default, deserialize_with = "deserialize_some")]
    pub affinity_ttl_secs: Option<Option<i32>>,
    /// PATCH-clearable tags. JSON `null` = clear all tags.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub tags: Option<Option<Vec<String>>>,
}

/// Validate routing-strategy override values mirror the CHECK
/// constraints on `models`. Both create + update use this to give a
/// 400 with a useful message instead of letting the SQL CHECK fail.
fn validate_routing_overrides(
    strategy: Option<&str>,
    affinity_mode: Option<&str>,
    affinity_ttl_secs: Option<i32>,
) -> Result<(), AppError> {
    if let Some(s) = strategy
        && !["weighted", "latency", "cost", "latency_cost"].contains(&s)
    {
        return Err(AppError::BadRequest(
            "routing_strategy must be one of: weighted, latency, cost, latency_cost".into(),
        ));
    }
    if let Some(m) = affinity_mode
        && !["none", "provider", "route"].contains(&m)
    {
        return Err(AppError::BadRequest(
            "affinity_mode must be one of: none, provider, route".into(),
        ));
    }
    if let Some(t) = affinity_ttl_secs
        && !(0..=86400).contains(&t)
    {
        return Err(AppError::BadRequest(
            "affinity_ttl_secs must be between 0 and 86400".into(),
        ));
    }
    Ok(())
}

#[utoipa::path(
    patch,
    path = "/api/admin/models/{id}",
    tag = "Models",
    security(("bearer_token" = [])),
    params(
        ("id" = uuid::Uuid, Path, description = "Model ID"),
    ),
    request_body = UpdateModelRequest,
    responses(
        (status = 200, description = "Model updated"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    )
)]
pub async fn update_model(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateModelRequest>,
) -> Result<Json<Model>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;
    let existing = sqlx::query_as::<_, Model>(
        r#"SELECT id, model_id, display_name, input_weight, output_weight,
                  routing_strategy, affinity_mode, affinity_ttl_secs, tags
           FROM models WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Model not found".into()))?;

    let new_in_w = req.input_weight.unwrap_or(existing.input_weight);
    let new_out_w = req.output_weight.unwrap_or(existing.output_weight);
    if new_in_w <= Decimal::ZERO || new_out_w <= Decimal::ZERO {
        return Err(AppError::BadRequest(
            "weights must be greater than zero".into(),
        ));
    }
    // Resolve PATCH semantics for the nullable overrides:
    // absent ⇒ preserve existing; Some(None) ⇒ clear; Some(Some(v)) ⇒ overwrite.
    let new_strategy: Option<String> = match &req.routing_strategy {
        None => existing.routing_strategy.clone(),
        Some(inner) => inner.clone(),
    };
    let new_affinity_mode: Option<String> = match &req.affinity_mode {
        None => existing.affinity_mode.clone(),
        Some(inner) => inner.clone(),
    };
    let new_affinity_ttl: Option<i32> = match req.affinity_ttl_secs {
        None => existing.affinity_ttl_secs,
        Some(inner) => inner,
    };
    let new_tags: Option<Vec<String>> = match &req.tags {
        None => existing.tags.clone(),
        Some(inner) => inner.clone(),
    };
    validate_routing_overrides(
        new_strategy.as_deref(),
        new_affinity_mode.as_deref(),
        new_affinity_ttl,
    )?;

    let updated = sqlx::query_as::<_, Model>(
        r#"UPDATE models SET
              display_name      = $2,
              input_weight      = $3,
              output_weight     = $4,
              routing_strategy  = $5,
              affinity_mode     = $6,
              affinity_ttl_secs = $7,
              tags              = $8
           WHERE id = $1
           RETURNING id, model_id, display_name, input_weight, output_weight,
                     routing_strategy, affinity_mode, affinity_ttl_secs, tags"#,
    )
    .bind(id)
    .bind(
        req.display_name
            .as_deref()
            .unwrap_or(&existing.display_name),
    )
    .bind(new_in_w)
    .bind(new_out_w)
    .bind(&new_strategy)
    .bind(&new_affinity_mode)
    .bind(new_affinity_ttl)
    .bind(new_tags.as_deref())
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("model.updated")
            .resource("model")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "model_id": existing.model_id })),
    );

    // Routing strategy / affinity overrides are baked into the
    // ModelRouter at load time, so flipping them requires a hot-swap
    // for the change to hit live traffic. Cheap (a single SELECT pass).
    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/api/admin/models/{id}",
    tag = "Models",
    security(("bearer_token" = [])),
    params(
        ("id" = uuid::Uuid, Path, description = "Model ID"),
    ),
    responses(
        (status = 200, description = "Model deleted"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    )
)]
pub async fn delete_model(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;
    let model_id: Option<String> = sqlx::query_scalar("SELECT model_id FROM models WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    sqlx::query("DELETE FROM models WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    state.audit.log(
        auth_user
            .audit("model.deleted")
            .resource("model")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "model_id": model_id })),
    );
    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// ---------------------------------------------------------------------------
// Lightweight list of every exposed model_id
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ModelIdRow {
    pub model_id: String,
    pub display_name: String,
}

/// GET /api/admin/models/ids
///
/// Minimal catalog listing used by the batch-import dialog's "attach to
/// existing model" picker. Paginated `list_models` would work but caps
/// at 200/page — this one is unpaginated since the expected ceiling is
/// under a thousand entries (the curated exposed catalog, not provider
/// remotes).
pub async fn list_model_ids(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<ModelIdRow>>, AppError> {
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;

    let rows = sqlx::query_as::<_, ModelIdRow>(
        "SELECT model_id, display_name FROM models ORDER BY model_id",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

// ---------------------------------------------------------------------------
// Bulk cleanup of orphan models (no routes at all)
// ---------------------------------------------------------------------------

/// `DELETE /api/admin/models/unrouted` — remove catalog entries with
/// zero `model_routes` rows. Used to clean up the aftermath of a large
/// batch-import where the admin only wanted to expose a handful of
/// models but the import created rows for all of them.
///
/// Note: `model_routes.provider_id` has `ON DELETE CASCADE`, so soft-
/// deleted providers still count as "having a route". We filter by the
/// provider's `deleted_at IS NULL` to avoid keeping around models whose
/// only routes point to dead providers.
pub async fn delete_unrouted_models(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    let result = sqlx::query(
        r#"DELETE FROM models
           WHERE model_id NOT IN (
             SELECT DISTINCT mr.model_id
             FROM model_routes mr
             JOIN providers p ON p.id = mr.provider_id AND p.deleted_at IS NULL
           )"#,
    )
    .execute(&state.db)
    .await?;

    let deleted = result.rows_affected() as i64;

    state.audit.log(
        auth_user
            .audit("models.unrouted_cleanup")
            .resource("models")
            .detail(serde_json::json!({ "deleted": deleted })),
    );

    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

// ---------------------------------------------------------------------------
// Bulk delete catalog entries by id
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BulkDeleteModelsRequest {
    pub ids: Vec<Uuid>,
}

/// `POST /api/admin/models/bulk-delete` — remove a curated subset of
/// catalog entries by their UUIDs. Cascades through `model_routes`,
/// so we rebuild the gateway router afterwards.
pub async fn bulk_delete_models(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BulkDeleteModelsRequest>,
) -> Result<Json<Value>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    if req.ids.is_empty() {
        return Err(AppError::BadRequest("ids is empty".into()));
    }

    let result = sqlx::query("DELETE FROM models WHERE id = ANY($1)")
        .bind(&req.ids)
        .execute(&state.db)
        .await?;

    let deleted = result.rows_affected() as i64;

    state.audit.log(
        auth_user
            .audit("models.bulk_deleted")
            .resource("models")
            .detail(serde_json::json!({
                "requested": req.ids.len(),
                "deleted": deleted,
            })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

// ---------------------------------------------------------------------------
// Model Routes CRUD
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ModelRouteRow {
    pub id: Uuid,
    pub model_id: String,
    pub provider_id: Uuid,
    pub provider_name: String,
    pub upstream_model: Option<String>,
    pub weight: i32,
    pub enabled: bool,
    /// Optional human-readable identifier (e.g. "EU-primary"). Pure
    /// metadata for the admin UI; ignored by the routing layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Free-form note. Surfaced in the edit dialog only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Per-route RPM cap. NULL = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_cap: Option<i32>,
    /// Per-route TPM cap. NULL = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm_cap: Option<i32>,
}

/// GET /api/admin/models/{model_id}/routes
pub async fn list_model_routes(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<Json<Vec<ModelRouteRow>>, AppError> {
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;

    let rows = sqlx::query_as::<_, ModelRouteRow>(
        r#"SELECT mr.id, mr.model_id, mr.provider_id, p.name AS provider_name,
                  mr.upstream_model, mr.weight, mr.enabled,
                  mr.label, mr.notes, mr.rpm_cap, mr.tpm_cap
           FROM model_routes mr
           JOIN providers p ON p.id = mr.provider_id
           WHERE mr.model_id = $1 AND p.deleted_at IS NULL
           ORDER BY mr.weight DESC"#,
    )
    .bind(&model_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(rows))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateModelRouteRequest {
    pub provider_id: Uuid,
    pub upstream_model: Option<String>,
    pub weight: Option<i32>,
    /// Optional — falls back to the column default (`TRUE`). New
    /// routes are live immediately on both manual and batch-import
    /// paths.
    pub enabled: Option<bool>,
    /// Optional human-readable identifier. NULL or empty = no label.
    #[serde(default)]
    pub label: Option<String>,
    /// Optional admin note (free text).
    #[serde(default)]
    pub notes: Option<String>,
    /// Per-route RPM cap (must be > 0 if set).
    #[serde(default)]
    pub rpm_cap: Option<i32>,
    /// Per-route TPM cap (must be > 0 if set).
    #[serde(default)]
    pub tpm_cap: Option<i32>,
}

/// POST /api/admin/models/{model_id}/routes
pub async fn create_model_route(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(model_id): Path<String>,
    Json(req): Json<CreateModelRouteRequest>,
) -> Result<Json<ModelRouteRow>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    // Verify model exists
    let model_exists: Option<String> =
        sqlx::query_scalar("SELECT model_id FROM models WHERE model_id = $1")
            .bind(&model_id)
            .fetch_optional(&state.db)
            .await?;
    if model_exists.is_none() {
        return Err(AppError::NotFound("Model not found".into()));
    }

    // Verify provider exists
    let provider_exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM providers WHERE id = $1 AND deleted_at IS NULL")
            .bind(req.provider_id)
            .fetch_optional(&state.db)
            .await?;
    if provider_exists.is_none() {
        return Err(AppError::BadRequest("Provider not found".into()));
    }

    let weight = req.weight.unwrap_or(100);

    // Check for existing route to give a friendly error instead of 500.
    // Uniqueness is on (model_id, provider_id, upstream_model), so the
    // dup check has to match — same provider with a different upstream
    // is a legal second route. `IS NOT DISTINCT FROM` mirrors the
    // schema's `UNIQUE NULLS NOT DISTINCT` semantics for NULL upstream.
    let existing: Option<Uuid> = sqlx::query_scalar(
        r#"SELECT id FROM model_routes
           WHERE model_id = $1
             AND provider_id = $2
             AND upstream_model IS NOT DISTINCT FROM $3"#,
    )
    .bind(&model_id)
    .bind(req.provider_id)
    .bind(&req.upstream_model)
    .fetch_optional(&state.db)
    .await?;
    if existing.is_some() {
        return Err(AppError::BadRequest(
            "A route for this model+provider+upstream already exists".into(),
        ));
    }

    if let Some(c) = req.rpm_cap
        && c <= 0
    {
        return Err(AppError::BadRequest("rpm_cap must be > 0".into()));
    }
    if let Some(c) = req.tpm_cap
        && c <= 0
    {
        return Err(AppError::BadRequest("tpm_cap must be > 0".into()));
    }

    let row = sqlx::query_as::<_, ModelRouteRow>(
        r#"INSERT INTO model_routes
              (model_id, provider_id, upstream_model, weight, enabled,
               label, notes, rpm_cap, tpm_cap)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
           RETURNING id, model_id, provider_id,
                     (SELECT name FROM providers WHERE id = provider_id) AS provider_name,
                     upstream_model, weight, enabled,
                     label, notes, rpm_cap, tpm_cap"#,
    )
    .bind(&model_id)
    .bind(req.provider_id)
    .bind(&req.upstream_model)
    .bind(weight)
    .bind(req.enabled.unwrap_or(true))
    .bind(req.label.as_deref().filter(|s| !s.is_empty()))
    .bind(req.notes.as_deref().filter(|s| !s.is_empty()))
    .bind(req.rpm_cap)
    .bind(req.tpm_cap)
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("model_route.created")
            .resource("model_route")
            .resource_id(row.id.to_string())
            .detail(serde_json::json!({
                "model_id": &model_id,
                "provider_id": req.provider_id,
            })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(row))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateModelRouteRequest {
    /// PATCH semantics: absent = unchanged, JSON `null` = clear (the
    /// route falls back to using `model_id` as the upstream identifier),
    /// JSON string = explicit override.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub upstream_model: Option<Option<String>>,
    pub weight: Option<i32>,
    pub enabled: Option<bool>,
    /// PATCH-clearable label.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub label: Option<Option<String>>,
    /// PATCH-clearable note.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub notes: Option<Option<String>>,
    /// PATCH-clearable RPM cap. Must be > 0 if set.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub rpm_cap: Option<Option<i32>>,
    /// PATCH-clearable TPM cap. Must be > 0 if set.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub tpm_cap: Option<Option<i32>>,
}

/// PATCH /api/admin/model-routes/{route_id}
pub async fn update_model_route(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(route_id): Path<Uuid>,
    Json(req): Json<UpdateModelRouteRequest>,
) -> Result<Json<ModelRouteRow>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    let (upstream_set, upstream_value) = match &req.upstream_model {
        None => (false, None),
        Some(inner) => (true, inner.as_deref()),
    };
    let (label_set, label_value) = match &req.label {
        None => (false, None),
        Some(inner) => (true, inner.as_deref().filter(|s| !s.is_empty())),
    };
    let (notes_set, notes_value) = match &req.notes {
        None => (false, None),
        Some(inner) => (true, inner.as_deref().filter(|s| !s.is_empty())),
    };
    let (rpm_set, rpm_value) = match req.rpm_cap {
        None => (false, None),
        Some(inner) => (true, inner),
    };
    let (tpm_set, tpm_value) = match req.tpm_cap {
        None => (false, None),
        Some(inner) => (true, inner),
    };
    if let Some(c) = rpm_value
        && c <= 0
    {
        return Err(AppError::BadRequest("rpm_cap must be > 0".into()));
    }
    if let Some(c) = tpm_value
        && c <= 0
    {
        return Err(AppError::BadRequest("tpm_cap must be > 0".into()));
    }

    let row = sqlx::query_as::<_, ModelRouteRow>(
        r#"UPDATE model_routes SET
              upstream_model = CASE WHEN $5  THEN $2  ELSE upstream_model END,
              weight   = COALESCE($3, weight),
              enabled  = COALESCE($4, enabled),
              label    = CASE WHEN $7  THEN $6  ELSE label    END,
              notes    = CASE WHEN $9  THEN $8  ELSE notes    END,
              rpm_cap  = CASE WHEN $11 THEN $10 ELSE rpm_cap  END,
              tpm_cap  = CASE WHEN $13 THEN $12 ELSE tpm_cap  END
           WHERE id = $1
           RETURNING id, model_id, provider_id,
                     (SELECT name FROM providers WHERE id = provider_id) AS provider_name,
                     upstream_model, weight, enabled,
                     label, notes, rpm_cap, tpm_cap"#,
    )
    .bind(route_id)
    .bind(upstream_value)
    .bind(req.weight)
    .bind(req.enabled)
    .bind(upstream_set)
    .bind(label_value)
    .bind(label_set)
    .bind(notes_value)
    .bind(notes_set)
    .bind(rpm_value)
    .bind(rpm_set)
    .bind(tpm_value)
    .bind(tpm_set)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Route not found".into()))?;

    state.audit.log(
        auth_user
            .audit("model_route.updated")
            .resource("model_route")
            .resource_id(route_id.to_string()),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(row))
}

/// DELETE /api/admin/model-routes/{route_id}
pub async fn delete_model_route(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(route_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    let result = sqlx::query("DELETE FROM model_routes WHERE id = $1")
        .bind(route_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Route not found".into()));
    }

    state.audit.log(
        auth_user
            .audit("model_route.deleted")
            .resource("model_route")
            .resource_id(route_id.to_string()),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// ---------------------------------------------------------------------------
// Flat route listing (all routes, paginated)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RouteListQuery {
    pub q: Option<String>,
    pub provider_id: Option<Uuid>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RouteListResponse {
    pub items: Vec<ModelRouteRow>,
    pub total: i64,
}

/// GET /api/admin/model-routes — paginated flat list of all routes.
pub async fn list_all_routes(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<RouteListQuery>,
) -> Result<Json<RouteListResponse>, AppError> {
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;

    let page_size = q.page_size.unwrap_or(50).clamp(1, 200);
    let page = q.page.unwrap_or(1).max(1);
    let offset = (page - 1) * page_size;
    let search = q.q.as_deref().unwrap_or("").trim();
    let search_pattern = format!("%{search}%");

    let (rows, total) = if search.is_empty() && q.provider_id.is_none() {
        let total: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*) FROM model_routes mr JOIN providers p ON p.id = mr.provider_id WHERE p.deleted_at IS NULL",
        )
        .fetch_one(&state.db)
        .await?;
        let rows = sqlx::query_as::<_, ModelRouteRow>(
            r#"SELECT mr.id, mr.model_id, mr.provider_id, p.name AS provider_name,
                      mr.upstream_model, mr.weight, mr.enabled,
                      mr.label, mr.notes, mr.rpm_cap, mr.tpm_cap
               FROM model_routes mr
               JOIN providers p ON p.id = mr.provider_id
               WHERE p.deleted_at IS NULL
               ORDER BY mr.model_id, mr.weight DESC
               LIMIT $1 OFFSET $2"#,
        )
        .bind(page_size)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
        (rows, total.unwrap_or(0))
    } else {
        let total: Option<i64> = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM model_routes mr
               JOIN providers p ON p.id = mr.provider_id
               WHERE p.deleted_at IS NULL
                 AND ($1 = '' OR mr.model_id ILIKE $2 OR p.name ILIKE $2)
                 AND ($3::UUID IS NULL OR mr.provider_id = $3)"#,
        )
        .bind(search)
        .bind(&search_pattern)
        .bind(q.provider_id)
        .fetch_one(&state.db)
        .await?;
        let rows = sqlx::query_as::<_, ModelRouteRow>(
            r#"SELECT mr.id, mr.model_id, mr.provider_id, p.name AS provider_name,
                      mr.upstream_model, mr.weight, mr.enabled,
                      mr.label, mr.notes, mr.rpm_cap, mr.tpm_cap
               FROM model_routes mr
               JOIN providers p ON p.id = mr.provider_id
               WHERE p.deleted_at IS NULL
                 AND ($1 = '' OR mr.model_id ILIKE $2 OR p.name ILIKE $2)
                 AND ($3::UUID IS NULL OR mr.provider_id = $3)
               ORDER BY mr.model_id, mr.weight DESC
               LIMIT $4 OFFSET $5"#,
        )
        .bind(search)
        .bind(&search_pattern)
        .bind(q.provider_id)
        .bind(page_size)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
        (rows, total.unwrap_or(0))
    };

    Ok(Json(RouteListResponse { items: rows, total }))
}

// ---------------------------------------------------------------------------
// Batch import routes (two-step dialog driver)
//
// Clients pick N remote models from a provider's catalog and decide per
// item whether each one should:
//
//   * `new`     — become a new exposed catalog entry (`models` row) with
//                 a route to the provider (upstream_model = same name)
//   * `attach`  — become a new route on an existing exposed model,
//                 optionally renaming (upstream_model = the remote name,
//                 model_id = whatever the admin already exposes)
//
// The second mode is the aggregator case: OpenRouter exposes
// `openai/gpt-4o`, but you already expose `gpt-4o` via direct OpenAI.
// Selecting `attach` adds OpenRouter as a fallback route on the same
// exposed entry — no duplicate catalog rows.
//
// Imported routes are enabled on creation (column default), so they
// land in `/v1/models` immediately. The dialog is two-step + per-item
// review precisely so the admin opts in deliberately, not in bulk.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BatchImportItem {
    /// Name as it appears in the provider's remote catalog. Always
    /// goes into `model_routes.upstream_model` (NULL when it matches
    /// the exposed model_id, to keep the table tidy).
    pub upstream: String,
    /// When set, attach a route to this existing `models.model_id`
    /// instead of creating a new catalog entry.
    pub target_model_id: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BatchCreateRoutesRequest {
    pub provider_id: Uuid,
    pub items: Vec<BatchImportItem>,
}

/// POST /api/admin/model-routes/batch
pub async fn batch_create_routes(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BatchCreateRoutesRequest>,
) -> Result<Json<Value>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    if req.items.is_empty() {
        return Err(AppError::BadRequest("items is empty".into()));
    }

    let provider_exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM providers WHERE id = $1 AND deleted_at IS NULL")
            .bind(req.provider_id)
            .fetch_optional(&state.db)
            .await?;
    if provider_exists.is_none() {
        return Err(AppError::BadRequest("Provider not found".into()));
    }

    // Split the request into the two flows. Each flow is one bulk
    // INSERT via UNNEST so we stay at O(1) round trips regardless of N.
    let mut new_ids: Vec<String> = Vec::new();
    let mut attach_targets: Vec<String> = Vec::new();
    let mut attach_upstreams: Vec<String> = Vec::new();

    for it in &req.items {
        match &it.target_model_id {
            None => new_ids.push(it.upstream.clone()),
            Some(target) => {
                attach_targets.push(target.clone());
                attach_upstreams.push(it.upstream.clone());
            }
        }
    }

    let mut tx = state.db.begin().await?;

    // --- "new" items -----------------------------------------------
    //
    // Catalog insert is idempotent. Route insert counts rows via the
    // RETURNING/CTE pattern so the response's `created` count reflects
    // only rows that actually landed (skipping ON CONFLICT dupes).
    let new_inserted: i64 = if new_ids.is_empty() {
        0
    } else {
        sqlx::query(
            r#"INSERT INTO models (model_id, display_name)
               SELECT m, m FROM UNNEST($1::TEXT[]) AS t(m)
               ON CONFLICT (model_id) DO NOTHING"#,
        )
        .bind(&new_ids)
        .execute(&mut *tx)
        .await?;

        sqlx::query_scalar::<_, i64>(
            r#"WITH ins AS (
                 INSERT INTO model_routes (model_id, provider_id, weight)
                 SELECT m, $2, 100 FROM UNNEST($1::TEXT[]) AS t(m)
                 ON CONFLICT (model_id, provider_id, upstream_model) DO NOTHING
                 RETURNING 1
               )
               SELECT COUNT(*) FROM ins"#,
        )
        .bind(&new_ids)
        .bind(req.provider_id)
        .fetch_one(&mut *tx)
        .await?
    };

    // --- "attach" items --------------------------------------------
    //
    // Targets that don't exist in `models` are silently skipped
    // (EXISTS guard below) to avoid a FK failure on a typo. The audit
    // log records the discrepancy via the created/requested deltas.
    let attach_inserted: i64 = if attach_targets.is_empty() {
        0
    } else {
        sqlx::query_scalar::<_, i64>(
            r#"WITH ins AS (
                 INSERT INTO model_routes
                     (model_id, provider_id, upstream_model, weight)
                 SELECT t.target, $3, t.upstream, 100
                 FROM UNNEST($1::TEXT[], $2::TEXT[])
                   AS t(target, upstream)
                 WHERE EXISTS (SELECT 1 FROM models m WHERE m.model_id = t.target)
                 ON CONFLICT (model_id, provider_id, upstream_model) DO NOTHING
                 RETURNING 1
               )
               SELECT COUNT(*) FROM ins"#,
        )
        .bind(&attach_targets)
        .bind(&attach_upstreams)
        .bind(req.provider_id)
        .fetch_one(&mut *tx)
        .await?
    };

    tx.commit().await?;
    let created = new_inserted + attach_inserted;

    state.audit.log(
        auth_user
            .audit("model_routes.batch_created")
            .resource("model_routes")
            .detail(serde_json::json!({
                "provider_id": req.provider_id,
                "new": new_ids.len(),
                "attach": attach_targets.len(),
                "created": created,
            })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(serde_json::json!({ "created": created })))
}

// ---------------------------------------------------------------------------
// Batch delete / enable-toggle routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BatchRouteIdsRequest {
    pub ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BatchUpdateRoutesRequest {
    pub ids: Vec<Uuid>,
    pub enabled: bool,
}

/// POST /api/admin/model-routes/batch-delete
pub async fn batch_delete_routes(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BatchRouteIdsRequest>,
) -> Result<Json<Value>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    if req.ids.is_empty() {
        return Err(AppError::BadRequest("ids is empty".into()));
    }

    let result = sqlx::query("DELETE FROM model_routes WHERE id = ANY($1)")
        .bind(&req.ids)
        .execute(&state.db)
        .await?;

    let deleted = result.rows_affected() as i64;

    state.audit.log(
        auth_user
            .audit("model_routes.batch_deleted")
            .resource("model_routes")
            .detail(serde_json::json!({
                "requested": req.ids.len(),
                "deleted": deleted,
            })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

// ---------------------------------------------------------------------------
// Batch weights update — used by the wizard's drag-to-redistribute bar and
// the [均分] / [同步自动] convenience buttons. One transaction, one rebuild.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BatchWeightUpdate {
    pub id: Uuid,
    pub weight: i32,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BatchWeightsRequest {
    pub updates: Vec<BatchWeightUpdate>,
}

/// PATCH /api/admin/model-routes/batch-weights
#[utoipa::path(
    patch,
    path = "/api/admin/model-routes/batch-weights",
    tag = "Models",
    security(("bearer_token" = [])),
    request_body = BatchWeightsRequest,
    responses(
        (status = 200, description = "Weights updated"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn batch_update_route_weights(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BatchWeightsRequest>,
) -> Result<Json<Value>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    if req.updates.is_empty() {
        return Err(AppError::BadRequest("updates is empty".into()));
    }
    if req.updates.iter().any(|u| u.weight < 0) {
        return Err(AppError::BadRequest("weight must be >= 0".into()));
    }

    // One transaction so partial failures roll back — admins shouldn't
    // see "1/3 of my drag landed".
    let mut tx = state.db.begin().await?;
    let mut updated = 0i64;
    for u in &req.updates {
        let result = sqlx::query("UPDATE model_routes SET weight = $1 WHERE id = $2")
            .bind(u.weight)
            .bind(u.id)
            .execute(&mut *tx)
            .await?;
        updated += result.rows_affected() as i64;
    }
    tx.commit().await?;

    state.audit.log(
        auth_user
            .audit("model_routes.batch_weights_updated")
            .resource("model_routes")
            .detail(serde_json::json!({
                "count": req.updates.len(),
                "updated": updated,
            })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(serde_json::json!({ "updated": updated })))
}

// ---------------------------------------------------------------------------
// Routing projection — "if this model's strategy is X and these are the
// candidates, here's the expected traffic split + cost." Used by the
// wizard for the "expected cost / 1M tokens" line and the "Match auto"
// convenience button.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RoutingProjectionEntry {
    pub route_id: Uuid,
    pub provider_name: String,
    pub upstream_model: Option<String>,
    pub label: Option<String>,
    pub weight: i32,
    pub expected_pct: f64,
    pub cost_per_token: Option<f64>,
    pub ewma_latency_ms: Option<f64>,
    pub healthy: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RoutingProjectionView {
    pub strategy: String,
    pub entries: Vec<RoutingProjectionEntry>,
    /// Traffic-weighted cost per 1M tokens under this projection
    /// (sum of `expected_pct * cost_per_token * 1_000_000`).
    pub expected_cost_per_1m_tokens: Option<f64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RoutingProjectionResponse {
    /// What the gateway will actually do given the model's stored
    /// strategy + current weights.
    pub current: RoutingProjectionView,
    /// What the gateway would do if the strategy were the global
    /// default (typically `latency_cost`). Used by "Match auto".
    pub auto: RoutingProjectionView,
}

/// GET /api/admin/models/{model_id}/routing-projection
#[utoipa::path(
    get,
    path = "/api/admin/models/{model_id}/routing-projection",
    tag = "Models",
    security(("bearer_token" = [])),
    params(("model_id" = String, Path, description = "Model ID")),
    responses(
        (status = 200, description = "Projection", body = RoutingProjectionResponse),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "No routes for model"),
    )
)]
pub async fn get_routing_projection(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<Json<RoutingProjectionResponse>, AppError> {
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;

    let router = state.gateway_router.load();
    // Note: `RouteEntry` isn't Clone (it owns an Arc<dyn Provider>), so
    // we can't `.cloned()` the Vec — we work with refs while the guard
    // is alive, capturing only the projection inputs we need.
    let entries_opt = router.route(&model_id);
    let Some(entries) = entries_opt else {
        return Err(AppError::NotFound("No routes registered for model".into()));
    };
    if entries.is_empty() {
        return Err(AppError::NotFound("No routes registered for model".into()));
    }
    let model_cfg = router.config_for(&model_id);

    // Snapshot health for each route. HealthTracker is a thin
    // Redis-backed facade — cheap to construct ad hoc.
    let health = think_watch_gateway::health::HealthTracker::new(state.redis.clone());
    let cb_window = state.dynamic_config.cb_window_secs().await;
    let mut health_snapshots = Vec::with_capacity(entries.len());
    for e in entries.iter() {
        let h = health.snapshot(e.route_id, cb_window).await;
        health_snapshots.push(h);
    }

    // Resolve current strategy (model override → global default).
    let global_default = state.dynamic_config.default_routing_strategy().await;
    use std::str::FromStr;
    use think_watch_gateway::strategy::{RouteSignal, RoutingStrategy, compute_weights};
    let current_strategy = model_cfg
        .strategy
        .unwrap_or_else(|| RoutingStrategy::from_str(&global_default).unwrap_or_default());
    let auto_strategy = RoutingStrategy::from_str(&global_default).unwrap_or_default();

    let latency_k = state.dynamic_config.latency_strategy_k().await;

    let project = |strategy: RoutingStrategy| -> RoutingProjectionView {
        let signals: Vec<RouteSignal> = entries
            .iter()
            .zip(health_snapshots.iter())
            .map(|(e, h)| RouteSignal {
                configured_weight: e.weight,
                ewma_latency_ms: h.ewma_latency_ms,
                cost_per_token: e.cost_per_token,
            })
            .collect();
        let weights = compute_weights(strategy, &signals, latency_k);
        let total: f64 = weights.iter().sum();
        let mut total_cost = 0.0_f64;
        let mut have_cost = false;
        let mut entries_out = Vec::with_capacity(entries.len());
        for ((e, h), w) in entries
            .iter()
            .zip(health_snapshots.iter())
            .zip(weights.iter())
        {
            let pct = if total > 0.0 { w / total } else { 0.0 };
            if let Some(c) = e.cost_per_token {
                total_cost += c * pct;
                have_cost = true;
            }
            entries_out.push(RoutingProjectionEntry {
                route_id: e.route_id,
                provider_name: e.provider_name.clone(),
                upstream_model: e.upstream_model.clone(),
                label: e.label.clone(),
                weight: e.weight as i32,
                expected_pct: pct * 100.0,
                cost_per_token: e.cost_per_token,
                ewma_latency_ms: h.ewma_latency_ms,
                healthy: matches!(
                    h.state,
                    think_watch_gateway::health::BreakerState::Closed
                        | think_watch_gateway::health::BreakerState::HalfOpen
                ),
            });
        }
        RoutingProjectionView {
            strategy: strategy.as_str().to_string(),
            entries: entries_out,
            expected_cost_per_1m_tokens: if have_cost {
                Some(total_cost * 1_000_000.0)
            } else {
                None
            },
        }
    };

    Ok(Json(RoutingProjectionResponse {
        current: project(current_strategy),
        auto: project(auto_strategy),
    }))
}

// ---------------------------------------------------------------------------
// Per-route history — feeds the wizard's inline latency sparkline.
// 60 one-minute buckets out of ClickHouse.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RouteHistoryBucket {
    /// Bucket start, unix seconds.
    pub ts: i64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub requests: u64,
    pub errors: u64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RouteHistoryResponse {
    pub buckets: Vec<RouteHistoryBucket>,
}

#[derive(Debug, Deserialize)]
pub struct RouteHistoryQuery {
    pub route_id: Uuid,
    /// Window length in seconds (default 3600 = 1 hour).
    pub window: Option<i64>,
}

/// GET /api/admin/models/{model_id}/route-history?route_id=X&window=3600
#[utoipa::path(
    get,
    path = "/api/admin/models/{model_id}/route-history",
    tag = "Models",
    security(("bearer_token" = [])),
    params(
        ("model_id" = String, Path, description = "Model ID"),
        ("route_id" = uuid::Uuid, Query, description = "Specific route to query"),
        ("window" = Option<i64>, Query, description = "Window in seconds (default 3600)"),
    ),
    responses(
        (status = 200, description = "Per-minute history", body = RouteHistoryResponse),
        (status = 403, description = "Forbidden"),
        (status = 503, description = "ClickHouse not configured"),
    )
)]
pub async fn get_route_history(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(_model_id): Path<String>,
    Query(q): Query<RouteHistoryQuery>,
) -> Result<Json<RouteHistoryResponse>, AppError> {
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;

    let Some(ch) = state.clickhouse.as_ref() else {
        // No CH attached (dev w/o opt-in) — return an empty history
        // rather than 503 so the sparkline just renders blank.
        return Ok(Json(RouteHistoryResponse {
            buckets: Vec::new(),
        }));
    };

    let window = q.window.unwrap_or(3600).clamp(60, 86400);
    let now = chrono::Utc::now().timestamp();
    let from = now - window;

    // Aggregate latency per minute. The actual table name + column
    // names match those used by `gateway_logs` writes; if the query
    // fails (table not yet provisioned, CH down), we fall back to an
    // empty response — the sparkline is a hint, not load-bearing.
    let sql = format!(
        r#"SELECT toUnixTimestamp(toStartOfMinute(toDateTime(ts))) AS bucket_ts,
                  quantile(0.50)(latency_ms)        AS p50,
                  quantile(0.95)(latency_ms)        AS p95,
                  count()                            AS requests,
                  countIf(error_class != '')         AS errors
           FROM gateway_logs
           WHERE route_id = '{}'
             AND ts >= toDateTime({})
           GROUP BY bucket_ts
           ORDER BY bucket_ts"#,
        q.route_id, from
    );

    #[derive(Debug, clickhouse::Row, serde::Deserialize)]
    struct Row {
        bucket_ts: i64,
        p50: f64,
        p95: f64,
        requests: u64,
        errors: u64,
    }

    let rows: Vec<Row> = match ch.query(&sql).fetch_all::<Row>().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("route-history CH query failed (returning empty): {e}");
            return Ok(Json(RouteHistoryResponse {
                buckets: Vec::new(),
            }));
        }
    };

    let buckets = rows
        .into_iter()
        .map(|r| RouteHistoryBucket {
            ts: r.bucket_ts,
            p50_ms: if r.p50.is_finite() { Some(r.p50) } else { None },
            p95_ms: if r.p95.is_finite() { Some(r.p95) } else { None },
            requests: r.requests,
            errors: r.errors,
        })
        .collect();

    Ok(Json(RouteHistoryResponse { buckets }))
}

/// POST /api/admin/model-routes/batch-update — flips `enabled` for many routes.
pub async fn batch_update_routes(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BatchUpdateRoutesRequest>,
) -> Result<Json<Value>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    if req.ids.is_empty() {
        return Err(AppError::BadRequest("ids is empty".into()));
    }

    let result = sqlx::query("UPDATE model_routes SET enabled = $1 WHERE id = ANY($2)")
        .bind(req.enabled)
        .bind(&req.ids)
        .execute(&state.db)
        .await?;

    let updated = result.rows_affected() as i64;

    state.audit.log(
        auth_user
            .audit("model_routes.batch_updated")
            .resource("model_routes")
            .detail(serde_json::json!({
                "requested": req.ids.len(),
                "updated": updated,
                "enabled": req.enabled,
            })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(serde_json::json!({ "updated": updated })))
}

// ---------------------------------------------------------------------------
// Fetch remote models from a provider (for the add dialog)
// ---------------------------------------------------------------------------

/// GET /api/admin/providers/{id}/remote-models
pub async fn list_remote_models(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(provider_id): Path<Uuid>,
) -> Result<Json<Vec<String>>, AppError> {
    auth_user.require_permission("models:read")?;

    let provider = sqlx::query_as::<_, think_watch_common::models::Provider>(
        "SELECT * FROM providers WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(provider_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("Provider not found".into()))?;

    let headers: Vec<ProviderHeader> = provider
        .config_json
        .get("headers")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let test_req = super::providers::TestProviderRequest {
        provider_type: provider.provider_type.clone(),
        base_url: provider.base_url.clone(),
        headers,
    };

    let Json(resp) = super::providers::run_provider_test(test_req).await?;
    if !resp.success {
        return Err(AppError::BadRequest(format!(
            "Provider unreachable: {}",
            resp.message
        )));
    }

    Ok(Json(resp.models.unwrap_or_default()))
}
