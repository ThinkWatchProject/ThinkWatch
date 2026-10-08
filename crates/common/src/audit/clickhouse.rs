//! Per-table ClickHouse ingest: batching, error retention, and the
//! one-time schema bootstrap (`ensure_clickhouse_tables`).
//!
//! Each `flush_<type>` function pulls the audit-shaped JSON detail
//! into the right ClickHouse Row struct and bulk-inserts it. Errors
//! retain entries (bounded by `CH_RETAIN_CAP`) so the next worker tick
//! can retry instead of dropping audit data on every transient outage.
//! The `flush_<type>` functions therefore read the batch without taking
//! entries out of it: an insert fails at `write` or `end` as often as at
//! `insert` (once the table's schema is cached, `insert` sends nothing),
//! and only `flush_to_clickhouse` clears the batch, once the insert has
//! succeeded.

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
    batch: &[AuditEntry],
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChAppLogRow>(table).await?;
    for entry in batch.iter().cloned() {
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
    batch: &[AuditEntry],
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChAccessRow>(table).await?;
    for entry in batch.iter().cloned() {
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
    batch: &[AuditEntry],
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChAuditRow>(table).await?;
    for mut entry in batch.iter().cloned() {
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
    batch: &[AuditEntry],
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChGatewayRow>(table).await?;
    for mut entry in batch.iter().cloned() {
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
    batch: &[AuditEntry],
) -> Result<(), clickhouse::error::Error> {
    let mut insert = client.insert::<ChMcpRow>(table).await?;
    for mut entry in batch.iter().cloned() {
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
        // 7 columns named explicitly so a future schema add doesn't
        // silently shift positions. 429s land in `throttled_requests`
        // (NOT `error_requests`) to match the MV's accounting — a
        // throttled upstream isn't down.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The columns of the five tables the writer inserts into that have no
    /// default, as of 3.2.1. Each one is written by every release since it
    /// was added. A column added later has to come with a default (see the
    /// note at the top of the log tables in 01_init.sql), so this list only
    /// ever shrinks.
    const WITHOUT_DEFAULT: &[(&str, &[&str])] = &[
        (
            "app_logs",
            &["id", "level", "target", "message", "fields", "span"],
        ),
        (
            "access_logs",
            &[
                "id",
                "method",
                "path",
                "status_code",
                "latency_ms",
                "port",
                "user_id",
                "user_email",
                "ip_address",
                "user_agent",
            ],
        ),
        (
            "audit_logs",
            &[
                "id",
                "user_id",
                "user_email",
                "api_key_id",
                "api_key_lineage_id",
                "action",
                "resource",
                "resource_id",
                "detail",
                "ip_address",
                "user_agent",
                "trace_id",
            ],
        ),
        (
            "gateway_logs",
            &[
                "id",
                "user_id",
                "user_email",
                "api_key_id",
                "api_key_lineage_id",
                "model_id",
                "provider",
                "upstream_model",
                "input_tokens",
                "output_tokens",
                "cost_usd",
                "latency_ms",
                "status_code",
                "ip_address",
                "user_agent",
                "detail",
                "trace_id",
                "session_id",
                "request_body",
                "response_body",
                "request_body_bytes",
                "response_body_bytes",
                "body_capture_status",
            ],
        ),
        (
            "mcp_logs",
            &[
                "id",
                "user_id",
                "user_email",
                "server_id",
                "server_name",
                "tool_name",
                "duration_ms",
                "status",
                "error_message",
                "ip_address",
                "detail",
                "trace_id",
                "tool_arguments",
                "tool_result",
                "arguments_bytes",
                "result_bytes",
                "body_capture_status",
            ],
        ),
    ];

    /// Every column `table` gets from 01_init.sql, from its CREATE TABLE and
    /// its `ADD COLUMN`s, and whether it has a default (DEFAULT,
    /// MATERIALIZED, ALIAS or EPHEMERAL).
    fn columns(sql: &str, table: &str) -> Vec<(String, bool)> {
        let has_default = |rest: &str| {
            rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|w| matches!(w, "DEFAULT" | "MATERIALIZED" | "ALIAS" | "EPHEMERAL"))
        };
        let mut out = Vec::new();
        let create = format!("CREATE TABLE IF NOT EXISTS {table} (");
        let start = sql.find(&create).expect("CREATE TABLE") + create.len();
        let body = &sql[start..start + sql[start..].find("\n) ENGINE").expect("ENGINE")];
        for line in body.lines() {
            let line = line.trim().trim_end_matches(',');
            let first = line.split_whitespace().next().unwrap_or("");
            if first.is_empty()
                || first.starts_with("--")
                || matches!(first, "INDEX" | "PROJECTION" | "CONSTRAINT")
            {
                continue;
            }
            out.push((first.to_string(), has_default(&line[first.len()..])));
        }
        let add = format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS ");
        for line in sql
            .lines()
            .filter_map(|l| l.trim().strip_prefix(add.as_str()))
        {
            let name = line.split_whitespace().next().expect("column name");
            out.push((name.to_string(), has_default(&line[name.len()..])));
        }
        out
    }

    /// The server validates each insert against the table: a column the
    /// inserting instance does not write must have a default. An instance of
    /// the previous release does not write a column added since, so a new
    /// column without a default makes its inserts fail during a rolling
    /// upgrade and after a rollback, losing log rows.
    #[test]
    fn new_log_table_columns_have_a_default() {
        let sql = include_str!("../../../../deploy/clickhouse/initdb.d/01_init.sql");
        for (table, known) in WITHOUT_DEFAULT {
            let cols = columns(sql, table);
            assert!(
                cols.iter().any(|(c, d)| c == "created_at" && *d),
                "{table}: the parser did not find the columns: {cols:?}"
            );
            let mut without: Vec<&str> = cols
                .iter()
                .filter(|(_, d)| !d)
                .map(|(c, _)| c.as_str())
                .collect();
            without.sort_unstable();
            let mut known = known.to_vec();
            known.sort_unstable();
            let new: Vec<&&str> = without.iter().filter(|c| !known.contains(c)).collect();
            assert!(
                new.is_empty(),
                "{table}: column(s) {new:?} have no default. Instances of the \
                 previous release do not write them, and their inserts into \
                 {table} fail until they have one: add `DEFAULT …` (for a \
                 Nullable column, `DEFAULT NULL`)."
            );
            assert_eq!(
                without, known,
                "{table}: a listed column is gone or has a default now; take it \
                 off WITHOUT_DEFAULT"
            );
        }
    }

    /// The parser sees a default where there is one.
    #[test]
    fn the_column_parser_reads_defaults() {
        let sql = "CREATE TABLE IF NOT EXISTS t (\n    a String,\n    b Nullable(String) DEFAULT NULL,\n    INDEX i a TYPE set(1) GRANULARITY 1\n) ENGINE = MergeTree()\nALTER TABLE t ADD COLUMN IF NOT EXISTS c Nullable(UInt32) AFTER b;\nALTER TABLE t ADD COLUMN IF NOT EXISTS d UInt8 DEFAULT 0;\n";
        assert_eq!(
            columns(sql, "t"),
            vec![
                ("a".to_string(), false),
                ("b".to_string(), true),
                ("c".to_string(), false),
                ("d".to_string(), true),
            ]
        );
    }

    /// A ClickHouse that refuses connections. Validation off, so
    /// `insert` asks nothing of the server and the failure comes where it
    /// does once a table's schema is cached: at `write` or `end`, after
    /// the rows have been read from the batch.
    fn unreachable_client() -> clickhouse::Client {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        clickhouse::Client::default()
            .with_url(format!("http://127.0.0.1:{port}"))
            .with_validation(false)
    }

    #[tokio::test]
    async fn a_failed_insert_keeps_every_entry_for_the_next_tick() {
        let ch = Some(unreachable_client());
        for log_type in [
            LogType::Access,
            LogType::App,
            LogType::Audit,
            LogType::Gateway,
            LogType::Mcp,
        ] {
            let mut batch: Vec<AuditEntry> = (0..3)
                .map(|i| {
                    #[allow(deprecated)]
                    let mut e = AuditEntry::new(format!("test.{i}"));
                    e.log_type = log_type;
                    e.request_body = Some(r#"{"q":1}"#.into());
                    e.response_body = Some(r#"{"a":1}"#.into());
                    e
                })
                .collect();
            let ids: Vec<String> = batch.iter().map(|e| e.id.clone()).collect();
            flush_to_clickhouse(&ch, log_type.index_id(), &mut batch).await;
            assert_eq!(
                batch.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
                ids,
                "{log_type:?}"
            );
            // Bodies are sanitised into the row, never taken out of the
            // retained entry: the retry writes the same row.
            assert!(
                batch
                    .iter()
                    .all(|e| e.request_body.is_some() && e.response_body.is_some()),
                "{log_type:?}"
            );
        }
    }
}
