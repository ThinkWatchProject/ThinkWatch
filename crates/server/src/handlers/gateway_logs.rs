use axum::Json;
use axum::extract::{Query, State};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use think_watch_common::cost_decimal::decode_i64;
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::*;

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct GatewayLogsQuery {
    /// Free-text search — substring match against model_id.
    pub q: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub user_id: Option<String>,
    /// `?api_key_id=X` is rotation-transparent: the backend resolves
    /// X to its lineage and filters on `api_key_lineage_id`, so the
    /// caller gets the full history of the logical key without
    /// having to know about rotation generations. The lineage column
    /// is an internal CH detail; it never appears on the public
    /// query / response surface.
    pub api_key_id: Option<String>,
    pub status_code: Option<i64>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub exclude: Option<String>,
    pub sort_by: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Wire shape returned to the frontend — `cost_usd` is the friendly
/// Decimal-as-string form (via `rust_decimal::serde::str_option`).
/// Separate from the CH read struct below because `clickhouse::Row`
/// needs the raw i64 decimal encoding, not a serialized string.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GatewayLogEntry {
    pub id: String,
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub model_id: Option<String>,
    pub provider: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    #[schema(value_type = Option<String>)]
    #[serde(with = "rust_decimal::serde::str_option")]
    pub cost_usd: Option<Decimal>,
    pub latency_ms: Option<i64>,
    pub status_code: Option<i64>,
    pub ip_address: Option<String>,
    pub created_at: String,
}

/// CH row shape — `cost_usd` is the raw i64 under a
/// `Nullable(Decimal(18, 10))` column. Mapped to `GatewayLogEntry`
/// via `decode_i64` before the response is serialized.
#[derive(Debug, Deserialize, clickhouse::Row)]
struct GatewayLogRow {
    id: String,
    user_id: Option<String>,
    api_key_id: Option<String>,
    model_id: Option<String>,
    provider: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cost_usd: Option<i64>,
    latency_ms: Option<i64>,
    status_code: Option<i64>,
    ip_address: Option<String>,
    created_at: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GatewayLogsResponse {
    pub items: Vec<GatewayLogEntry>,
    pub total: u64,
}

#[utoipa::path(
    get,
    path = "/api/gateway/logs",
    tag = "Gateway Logs",
    params(GatewayLogsQuery),
    responses(
        (status = 200, description = "Paginated AI gateway request logs", body = GatewayLogsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_gateway_logs(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<GatewayLogsQuery>,
) -> Result<Json<GatewayLogsResponse>, AppError> {
    auth_user.require_permission("logs:read_all")?;
    if !ch_available(&state) {
        return Ok(Json(GatewayLogsResponse {
            total: 0,
            items: vec![],
        }));
    }
    let ch = ch_client(&state)?;
    let (limit, offset) = clamp_pagination(params.limit, params.offset, 200);

    // Build dynamic WHERE conditions
    let mut conditions: Vec<String> = Vec::new();
    let mut bind_values: Vec<String> = Vec::new();

    // ClickHouse SDK uses ? for bind params in order
    if let Some(ref v) = params.model {
        conditions.push("model_id = ?".to_string());
        bind_values.push(v.clone());
    }
    if let Some(ref v) = params.provider {
        conditions.push("provider = ?".to_string());
        bind_values.push(v.clone());
    }
    if let Some(ref v) = params.user_id {
        conditions.push("user_id = ?".to_string());
        bind_values.push(v.clone());
    }
    // Rotation-transparent api-key filter: callers pass an exact
    // api_key.id, but we resolve it to its lineage and filter on
    // `api_key_lineage_id` so the result includes every generation
    // of the rotation chain. Keeps the public API shape stable
    // while delivering "this logical key's full history" for free.
    //
    // Edge cases:
    //   - api_key_id parses but the row is gone (hard-deleted): we
    //     fall back to filtering on the raw api_key_id column so
    //     historical rows still surface.
    //   - api_key_id isn't a UUID: skip the lookup and filter raw,
    //     letting the empty result speak for itself.
    if let Some(ref raw) = params.api_key_id {
        let lineage_id = match raw.parse::<uuid::Uuid>() {
            Ok(id) => {
                sqlx::query_scalar::<_, uuid::Uuid>("SELECT lineage_id FROM api_keys WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&state.db)
                    .await
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("lineage lookup: {e}")))?
            }
            Err(_) => None,
        };
        if let Some(lid) = lineage_id {
            conditions.push("api_key_lineage_id = ?".to_string());
            bind_values.push(lid.to_string());
        } else {
            conditions.push("api_key_id = ?".to_string());
            bind_values.push(raw.clone());
        }
    }
    if let Some(v) = params.status_code {
        conditions.push("status_code = ?".to_string());
        bind_values.push(v.to_string());
    }
    if let Some(ref from) = params.from {
        conditions.push("created_at >= ?".to_string());
        bind_values.push(from.clone());
    }
    if let Some(ref to) = params.to {
        conditions.push("created_at <= ?".to_string());
        bind_values.push(to.clone());
    }
    // Free-text `q` searches model_id with case-insensitive substring match.
    // The user input is escaped for LIKE wildcards (% / _ / \) so they can
    // only match literal characters, not patterns.
    if let Some(ref v) = params.q
        && !v.is_empty()
    {
        conditions.push("model_id LIKE ?".to_string());
        let escaped = v
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        bind_values.push(format!("%{escaped}%"));
    }

    for (frag, val) in parse_exclude_param(
        params.exclude.as_deref(),
        &[
            ("model", "model_id", ExcludeMode::Equals),
            ("provider", "provider", ExcludeMode::Equals),
            ("user_id", "user_id", ExcludeMode::Equals),
            ("api_key_id", "api_key_id", ExcludeMode::Equals),
            ("status_code", "status_code", ExcludeMode::Equals),
        ],
    ) {
        conditions.push(frag);
        bind_values.push(val);
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let order_by = match params.sort_by.as_deref() {
        Some("cost_usd") => "cost_usd DESC",
        Some("latency_ms") => "latency_ms DESC",
        _ => "created_at DESC",
    };

    // Count query
    let count_sql = format!("SELECT count() FROM gateway_logs {where_clause}");
    let mut count_query = ch.query(&count_sql);
    for v in &bind_values {
        count_query = count_query.bind(v.as_str());
    }
    let total: u64 = count_query
        .fetch_one()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse query failed: {e}")))?;

    // Data query
    let data_sql = format!(
        "SELECT id, user_id, api_key_id, model_id, provider, input_tokens, output_tokens, \
         cost_usd, latency_ms, status_code, ip_address, \
         toString(created_at) as created_at \
         FROM gateway_logs {where_clause} ORDER BY {order_by} LIMIT {limit} OFFSET {offset}"
    );
    let mut data_query = ch.query(&data_sql);
    for v in &bind_values {
        data_query = data_query.bind(v.as_str());
    }
    let rows: Vec<GatewayLogRow> = data_query
        .fetch_all()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse query failed: {e}")))?;

    let items: Vec<GatewayLogEntry> = rows
        .into_iter()
        .map(|r| GatewayLogEntry {
            id: r.id,
            user_id: r.user_id,
            api_key_id: r.api_key_id,
            model_id: r.model_id,
            provider: r.provider,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cost_usd: r.cost_usd.map(decode_i64),
            latency_ms: r.latency_ms,
            status_code: r.status_code,
            ip_address: r.ip_address,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(GatewayLogsResponse { total, items }))
}
