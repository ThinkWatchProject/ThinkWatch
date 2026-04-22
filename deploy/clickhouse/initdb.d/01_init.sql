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
    model_id         LowCardinality(Nullable(String)),
    provider         LowCardinality(Nullable(String)),
    input_tokens     Nullable(Int64)   CODEC(Delta, ZSTD(1)),
    output_tokens    Nullable(Int64)   CODEC(Delta, ZSTD(1)),
    cost_usd         Nullable(Float64) CODEC(Delta, ZSTD(1)),
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
    INDEX idx_model      model_id    TYPE set(200)     GRANULARITY 2,
    INDEX idx_provider   provider    TYPE set(50)      GRANULARITY 2,
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

ALTER TABLE gateway_logs ADD PROJECTION IF NOT EXISTS proj_by_cost (
    SELECT * ORDER BY cost_usd, created_at
);

ALTER TABLE gateway_logs ADD PROJECTION IF NOT EXISTS proj_by_latency (
    SELECT * ORDER BY latency_ms, created_at
);

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

CREATE TABLE IF NOT EXISTS platform_logs (
    id               String,
    user_id          LowCardinality(Nullable(String)),
    user_email       LowCardinality(Nullable(String)),
    action           LowCardinality(String),
    resource         LowCardinality(Nullable(String)),
    resource_id      Nullable(String),
    detail           Nullable(String) CODEC(ZSTD(3)),
    ip_address       Nullable(String),
    user_agent       Nullable(String) CODEC(ZSTD(3)),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_user_id    user_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_user_email user_email TYPE bloom_filter GRANULARITY 4,
    INDEX idx_action     action     TYPE set(100)     GRANULARITY 2,
    INDEX idx_resource   resource   TYPE set(100)     GRANULARITY 2,
    INDEX idx_ip         ip_address TYPE bloom_filter GRANULARITY 4,
    INDEX idx_search     id         TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

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
CREATE TABLE IF NOT EXISTS provider_health_5m (
    bucket_5m        DateTime CODEC(DoubleDelta, ZSTD(1)),
    provider         LowCardinality(String),
    total_requests   UInt64,
    error_requests   UInt64,
    sum_latency_ms   Int64,
    requests_latency UInt64
) ENGINE = SummingMergeTree()
PARTITION BY toYYYYMM(bucket_5m)
ORDER BY (provider, bucket_5m);

CREATE MATERIALIZED VIEW IF NOT EXISTS provider_health_5m_mv
TO provider_health_5m AS
SELECT
    toStartOfFiveMinutes(created_at)                 AS bucket_5m,
    provider                                         AS provider,
    toUInt64(1)                                      AS total_requests,
    toUInt64(if(status_code >= 400, 1, 0))           AS error_requests,
    ifNull(latency_ms, 0)                            AS sum_latency_ms,
    toUInt64(if(latency_ms IS NOT NULL, 1, 0))       AS requests_latency
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
    request_count  UInt64,
    input_tokens   Int64,
    output_tokens  Int64,
    cost_usd       Float64
) ENGINE = SummingMergeTree()
PARTITION BY toYYYYMM(hour)
ORDER BY (hour, model_id, provider, user_id, api_key_id)
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
    toUInt64(1) AS request_count,
    ifNull(input_tokens, 0) AS input_tokens,
    ifNull(output_tokens, 0) AS output_tokens,
    ifNull(cost_usd, 0) AS cost_usd
FROM gateway_logs;
