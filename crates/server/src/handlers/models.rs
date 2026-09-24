// ============================================================================
// Admin model CRUD
//
// Manages rows in the `models` table — the exposed catalog clients see
// via `/v1/models`. Each row carries `input_weight` / `output_weight`
// (relative factors against `platform_pricing` for cost reporting +
// weighted-token quota accounting), and optional cache-read / cache-write
// weights that default from the input weight. Routing to providers is handled by
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

use think_watch_common::errors::AppError;
use think_watch_common::models::Model;
use think_watch_gateway::output_guardrails::{MAX_LENGTH_CAP_CEILING, OutputGuardrail};

use super::serde_util::deserialize_some;
use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;
use crate::services::model_repository::{
    self as repo, ModelFields, ModelIdRow, ModelRouteRow, ModelRow, NewRoute, RouteImport,
    RouteUpdate,
};
use crate::services::provider_repository;

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
    let status = query.status.as_deref().unwrap_or("");
    let (total, items) = repo::list(&state.db, search, status, page_size, offset).await?;
    Ok(Json(ModelListResponse { items, total }))
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
    /// Cache-read input weight. Unset ⇒ `input_weight × 0.1`.
    #[serde(default)]
    #[schema(value_type = Option<f64>)]
    pub cache_read_weight: Option<Decimal>,
    /// Cache-write input weight (5-minute). Unset ⇒ `input_weight × 1.25`.
    #[serde(default)]
    #[schema(value_type = Option<f64>)]
    pub cache_write_weight: Option<Decimal>,
    /// Cache-write input weight (1-hour). Unset ⇒ `input_weight × 2`.
    #[serde(default)]
    #[schema(value_type = Option<f64>)]
    pub cache_write_1h_weight: Option<Decimal>,
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
    let cache = [
        req.cache_read_weight,
        req.cache_write_weight,
        req.cache_write_1h_weight,
    ];
    validate_cache_weights(&cache)?;
    validate_routing_overrides(
        req.routing_strategy.as_deref(),
        req.affinity_mode.as_deref(),
        req.affinity_ttl_secs,
    )?;
    let guardrails = req.output_guardrails.unwrap_or_default();
    validate_output_guardrails(&guardrails)?;
    let guardrails_json = serde_json::to_value(&guardrails)
        .map_err(|e| AppError::BadRequest(format!("failed to serialize output_guardrails: {e}")))?;

    let model = repo::insert(
        &state.db,
        &req.model_id,
        &ModelFields {
            display_name: &req.display_name,
            input_weight: in_w,
            output_weight: out_w,
            routing_strategy: req.routing_strategy.as_deref(),
            affinity_mode: req.affinity_mode.as_deref(),
            affinity_ttl_secs: req.affinity_ttl_secs,
            tags: req.tags.as_deref(),
            output_guardrails: &guardrails_json,
            cache_weights: cache,
        },
    )
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
    /// PATCH-clearable cache weights: absent = unchanged, JSON `null` =
    /// clear (derive from `input_weight` again), number = set.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<f64>)]
    pub cache_read_weight: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<f64>)]
    pub cache_write_weight: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<f64>)]
    pub cache_write_1h_weight: Option<Option<Decimal>>,
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

/// Cache weights may be zero (an upstream that does not bill cache
/// reads) but not negative — the column's CHECK, as a useful 400.
fn validate_cache_weights(weights: &[Option<Decimal>]) -> Result<(), AppError> {
    if weights.iter().flatten().any(|w| *w < Decimal::ZERO) {
        return Err(AppError::BadRequest(
            "cache weights must not be negative".into(),
        ));
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
    let existing = repo::find(&state.db, id)
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
    let cache = [
        req.cache_read_weight.unwrap_or(existing.cache_read_weight),
        req.cache_write_weight
            .unwrap_or(existing.cache_write_weight),
        req.cache_write_1h_weight
            .unwrap_or(existing.cache_write_1h_weight),
    ];
    validate_cache_weights(&cache)?;
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

    let updated = repo::update(
        &state.db,
        id,
        &ModelFields {
            display_name: req
                .display_name
                .as_deref()
                .unwrap_or(&existing.display_name),
            input_weight: new_in_w,
            output_weight: new_out_w,
            routing_strategy: new_strategy.as_deref(),
            affinity_mode: new_affinity_mode.as_deref(),
            affinity_ttl_secs: new_affinity_ttl,
            tags: new_tags.as_deref(),
            output_guardrails: &new_guardrails_json,
            cache_weights: cache,
        },
        req.enabled.unwrap_or(existing.enabled),
    )
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
    let model_id = repo::model_id_of(&state.db, id).await?;
    repo::delete(&state.db, id).await?;
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

    Ok(Json(repo::list_ids(&state.db).await?))
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

    let deleted = repo::delete_unrouted(&state.db).await? as i64;

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

    let deleted = repo::delete_many(&state.db, &req.ids).await? as i64;

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

    let updated = repo::set_enabled_many(&state.db, &req.ids, req.enabled).await? as i64;

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

/// GET /api/admin/models/{model_id}/routes
pub async fn list_model_routes(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<Json<Vec<ModelRouteRow>>, AppError> {
    auth_user
        .require_global_permission(&state.db, "models:read")
        .await?;

    Ok(Json(repo::routes_of(&state.db, &model_id).await?))
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

    if !repo::exists(&state.db, &model_id).await? {
        return Err(AppError::NotFound("Model not found".into()));
    }
    let provider = provider_repository::find_live(&state.db, req.provider_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("Provider not found".into()))?;

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
    if repo::route_exists(&state.db, &model_id, req.provider_id, &upstream_model).await? {
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

    // Same check the bulk import runs: don't create a route the
    // upstream has told us it won't serve. Refusing here beats creating
    // it and letting the operator find out on their first call.
    let verdict = crate::protocol_probe::resolve(
        &state.db,
        &provider,
        &state.config.encryption_key,
        std::slice::from_ref(&upstream_model),
        false,
    )
    .await
    .remove(&upstream_model)
    .unwrap_or(crate::protocol_probe::Verdict::Unknown);
    if let crate::protocol_probe::Verdict::Unavailable(reason) = &verdict {
        return Err(AppError::BadRequest(format!(
            "Provider does not serve '{upstream_model}': {reason}"
        )));
    }
    let upstream_protocol = verdict.protocol().map(|p| p.as_str().to_string());

    let row = repo::insert_route(
        &state.db,
        &NewRoute {
            model_id: &model_id,
            provider_id: req.provider_id,
            upstream_model: &upstream_model,
            weight,
            enabled: req.enabled.unwrap_or(true),
            label: req.label.as_deref().filter(|s| !s.is_empty()),
            notes: req.notes.as_deref().filter(|s| !s.is_empty()),
            rpm_cap: req.rpm_cap,
            tpm_cap: req.tpm_cap,
            upstream_protocol: upstream_protocol.as_deref(),
        },
    )
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
    if let Some(Some(c)) = req.rpm_cap
        && c <= 0
    {
        return Err(AppError::BadRequest("rpm_cap must be > 0".into()));
    }
    if let Some(Some(c)) = req.tpm_cap
        && c <= 0
    {
        return Err(AppError::BadRequest("tpm_cap must be > 0".into()));
    }

    fn non_empty(v: &Option<String>) -> Option<&str> {
        v.as_deref().filter(|s| !s.is_empty())
    }
    let row = repo::update_route(
        &state.db,
        route_id,
        &RouteUpdate {
            upstream_model: upstream_value,
            weight: req.weight,
            enabled: req.enabled,
            label: req.label.as_ref().map(non_empty),
            notes: req.notes.as_ref().map(non_empty),
            rpm_cap: req.rpm_cap,
            tpm_cap: req.tpm_cap,
        },
    )
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

    if !repo::delete_route(&state.db, route_id).await? {
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
    let (total, rows) =
        repo::list_routes(&state.db, search, q.provider_id, page_size, offset).await?;

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

    let provider = provider_repository::find_live(&state.db, req.provider_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("Provider not found".into()))?;

    // Split the request into the two flows. Each flow is one bulk
    // INSERT via UNNEST so we stay at O(1) round trips regardless of N.
    //
    // For "new" items we carry both the exposed id (what clients call)
    // and the upstream id (what the provider expects). They're equal
    // when the admin didn't customize.
    // Ask the upstream whether it will serve each selected model before
    // creating anything. Refused models are dropped from the import
    // rather than turned into routes that fail on first use; cached
    // verdicts make a repeat import free. Models the probe couldn't
    // conclude on (timeout, transport) are imported anyway — that's the
    // pre-probe behaviour, and the runtime relearn path covers them.
    let selected: Vec<String> = {
        let mut v: Vec<String> = req.items.iter().map(|it| it.upstream.clone()).collect();
        v.sort();
        v.dedup();
        v
    };
    let verdicts = crate::protocol_probe::resolve(
        &state.db,
        &provider,
        &state.config.encryption_key,
        &selected,
        false,
    )
    .await;
    let skipped: Vec<serde_json::Value> = verdicts
        .iter()
        .filter_map(|(model, verdict)| match verdict {
            crate::protocol_probe::Verdict::Unavailable(reason) => Some(serde_json::json!({
                "upstream": model,
                "reason": reason,
            })),
            _ => None,
        })
        .collect();

    let mut import = RouteImport::default();

    for it in &req.items {
        let verdict = verdicts.get(&it.upstream);
        if matches!(
            verdict,
            Some(crate::protocol_probe::Verdict::Unavailable(_))
        ) {
            continue;
        }
        let protocol = verdict
            .and_then(|v| v.protocol())
            .map(|p| p.as_str().to_string());
        match &it.target_model_id {
            None => {
                let exposed = it
                    .new_model_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&it.upstream)
                    .to_string();
                import.new_exposed.push(exposed);
                import.new_upstreams.push(it.upstream.clone());
                import.new_protocols.push(protocol);
            }
            Some(target) => {
                import.attach_targets.push(target.clone());
                import.attach_upstreams.push(it.upstream.clone());
                import.attach_protocols.push(protocol);
            }
        }
    }

    // Imported routes that already exist, or attach to a catalog entry
    // that does not, are skipped; the audit row records the discrepancy
    // via the created/requested deltas.
    let created = repo::import_routes(&state.db, req.provider_id, &import).await?;

    state.audit.log(
        auth_user
            .audit("model_routes.batch_created")
            .resource("model_routes")
            .detail(serde_json::json!({
                "provider_id": req.provider_id,
                "new": import.new_exposed.len(),
                "attach": import.attach_targets.len(),
                "created": created,
            })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    // `skipped` is the whole point of probing first: the admin selected
    // these and they are not being imported, so say which and why.
    Ok(Json(serde_json::json!({
        "created": created,
        "skipped": skipped,
    })))
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

    let deleted = repo::delete_routes(&state.db, &req.ids).await? as i64;

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
    let weights: Vec<(Uuid, i32)> = req.updates.iter().map(|u| (u.id, u.weight)).collect();
    let updated = repo::set_route_weights(&state.db, &weights).await? as i64;

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
    let route = repo::route_log_identity(&state.db, q.route_id)
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

    let updated = repo::set_routes_enabled(&state.db, &req.ids, req.enabled).await? as i64;

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
) -> Result<Json<Vec<Value>>, AppError> {
    auth_user.require_permission("models:read")?;

    let provider = provider_repository::find_live(&state.db, provider_id)
        .await?
        .ok_or(AppError::NotFound("Provider not found".into()))?;

    // Stored header values are `{"$enc": …}` envelopes, so they have to
    // be decrypted here — deserializing them straight into
    // `Vec<ProviderHeader>` fails and yields an empty list, which sent
    // the model probe upstream with no API key at all.
    let headers = super::providers::decrypt_headers_from_config(
        &provider.config_json,
        &state.config.encryption_key,
        &provider.name,
    );
    let test_req = super::providers::TestProviderRequest {
        provider_type: provider.provider_type.clone(),
        base_url: provider.base_url.clone(),
        headers,
        provider_id: Some(provider.id),
    };

    let http_client = (**state.http_client.load()).clone();
    let Json(resp) =
        super::providers::run_provider_test(test_req, http_client, &state.url_validator).await?;
    if !resp.success {
        return Err(AppError::BadRequest(format!(
            "Provider unreachable: {}",
            resp.message
        )));
    }

    let models = resp.models.unwrap_or_default();

    // Attach whatever we already know about each model so the import
    // picker can mark the ones this upstream refuses without spending a
    // request. Models with no cached verdict come back unmarked — the
    // probe runs when the admin actually imports them.
    let verdicts = crate::protocol_probe::cached_verdicts(&state.db, provider.id)
        .await
        .unwrap_or_default();
    let entries: Vec<serde_json::Value> = models
        .into_iter()
        .map(|model| match verdicts.get(&model) {
            Some(crate::protocol_probe::Verdict::Unavailable(reason)) => serde_json::json!({
                "id": model, "available": false, "reason": reason,
            }),
            Some(crate::protocol_probe::Verdict::Ok(_)) => serde_json::json!({
                "id": model, "available": true,
            }),
            _ => serde_json::json!({ "id": model }),
        })
        .collect();

    Ok(Json(entries))
}

/// POST /api/admin/providers/{provider_id}/recheck-models
///
/// Re-probe the provider's entire catalog, overwriting every cached
/// verdict. This is the only way a model that was refused becomes
/// importable again: verdicts never expire and nothing polls the
/// upstream, so an operator who enables a model there tells us by
/// pressing this.
pub async fn recheck_provider_models(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(provider_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    auth_user
        .require_global_permission(&state.db, "models:write")
        .await?;

    let provider = provider_repository::find_live(&state.db, provider_id)
        .await?
        .ok_or(AppError::NotFound("Provider not found".into()))?;

    let headers = super::providers::decrypt_headers_from_config(
        &provider.config_json,
        &state.config.encryption_key,
        &provider.name,
    );
    let http_client = (**state.http_client.load()).clone();
    let Json(resp) = super::providers::run_provider_test(
        super::providers::TestProviderRequest {
            provider_type: provider.provider_type.clone(),
            base_url: provider.base_url.clone(),
            headers,
            provider_id: Some(provider.id),
        },
        http_client,
        &state.url_validator,
    )
    .await?;
    if !resp.success {
        return Err(AppError::BadRequest(format!(
            "Provider unreachable: {}",
            resp.message
        )));
    }
    let models = resp.models.unwrap_or_default();

    let verdicts = crate::protocol_probe::resolve(
        &state.db,
        &provider,
        &state.config.encryption_key,
        &models,
        true,
    )
    .await;

    let mut available = 0usize;
    let mut unavailable: Vec<serde_json::Value> = Vec::new();
    let mut inconclusive = 0usize;
    for (model, verdict) in &verdicts {
        match verdict {
            crate::protocol_probe::Verdict::Ok(_) => available += 1,
            crate::protocol_probe::Verdict::Unavailable(reason) => {
                unavailable.push(serde_json::json!({ "upstream": model, "reason": reason }))
            }
            crate::protocol_probe::Verdict::Unknown => inconclusive += 1,
        }
    }

    // A model that just became unavailable is still routed until an
    // operator removes it — we report it rather than deleting routes
    // behind their back.
    state.audit.log(
        auth_user
            .audit("provider.models_rechecked")
            .resource("provider")
            .resource_id(provider_id.to_string())
            .detail(serde_json::json!({
                "checked": verdicts.len(),
                "available": available,
                "unavailable": unavailable.len(),
            })),
    );

    Ok(Json(serde_json::json!({
        "checked": verdicts.len(),
        "available": available,
        "inconclusive": inconclusive,
        "unavailable": unavailable,
    })))
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
