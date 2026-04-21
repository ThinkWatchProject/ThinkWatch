//! Chargeback report — per-cost-center spend roll-up exportable as CSV.
//!
//! Cost centers are operator-assigned tags on api_keys (already in the
//! schema). For each tag we sum `gateway_logs.cost_usd` over a
//! billing period, plus the per-model breakdown so finance can see
//! where the spend went. Output is CSV (text/csv) so it pastes
//! straight into a spreadsheet — JSON is fine for the UI but procurement
//! reaches for CSV ten times out of ten.
//!
//! ClickHouse `gateway_logs` doesn't store `cost_center` directly (it's
//! a mutable label on `api_keys` that can change over time), so we
//! aggregate by `api_key_id` in CH and enrich with a single PG lookup
//! of id→cost_center. Keys deleted from `api_keys` still appear in CH
//! rows; those map to the `(unassigned)` bucket, same as an active
//! key with no cost_center tag.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use chrono::{DateTime, Datelike, Utc};
use serde::Deserialize;
use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::handlers::clickhouse_util::ch_client;
use crate::middleware::auth_guard::AuthUser;

#[derive(Debug, Deserialize)]
pub struct ChargebackQuery {
    /// ISO-8601 lower bound (inclusive). Defaults to start of current month.
    pub from: Option<DateTime<Utc>>,
    /// ISO-8601 upper bound (exclusive). Defaults to now.
    pub to: Option<DateTime<Utc>>,
}

/// GET /api/admin/chargeback.csv — per-cost-center spend roll-up.
#[utoipa::path(
    get,
    path = "/api/admin/chargeback.csv",
    tag = "Analytics",
    params(
        ("from" = Option<String>, Query, description = "ISO-8601 lower bound (inclusive)"),
        ("to"   = Option<String>, Query, description = "ISO-8601 upper bound (exclusive)"),
    ),
    responses(
        (status = 200, description = "text/csv chargeback report"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn export_chargeback_csv(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<ChargebackQuery>,
) -> Result<impl IntoResponse, AppError> {
    auth_user.require_permission("analytics:read_all")?;
    auth_user
        .assert_scope_global(&state.db, "analytics:read_all")
        .await?;

    let now = Utc::now();
    let from = q.from.unwrap_or_else(|| {
        // Default lower bound: start of the current calendar month UTC.
        chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|n| n.and_utc())
            .unwrap_or(now)
    });
    let to = q.to.unwrap_or(now);

    // Stage 1 — aggregate per (api_key_id, model_id) in ClickHouse.
    // We don't know cost_center in CH (it lives on the mutable
    // `api_keys` row), so we carry api_key_id through the first
    // pass and label in Rust.
    #[derive(clickhouse::Row, Deserialize)]
    struct ChRow {
        api_key_id: String,
        model_id: String,
        cost_usd: f64,
        total_tokens: u64,
        request_count: u64,
    }

    let ch = ch_client(&state)?;
    let from_str = from.format("%Y-%m-%d %H:%M:%S").to_string();
    let to_str = to.format("%Y-%m-%d %H:%M:%S").to_string();
    let ch_rows: Vec<ChRow> = ch
        .query(
            "SELECT \
                ifNull(api_key_id, '') AS api_key_id, \
                ifNull(model_id, '')   AS model_id, \
                sum(ifNull(cost_usd, 0))                                      AS cost_usd, \
                toUInt64(sum(ifNull(input_tokens, 0)) + sum(ifNull(output_tokens, 0))) AS total_tokens, \
                toUInt64(count())                                             AS request_count \
             FROM gateway_logs \
             WHERE created_at >= parseDateTimeBestEffort(?) \
               AND created_at <  parseDateTimeBestEffort(?) \
             GROUP BY api_key_id, model_id",
        )
        .bind(&from_str)
        .bind(&to_str)
        .fetch_all::<ChRow>()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("chargeback ClickHouse query: {e}")))?;

    // Stage 2 — resolve api_key_id → cost_center with one PG pass.
    // Includes soft-deleted keys; a deleted key whose cost_center
    // was set at delete time still maps to that label here. Keys
    // absent from `api_keys` entirely (hard-deleted, or
    // gateway_logs.api_key_id was NULL) fall into "(unassigned)".
    let referenced_keys: Vec<uuid::Uuid> = ch_rows
        .iter()
        .filter_map(|r| uuid::Uuid::parse_str(&r.api_key_id).ok())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let key_to_center: std::collections::HashMap<uuid::Uuid, String> = if referenced_keys.is_empty()
    {
        std::collections::HashMap::new()
    } else {
        let rows: Vec<(uuid::Uuid, Option<String>)> =
            sqlx::query_as("SELECT id, cost_center FROM api_keys WHERE id = ANY($1)")
                .bind(&referenced_keys)
                .fetch_all(&state.db)
                .await?;
        rows.into_iter()
            .filter_map(|(id, cc)| cc.map(|c| (id, c)))
            .collect()
    };

    // Stage 3 — regroup by (cost_center, model_id). Two CH rows for
    // the same model may now collapse into one line if they share a
    // cost_center (e.g. two api_keys both tagged "marketing").
    use std::collections::BTreeMap;
    type Totals = (f64, u64, u64);
    let mut grouped: BTreeMap<(String, String), Totals> = BTreeMap::new();
    for r in ch_rows {
        let cost_center = uuid::Uuid::parse_str(&r.api_key_id)
            .ok()
            .and_then(|id| key_to_center.get(&id).cloned())
            .unwrap_or_else(|| "(unassigned)".to_string());
        let entry = grouped
            .entry((cost_center, r.model_id))
            .or_insert((0.0, 0, 0));
        entry.0 += r.cost_usd;
        entry.1 += r.total_tokens;
        entry.2 += r.request_count;
    }

    // Sort: cost_center ASC, cost_usd DESC within each group —
    // matches the original SQL ORDER BY.
    let mut ordered: Vec<((String, String), Totals)> = grouped.into_iter().collect();
    ordered.sort_by(|a, b| {
        a.0.0.cmp(&b.0.0).then_with(|| {
            b.1.0
                .partial_cmp(&a.1.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    // Manual CSV serialisation — pulling in a csv crate for one
    // endpoint isn't worth the dep. Each value is escaped with
    // RFC 4180 quoting only when needed (commas, quotes, newlines).
    let mut body = String::with_capacity(ordered.len() * 80);
    body.push_str("cost_center,model_id,cost_usd,total_tokens,request_count\n");
    for ((cost_center, model_id), (cost_usd, total_tokens, request_count)) in ordered {
        body.push_str(&format!(
            "{},{},{},{},{}\n",
            csv_escape(&cost_center),
            csv_escape(&model_id),
            cost_usd,
            total_tokens,
            request_count,
        ));
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "text/csv; charset=utf-8".parse().unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!(
            "attachment; filename=\"chargeback_{}_{}.csv\"",
            from.format("%Y%m%d"),
            to.format("%Y%m%d"),
        )
        .parse()
        .unwrap(),
    );
    Ok((headers, body))
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') {
        let escaped = value.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}
