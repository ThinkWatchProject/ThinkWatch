//! Platform pricing repository — the single-row `platform_pricing` table
//! (PK fixed at 1): the baseline price per token the model weights
//! multiply.

use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use think_watch_common::errors::AppError;

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct PlatformPricing {
    #[schema(value_type = f64)]
    pub input_price_per_token: Decimal,
    #[schema(value_type = f64)]
    pub output_price_per_token: Decimal,
    pub currency: String,
}

pub async fn get(pool: &PgPool) -> Result<PlatformPricing, AppError> {
    Ok(sqlx::query_as::<_, PlatformPricing>(
        "SELECT input_price_per_token, output_price_per_token, currency \
         FROM platform_pricing WHERE id = 1",
    )
    .fetch_one(pool)
    .await?)
}

/// Set whichever of the three are given; the others keep their value.
pub async fn update(
    pool: &PgPool,
    input_price_per_token: Option<Decimal>,
    output_price_per_token: Option<Decimal>,
    currency: Option<&str>,
) -> Result<PlatformPricing, AppError> {
    Ok(sqlx::query_as::<_, PlatformPricing>(
        r#"UPDATE platform_pricing SET
              input_price_per_token  = COALESCE($1, input_price_per_token),
              output_price_per_token = COALESCE($2, output_price_per_token),
              currency               = COALESCE($3, currency),
              updated_at             = now()
           WHERE id = 1
           RETURNING input_price_per_token, output_price_per_token, currency"#,
    )
    .bind(input_price_per_token)
    .bind(output_price_per_token)
    .bind(currency)
    .fetch_one(pool)
    .await?)
}
