use axum::Json;
use axum::extract::{Path, Query, State};
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
    pub upstream_model: Option<String>,
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
    pub upstream_model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    #[schema(value_type = Option<String>)]
    #[serde(with = "rust_decimal::serde::str_option")]
    pub cost_usd: Option<Decimal>,
    pub latency_ms: Option<i64>,
    pub status_code: Option<i64>,
    pub ip_address: Option<String>,
    pub created_at: String,
    /// Captured-body sizes + status surfaced in the list so the
    /// frontend's "View bodies" button can show "(2.3 MB)" before
    /// the auditor commits to a fetch (each fetch fires an
    /// `audit.body_viewed` second-order audit row, so click-through
    /// is a recordable event the auditor should think about). The
    /// actual body content stays behind the `logs:read_bodies` perm
    /// + the dedicated `/body` endpoint.
    pub request_body_bytes: Option<i64>,
    pub response_body_bytes: Option<i64>,
    pub body_capture_status: Option<String>,
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
    upstream_model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cost_usd: Option<i64>,
    latency_ms: Option<i64>,
    status_code: Option<i64>,
    ip_address: Option<String>,
    created_at: String,
    request_body_bytes: Option<u32>,
    response_body_bytes: Option<u32>,
    body_capture_status: Option<String>,
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
    auth_user
        .require_global_permission(&state.db, "logs:read_all")
        .await?;
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
    if let Some(ref v) = params.upstream_model {
        conditions.push("upstream_model = ?".to_string());
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
    push_time_range_conditions(
        &mut conditions,
        &mut bind_values,
        params.from.as_deref(),
        params.to.as_deref(),
    )?;
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
            ("upstream_model", "upstream_model", ExcludeMode::Equals),
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

    // Data query — pulls the body-size + status columns (NOT the
    // body content) so the frontend can show "(2.3 MB)" on the
    // View bodies button. The actual prompts / completions stay
    // behind the dedicated `/body` endpoint + `logs:read_bodies`.
    let data_sql = format!(
        "SELECT id, user_id, api_key_id, model_id, provider, upstream_model, \
         input_tokens, output_tokens, \
         cost_usd, latency_ms, status_code, ip_address, \
         toString(created_at) as created_at, \
         request_body_bytes, response_body_bytes, body_capture_status \
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
            upstream_model: r.upstream_model,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cost_usd: r.cost_usd.map(decode_i64),
            latency_ms: r.latency_ms,
            status_code: r.status_code,
            ip_address: r.ip_address,
            created_at: r.created_at,
            request_body_bytes: r.request_body_bytes.map(|v| v as i64),
            response_body_bytes: r.response_body_bytes.map(|v| v as i64),
            body_capture_status: r.body_capture_status,
        })
        .collect();

    Ok(Json(GatewayLogsResponse { total, items }))
}

// ---------------------------------------------------------------------------
// Body viewer — separate endpoint, separate permission
// ---------------------------------------------------------------------------

/// Wire shape returned by `GET /api/admin/gateway/logs/{id}/body`.
/// The list endpoint NEVER includes these fields because the
/// permission profile for "see a row" is strictly weaker than "see
/// the user's prompt content".
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GatewayLogBodyResponse {
    pub id: String,
    pub trace_id: Option<String>,
    pub user_id: Option<String>,
    pub model_id: Option<String>,
    pub created_at: String,
    /// JSON-serialized `messages` array as the user authored it
    /// (pre-PII-redaction). `None` when the capture toggle was off
    /// at write time or the request hit a pre-route-selection
    /// failure path. Inspect `body_capture_status` for the reason.
    pub request_body: Option<String>,
    /// JSON-serialized completion. `None` for the same reasons as
    /// `request_body`, plus error paths that produced no upstream
    /// response.
    pub response_body: Option<String>,
    pub request_body_bytes: Option<u32>,
    pub response_body_bytes: Option<u32>,
    /// `captured` / `truncated` / `disabled` / `from_cache` / NULL.
    pub body_capture_status: Option<String>,
}

#[derive(Debug, Deserialize, clickhouse::Row)]
struct GatewayLogBodyRow {
    id: String,
    trace_id: Option<String>,
    user_id: Option<String>,
    model_id: Option<String>,
    created_at: String,
    request_body: Option<String>,
    response_body: Option<String>,
    request_body_bytes: Option<u32>,
    response_body_bytes: Option<u32>,
    body_capture_status: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/admin/gateway/logs/{id}/body",
    tag = "Gateway Logs",
    params(("id" = String, Path, description = "Gateway log row id (uuid string)")),
    responses(
        (status = 200, description = "Captured request + response bodies", body = GatewayLogBodyResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden — missing logs:read_bodies"),
        (status = 404, description = "Row not found in ClickHouse retention window"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_gateway_log_body(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<GatewayLogBodyResponse>, AppError> {
    // logs:read_bodies is a separate, dangerous-tier permission. It
    // is NOT included in the seeded admin policy — only super_admin
    // gets it via wildcard, plus any custom role an operator chose
    // to grant it to (compliance, incident response). The list
    // endpoint gates on logs:read_all; this one needs the stronger
    // grant explicitly.
    auth_user
        .require_global_permission(&state.db, "logs:read_bodies")
        .await?;
    if !ch_available(&state) {
        return Err(AppError::NotFound("ClickHouse not configured".into()));
    }
    let ch = ch_client(&state)?;
    let row: Option<GatewayLogBodyRow> = ch
        .query(
            "SELECT id, trace_id, user_id, model_id, \
                    formatDateTime(created_at, '%Y-%m-%dT%H:%M:%S.%fZ', 'UTC') AS created_at, \
                    request_body, response_body, request_body_bytes, \
                    response_body_bytes, body_capture_status \
             FROM gateway_logs WHERE id = ? LIMIT 1",
        )
        .bind(&id)
        .fetch_optional()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse body query: {e}")))?;
    let row = row.ok_or_else(|| AppError::NotFound("Gateway log row not found".into()))?;

    // Second-order audit: viewing a body IS itself a sensitive
    // action and shows up in `audit_logs` so an operator can answer
    // "who looked at this user's prompts" in the same way they can
    // answer "who deleted this api key". The detail JSON carries the
    // viewed row's id + trace_id + subject user so the audit row is
    // self-contained without a join.
    state.audit.log(
        auth_user
            .audit("audit.body_viewed")
            .resource("gateway_logs")
            .resource_id(row.id.clone())
            .detail(serde_json::json!({
                "log_kind": "gateway",
                "log_id": row.id,
                "trace_id": row.trace_id,
                "subject_user_id": row.user_id,
                "model_id": row.model_id,
            })),
    );

    // Dereference s3:// pointers if the body was offloaded. The
    // audit row carries either the raw payload OR an `s3://bucket/key`
    // URL — auditors always want the actual content, so resolve here
    // so the frontend doesn't need to know about offload at all.
    let request_body = deref_body(&state, row.request_body).await?;
    let response_body = deref_body(&state, row.response_body).await?;

    Ok(Json(GatewayLogBodyResponse {
        id: row.id,
        trace_id: row.trace_id,
        user_id: row.user_id,
        model_id: row.model_id,
        created_at: row.created_at,
        request_body,
        response_body,
        request_body_bytes: row.request_body_bytes,
        response_body_bytes: row.response_body_bytes,
        body_capture_status: row.body_capture_status,
    }))
}

/// Resolve a body cell back to its actual content. Inline cells pass
/// through unchanged; `s3://bucket/key` URLs trigger a fetch against
/// the configured blob store. Surfaces blob-store errors as 502s so
/// auditors don't confuse "S3 backend down" with "no body captured".
pub(crate) async fn deref_body(
    state: &AppState,
    cell: Option<String>,
) -> Result<Option<String>, AppError> {
    let Some(s) = cell else { return Ok(None) };
    if !s.starts_with("s3://") {
        return Ok(Some(s));
    }
    let bytes = state
        .blob_store
        .fetch(&s)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("offloaded body fetch failed: {e}")))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("offloaded body not utf-8: {e}")))
}
