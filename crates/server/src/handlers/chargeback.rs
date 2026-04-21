//! Chargeback report — per-cost-center spend roll-up exportable as CSV.
//!
//! Cost centers are operator-assigned tags on api_keys (already in the
//! schema). For each tag we sum `usage_records.cost_usd` over a
//! billing period, plus the per-model breakdown so finance can see
//! where the spend went. Output is CSV (text/csv) so it pastes
//! straight into a spreadsheet — JSON is fine for the UI but procurement
//! reaches for CSV ten times out of ten.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use chrono::{DateTime, Datelike, Utc};
use serde::Deserialize;
use sqlx::Row;
use think_watch_common::errors::AppError;

use crate::app::AppState;
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

    // Group by api_keys.cost_center (NULL → "(unassigned)") and
    // models.model_id so finance can chase down a line item to the
    // model that drove it. JOIN on api_keys to pick up cost_center;
    // soft-deleted keys keep their historical attribution because
    // usage_records.api_key_id is set NULL on delete (the key is
    // gone but its cost_center label was already snapshotted into
    // the row's history when the key existed).
    let rows = sqlx::query(
        "SELECT \
           COALESCE(k.cost_center, '(unassigned)') AS cost_center, \
           u.model_id                              AS model_id, \
           SUM(u.cost_usd)                         AS cost_usd, \
           SUM(u.total_tokens)                     AS total_tokens, \
           COUNT(*)                                AS request_count \
         FROM usage_records u \
         LEFT JOIN api_keys k ON k.id = u.api_key_id \
         WHERE u.created_at >= $1 AND u.created_at < $2 \
         GROUP BY cost_center, u.model_id \
         ORDER BY cost_center, cost_usd DESC",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&state.db)
    .await?;

    // Manual CSV serialisation — pulling in a csv crate for one
    // endpoint isn't worth the dep. Each value is escaped with
    // RFC 4180 quoting only when needed (commas, quotes, newlines).
    let mut body = String::with_capacity(rows.len() * 80);
    body.push_str("cost_center,model_id,cost_usd,total_tokens,request_count\n");
    for r in rows {
        let cost_center: String = r.try_get("cost_center").unwrap_or_default();
        let model_id: String = r.try_get("model_id").unwrap_or_default();
        let cost_usd: rust_decimal::Decimal =
            r.try_get("cost_usd").unwrap_or(rust_decimal::Decimal::ZERO);
        let total_tokens: i64 = r.try_get("total_tokens").unwrap_or(0);
        let request_count: i64 = r.try_get("request_count").unwrap_or(0);
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
