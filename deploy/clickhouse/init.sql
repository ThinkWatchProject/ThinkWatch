-- ClickHouse schema for ThinkWatch log storage
-- Optimised for append-heavy, time-range + filter query workload.
--
-- Encoding strategy:
--   LowCardinality   → categorical columns (action, resource, provider …)
--   DoubleDelta+ZSTD  → monotonic timestamps
--   Delta+ZSTD        → numeric metrics (tokens, latency, cost)
--   ZSTD(3)           → large variable-length text (detail, user_agent, error)
--
-- Skip indices (per-granule):
--   bloom_filter → high-cardinality ID lookups  (user_id, api_key_id …)
--   set(100)     → low-cardinality exact match  (action, provider, status …)
--   tokenbf_v1   → ILIKE '%keyword%' substring search
--
-- Projections:
--   Alternative ORDER BY for secondary sort queries (cost, latency, duration)
--
-- Partitioning: monthly.  ttl_only_drop_parts = 1 → whole-part TTL drops.

CREATE DATABASE IF NOT EXISTS think_watch;
USE think_watch;

-- ================================================================
--  app_logs  (runtime tracing: info / warn / error / debug)
-- ================================================================
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

-- ================================================================
--  access_logs  (HTTP access log for gateway + console)
-- ================================================================
CREATE TABLE IF NOT EXISTS access_logs (
    id               String,
    method           LowCardinality(String),
    path             String,
    status_code      UInt16,
    latency_ms       Int64 CODEC(Delta(8), ZSTD(1)),
    port             UInt16,
    user_id          LowCardinality(Nullable(String)),
    ip_address       Nullable(String),
    user_agent       Nullable(String) CODEC(ZSTD(3)),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_method    method      TYPE set(10)       GRANULARITY 2,
    INDEX idx_status    status_code TYPE set(100)      GRANULARITY 2,
    INDEX idx_port      port        TYPE set(4)        GRANULARITY 2,
    INDEX idx_user_id   user_id     TYPE bloom_filter  GRANULARITY 4,
    INDEX idx_path      path        TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 30 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

-- ================================================================
--  audit_logs
-- ================================================================
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
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    -- Skip indices: exact-match filters
    INDEX idx_user_id   user_id   TYPE bloom_filter GRANULARITY 4,
    INDEX idx_api_key   api_key_id TYPE bloom_filter GRANULARITY 4,
    INDEX idx_action    action     TYPE set(100)     GRANULARITY 2,
    INDEX idx_resource  resource   TYPE set(100)     GRANULARITY 2,
    INDEX idx_ip        ip_address TYPE bloom_filter GRANULARITY 4,

    -- Skip index: substring search on id column (String type, not LowCardinality)
    INDEX idx_search id TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

-- ================================================================
--  gateway_logs
-- ================================================================
CREATE TABLE IF NOT EXISTS gateway_logs (
    id               String,
    user_id          LowCardinality(Nullable(String)),
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
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    -- Skip indices
    INDEX idx_user_id    user_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_api_key    api_key_id TYPE bloom_filter GRANULARITY 4,
    INDEX idx_model      model_id   TYPE set(200)     GRANULARITY 2,
    INDEX idx_provider   provider   TYPE set(50)      GRANULARITY 2,
    INDEX idx_status     status_code TYPE set(50)     GRANULARITY 2,
    INDEX idx_search     id         TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

-- Projection: ORDER BY cost_usd (for "top cost" queries)
ALTER TABLE gateway_logs ADD PROJECTION IF NOT EXISTS proj_by_cost (
    SELECT * ORDER BY cost_usd, created_at
);

-- Projection: ORDER BY latency_ms (for "slowest" queries)
ALTER TABLE gateway_logs ADD PROJECTION IF NOT EXISTS proj_by_latency (
    SELECT * ORDER BY latency_ms, created_at
);

-- ================================================================
--  mcp_logs
-- ================================================================
CREATE TABLE IF NOT EXISTS mcp_logs (
    id               String,
    user_id          LowCardinality(Nullable(String)),
    server_id        LowCardinality(Nullable(String)),
    server_name      LowCardinality(Nullable(String)),
    tool_name        LowCardinality(Nullable(String)),
    duration_ms      Nullable(Int64) CODEC(Delta, ZSTD(1)),
    status           LowCardinality(Nullable(String)),
    error_message    Nullable(String) CODEC(ZSTD(3)),
    ip_address       Nullable(String),
    detail           Nullable(String) CODEC(ZSTD(3)),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    -- Skip indices
    INDEX idx_user_id    user_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_server_id  server_id  TYPE bloom_filter GRANULARITY 4,
    INDEX idx_tool       tool_name  TYPE set(200)     GRANULARITY 2,
    INDEX idx_status     status     TYPE set(20)      GRANULARITY 2,
    INDEX idx_search     id         TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

-- Projection: ORDER BY duration_ms (for "slowest tool" queries)
ALTER TABLE mcp_logs ADD PROJECTION IF NOT EXISTS proj_by_duration (
    SELECT * ORDER BY duration_ms, created_at
);

-- ================================================================
--  Skip-index backfills (P2-11 audit)
--
--  Extra skip indices added after the initial schema shipped. These
--  are idempotent ALTERs so they safely apply on every boot. Each
--  index is justified by an actual query in the codebase that was
--  otherwise forcing a full-granule scan.
-- ================================================================
-- audit_logs: user_email is filtered in the admin "who did what" UI.
ALTER TABLE audit_logs ADD INDEX IF NOT EXISTS idx_user_email user_email TYPE bloom_filter GRANULARITY 4;
-- mcp_logs: error_message full-text search for the /logs drill-down.
ALTER TABLE mcp_logs ADD INDEX IF NOT EXISTS idx_error_msg error_message TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4;
-- gateway_logs: detail column is used by the cost drill-down's
-- "search requests by id/payload keyword" feature.
ALTER TABLE gateway_logs ADD INDEX IF NOT EXISTS idx_detail detail TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4;
-- platform_logs: user_email for the role-history / team-change audit UI.
ALTER TABLE platform_logs ADD INDEX IF NOT EXISTS idx_user_email user_email TYPE bloom_filter GRANULARITY 4;

-- ================================================================
--  mcp_server_call_counts  (P2-10)
--
--  Pre-aggregates mcp_logs by server_id so GET /api/mcp/servers
--  doesn't have to scan up to 90 days of log rows per request. Uses
--  SummingMergeTree: the MV pushes 1-per-row, CH merges on read via
--  sum() GROUP BY.
-- ================================================================
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

-- ================================================================
--  provider_health_5m  (P2-9)
--
--  5-minute rollup of gateway_logs by provider for the dashboard
--  "provider health" widget. SummingMergeTree stores additive
--  counters; latency is stored as sum + count so callers can compute
--  a weighted average over any time window with one GROUP BY.
-- ================================================================
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

-- ================================================================
--  platform_logs
-- ================================================================
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

    -- Skip indices
    INDEX idx_user_id   user_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_action    action     TYPE set(100)     GRANULARITY 2,
    INDEX idx_resource  resource   TYPE set(100)     GRANULARITY 2,
    INDEX idx_ip        ip_address TYPE bloom_filter GRANULARITY 4,
    INDEX idx_search    id         TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;
