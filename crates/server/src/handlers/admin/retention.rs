//! ClickHouse retention plumbing for admin-driven
//! `data.retention_days_*` settings: applies per-table TTL,
//! reconciles on boot, and warns when the captured-body retention
//! exceeds the blob-store bucket lifecycle (i.e. the body has been
//! GC'd but its audit row still references it).

use std::collections::HashMap;

use crate::app::AppState;

/// Map of `data.retention_days_*` setting keys to their ClickHouse table name.
const RETENTION_TABLES: &[(&str, &str)] = &[
    ("data.retention_days_audit", "audit_logs"),
    ("data.retention_days_gateway", "gateway_logs"),
    ("data.retention_days_mcp", "mcp_logs"),
    ("data.retention_days_access", "access_logs"),
    ("data.retention_days_app", "app_logs"),
];

/// Maximum retention window we will accept, in days. Anything bigger is
/// almost certainly a typo and risks accidentally turning the window off.
pub(super) const MAX_RETENTION_DAYS: i64 = 36500; // 100 years

/// Whitelist of valid ClickHouse log table identifiers. Used as a
/// belt-and-braces guard so we never inject anything we don't already
/// know about into a `ALTER TABLE ...` statement, even though all call
/// sites today only pass &'static str literals from RETENTION_TABLES.
const VALID_LOG_TABLES: &[&str] = &[
    "audit_logs",
    "gateway_logs",
    "mcp_logs",
    "access_logs",
    "app_logs",
];

/// Issue a single `ALTER TABLE ... MODIFY TTL` against ClickHouse.
/// Validates `table` against an explicit whitelist and clamps `days`
/// into a sane range. Returns `false` if the call was skipped or failed.
async fn apply_single_ttl(ch: &clickhouse::Client, table: &str, days: i64) -> bool {
    if !VALID_LOG_TABLES.contains(&table) {
        tracing::error!(table, "refusing TTL update for unknown table");
        return false;
    }
    if !(1..=MAX_RETENTION_DAYS).contains(&days) {
        tracing::error!(table, days, "refusing TTL update: days out of range");
        return false;
    }
    let sql =
        format!("ALTER TABLE {table} MODIFY TTL toDateTime(created_at) + INTERVAL {days} DAY");
    match ch.query(&sql).execute().await {
        Ok(()) => {
            tracing::info!(table, days, "ClickHouse TTL updated");
            true
        }
        Err(e) => {
            tracing::error!(table, days, "Failed to update ClickHouse TTL: {e}");
            false
        }
    }
}

/// Issue `ALTER TABLE ... MODIFY TTL` for every retention setting included in
/// the update. Failures are logged but not surfaced — the setting is already
/// persisted, and ClickHouse may be temporarily unavailable.
pub(super) async fn apply_clickhouse_ttls(
    state: &AppState,
    settings: &HashMap<String, serde_json::Value>,
) {
    let Some(ch) = state.clickhouse.as_ref() else {
        return;
    };
    for (key, table) in RETENTION_TABLES {
        let Some(value) = settings.get(*key) else {
            continue;
        };
        let Some(days) = value.as_i64() else { continue };
        if days <= 0 {
            continue;
        }
        apply_single_ttl(ch, table, days).await;
    }
    // Body-column TTL is administered through a separate setting that
    // shortens the lifetime of the heavy payload columns without
    // touching the row TTL. Only apply on the PATCH path when the
    // operator actually included it in the request, so unrelated edits
    // (e.g. a single bump to access-log retention) don't churn the
    // body-column metadata.
    if let Some(value) = settings.get("audit.body_retention_days")
        && let Some(days) = value.as_i64()
    {
        apply_body_column_ttls(ch, days).await;
        // Re-check the bucket lifecycle horizon — if the operator
        // just raised retention above the bucket's GC, surface the
        // mismatch in their PATCH-response log line rather than
        // waiting for an auditor to hit a 404.
        check_body_retention_vs_lifecycle(state).await;
    }
}

/// Apply the blob-store bucket lifecycle rule for any settings PATCH
/// that touched `audit.body_s3_lifecycle_days`. Mirrors the
/// `apply_clickhouse_ttls` pattern: persisted-then-applied, failures
/// log but don't surface (the bucket might be temporarily
/// unreachable; the next boot's `reconcile_blob_lifecycle` will
/// retry). InlineStore deployments no-op via the trait default.
pub(super) async fn apply_blob_lifecycle(
    state: &AppState,
    settings: &HashMap<String, serde_json::Value>,
) {
    let Some(value) = settings.get("audit.body_s3_lifecycle_days") else {
        return;
    };
    let Some(days) = value.as_i64() else { return };
    if !(1..=MAX_RETENTION_DAYS).contains(&days) {
        tracing::error!(days, "refusing blob lifecycle update: days out of range");
        return;
    }
    if !state.blob_store.can_offload() {
        // Setting persists for when offload is later configured, but
        // there's no bucket to PUT against today.
        return;
    }
    match state.blob_store.set_lifecycle_days(days as u32).await {
        Ok(()) => {
            tracing::info!(days, "blob-store bucket lifecycle updated");
            // Same cross-check the body-column TTL path runs — the
            // operator may have just resolved or freshly broken the
            // retention-vs-lifecycle invariant.
            check_body_retention_vs_lifecycle(state).await;
        }
        Err(e) => {
            tracing::error!(
                days,
                error = %e,
                "Failed to update blob-store bucket lifecycle — \
                 next boot's reconcile will retry"
            );
        }
    }
}

/// Push the current persisted `audit.body_s3_lifecycle_days` to the
/// bucket. Boot-time companion to [`apply_blob_lifecycle`] — without
/// this, a fresh deployment would never install the lifecycle rule
/// (the operator hasn't touched the setting, so no PATCH fires).
/// No-op when blob-store offload isn't configured.
pub async fn reconcile_blob_lifecycle(state: &AppState) {
    if !state.blob_store.can_offload() {
        return;
    }
    let days = state.dynamic_config.audit_body_s3_lifecycle_days().await;
    if !(1..=MAX_RETENTION_DAYS).contains(&days) {
        tracing::error!(days, "blob lifecycle reconcile: days out of range");
        return;
    }
    match state.blob_store.set_lifecycle_days(days as u32).await {
        Ok(()) => tracing::info!(days, "blob-store bucket lifecycle reconciled at boot"),
        Err(e) => tracing::warn!(
            days,
            error = %e,
            "blob-store bucket lifecycle reconcile failed at boot — \
             operator can re-trigger via PATCH /api/admin/settings"
        ),
    }
}

/// Apply current persisted retention settings to all ClickHouse log tables.
/// Called once at server startup so settings survive restarts. Silently no-ops
/// if ClickHouse is not configured.
pub async fn reconcile_clickhouse_ttls(state: &AppState) {
    let Some(ch) = state.clickhouse.as_ref() else {
        return;
    };
    let dc = &state.dynamic_config;
    let pairs: [(i64, &str); 5] = [
        (dc.data_retention_days_audit().await, "audit_logs"),
        (dc.data_retention_days_gateway().await, "gateway_logs"),
        (dc.data_retention_days_mcp().await, "mcp_logs"),
        (dc.data_retention_days_access().await, "access_logs"),
        (dc.data_retention_days_app().await, "app_logs"),
    ];
    for (days, table) in pairs {
        if days <= 0 {
            continue;
        }
        apply_single_ttl(ch, table, days).await;
    }
    apply_body_column_ttls(ch, dc.audit_body_retention_days().await).await;
}

/// Detect mismatches between `audit.body_retention_days` (the CH
/// column TTL) and `audit.body_s3_lifecycle_days` (the bucket
/// lifecycle rule the app pushes to S3). When the CH retention is
/// set ABOVE the bucket horizon, every `s3://bucket/key` URL stored
/// in CH between (bucket_days, ch_days] will 404 on read — the
/// audit row outlives its referenced object. Silent in production
/// until an auditor hits a "body fetch failed" 502.
///
/// Called at startup and after PATCH /api/admin/settings. Fail-OPEN:
/// blob-store backends that can't report a lifecycle (`InlineStore`,
/// or an S3 backend without a configured rule) skip the check
/// cleanly. Transport errors are logged but don't fail startup —
/// we WANT the server up even if the lifecycle query is flaky.
pub async fn check_body_retention_vs_lifecycle(state: &AppState) {
    if !state.blob_store.can_offload() {
        return;
    }
    let configured_days = state.dynamic_config.audit_body_retention_days().await;
    if configured_days <= 0 {
        return;
    }
    let bucket_days = match state.blob_store.lifecycle_days().await {
        Ok(Some(days)) => days,
        Ok(None) => {
            // No rule configured on the bucket — operator has to
            // own the cleanup themselves. Log info so the audit
            // posture is visible but don't warn.
            tracing::info!(
                "blob-store bucket has no lifecycle rule covering `bodies/`; \
                 audit.body_retention_days={configured_days} relies on operator-driven cleanup"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "blob-store lifecycle query failed; skipping retention cross-check"
            );
            return;
        }
    };
    if (configured_days as u32) > bucket_days {
        tracing::warn!(
            audit_body_retention_days = configured_days,
            bucket_lifecycle_days = bucket_days,
            "audit.body_retention_days is ABOVE the bucket lifecycle horizon — \
             offloaded body URLs between {bucket_days}d and {configured_days}d will 404 on \
             read; raise audit.body_s3_lifecycle_days OR lower audit.body_retention_days"
        );
        metrics::counter!("audit_body_retention_above_bucket_lifecycle_total").increment(1);
    } else {
        tracing::info!(
            audit_body_retention_days = configured_days,
            bucket_lifecycle_days = bucket_days,
            "audit.body_retention_days vs bucket lifecycle check passed"
        );
    }
}

/// `(table, column)` pairs that hold captured request/response bodies and
/// therefore deserve their own (shorter) TTL — auditors typically need
/// recent replay, but holding terabytes of week-old prompts wastes
/// storage. The byte-count + status columns are tiny and stay on the
/// row's normal TTL.
const BODY_COLUMNS: &[(&str, &str)] = &[
    ("gateway_logs", "request_body"),
    ("gateway_logs", "response_body"),
    ("mcp_logs", "tool_arguments"),
    ("mcp_logs", "tool_result"),
];

/// Issue per-column `ALTER TABLE ... MODIFY COLUMN <col> TTL ...` against
/// each body column. Column-level TTL is independent of the row TTL: when
/// it expires, ClickHouse merges the column to its default (NULL for our
/// Nullable(String) columns) while leaving the row in place until the
/// table-level TTL kicks in. Failures are logged but not surfaced —
/// startup races and intermittent CH availability shouldn't prevent the
/// server from coming up.
async fn apply_body_column_ttls(ch: &clickhouse::Client, days: i64) {
    if !(1..=MAX_RETENTION_DAYS).contains(&days) {
        tracing::error!(days, "refusing body TTL update: days out of range");
        return;
    }
    for (table, column) in BODY_COLUMNS {
        if !VALID_LOG_TABLES.contains(table) {
            // Guard against future drift even though the const is
            // hand-curated — if someone adds a body column on an
            // unaudited table, refuse to ALTER it rather than letting
            // a typo through.
            tracing::error!(table, "body column points at non-whitelisted table");
            continue;
        }
        let sql = format!(
            "ALTER TABLE {table} MODIFY COLUMN {column} \
             TTL toDateTime(created_at) + INTERVAL {days} DAY"
        );
        match ch.query(&sql).execute().await {
            Ok(()) => tracing::info!(table, column, days, "ClickHouse body-column TTL updated"),
            Err(e) => {
                tracing::error!(table, column, days, "Failed to update body-column TTL: {e}")
            }
        }
    }
}
