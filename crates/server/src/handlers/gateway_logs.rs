use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use agent_bastion_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Deserialize)]
pub struct GatewayLogsQuery {
    pub model: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct GatewayLogEntry {
    pub id: uuid::Uuid,
    pub model_id: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cost_usd: rust_decimal::Decimal,
    pub latency_ms: Option<i32>,
    pub status_code: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn list_gateway_logs(
    _auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<GatewayLogsQuery>,
) -> Result<Json<Vec<GatewayLogEntry>>, AppError> {
    let limit = params.limit.unwrap_or(100).min(500);

    let rows = if let Some(ref model) = params.model {
        sqlx::query_as::<_, GatewayLogEntry>(
            r#"SELECT id, model_id, input_tokens, output_tokens, cost_usd, latency_ms, status_code, created_at
               FROM usage_records
               WHERE model_id ILIKE '%' || $1 || '%'
               ORDER BY created_at DESC
               LIMIT $2"#,
        )
        .bind(model)
        .bind(limit)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as::<_, GatewayLogEntry>(
            r#"SELECT id, model_id, input_tokens, output_tokens, cost_usd, latency_ms, status_code, created_at
               FROM usage_records
               ORDER BY created_at DESC
               LIMIT $1"#,
        )
        .bind(limit)
        .fetch_all(&state.db)
        .await?
    };

    Ok(Json(rows))
}
