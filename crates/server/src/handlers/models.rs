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
use think_watch_gateway::output_guardrails::{MAX_LENGTH_CAP_CEILING, OutputGuardrail};

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
    /// Model-level kill switch. FALSE ⇒ all routes are skipped at
    /// router-bootstrap (gateway behaves as if the model has no routes).
    /// Independent of per-route `enabled` so flipping back restores the
    /// previous traffic split exactly.
    pub enabled: bool,
    /// Provider display names (or `name` if display_name is null) for
    /// every route attached to the model, ordered by weight DESC. Lets
    /// the list table show "who serves this?" without an extra fetch.
    pub providers: Vec<String>,
    /// Per-model routing override. `None` ⇒ inherit
    /// `gateway.default_routing_strategy`. The detail drawer reads this
    /// to label the strategy picker — without it, refetch-after-PATCH
    /// can't reflect the new value.
    pub routing_strategy: Option<String>,
    pub affinity_mode: Option<String>,
    pub affinity_ttl_secs: Option<i32>,
    /// Output guardrails as stored in JSONB. The list endpoint returns
    /// the raw `Value` (rather than `Vec<OutputGuardrail>`) so the UI
    /// can render unrecognised future variants without breaking. The
    /// shape is `[{ "type": "max_length", "max_chars": N }, ...]`.
    #[schema(value_type = serde_json::Value)]
    pub output_guardrails: serde_json::Value,
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
    auth_user
        .require_global_permission(&state.db, "models:read")
        .await?;

    let page_size = query.page_size.unwrap_or(50).clamp(1, 200);
    let page = query.page.unwrap_or(1).max(1);
    let offset = (page - 1) * page_size;
    let search = query.q.as_deref().unwrap_or("").trim();
    let search_pattern = format!("%{search}%");
    let status = query.status.as_deref().unwrap_or("");

    // Unified query with `$1='' OR ...` to combine optional search +
    // status filter. `status_filter`:
    //   'active'    — m.enabled = true AND enabled_route_count > 0
    //   'disabled'  — m.enabled = false, OR
    //                 (m.enabled = true AND route_count > 0 AND enabled_route_count = 0)
    //   'unrouted'  — route_count = 0
    //   otherwise   — no filter
    //
    // We compute `route_count` / `enabled_route_count` via `LATERAL`
    // subquery so the filter happens on the joined shape; PG rewrites
    // this to a HashAggregate over `model_routes`.
    let status_filter_sql = match status {
        "active" => "AND m.enabled = true AND rc.enabled_route_count > 0",
        "disabled" => {
            "AND (m.enabled = false OR (rc.route_count > 0 AND rc.enabled_route_count = 0))"
        }
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
                  m.enabled,
                  COALESCE(rc.providers, '{{}}'::text[]) AS providers,
                  m.routing_strategy, m.affinity_mode, m.affinity_ttl_secs,
                  m.output_guardrails
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
    /// inherit. One of weighted/latency/health/latency_health.
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
    /// Optional per-model output guardrails. NULL/missing = empty
    /// list. See [`OutputGuardrail`] for the variant set; the
    /// gateway crate is the source of truth.
    #[serde(default)]
    #[schema(value_type = Vec<serde_json::Value>)]
    pub output_guardrails: Option<Vec<OutputGuardrail>>,
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
    auth_user
        .require_global_permission(&state.db, "models:write")
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
    let guardrails = req.output_guardrails.unwrap_or_default();
    validate_output_guardrails(&guardrails)?;
    let guardrails_json = serde_json::to_value(&guardrails)
        .map_err(|e| AppError::BadRequest(format!("failed to serialize output_guardrails: {e}")))?;

    let model = sqlx::query_as::<_, Model>(
        r#"INSERT INTO models
              (model_id, display_name, input_weight, output_weight,
               routing_strategy, affinity_mode, affinity_ttl_secs, tags,
               output_guardrails)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
           RETURNING id, model_id, display_name, input_weight, output_weight,
                     routing_strategy, affinity_mode, affinity_ttl_secs, tags, enabled,
                     output_guardrails"#,
    )
    .bind(&req.model_id)
    .bind(&req.display_name)
    .bind(in_w)
    .bind(out_w)
    .bind(&req.routing_strategy)
    .bind(&req.affinity_mode)
    .bind(req.affinity_ttl_secs)
    .bind(req.tags.as_deref())
    .bind(&guardrails_json)
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("model.created")
            .resource("model")
            .resource_id(model.id.to_string())
            .detail(serde_json::json!({ "model_id": &req.model_id })),
    );

    // Invalidate the shared weight cache so subsequent gateway requests
    // pick up the new model's weights immediately — otherwise the
    // limits engine and cost tracker run on stale cache misses until
    // the 5-min TTL elapses.
    state.weight_cache.invalidate_all().await;

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
    /// to global default), string = override.
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
    /// Model-level kill switch. Absent = unchanged.
    pub enabled: Option<bool>,
    /// PATCH-clearable output guardrails. Absent = unchanged, JSON
    /// `null` = clear (no guardrails), array = replace the whole
    /// list. Validation runs over the supplied list before persisting.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<Vec<serde_json::Value>>)]
    pub output_guardrails: Option<Option<Vec<OutputGuardrail>>>,
}

/// Validate each guardrail's parameters before they hit the DB.
/// Today only `MaxLength` is wired; future variants land here as
/// their own match arm. Rejection short-circuits with a 400 so the
/// admin sees a useful message rather than the row landing and then
/// blowing up at request time.
pub(crate) fn validate_output_guardrails(rules: &[OutputGuardrail]) -> Result<(), AppError> {
    for rule in rules {
        match rule {
            OutputGuardrail::MaxLength { max_chars } => {
                // 0 is a config bug (every response is rejected). The
                // ceiling caps absurd values so the column can't be
                // used as a "guardrail off-but-not-removed" toggle.
                if *max_chars == 0 || *max_chars > MAX_LENGTH_CAP_CEILING {
                    return Err(AppError::BadRequest(format!(
                        "output_guardrails: max_length.max_chars must be 1..={MAX_LENGTH_CAP_CEILING}"
                    )));
                }
            }
        }
    }
    Ok(())
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
        && !["weighted", "latency", "health", "latency_health"].contains(&s)
    {
        return Err(AppError::BadRequest(
            "routing_strategy must be one of: weighted, latency, health, latency_health".into(),
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
    auth_user
        .require_global_permission(&state.db, "models:write")
        .await?;
    let existing = sqlx::query_as::<_, Model>(
        r#"SELECT id, model_id, display_name, input_weight, output_weight,
                  routing_strategy, affinity_mode, affinity_ttl_secs, tags, enabled,
                  output_guardrails
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
    // Guardrails PATCH: absent ⇒ keep existing JSON as-is; Some(None)
    // ⇒ clear (empty list); Some(Some(rules)) ⇒ validate + replace.
    let new_guardrails_json: serde_json::Value = match &req.output_guardrails {
        None => existing.output_guardrails.clone(),
        Some(None) => serde_json::Value::Array(Vec::new()),
        Some(Some(rules)) => {
            validate_output_guardrails(rules)?;
            serde_json::to_value(rules).map_err(|e| {
                AppError::BadRequest(format!("failed to serialize output_guardrails: {e}"))
            })?
        }
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
              tags              = $8,
              enabled           = $9,
              output_guardrails = $10
           WHERE id = $1
           RETURNING id, model_id, display_name, input_weight, output_weight,
                     routing_strategy, affinity_mode, affinity_ttl_secs, tags, enabled,
                     output_guardrails"#,
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
    .bind(req.enabled.unwrap_or(existing.enabled))
    .bind(&new_guardrails_json)
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

    // Drop the per-model weight cache so updated input/output weights
    // take effect immediately on the next gateway request.
    state.weight_cache.invalidate_all().await;

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
    auth_user
        .require_global_permission(&state.db, "models:write")
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
    state.weight_cache.invalidate_all().await;
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
    auth_user
        .require_global_permission(&state.db, "models:read")
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
    auth_user
        .require_global_permission(&state.db, "models:write")
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
    auth_user
        .require_global_permission(&state.db, "models:write")
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
    state.weight_cache.invalidate_all().await;

    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

// ---------------------------------------------------------------------------
// Bulk flip the model-level kill switch on a set of catalog entries
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BulkSetEnabledModelsRequest {
    pub ids: Vec<Uuid>,
    pub enabled: bool,
}

/// `POST /api/admin/models/bulk-set-enabled` — flip the `enabled` flag
/// on each catalog entry. The route-level `enabled` bits are left
/// alone, so re-enabling restores the previous traffic split exactly
/// (model-level kill switch is orthogonal to per-route load mgmt).
pub async fn bulk_set_enabled_models(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BulkSetEnabledModelsRequest>,
) -> Result<Json<Value>, AppError> {
    auth_user
        .require_global_permission(&state.db, "models:write")
        .await?;

    if req.ids.is_empty() {
        return Err(AppError::BadRequest("ids is empty".into()));
    }

    let result = sqlx::query(
        r#"UPDATE models
              SET enabled = $2
            WHERE id = ANY($1)
              AND enabled IS DISTINCT FROM $2"#,
    )
    .bind(&req.ids)
    .bind(req.enabled)
    .execute(&state.db)
    .await?;

    let updated = result.rows_affected() as i64;

    state.audit.log(
        auth_user
            .audit("models.bulk_set_enabled")
            .resource("models")
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
// Model Routes CRUD
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ModelRouteRow {
    pub id: Uuid,
    pub model_id: String,
    pub provider_id: Uuid,
    pub provider_name: String,
    pub upstream_model: String,
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
    auth_user
        .require_global_permission(&state.db, "models:read")
        .await?;

    // Order by creation time so the routes table and the traffic-share
    // sliders stay in the same place when admins drag weights — sorting
    // by weight DESC made rows jump around as soon as you adjusted the
    // ratios, which the operator UI shouldn't do.
    let rows = sqlx::query_as::<_, ModelRouteRow>(
        r#"SELECT mr.id, mr.model_id, mr.provider_id, p.name AS provider_name,
                  mr.upstream_model, mr.weight, mr.enabled,
                  mr.label, mr.notes, mr.rpm_cap, mr.tpm_cap
           FROM model_routes mr
           JOIN providers p ON p.id = mr.provider_id
           WHERE mr.model_id = $1 AND p.deleted_at IS NULL
           ORDER BY mr.created_at, mr.id"#,
    )
    .bind(&model_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(rows))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateModelRouteRequest {
    pub provider_id: Uuid,
    /// Upstream model name sent to the provider. Optional on the wire —
    /// when absent or empty the server fills it with `model_id`.
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
    auth_user
        .require_global_permission(&state.db, "models:write")
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
    let upstream_model = req
        .upstream_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&model_id)
        .to_string();

    // Check for existing route to give a friendly error instead of 500.
    // Uniqueness is on (model_id, provider_id, upstream_model), so the
    // dup check has to match — same provider with a different upstream
    // is a legal second route.
    let existing: Option<Uuid> = sqlx::query_scalar(
        r#"SELECT id FROM model_routes
           WHERE model_id = $1
             AND provider_id = $2
             AND upstream_model = $3"#,
    )
    .bind(&model_id)
    .bind(req.provider_id)
    .bind(&upstream_model)
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
    .bind(&upstream_model)
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
    /// PATCH semantics: absent = unchanged, JSON string = new value.
    /// Cannot be cleared — the column is NOT NULL.
    pub upstream_model: Option<String>,
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
    auth_user
        .require_global_permission(&state.db, "models:write")
        .await?;

    let upstream_value = req
        .upstream_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if req.upstream_model.is_some() && upstream_value.is_none() {
        return Err(AppError::BadRequest(
            "upstream_model cannot be empty".into(),
        ));
    }
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
              upstream_model = COALESCE($2, upstream_model),
              weight   = COALESCE($3, weight),
              enabled  = COALESCE($4, enabled),
              label    = CASE WHEN $6  THEN $5  ELSE label    END,
              notes    = CASE WHEN $8  THEN $7  ELSE notes    END,
              rpm_cap  = CASE WHEN $10 THEN $9  ELSE rpm_cap  END,
              tpm_cap  = CASE WHEN $12 THEN $11 ELSE tpm_cap  END
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
    auth_user
        .require_global_permission(&state.db, "models:write")
        .await?;

    let result = sqlx::query("DELETE FROM model_routes WHERE id = $1")
        .bind(route_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Route not found".into()));
    }

    // Purge the per-route Redis keys (samples / state / counters).
    // The lifetime counter hash has no TTL by design, so without this
    // it would leak forever every time an admin deletes a route.
    think_watch_gateway::health::HealthTracker::new(state.redis.clone())
        .forget(route_id)
        .await;

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
    auth_user
        .require_global_permission(&state.db, "models:read")
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
//                 a route to the provider (upstream_model = exposed id)
//   * `attach`  — become a new route on an existing exposed model
//                 (upstream_model = the remote name, model_id = whatever
//                 the admin already exposes)
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
    /// Name as it appears in the provider's remote catalog. Goes into
    /// `model_routes.upstream_model`.
    pub upstream: String,
    /// When set, attach a route to this existing `models.model_id`
    /// instead of creating a new catalog entry.
    pub target_model_id: Option<String>,
    /// Used only when `target_model_id` is None — overrides the
    /// exposed model_id of the new catalog entry. Lets the admin
    /// expose a tidy alias (e.g. "deepseek-v4") for a verbose upstream
    /// name. When None, the exposed id defaults to `upstream`.
    pub new_model_id: Option<String>,
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
    auth_user
        .require_global_permission(&state.db, "models:write")
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
    //
    // For "new" items we carry both the exposed id (what clients call)
    // and the upstream id (what the provider expects). They're equal
    // when the admin didn't customize.
    let mut new_exposed: Vec<String> = Vec::new();
    let mut new_upstreams: Vec<String> = Vec::new();
    let mut attach_targets: Vec<String> = Vec::new();
    let mut attach_upstreams: Vec<String> = Vec::new();

    for it in &req.items {
        match &it.target_model_id {
            None => {
                let exposed = it
                    .new_model_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&it.upstream)
                    .to_string();
                new_exposed.push(exposed);
                new_upstreams.push(it.upstream.clone());
            }
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
    let new_inserted: i64 = if new_exposed.is_empty() {
        0
    } else {
        sqlx::query(
            r#"INSERT INTO models (model_id, display_name)
               SELECT exposed, exposed
               FROM UNNEST($1::TEXT[]) AS t(exposed)
               ON CONFLICT (model_id) DO NOTHING"#,
        )
        .bind(&new_exposed)
        .execute(&mut *tx)
        .await?;

        sqlx::query_scalar::<_, i64>(
            r#"WITH ins AS (
                 INSERT INTO model_routes
                     (model_id, provider_id, upstream_model, weight)
                 SELECT exposed, $3, upstream, 100
                 FROM UNNEST($1::TEXT[], $2::TEXT[]) AS t(exposed, upstream)
                 ON CONFLICT (model_id, provider_id, upstream_model) DO NOTHING
                 RETURNING 1
               )
               SELECT COUNT(*) FROM ins"#,
        )
        .bind(&new_exposed)
        .bind(&new_upstreams)
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
                "new": new_exposed.len(),
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
    auth_user
        .require_global_permission(&state.db, "models:write")
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
    auth_user
        .require_global_permission(&state.db, "models:write")
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
    auth_user
        .require_global_permission(&state.db, "models:read")
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

    // gateway_logs has no route_id column — routes live in Postgres
    // and the log table records the resolved (model, provider name,
    // upstream_model) tuple instead. Look those up here so the CH
    // query can filter on what it actually has.
    let route = sqlx::query_as::<_, (String, String, String)>(
        "SELECT mr.model_id, p.name, mr.upstream_model \
         FROM model_routes mr \
         JOIN providers p ON p.id = mr.provider_id AND p.deleted_at IS NULL \
         WHERE mr.id = $1",
    )
    .bind(q.route_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("route lookup: {e}")))?;
    let Some((model_id, provider_name, upstream_model)) = route else {
        // Route was deleted between page load and refresh — return
        // an empty history so the sparkline stays blank rather than
        // 404ing the row out of the table.
        return Ok(Json(RouteHistoryResponse {
            buckets: Vec::new(),
        }));
    };

    // Per-minute latency rollup against gateway_logs' actual schema:
    // `created_at` (not `ts`), filtered by the resolved
    // (model_id, provider, upstream_model) tuple. `errors` counts
    // 4xx/5xx but NOT 429 — rate-limited requests are upstream
    // policy, not provider failures, so lumping them in here would
    // mirror the same misclassification A2 just fixed on the
    // provider-health widget. If CH is down or the table isn't
    // provisioned yet, we fall back to an empty response — the
    // sparkline is a hint, not load-bearing.
    let sql = format!(
        "SELECT toUnixTimestamp(toStartOfMinute(created_at)) AS bucket_ts, \
                quantile(0.50)(latency_ms)                 AS p50, \
                quantile(0.95)(latency_ms)                 AS p95, \
                count()                                     AS requests, \
                countIf(status_code >= 400 AND status_code != 429) AS errors \
         FROM gateway_logs \
         WHERE created_at >= toDateTime({from}) \
           AND model_id = ? \
           AND provider = ? \
           AND upstream_model = ? \
         GROUP BY bucket_ts \
         ORDER BY bucket_ts"
    );

    #[derive(Debug, clickhouse::Row, serde::Deserialize)]
    struct Row {
        bucket_ts: i64,
        p50: f64,
        p95: f64,
        requests: u64,
        errors: u64,
    }

    let rows: Vec<Row> = match ch
        .query(&sql)
        .bind(&model_id)
        .bind(&provider_name)
        .bind(&upstream_model)
        .fetch_all::<Row>()
        .await
    {
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
    auth_user
        .require_global_permission(&state.db, "models:write")
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

    let http_client = (**state.http_client.load()).clone();
    let Json(resp) = super::providers::run_provider_test(test_req, http_client).await?;
    if !resp.success {
        return Err(AppError::BadRequest(format!(
            "Provider unreachable: {}",
            resp.message
        )));
    }

    Ok(Json(resp.models.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_overrides_pass_when_all_none() {
        assert!(validate_routing_overrides(None, None, None).is_ok());
    }

    #[test]
    fn routing_strategy_accepts_each_canonical_value() {
        // Mirrors `crates/gateway/src/strategy.rs::RoutingStrategy` and
        // the DB CHECK constraint — adding a new strategy here without
        // updating either side would silently let the value through
        // until SQL trips.
        for ok in ["weighted", "latency", "health", "latency_health"] {
            assert!(
                validate_routing_overrides(Some(ok), None, None).is_ok(),
                "{ok} should be accepted"
            );
        }
    }

    #[test]
    fn routing_strategy_rejects_unknown_values() {
        for bad in ["round_robin", "", "WEIGHTED", "random"] {
            assert!(
                validate_routing_overrides(Some(bad), None, None).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn affinity_mode_accepts_each_canonical_value() {
        for ok in ["none", "provider", "route"] {
            assert!(
                validate_routing_overrides(None, Some(ok), None).is_ok(),
                "{ok} should be accepted"
            );
        }
    }

    #[test]
    fn affinity_mode_rejects_unknown_values() {
        for bad in ["sticky", "", "PROVIDER"] {
            assert!(
                validate_routing_overrides(None, Some(bad), None).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn affinity_ttl_accepts_zero_and_max() {
        // 0 means "off"; 86400 (24h) is the documented upper bound.
        assert!(validate_routing_overrides(None, None, Some(0)).is_ok());
        assert!(validate_routing_overrides(None, None, Some(86400)).is_ok());
    }

    #[test]
    fn affinity_ttl_rejects_negative() {
        assert!(validate_routing_overrides(None, None, Some(-1)).is_err());
    }

    #[test]
    fn affinity_ttl_rejects_above_86400() {
        // Cap is one day — protect operators from accidentally pinning
        // affinity for a week and not understanding why traffic stays
        // skewed.
        assert!(validate_routing_overrides(None, None, Some(86401)).is_err());
        assert!(validate_routing_overrides(None, None, Some(i32::MAX)).is_err());
    }

    #[test]
    fn routing_overrides_combine_independently() {
        // All three fields valid together → ok. A failure on any single
        // field returns immediately, but a happy-path combination must
        // still pass.
        assert!(validate_routing_overrides(Some("latency"), Some("route"), Some(300)).is_ok());
    }

    #[test]
    fn output_guardrails_empty_passes() {
        // No rules = no constraints — trivially valid.
        assert!(validate_output_guardrails(&[]).is_ok());
    }

    #[test]
    fn output_guardrails_accepts_canonical_value() {
        let rules = [OutputGuardrail::MaxLength { max_chars: 4096 }];
        assert!(validate_output_guardrails(&rules).is_ok());
    }

    #[test]
    fn output_guardrails_accepts_inclusive_endpoints() {
        // 1 is the smallest sensible cap (one-character responses are
        // pathological but not invalid); the ceiling is the documented
        // upper bound. Lock both edges so a future tightening doesn't
        // silently invalidate previously-stored configs.
        assert!(validate_output_guardrails(&[OutputGuardrail::MaxLength { max_chars: 1 }]).is_ok());
        assert!(
            validate_output_guardrails(&[OutputGuardrail::MaxLength {
                max_chars: MAX_LENGTH_CAP_CEILING,
            }])
            .is_ok()
        );
    }

    #[test]
    fn output_guardrails_rejects_zero_max_chars() {
        // 0 would reject every response — that's a config bug, not a
        // valid "guardrail off" toggle. Admins clear by removing the
        // rule entirely.
        let rules = [OutputGuardrail::MaxLength { max_chars: 0 }];
        assert!(validate_output_guardrails(&rules).is_err());
    }

    #[test]
    fn output_guardrails_rejects_above_ceiling() {
        let rules = [OutputGuardrail::MaxLength {
            max_chars: MAX_LENGTH_CAP_CEILING + 1,
        }];
        assert!(validate_output_guardrails(&rules).is_err());
    }

    #[test]
    fn output_guardrails_rejects_any_bad_rule_in_list() {
        // A list with one valid + one invalid rule must still fail —
        // partial-acceptance would let admins store a misconfiguration
        // and only notice at runtime.
        let rules = [
            OutputGuardrail::MaxLength { max_chars: 100 },
            OutputGuardrail::MaxLength { max_chars: 0 },
        ];
        assert!(validate_output_guardrails(&rules).is_err());
    }
}
