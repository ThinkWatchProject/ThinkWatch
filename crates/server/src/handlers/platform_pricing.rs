// ============================================================================
// Platform pricing (singleton)
//
// Holds the global baseline `(input_price_per_token, output_price_per_token)`
// that feeds the gateway cost tracker. Per-model `input_weight`/`output_weight`
// are multiplied against this baseline to compute `cost_usd` on each request.
//
// The table is a single-row singleton (PK fixed at 1). Admins with
// `settings:write` can PATCH to adjust. Any change invalidates the
// in-memory cache in the CostTracker so new requests pick it up
// immediately; other server processes see it within the 60s TTL.
// ============================================================================

use axum::Json;
use axum::extract::State;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct PlatformPricing {
    #[schema(value_type = f64)]
    pub input_price_per_token: Decimal,
    #[schema(value_type = f64)]
    pub output_price_per_token: Decimal,
    pub currency: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdatePlatformPricingRequest {
    #[schema(value_type = Option<f64>)]
    pub input_price_per_token: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub output_price_per_token: Option<Decimal>,
    pub currency: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/admin/platform-pricing",
    tag = "Platform Pricing",
    security(("bearer_token" = [])),
    responses(
        (status = 200, description = "Baseline pricing", body = PlatformPricing),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn get_platform_pricing(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<PlatformPricing>, AppError> {
    auth_user.require_permission("settings:read")?;
    let row = sqlx::query_as::<_, PlatformPricing>(
        "SELECT input_price_per_token, output_price_per_token, currency \
         FROM platform_pricing WHERE id = 1",
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

#[utoipa::path(
    patch,
    path = "/api/admin/platform-pricing",
    tag = "Platform Pricing",
    security(("bearer_token" = [])),
    request_body = UpdatePlatformPricingRequest,
    responses(
        (status = 200, description = "Updated baseline pricing", body = PlatformPricing),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn update_platform_pricing(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<UpdatePlatformPricingRequest>,
) -> Result<Json<PlatformPricing>, AppError> {
    auth_user.require_permission("settings:write")?;
    auth_user
        .assert_scope_global(&state.db, "settings:write")
        .await?;

    if let Some(v) = req.input_price_per_token
        && v < Decimal::ZERO
    {
        return Err(AppError::BadRequest(
            "input_price_per_token must be >= 0".into(),
        ));
    }
    if let Some(v) = req.output_price_per_token
        && v < Decimal::ZERO
    {
        return Err(AppError::BadRequest(
            "output_price_per_token must be >= 0".into(),
        ));
    }

    let updated = sqlx::query_as::<_, PlatformPricing>(
        r#"UPDATE platform_pricing SET
              input_price_per_token  = COALESCE($1, input_price_per_token),
              output_price_per_token = COALESCE($2, output_price_per_token),
              currency               = COALESCE($3, currency),
              updated_at             = now()
           WHERE id = 1
           RETURNING input_price_per_token, output_price_per_token, currency"#,
    )
    .bind(req.input_price_per_token)
    .bind(req.output_price_per_token)
    .bind(req.currency.as_ref())
    .fetch_one(&state.db)
    .await?;

    state.audit.log(
        auth_user
            .audit("platform_pricing.updated")
            .resource("platform_pricing")
            .detail(serde_json::json!({
                "input_price_per_token": updated.input_price_per_token.to_string(),
                "output_price_per_token": updated.output_price_per_token.to_string(),
                "currency": &updated.currency,
            })),
    );

    // The CostTracker has a 60s baseline TTL, so new prices take
    // effect on every process within a minute without restart. If we
    // need instant propagation later, share the CostTracker handle on
    // `AppState` and call `invalidate_baseline()` here.

    Ok(Json(updated))
}
