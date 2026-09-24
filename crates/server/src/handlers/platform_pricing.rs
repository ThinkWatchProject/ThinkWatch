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
use serde::Deserialize;

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;
use crate::services::pricing_repository::{self as repo, PlatformPricing};

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
    Ok(Json(repo::get(&state.db).await?))
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
    auth_user
        .require_global_permission(&state.db, "settings:write")
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

    let updated = repo::update(
        &state.db,
        req.input_price_per_token,
        req.output_price_per_token,
        req.currency.as_deref(),
    )
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

    // Drop the local CostTracker baseline cache so the next request
    // on THIS process reloads the new prices from PG. Other server
    // processes still pick the change up within the 60s TTL — but
    // without this call, the very process that just ACK'd the PATCH
    // would keep billing at the old baseline for up to a minute,
    // which is the worst place to see staleness (the admin who
    // changed it expects "immediate" semantics).
    state.cost_tracker.invalidate_baseline().await;

    Ok(Json(updated))
}
