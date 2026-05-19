-- ClickHouse base schema for ThinkWatch log storage.
--
-- Mounted by docker-entrypoint on first boot, and also embedded into
-- the binary via include_str! and re-applied on startup when the data
-- dir already exists but tables are missing (see crates/common/src/audit.rs).
--
-- Encoding strategy:
--   LowCardinality   → categorical columns (action, resource, provider …)
--   DoubleDelta+ZSTD  → monotonic timestamps
--   Delta+ZSTD        → numeric metrics (tokens, latency, cost)
--   ZSTD(3)           → large variable-length text (detail, user_agent, error)
--
-- Skip indices (per-granule):
--   bloom_filter → high-cardinality ID lookups (user_id, api_key_id …)
--   set(N)       → low-cardinality exact match (action, provider, status …)
--   tokenbf_v1   → ILIKE '%keyword%' substring search. Strings only:
--                  for Nullable columns, the index expression is wrapped
--                  in ifNull(col, '') and queries must reference the
--                  same expression to benefit from the skip.
--
-- Partitioning: monthly. ttl_only_drop_parts = 1 → whole-part TTL drops.

CREATE DATABASE IF NOT EXISTS think_watch;
USE think_watch;

-- ---------------------------------------------------------------------------
-- Log tables
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS app_logs (
    id               String,
    level            LowCardinality(String),
    target           LowCardinality(String),
    message          String CODEC(ZSTD(3)),
    fields           Nullable(String) CODEC(ZSTD(3)),
    span             Nullable(String) CODEC(ZSTD(3)),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_level   level  TYPE set(10)    GRANULARITY 2,
    INDEX idx_target  target TYPE set(200)   GRANULARITY 2,
    INDEX idx_msg     message TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 30 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

CREATE TABLE IF NOT EXISTS access_logs (
    id               String,
    method           LowCardinality(String),
    path             String,
    status_code      UInt16,
    latency_ms       Int64 CODEC(Delta(8), ZSTD(1)),
    port             UInt16,
    user_id          LowCardinality(Nullable(String)),
    -- Snapshot of users.email at request time so audit queries remain
    -- correct after the user row is hard-deleted. Populated by the
    -- auth middleware via AccessLogUserSlot.
    user_email       LowCardinality(Nullable(String)),
    ip_address       Nullable(String),
    user_agent       Nullable(String) CODEC(ZSTD(3)),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_method    method      TYPE set(10)       GRANULARITY 2,
    INDEX idx_status    status_code TYPE set(100)      GRANULARITY 2,
    INDEX idx_port      port        TYPE set(4)        GRANULARITY 2,
    INDEX idx_user_id   user_id     TYPE bloom_filter  GRANULARITY 4,
    INDEX idx_user_email user_email TYPE bloom_filter  GRANULARITY 4,
    INDEX idx_path      path        TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 30 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

ALTER TABLE access_logs ADD COLUMN IF NOT EXISTS user_email LowCardinality(Nullable(String)) AFTER user_id;
ALTER TABLE access_logs ADD INDEX IF NOT EXISTS idx_user_email user_email TYPE bloom_filter GRANULARITY 4;

CREATE TABLE IF NOT EXISTS audit_logs (
    id               String,
    user_id          LowCardinality(Nullable(String)),
    user_email       LowCardinality(Nullable(String)),
    api_key_id       LowCardinality(Nullable(String)),
    -- Stable identity that survives api-key rotation. Every row
    -- emitted on behalf of any generation in a rotation chain
    -- carries the same `api_key_lineage_id`, so per-key analytics
    -- can roll up usage across generations without recursing on PG.
    api_key_lineage_id LowCardinality(Nullable(String)),
    action           LowCardinality(String),
    resource         LowCardinality(Nullable(String)),
    resource_id      Nullable(String),
    detail           Nullable(String) CODEC(ZSTD(3)),
    ip_address       Nullable(String),
    user_agent       Nullable(String) CODEC(ZSTD(3)),
    -- trace_id correlates this event with the AI gateway / MCP request
    -- that produced it. Set by the originating handler's middleware,
    -- NULL for standalone admin actions.
    trace_id         Nullable(String),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_user_id    user_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_user_email user_email TYPE bloom_filter GRANULARITY 4,
    INDEX idx_api_key    api_key_id TYPE bloom_filter GRANULARITY 4,
    INDEX idx_action     action     TYPE set(100)     GRANULARITY 2,
    INDEX idx_resource   resource   TYPE set(100)     GRANULARITY 2,
    INDEX idx_ip         ip_address TYPE bloom_filter GRANULARITY 4,
    INDEX idx_trace      trace_id   TYPE bloom_filter GRANULARITY 4,
    INDEX idx_search     id         TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

CREATE TABLE IF NOT EXISTS gateway_logs (
    id               String,
    user_id          LowCardinality(Nullable(String)),
    -- Snapshot of users.email at request time. See access_logs notes.
    user_email       LowCardinality(Nullable(String)),
    api_key_id       LowCardinality(Nullable(String)),
    -- Stable identity across api-key rotation. See `audit_logs` above.
    api_key_lineage_id LowCardinality(Nullable(String)),
    model_id         LowCardinality(Nullable(String)),
    provider         LowCardinality(Nullable(String)),
    -- Upstream model name actually sent to the provider (e.g.
    -- "gpt-4-turbo-2024-04-09"). Distinct from `model_id`, which is
    -- the abstract id the client requested. NULL on synthetic
    -- prefix-fallback routes that pass the client's model through.
    upstream_model   LowCardinality(Nullable(String)),
    input_tokens     Nullable(Int64)   CODEC(Delta, ZSTD(1)),
    output_tokens    Nullable(Int64)   CODEC(Delta, ZSTD(1)),
    -- cost_usd stored as Decimal(18, 10): precision 18 total digits,
    -- scale 10 fractional. Float64 would accumulate rounding errors
    -- under sum() — Decimal keeps billing aggregates exact. Scale 10
    -- covers sub-cent token pricing (cheapest commercial model is
    -- ~$1.5e-7 / token). See `common::cost_decimal` for the Rust-side
    -- encode/decode helpers — the clickhouse crate has no Decimal
    -- type of its own, so the wire is the column's raw i64 / i128.
    cost_usd         Nullable(Decimal(18, 10)) CODEC(ZSTD(1)),
    latency_ms       Nullable(Int64)   CODEC(Delta, ZSTD(1)),
    status_code      Nullable(Int64),
    ip_address       Nullable(String),
    user_agent       Nullable(String) CODEC(ZSTD(3)),
    detail           Nullable(String) CODEC(ZSTD(3)),
    -- trace_id: shared ID across gateway / mcp / audit rows for one request.
    trace_id         Nullable(String),
    -- session_id: optional grouping for multi-turn conversations.
    -- Set by the client via the `x-session-id` header (or the
    -- equivalent request body field once SDKs surface it). All turns
    -- of one chat carry the same id, so the trace UI can collapse a
    -- conversation into one expandable row instead of a dozen
    -- siblings (FEAT-10).
    session_id       LowCardinality(Nullable(String)),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_user_id    user_id     TYPE bloom_filter GRANULARITY 4,
    INDEX idx_user_email user_email  TYPE bloom_filter GRANULARITY 4,
    INDEX idx_api_key    api_key_id  TYPE bloom_filter GRANULARITY 4,
    INDEX idx_model      model_id        TYPE set(200)     GRANULARITY 2,
    INDEX idx_provider   provider        TYPE set(50)      GRANULARITY 2,
    INDEX idx_upstream   upstream_model  TYPE set(200)     GRANULARITY 2,
    INDEX idx_status     status_code TYPE set(50)      GRANULARITY 2,
    INDEX idx_trace      trace_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_session    session_id  TYPE bloom_filter GRANULARITY 4,
    INDEX idx_search     id          TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4,
    INDEX idx_detail     ifNull(detail, '') TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

ALTER TABLE gateway_logs ADD COLUMN IF NOT EXISTS user_email LowCardinality(Nullable(String)) AFTER user_id;
ALTER TABLE gateway_logs ADD INDEX IF NOT EXISTS idx_user_email user_email TYPE bloom_filter GRANULARITY 4;
ALTER TABLE gateway_logs ADD COLUMN IF NOT EXISTS session_id LowCardinality(Nullable(String)) AFTER trace_id;
ALTER TABLE gateway_logs ADD INDEX IF NOT EXISTS idx_session session_id TYPE bloom_filter GRANULARITY 4;
ALTER TABLE gateway_logs ADD COLUMN IF NOT EXISTS upstream_model LowCardinality(Nullable(String)) AFTER provider;
ALTER TABLE gateway_logs ADD INDEX IF NOT EXISTS idx_upstream upstream_model TYPE set(200) GRANULARITY 2;

ALTER TABLE gateway_logs ADD PROJECTION IF NOT EXISTS proj_by_cost (
    SELECT * ORDER BY cost_usd, created_at
);

ALTER TABLE gateway_logs ADD PROJECTION IF NOT EXISTS proj_by_latency (
    SELECT * ORDER BY latency_ms, created_at
);

-- Full request/response body capture for enterprise audit.
-- ZSTD(6) trades a bit more CPU on insert for ~3-4x compression on
-- JSON payloads; auditors typically search bodies infrequently and
-- compression dominates the cold-storage footprint. Sit AFTER
-- session_id so the existing column order is preserved and new
-- deployments + upgraded ones converge on the same shape.
ALTER TABLE gateway_logs ADD COLUMN IF NOT EXISTS request_body  Nullable(String) CODEC(ZSTD(6)) AFTER session_id;
ALTER TABLE gateway_logs ADD COLUMN IF NOT EXISTS response_body Nullable(String) CODEC(ZSTD(6)) AFTER request_body;
ALTER TABLE gateway_logs ADD COLUMN IF NOT EXISTS request_body_bytes  Nullable(UInt32) AFTER response_body;
ALTER TABLE gateway_logs ADD COLUMN IF NOT EXISTS response_body_bytes Nullable(UInt32) AFTER request_body_bytes;
-- 'captured' | 'truncated' | 'disabled' | 'from_cache' | 'error'
ALTER TABLE gateway_logs ADD COLUMN IF NOT EXISTS body_capture_status LowCardinality(Nullable(String)) AFTER response_body_bytes;

-- Body columns get a SHORTER TTL than the row-level retention. The
-- Rust side (`apply_body_column_ttls` in handlers/admin.rs) re-issues
-- these at startup against the operator-configurable
-- `audit.body_retention_days` setting (default 30); this seed-default
-- exists so a CH bootstrap that happens before the server ever runs
-- still has the right shape. When the column TTL fires, the value is
-- reset to NULL while the row stays around for the full table TTL —
-- so metadata queries remain whole even after bodies have aged out.
ALTER TABLE gateway_logs MODIFY COLUMN request_body  TTL toDateTime(created_at) + INTERVAL 30 DAY;
ALTER TABLE gateway_logs MODIFY COLUMN response_body TTL toDateTime(created_at) + INTERVAL 30 DAY;

CREATE TABLE IF NOT EXISTS mcp_logs (
    id               String,
    user_id          LowCardinality(Nullable(String)),
    -- Snapshot of users.email at request time. See access_logs notes.
    user_email       LowCardinality(Nullable(String)),
    server_id        LowCardinality(Nullable(String)),
    server_name      LowCardinality(Nullable(String)),
    tool_name        LowCardinality(Nullable(String)),
    duration_ms      Nullable(Int64) CODEC(Delta, ZSTD(1)),
    status           LowCardinality(Nullable(String)),
    error_message    Nullable(String) CODEC(ZSTD(3)),
    ip_address       Nullable(String),
    detail           Nullable(String) CODEC(ZSTD(3)),
    trace_id         Nullable(String),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_user_id    user_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_user_email user_email TYPE bloom_filter GRANULARITY 4,
    INDEX idx_server_id  server_id  TYPE bloom_filter GRANULARITY 4,
    INDEX idx_tool       tool_name  TYPE set(200)     GRANULARITY 2,
    INDEX idx_status     status     TYPE set(20)      GRANULARITY 2,
    INDEX idx_trace      trace_id   TYPE bloom_filter GRANULARITY 4,
    INDEX idx_search     id         TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4,
    INDEX idx_error_msg  ifNull(error_message, '') TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4,
    INDEX idx_detail     ifNull(detail, '')        TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

ALTER TABLE mcp_logs ADD COLUMN IF NOT EXISTS user_email LowCardinality(Nullable(String)) AFTER user_id;
ALTER TABLE mcp_logs ADD INDEX IF NOT EXISTS idx_user_email user_email TYPE bloom_filter GRANULARITY 4;

ALTER TABLE mcp_logs ADD PROJECTION IF NOT EXISTS proj_by_duration (
    SELECT * ORDER BY duration_ms, created_at
);

-- Tool call body capture for audit. `tool_arguments` was previously
-- only embedded in the detail JSON (with secret-shaped keys redacted
-- by sanitize_detail); promote both arguments and the upstream result
-- to first-class columns so audit queries don't have to JSON-parse on
-- every row. Same ZSTD(6) trade-off as gateway_logs.
ALTER TABLE mcp_logs ADD COLUMN IF NOT EXISTS tool_arguments     Nullable(String) CODEC(ZSTD(6)) AFTER detail;
ALTER TABLE mcp_logs ADD COLUMN IF NOT EXISTS tool_result        Nullable(String) CODEC(ZSTD(6)) AFTER tool_arguments;
ALTER TABLE mcp_logs ADD COLUMN IF NOT EXISTS arguments_bytes    Nullable(UInt32) AFTER tool_result;
ALTER TABLE mcp_logs ADD COLUMN IF NOT EXISTS result_bytes       Nullable(UInt32) AFTER arguments_bytes;
ALTER TABLE mcp_logs ADD COLUMN IF NOT EXISTS body_capture_status LowCardinality(Nullable(String)) AFTER result_bytes;

-- Same body-column TTL story as gateway_logs above; see comment there.
ALTER TABLE mcp_logs MODIFY COLUMN tool_arguments TTL toDateTime(created_at) + INTERVAL 30 DAY;
ALTER TABLE mcp_logs MODIFY COLUMN tool_result    TTL toDateTime(created_at) + INTERVAL 30 DAY;

-- platform_logs used to live here as a separate table for management
-- operations. Its schema was a strict subset of audit_logs (no
-- api_key_id, no trace_id), so the split only fragmented the audit
-- explorer and broke trace correlation for admin actions. Everything
-- now flows into audit_logs; LogType::Platform was removed.
DROP TABLE IF EXISTS platform_logs;

-- ---------------------------------------------------------------------------
-- Materialized views and rollups
-- ---------------------------------------------------------------------------

-- Pre-aggregates mcp_logs by server_id so GET /api/mcp/servers doesn't
-- have to scan up to 90 days of log rows per request. SummingMergeTree:
-- the MV pushes 1-per-row, CH merges on read via sum() GROUP BY.
CREATE TABLE IF NOT EXISTS mcp_server_call_counts (
    server_id LowCardinality(String),
    calls     UInt64
) ENGINE = SummingMergeTree()
ORDER BY server_id;

CREATE MATERIALIZED VIEW IF NOT EXISTS mcp_server_call_counts_mv
TO mcp_server_call_counts AS
SELECT server_id, toUInt64(1) AS calls
FROM mcp_logs
WHERE server_id IS NOT NULL;

-- 5-minute rollup of gateway_logs by provider for the dashboard
-- "provider health" widget. Latency is stored as sum + count so callers
-- can compute a weighted average over any time window with one GROUP BY.
--
-- `throttled_requests` (429) is tracked separately from `error_requests`
-- (other 4xx / 5xx) — rate-limiting means the upstream is *responsive
-- and refusing*, not *down*, and conflating the two paints a healthy
-- provider red whenever the caller exceeds their quota. Surfaced
-- independently on the dashboard so operators can tell "upstream broke"
-- from "upstream is throttling us".
CREATE TABLE IF NOT EXISTS provider_health_5m (
    bucket_5m          DateTime CODEC(DoubleDelta, ZSTD(1)),
    provider           LowCardinality(String),
    total_requests     UInt64,
    error_requests     UInt64,
    throttled_requests UInt64,
    sum_latency_ms     Int64,
    requests_latency   UInt64
) ENGINE = SummingMergeTree()
PARTITION BY toYYYYMM(bucket_5m)
ORDER BY (provider, bucket_5m);

-- Migration for existing deployments — the column is appended after
-- error_requests so a fresh CREATE and an upgraded table converge.
ALTER TABLE provider_health_5m ADD COLUMN IF NOT EXISTS throttled_requests UInt64 AFTER error_requests;

-- Replace the MV so new gateway_logs rows route into the right bucket.
-- Existing aggregates already in provider_health_5m keep their old
-- error_requests counts (which lumped 429 in), but the dashboard's
-- 15-minute window washes those out within one window.
DROP VIEW IF EXISTS provider_health_5m_mv;
CREATE MATERIALIZED VIEW IF NOT EXISTS provider_health_5m_mv
TO provider_health_5m AS
SELECT
    toStartOfFiveMinutes(created_at)                            AS bucket_5m,
    provider                                                    AS provider,
    toUInt64(1)                                                 AS total_requests,
    toUInt64(if(status_code >= 400 AND status_code != 429, 1, 0)) AS error_requests,
    toUInt64(if(status_code = 429, 1, 0))                       AS throttled_requests,
    ifNull(latency_ms, 0)                                       AS sum_latency_ms,
    toUInt64(if(latency_ms IS NOT NULL, 1, 0))                  AS requests_latency
FROM gateway_logs
WHERE provider IS NOT NULL;

-- Hourly cost aggregation for the costs analytics page (group-by:
-- model / provider / user / api_key, time range 24h–MTD).
CREATE TABLE IF NOT EXISTS cost_rollup_hourly (
    hour           DateTime CODEC(DoubleDelta, ZSTD(1)),
    model_id       LowCardinality(String),
    provider       LowCardinality(Nullable(String)),
    user_id        LowCardinality(Nullable(String)),
    api_key_id     LowCardinality(Nullable(String)),
    -- Lineage identity matching `gateway_logs.api_key_lineage_id`
    -- so the costs page can pivot on a stable per-key identity
    -- across rotation generations.
    api_key_lineage_id LowCardinality(Nullable(String)),
    request_count  UInt64,
    input_tokens   Int64,
    output_tokens  Int64,
    -- Decimal(38, 10) for the rollup — wider than the source column
    -- so repeated sum() under SummingMergeTree can't overflow across
    -- an extremely active hour. Scale matches the base column.
    cost_usd       Decimal(38, 10)
) ENGINE = SummingMergeTree()
PARTITION BY toYYYYMM(hour)
ORDER BY (hour, model_id, provider, user_id, api_key_id, api_key_lineage_id)
-- provider/user_id/api_key_id are Nullable; CH 26.3 rejects them in
-- ORDER BY unless this is opted in per-table.
SETTINGS allow_nullable_key = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS cost_rollup_hourly_mv
TO cost_rollup_hourly AS
SELECT
    toStartOfHour(created_at) AS hour,
    model_id,
    provider,
    user_id,
    api_key_id,
    api_key_lineage_id,
    toUInt64(1) AS request_count,
    ifNull(input_tokens, 0) AS input_tokens,
    ifNull(output_tokens, 0) AS output_tokens,
    ifNull(cost_usd, 0) AS cost_usd
FROM gateway_logs;
