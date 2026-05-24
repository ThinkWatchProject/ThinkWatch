//! Per-table ClickHouse ingest: batching, error retention, and the
//! one-time schema bootstrap (`ensure_clickhouse_tables`).
//!
//! Each `flush_<type>` function pulls the audit-shaped JSON detail
//! into the right ClickHouse Row struct and bulk-inserts via the
//! clickhouse 0.13 crate. Errors retain entries (bounded by
//! `CH_RETAIN_CAP`) so the next worker tick can retry instead of
//! dropping audit data on every transient outage.

use super::sanitize::sanitize_body_if_json;
use super::types::{
    AuditEntry, ChAccessRow, ChAppLogRow, ChAuditRow, ChGatewayRow, ChMcpRow, LogType,
    detail_cost_usd, detail_field, detail_field_non_neg_i64, detail_str, parse_created_at,
};

/// Upper bound on how many entries we retain after a flush error.
/// At the default 50-entry / 2-second flush cadence this is ~40s of
/// buffering — long enough to ride out a typical CH restart, bounded
/// enough that a permanent CH outage doesn't grow audit memory
/// unboundedly. Oldest entries get dropped first on overflow; recent
/// ones are typically more valuable for incident response.
const CH_RETAIN_CAP: usize = 1000;

pub(super) async fn flush_to_clickhouse(
    ch: &Option<clickhouse::Client>,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) {
    let Some(client) = ch else {
        // No CH configured — entries can never be flushed; drop with
        // a metric so the operator knows we're losing audit data.
        if !batch.is_empty() {
            metrics::counter!("audit_ch_dropped_total", "reason" => "no_client")
                .increment(batch.len() as u64);
        }
        batch.clear();
        return;
    };

    let count = batch.len();

    // Determine log type from first entry (all entries in a batch share the same table)
    let log_type = batch.first().map(|e| e.log_type);
    let result = match log_type {
        Some(LogType::Access) => flush_access(client, table, batch).await,
        Some(LogType::App) => flush_app(client, table, batch).await,
        Some(LogType::Audit) => flush_audit(client, table, batch).await,
        Some(LogType::Gateway) => flush_gateway(client, table, batch).await,
        Some(LogType::Mcp) => flush_mcp(client, table, batch).await,
        None => {
            batch.clear();
            return;
        }
    };

    match result {
        Ok(()) => {
            tracing::debug!("Flushed {count} entries to ClickHouse table {table}");
            batch.clear();
        }
        Err(e) => {
            // Retain on error so the next tick retries. Previously
            // we cleared unconditionally — a transient CH outage
            // dropped every in-flight audit entry irrecoverably.
            tracing::error!(
                "ClickHouse insert failed for {table}: {e} — retaining {count} entries for retry"
            );
            metrics::counter!("audit_ch_flush_failed_total", "table" => table.to_string())
                .increment(1);
            if batch.len() > CH_RETAIN_CAP {
                // Bounded retention: drop oldest, keep newest. Surface
                // the drop count so a sustained CH outage is loud, not
                // silent.
                let drop = batch.len() - CH_RETAIN_CAP;
                metrics::counter!("audit_ch_dropped_total", "reason" => "retention_cap")
                    .increment(drop as u64);
                batch.drain(..drop);
            }
        }
    }
}

async fn flush_app(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChAppLogRow>(table)?;
    for entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        insert
            .write(&ChAppLogRow {
                id: entry.id,
                level: entry.action, // we store level in action field
                target: entry.resource.unwrap_or_default(),
                message: entry.resource_id.unwrap_or_default(),
                fields: entry.detail.map(|v| v.to_string()),
                span: entry.user_agent, // repurpose user_agent for span info
                created_at: ts,
            })
            .await?;
    }
    insert.end().await
}

async fn flush_access(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChAccessRow>(table)?;
    for entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        insert
            .write(&ChAccessRow {
                id: entry.id,
                method: detail_field(&entry.detail, "method").unwrap_or_default(),
                path: detail_field(&entry.detail, "path").unwrap_or_default(),
                status_code: detail_field(&entry.detail, "status_code").unwrap_or(0),
                latency_ms: detail_field_non_neg_i64(&entry.detail, "latency_ms").unwrap_or(0),
                port: detail_field(&entry.detail, "port").unwrap_or(0),
                user_id: entry.user_id,
                user_email: entry.user_email,
                ip_address: entry.ip_address,
                user_agent: entry.user_agent,
                created_at: ts,
            })
            .await?;
    }
    insert.end().await
}

async fn flush_audit(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChAuditRow>(table)?;
    for mut entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        insert
            .write(&ChAuditRow {
                id: entry.id,
                user_id: entry.user_id,
                user_email: entry.user_email,
                api_key_id: entry.api_key_id,
                api_key_lineage_id: entry.api_key_lineage_id,
                action: entry.action,
                resource: entry.resource,
                resource_id: entry.resource_id,
                detail: detail_str(&mut entry.detail),
                ip_address: entry.ip_address,
                user_agent: entry.user_agent,
                trace_id: entry.trace_id,
                created_at: ts,
            })
            .await?;
    }
    insert.end().await
}

async fn flush_gateway(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChGatewayRow>(table)?;
    for mut entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        // Sanitise first, then measure — so the bytes column reflects
        // what is ACTUALLY stored in the body cell, not the pre-redaction
        // size. The explicit byte counts on the entry win for offloaded
        // bodies (where the cell is an S3 URL and the user's payload size
        // is the auditable fact).
        let sanitized_request = sanitize_body_if_json(entry.request_body.take());
        let sanitized_response = sanitize_body_if_json(entry.response_body.take());
        let row_request_body_bytes = entry
            .request_body_bytes
            .or_else(|| sanitized_request.as_ref().map(|s| s.len() as u32));
        let row_response_body_bytes = entry
            .response_body_bytes
            .or_else(|| sanitized_response.as_ref().map(|s| s.len() as u32));
        let row = ChGatewayRow {
            id: entry.id,
            user_id: entry.user_id,
            user_email: entry.user_email,
            api_key_id: entry.api_key_id,
            api_key_lineage_id: entry.api_key_lineage_id,
            model_id: detail_field(&entry.detail, "model_id"),
            provider: detail_field(&entry.detail, "provider"),
            upstream_model: detail_field(&entry.detail, "upstream_model"),
            input_tokens: detail_field_non_neg_i64(&entry.detail, "input_tokens"),
            output_tokens: detail_field_non_neg_i64(&entry.detail, "output_tokens"),
            cost_usd: detail_cost_usd(&entry.detail),
            latency_ms: detail_field_non_neg_i64(&entry.detail, "latency_ms"),
            status_code: detail_field_non_neg_i64(&entry.detail, "status_code"),
            ip_address: entry.ip_address,
            user_agent: entry.user_agent,
            detail: detail_str(&mut entry.detail),
            trace_id: entry.trace_id,
            session_id: entry.session_id,
            request_body_bytes: row_request_body_bytes,
            response_body_bytes: row_response_body_bytes,
            request_body: sanitized_request,
            response_body: sanitized_response,
            body_capture_status: entry.body_capture_status,
            created_at: ts,
        };
        insert.write(&row).await?;
    }
    insert.end().await
}

async fn flush_mcp(
    client: &clickhouse::Client,
    table: &str,
    batch: &mut Vec<AuditEntry>,
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChMcpRow>(table)?;
    for mut entry in batch.drain(..) {
        let ts = parse_created_at(&entry.created_at);
        // Same sanitise-then-measure ordering as flush_gateway so
        // `length(tool_arguments) == arguments_bytes` for inline rows.
        let sanitized_args = sanitize_body_if_json(entry.request_body.take());
        let sanitized_result = sanitize_body_if_json(entry.response_body.take());
        let row_arguments_bytes = entry
            .request_body_bytes
            .or_else(|| sanitized_args.as_ref().map(|s| s.len() as u32));
        let row_result_bytes = entry
            .response_body_bytes
            .or_else(|| sanitized_result.as_ref().map(|s| s.len() as u32));
        let row = ChMcpRow {
            id: entry.id,
            user_id: entry.user_id,
            user_email: entry.user_email,
            server_id: detail_field(&entry.detail, "server_id"),
            server_name: detail_field(&entry.detail, "server_name"),
            tool_name: detail_field(&entry.detail, "tool_name"),
            duration_ms: detail_field_non_neg_i64(&entry.detail, "duration_ms"),
            status: detail_field(&entry.detail, "status"),
            error_message: detail_field(&entry.detail, "error_message"),
            ip_address: entry.ip_address,
            detail: detail_str(&mut entry.detail),
            arguments_bytes: row_arguments_bytes,
            result_bytes: row_result_bytes,
            tool_arguments: sanitized_args,
            tool_result: sanitized_result,
            body_capture_status: entry.body_capture_status,
            trace_id: entry.trace_id,
            created_at: ts,
        };
        insert.write(&row).await?;
    }
    insert.end().await
}

/// Ensure ClickHouse tables exist. Call once at startup.
/// Run the ClickHouse schema bootstrap (initdb.d/*.sql) exactly once
/// at startup.
///
/// Returns `Ok(())` when ClickHouse isn't configured at all
/// (`ch = None` is a valid deployment — operators who don't want
/// columnar audit can opt out via env), or when every CREATE
/// statement succeeded. Returns `Err` on the first failed statement
/// so the caller can retry with backoff and refuse to start the
/// gateway if the database is permanently unreachable.
pub async fn ensure_clickhouse_tables(
    ch: &Option<clickhouse::Client>,
) -> Result<(), clickhouse::error::Error> {
    let Some(client) = ch else {
        return Ok(());
    };

    // Schema bootstrap. Mirrors the docker entrypoint mount of
    // deploy/clickhouse/initdb.d/, embedded at compile time so the
    // binary can re-bootstrap on startup when the ClickHouse data
    // dir already exists (in which case the entrypoint init scripts
    // don't run).
    let init_sql = include_str!("../../../../deploy/clickhouse/initdb.d/01_init.sql");

    // Strip `--` line comments before splitting on `;`, otherwise a
    // semicolon inside a comment (e.g. "originating handler's
    // middleware;") splits a CREATE TABLE in half. The init file
    // contains no string literals with `--`, so naive line-prefix
    // stripping is safe.
    let cleaned: String = init_sql
        .lines()
        .map(|l| l.split_once("--").map(|(code, _)| code).unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n");

    for statement in cleaned.split(';') {
        let stmt = statement.trim();
        if stmt.is_empty() {
            continue;
        }

        if let Err(e) = client.query(stmt).execute().await {
            tracing::warn!("ClickHouse init statement failed: {e}");
            return Err(e);
        }
    }

    // One-shot backfill of aggregate tables. The MVs attached to
    // mcp_logs / gateway_logs only capture rows inserted *after* the
    // MV is created, so on first boot (or when schema is upgraded to
    // include these MVs) we seed the aggregate tables with a snapshot
    // of whatever history is still retained. Gated on emptiness so it
    // runs exactly once per aggregate — cheap to check, safe to skip.
    backfill_if_empty(
        client,
        "mcp_server_call_counts",
        "INSERT INTO mcp_server_call_counts \
         SELECT server_id, toUInt64(count()) AS calls \
         FROM mcp_logs WHERE server_id IS NOT NULL GROUP BY server_id",
    )
    .await;
    backfill_if_empty(
        client,
        "provider_health_5m",
        // 7 columns, named explicitly. The table has a `throttled_requests`
        // column (added by a later ALTER) that this backfill used to omit
        // — `INSERT ... SELECT 6 cols` would fail outright with a count
        // mismatch on any deployment that had gateway_logs traffic to
        // backfill from. Also: split status_code=429 out of error_requests
        // so the backfill matches the MV's accounting (the MV was
        // rewritten to separate throttle from error after operators
        // mis-read throttled upstreams as 'down'; the backfill was left
        // on the old lumped accounting).
        "INSERT INTO provider_health_5m \
            (bucket_5m, provider, total_requests, error_requests, \
             throttled_requests, sum_latency_ms, requests_latency) \
         SELECT toStartOfFiveMinutes(created_at) AS bucket_5m, \
                provider, \
                toUInt64(count()) AS total_requests, \
                toUInt64(countIf(status_code >= 400 AND status_code != 429)) AS error_requests, \
                toUInt64(countIf(status_code = 429)) AS throttled_requests, \
                sum(ifNull(latency_ms, 0)) AS sum_latency_ms, \
                toUInt64(countIf(latency_ms IS NOT NULL)) AS requests_latency \
         FROM gateway_logs WHERE provider IS NOT NULL \
         GROUP BY bucket_5m, provider",
    )
    .await;
    // cost_rollup_hourly mirrors gateway_logs at hourly granularity.
    // The MV at line ~309 of 01_init.sql captures every new row, but
    // first boot (or schema upgrade adding the MV) leaves the rollup
    // empty until backfilled. Without this, the cost dashboards show
    // truncated history despite gateway_logs holding the source rows.
    backfill_if_empty(
        client,
        "cost_rollup_hourly",
        "INSERT INTO cost_rollup_hourly \
         SELECT toStartOfHour(created_at) AS hour, \
                model_id, \
                provider, \
                user_id, \
                api_key_id, \
                api_key_lineage_id, \
                toUInt64(count()) AS request_count, \
                sum(ifNull(input_tokens, 0)) AS input_tokens, \
                sum(ifNull(output_tokens, 0)) AS output_tokens, \
                sum(ifNull(cost_usd, 0)) AS cost_usd \
         FROM gateway_logs \
         GROUP BY hour, model_id, provider, user_id, api_key_id, api_key_lineage_id",
    )
    .await;

    tracing::info!("ClickHouse tables initialized");
    Ok(())
}

/// Run `insert_sql` against `client` iff `table` is currently empty.
/// Failures are logged but not propagated — a missing backfill yields
/// a temporarily-low metric, never a failed boot.
async fn backfill_if_empty(client: &clickhouse::Client, table: &str, insert_sql: &str) {
    let count_sql = format!("SELECT count() FROM {table}");
    match client.query(&count_sql).fetch_one::<u64>().await {
        Ok(0) => {
            if let Err(e) = client.query(insert_sql).execute().await {
                tracing::warn!("ClickHouse backfill of {table} failed: {e}");
            } else {
                tracing::info!("ClickHouse aggregate {table} backfilled from source log table");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("ClickHouse count({table}) failed: {e}"),
    }
}
