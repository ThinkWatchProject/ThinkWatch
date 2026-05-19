use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::clickhouse_util::*;

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct McpLogsQuery {
    pub user_id: Option<String>,
    pub server_id: Option<String>,
    pub tool_name: Option<String>,
    pub status: Option<String>,
    /// Free-text search across tool_name (substring, case-insensitive).
    pub q: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub exclude: Option<String>,
    pub sort_by: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, clickhouse::Row, utoipa::ToSchema)]
pub struct McpLogEntry {
    pub id: String,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub tool_name: Option<String>,
    pub duration_ms: Option<i64>,
    pub status: Option<String>,
    pub error_message: Option<String>,
    pub ip_address: Option<String>,
    pub created_at: String,
    /// Same body-size + capture-status surfacing as the gateway list
    /// endpoint — lets the frontend show "(2.3 MB)" on the View
    /// bodies button without leaking the actual tool arguments /
    /// results (those still gate behind `logs:read_bodies` + the
    /// dedicated `/body` endpoint).
    pub arguments_bytes: Option<i64>,
    pub result_bytes: Option<i64>,
    pub body_capture_status: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct McpLogsResponse {
    pub items: Vec<McpLogEntry>,
    pub total: u64,
}

#[utoipa::path(
    get,
    path = "/api/mcp/logs",
    tag = "MCP Logs",
    params(McpLogsQuery),
    responses(
        (status = 200, description = "Paginated MCP tool call logs", body = McpLogsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_mcp_logs(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<McpLogsQuery>,
) -> Result<Json<McpLogsResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "logs:read_all")
        .await?;
    if !ch_available(&state) {
        return Ok(Json(McpLogsResponse {
            total: 0,
            items: vec![],
        }));
    }
    let ch = ch_client(&state)?;
    let (limit, offset) = clamp_pagination(params.limit, params.offset, 200);

    let mut conditions: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref v) = params.user_id {
        conditions.push("user_id = ?".into());
        binds.push(v.clone());
    }
    if let Some(ref v) = params.server_id {
        conditions.push("server_id = ?".into());
        binds.push(v.clone());
    }
    if let Some(ref v) = params.tool_name {
        conditions.push("tool_name = ?".into());
        binds.push(v.clone());
    }
    if let Some(ref v) = params.status {
        conditions.push("status = ?".into());
        binds.push(v.clone());
    }
    push_time_range_conditions(
        &mut conditions,
        &mut binds,
        params.from.as_deref(),
        params.to.as_deref(),
    )?;
    if let Some(ref v) = params.q
        && !v.is_empty()
    {
        conditions.push("tool_name LIKE ?".into());
        let escaped = v
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        binds.push(format!("%{escaped}%"));
    }

    for (frag, val) in parse_exclude_param(
        params.exclude.as_deref(),
        &[
            ("user_id", "user_id", ExcludeMode::Equals),
            ("server_id", "server_id", ExcludeMode::Equals),
            ("tool_name", "tool_name", ExcludeMode::Equals),
            ("status", "status", ExcludeMode::Equals),
        ],
    ) {
        conditions.push(frag);
        binds.push(val);
    }

    let wc = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let ob = match params.sort_by.as_deref() {
        Some("duration_ms") => "duration_ms DESC",
        _ => "created_at DESC",
    };

    let count_sql = format!("SELECT count() FROM mcp_logs {wc}");
    let mut q = ch.query(&count_sql);
    for v in &binds {
        q = q.bind(v.as_str());
    }
    let total: u64 = q
        .fetch_one()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    // Surface body sizes + status so the View bodies button can show
    // "(N bytes)" without an extra round-trip. The actual content is
    // still gated by `logs:read_bodies` on the dedicated /body endpoint.
    // toInt64 because the underlying columns are UInt32 but the wire
    // contract is Option<i64> for consistency with input_tokens etc.
    let data_sql = format!(
        "SELECT id, user_id, user_email, server_id, server_name, tool_name, duration_ms, status, error_message, ip_address, toString(created_at) as created_at, \
         toNullable(toInt64(arguments_bytes)) AS arguments_bytes, \
         toNullable(toInt64(result_bytes)) AS result_bytes, \
         body_capture_status \
         FROM mcp_logs {wc} ORDER BY {ob} LIMIT {limit} OFFSET {offset}"
    );
    let mut q = ch.query(&data_sql);
    for v in &binds {
        q = q.bind(v.as_str());
    }
    let items: Vec<McpLogEntry> = q
        .fetch_all()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse: {e}")))?;

    Ok(Json(McpLogsResponse { total, items }))
}

// ---------------------------------------------------------------------------
// Body viewer — same shape + permission story as the gateway version
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct McpLogBodyResponse {
    pub id: String,
    pub trace_id: Option<String>,
    pub user_id: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub tool_name: Option<String>,
    pub created_at: String,
    /// JSON of `tools/call.params.arguments` exactly as the caller
    /// submitted (modulo secret-key sanitization on the detail
    /// pipeline that runs orthogonally). `None` when the
    /// `audit.capture_tool_arguments` toggle was off at write time.
    pub tool_arguments: Option<String>,
    /// JSON of the upstream JSON-RPC `result` field. `None` for
    /// failures (the response carries `error` instead) or when
    /// `audit.capture_tool_results` was off.
    pub tool_result: Option<String>,
    pub arguments_bytes: Option<u32>,
    pub result_bytes: Option<u32>,
    pub body_capture_status: Option<String>,
}

#[derive(Debug, Deserialize, clickhouse::Row)]
struct McpLogBodyRow {
    id: String,
    trace_id: Option<String>,
    user_id: Option<String>,
    server_id: Option<String>,
    server_name: Option<String>,
    tool_name: Option<String>,
    created_at: String,
    tool_arguments: Option<String>,
    tool_result: Option<String>,
    arguments_bytes: Option<u32>,
    result_bytes: Option<u32>,
    body_capture_status: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/admin/mcp/logs/{id}/body",
    tag = "MCP Logs",
    params(("id" = String, Path, description = "MCP log row id (uuid string)")),
    responses(
        (status = 200, description = "Captured tool arguments + result", body = McpLogBodyResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden — missing logs:read_bodies"),
        (status = 404, description = "Row not found in ClickHouse retention window"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_mcp_log_body(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<McpLogBodyResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "logs:read_bodies")
        .await?;
    if !ch_available(&state) {
        return Err(AppError::NotFound("ClickHouse not configured".into()));
    }
    let ch = ch_client(&state)?;
    let row: Option<McpLogBodyRow> = ch
        .query(
            "SELECT id, trace_id, user_id, server_id, server_name, tool_name, \
                    formatDateTime(created_at, '%Y-%m-%dT%H:%M:%S.%fZ', 'UTC') AS created_at, \
                    tool_arguments, tool_result, arguments_bytes, result_bytes, \
                    body_capture_status \
             FROM mcp_logs WHERE id = ? LIMIT 1",
        )
        .bind(&id)
        .fetch_optional()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse body query: {e}")))?;
    let row = row.ok_or_else(|| AppError::NotFound("MCP log row not found".into()))?;

    // Second-order audit — same contract as the gateway body endpoint.
    state.audit.log(
        auth_user
            .audit("audit.body_viewed")
            .resource("mcp_logs")
            .resource_id(row.id.clone())
            .detail(serde_json::json!({
                "log_kind": "mcp",
                "log_id": row.id,
                "trace_id": row.trace_id,
                "subject_user_id": row.user_id,
                "server_id": row.server_id,
                "server_name": row.server_name,
                "tool_name": row.tool_name,
            })),
    );

    // Same s3:// dereference contract as gateway/logs/{id}/body —
    // reuse the helper so a single bug in offload-resolution can't
    // diverge between the two endpoints.
    let tool_arguments = super::gateway_logs::deref_body(&state, row.tool_arguments).await?;
    let tool_result = super::gateway_logs::deref_body(&state, row.tool_result).await?;

    Ok(Json(McpLogBodyResponse {
        id: row.id,
        trace_id: row.trace_id,
        user_id: row.user_id,
        server_id: row.server_id,
        server_name: row.server_name,
        tool_name: row.tool_name,
        created_at: row.created_at,
        tool_arguments,
        tool_result,
        arguments_bytes: row.arguments_bytes,
        result_bytes: row.result_bytes,
        body_capture_status: row.body_capture_status,
    }))
}
