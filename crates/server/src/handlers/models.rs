// ============================================================================
// Admin model CRUD
//
// Manages rows in the `models` table — the per-model price + multiplier
// catalog the gateway uses for cost reporting and weighted-token quota
// accounting. The router fall-back behavior (default prefix matching
// when the table is empty) is preserved: this handler only adds rows
// when an admin explicitly opts in.
//
// Why this exists: until now the `models` table had no UI surface,
// which meant `input_multiplier` / `output_multiplier` were stuck at
// 1.0 in practice and the README's "weighted token" quotas couldn't
// actually be tuned without raw SQL. This handler is the missing UI
// dependency.
//
// Permissions: `models:read` for GET, `models:write` for POST/PATCH/DELETE.
// ============================================================================

use axum::Json;
use axum::extract::{Path, State};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use think_watch_common::errors::AppError;
use think_watch_common::models::Model;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

/// Row shape returned by `GET /api/admin/models`. Joins in the
/// provider name so the UI can render "openai / gpt-4o" without
/// needing a second round-trip per row.
#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ModelRow {
    pub id: Uuid,
    pub provider_id: Uuid,
    pub provider_name: String,
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
              m.id, m.provider_id, p.name AS provider_name,
              m.model_id, m.display_name,
              m.input_price, m.output_price,
              m.input_multiplier, m.output_multiplier,
              m.is_active
           FROM models m
           JOIN providers p ON p.id = m.provider_id
           WHERE p.deleted_at IS NULL
           ORDER BY p.name, m.model_id"#,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateModelRequest {
    pub provider_id: Uuid,
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
    // Verify the provider exists and isn't soft-deleted before
    // foreign-keying to it.
    let provider_exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM providers WHERE id = $1 AND deleted_at IS NULL")
            .bind(req.provider_id)
            .fetch_optional(&state.db)
            .await?;
    if provider_exists.is_none() {
        return Err(AppError::BadRequest("provider not found".into()));
    }

    let model = sqlx::query_as::<_, Model>(
        r#"INSERT INTO models
              (provider_id, model_id, display_name,
               input_price, output_price,
               input_multiplier, output_multiplier, is_active)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           RETURNING id, provider_id, model_id, display_name,
                     input_price, output_price,
                     input_multiplier, output_multiplier, is_active"#,
    )
    .bind(req.provider_id)
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
        r#"SELECT id, provider_id, model_id, display_name,
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
           RETURNING id, provider_id, model_id, display_name,
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
