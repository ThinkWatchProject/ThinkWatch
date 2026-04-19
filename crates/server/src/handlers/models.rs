// ============================================================================
// Admin model CRUD
//
// Manages rows in the `models` table — the per-model price + multiplier
// catalog the gateway uses for cost reporting and weighted-token quota
// accounting. Models are now standalone (no provider_id FK); routing to
// providers is handled by the `model_routes` table.
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
    //   'draft'     — route_count > 0 AND enabled_route_count = 0
    //   'unrouted'  — route_count = 0
    //   otherwise   — no filter
    //
    // We compute `route_count` / `enabled_route_count` via `LATERAL`
    // subquery so the filter happens on the joined shape; PG rewrites
    // this to a HashAggregate over `model_routes`.
    let status_filter_sql = match status {
        "active" => "AND rc.enabled_route_count > 0",
        "draft" => "AND rc.route_count > 0 AND rc.enabled_route_count = 0",
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
                  COALESCE(rc.enabled_route_count, 0) AS enabled_route_count
           FROM models m
           LEFT JOIN LATERAL (
             SELECT COUNT(*)                                 AS route_count,
                    COUNT(*) FILTER (WHERE mr.enabled = true) AS enabled_route_count
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

    let model = sqlx::query_as::<_, Model>(
        r#"INSERT INTO models (model_id, display_name, input_weight, output_weight)
           VALUES ($1, $2, $3, $4)
           RETURNING id, model_id, display_name, input_weight, output_weight"#,
    )
    .bind(&req.model_id)
    .bind(&req.display_name)
    .bind(in_w)
    .bind(out_w)
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
        r#"SELECT id, model_id, display_name, input_weight, output_weight
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

    let updated = sqlx::query_as::<_, Model>(
        r#"UPDATE models SET
              display_name   = $2,
              input_weight   = $3,
              output_weight  = $4
           WHERE id = $1
           RETURNING id, model_id, display_name, input_weight, output_weight"#,
    )
    .bind(id)
    .bind(
        req.display_name
            .as_deref()
            .unwrap_or(&existing.display_name),
    )
    .bind(new_in_w)
    .bind(new_out_w)
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("model.updated")
            .resource("model")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "model_id": existing.model_id })),
    );

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
// Sync models from upstream provider
// ---------------------------------------------------------------------------

pub async fn sync_models(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(provider_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    auth_user.require_permission("models:write")?;
    auth_user
        .assert_scope_global(&state.db, "models:write")
        .await?;

    // Load the provider
    let provider = sqlx::query_as::<_, think_watch_common::models::Provider>(
        "SELECT * FROM providers WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(provider_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("Provider not found".into()))?;

    // Build test request from provider data
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

    // Run the connectivity test
    let Json(resp) = super::providers::run_provider_test(test_req).await?;
    if !resp.success {
        return Err(AppError::BadRequest(format!(
            "Provider test failed: {}",
            resp.message
        )));
    }

    let remote_models = resp.models.unwrap_or_default();
    if remote_models.is_empty() {
        return Ok(Json(serde_json::json!({"synced": 0, "deactivated": 0})));
    }

    // Bulk-insert models + routes using UNNEST arrays. Prior loop issued
    // 2 round-trips per model (hundreds for large provider catalogs like
    // OpenRouter). Now it's 2 total round-trips regardless of catalog size.
    let model_slice: &[String] = &remote_models;
    let model_result = sqlx::query(
        r#"INSERT INTO models (model_id, display_name)
           SELECT m, m FROM UNNEST($1::TEXT[]) AS t(m)
           ON CONFLICT (model_id) DO NOTHING"#,
    )
    .bind(model_slice)
    .execute(&state.db)
    .await?;
    let synced: i64 = model_result.rows_affected() as i64;

    sqlx::query(
        r#"INSERT INTO model_routes (model_id, provider_id, weight, priority)
           SELECT m, $2, 100, 0 FROM UNNEST($1::TEXT[]) AS t(m)
           ON CONFLICT (model_id, provider_id) DO NOTHING"#,
    )
    .bind(model_slice)
    .bind(provider_id)
    .execute(&state.db)
    .await?;

    // Deactivate routes for models not in the remote list
    let deactivated = sqlx::query(
        r#"UPDATE model_routes SET enabled = false
           WHERE provider_id = $1 AND model_id != ALL($2) AND enabled = true"#,
    )
    .bind(provider_id)
    .bind(&remote_models)
    .execute(&state.db)
    .await?;

    // Re-enable routes that are in the remote list but were previously disabled
    sqlx::query(
        r#"UPDATE model_routes SET enabled = true
           WHERE provider_id = $1 AND model_id = ANY($2) AND enabled = false"#,
    )
    .bind(provider_id)
    .bind(&remote_models)
    .execute(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("models.synced")
            .resource("provider")
            .resource_id(provider_id.to_string())
            .detail(serde_json::json!({
                "synced": synced,
                "deactivated": deactivated.rows_affected(),
            })),
    );

    crate::app::rebuild_gateway_router(&state).await;

    Ok(Json(serde_json::json!({
        "synced": synced,
        "deactivated": deactivated.rows_affected(),
    })))
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
    pub priority: i32,
    pub enabled: bool,
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
                  mr.upstream_model, mr.weight, mr.priority, mr.enabled
           FROM model_routes mr
           JOIN providers p ON p.id = mr.provider_id
           WHERE mr.model_id = $1 AND p.deleted_at IS NULL
           ORDER BY mr.priority ASC, mr.weight DESC"#,
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
    pub priority: Option<i32>,
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
    let priority = req.priority.unwrap_or(0);

    // Check for existing route to give a friendly error instead of 500
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM model_routes WHERE model_id = $1 AND provider_id = $2")
            .bind(&model_id)
            .bind(req.provider_id)
            .fetch_optional(&state.db)
            .await?;
    if existing.is_some() {
        return Err(AppError::BadRequest(
            "A route for this model+provider already exists".into(),
        ));
    }

    let row = sqlx::query_as::<_, ModelRouteRow>(
        r#"INSERT INTO model_routes (model_id, provider_id, upstream_model, weight, priority)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id, model_id, provider_id,
                     (SELECT name FROM providers WHERE id = provider_id) AS provider_name,
                     upstream_model, weight, priority, enabled"#,
    )
    .bind(&model_id)
    .bind(req.provider_id)
    .bind(&req.upstream_model)
    .bind(weight)
    .bind(priority)
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
    pub upstream_model: Option<String>,
    pub weight: Option<i32>,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
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

    let row = sqlx::query_as::<_, ModelRouteRow>(
        r#"UPDATE model_routes SET
              upstream_model = COALESCE($2, upstream_model),
              weight = COALESCE($3, weight),
              priority = COALESCE($4, priority),
              enabled = COALESCE($5, enabled)
           WHERE id = $1
           RETURNING id, model_id, provider_id,
                     (SELECT name FROM providers WHERE id = provider_id) AS provider_name,
                     upstream_model, weight, priority, enabled"#,
    )
    .bind(route_id)
    .bind(&req.upstream_model)
    .bind(req.weight)
    .bind(req.priority)
    .bind(req.enabled)
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
                      mr.upstream_model, mr.weight, mr.priority, mr.enabled
               FROM model_routes mr
               JOIN providers p ON p.id = mr.provider_id
               WHERE p.deleted_at IS NULL
               ORDER BY mr.model_id, mr.priority, mr.weight DESC
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
                      mr.upstream_model, mr.weight, mr.priority, mr.enabled
               FROM model_routes mr
               JOIN providers p ON p.id = mr.provider_id
               WHERE p.deleted_at IS NULL
                 AND ($1 = '' OR mr.model_id ILIKE $2 OR p.name ILIKE $2)
                 AND ($3::UUID IS NULL OR mr.provider_id = $3)
               ORDER BY mr.model_id, mr.priority, mr.weight DESC
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
// All imported routes default to `enabled = false` so `/v1/models`
// stays clean until the admin flips them on.
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
    /// Route priority. Defaults to 0 for `new`, 1 for `attach`
    /// (sensible guess: a brand-new exposed model is primary, while
    /// attaching to an already-served model is usually fallback).
    pub priority: Option<i32>,
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
    let mut attach_priorities: Vec<i32> = Vec::new();

    for it in &req.items {
        match &it.target_model_id {
            None => new_ids.push(it.upstream.clone()),
            Some(target) => {
                attach_targets.push(target.clone());
                attach_upstreams.push(it.upstream.clone());
                attach_priorities.push(it.priority.unwrap_or(1));
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
                 INSERT INTO model_routes
                     (model_id, provider_id, weight, priority, enabled)
                 SELECT m, $2, 100, 0, false FROM UNNEST($1::TEXT[]) AS t(m)
                 ON CONFLICT (model_id, provider_id) DO NOTHING
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
                     (model_id, provider_id, upstream_model, weight, priority, enabled)
                 SELECT t.target, $4, t.upstream, 100, t.pri, false
                 FROM UNNEST($1::TEXT[], $2::TEXT[], $3::INT[])
                   AS t(target, upstream, pri)
                 WHERE EXISTS (SELECT 1 FROM models m WHERE m.model_id = t.target)
                 ON CONFLICT (model_id, provider_id) DO NOTHING
                 RETURNING 1
               )
               SELECT COUNT(*) FROM ins"#,
        )
        .bind(&attach_targets)
        .bind(&attach_upstreams)
        .bind(&attach_priorities)
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
