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
use axum::extract::{Path, State};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use think_watch_common::dto::ProviderHeader;
use think_watch_common::errors::AppError;
use think_watch_common::models::Model;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// Row shape returned by `GET /api/admin/models`.
#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ModelRow {
    pub id: Uuid,
    pub model_id: String,
    pub display_name: String,
    #[schema(value_type = Option<f64>)]
    pub input_price: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub output_price: Option<Decimal>,
    #[schema(value_type = f64)]
    pub input_multiplier: Decimal,
    #[schema(value_type = f64)]
    pub output_multiplier: Decimal,
    pub is_active: bool,
}

#[utoipa::path(
    get,
    path = "/api/admin/models",
    tag = "Models",
    security(("bearer_token" = [])),
    responses(
        (status = 200, description = "List of models", body = Vec<ModelRow>),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn list_models(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<ModelRow>>, AppError> {
    auth_user.require_permission("models:read")?;
    auth_user
        .assert_scope_global(&state.db, "models:read")
        .await?;
    let rows = sqlx::query_as::<_, ModelRow>(
        r#"SELECT
              m.id, m.model_id, m.display_name,
              m.input_price, m.output_price,
              m.input_multiplier, m.output_multiplier,
              m.is_active
           FROM models m
           ORDER BY m.model_id"#,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateModelRequest {
    pub model_id: String,
    pub display_name: String,
    #[schema(value_type = Option<f64>)]
    pub input_price: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub output_price: Option<Decimal>,
    /// Defaults to 1.0 if omitted.
    #[schema(value_type = Option<f64>)]
    pub input_multiplier: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub output_multiplier: Option<Decimal>,
    pub is_active: Option<bool>,
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
    let in_mult = req.input_multiplier.unwrap_or(Decimal::ONE);
    let out_mult = req.output_multiplier.unwrap_or(Decimal::ONE);
    if in_mult <= Decimal::ZERO || out_mult <= Decimal::ZERO {
        return Err(AppError::BadRequest(
            "multipliers must be greater than zero".into(),
        ));
    }

    let model = sqlx::query_as::<_, Model>(
        r#"INSERT INTO models
              (model_id, display_name,
               input_price, output_price,
               input_multiplier, output_multiplier, is_active)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           RETURNING id, model_id, display_name,
                     input_price, output_price,
                     input_multiplier, output_multiplier, is_active"#,
    )
    .bind(&req.model_id)
    .bind(&req.display_name)
    .bind(req.input_price)
    .bind(req.output_price)
    .bind(in_mult)
    .bind(out_mult)
    .bind(req.is_active.unwrap_or(true))
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
    pub input_price: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub output_price: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub input_multiplier: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub output_multiplier: Option<Decimal>,
    pub is_active: Option<bool>,
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
        r#"SELECT id, model_id, display_name,
                  input_price, output_price,
                  input_multiplier, output_multiplier, is_active
           FROM models WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Model not found".into()))?;

    let new_in_mult = req.input_multiplier.unwrap_or(existing.input_multiplier);
    let new_out_mult = req.output_multiplier.unwrap_or(existing.output_multiplier);
    if new_in_mult <= Decimal::ZERO || new_out_mult <= Decimal::ZERO {
        return Err(AppError::BadRequest(
            "multipliers must be greater than zero".into(),
        ));
    }

    let updated = sqlx::query_as::<_, Model>(
        r#"UPDATE models SET
              display_name = $2,
              input_price = $3,
              output_price = $4,
              input_multiplier = $5,
              output_multiplier = $6,
              is_active = $7
           WHERE id = $1
           RETURNING id, model_id, display_name,
                     input_price, output_price,
                     input_multiplier, output_multiplier, is_active"#,
    )
    .bind(id)
    .bind(
        req.display_name
            .as_deref()
            .unwrap_or(&existing.display_name),
    )
    .bind(req.input_price.or(existing.input_price))
    .bind(req.output_price.or(existing.output_price))
    .bind(new_in_mult)
    .bind(new_out_mult)
    .bind(req.is_active.unwrap_or(existing.is_active))
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
           WHERE mr.model_id = $1
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

    Ok(Json(serde_json::json!({"status": "deleted"})))
}
