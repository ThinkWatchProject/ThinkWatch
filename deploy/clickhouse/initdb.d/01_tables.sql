-- ClickHouse base schema for ThinkWatch log storage.
--
-- Encoding strategy:
--   LowCardinality   → categorical columns (action, resource, provider …)
--   DoubleDelta+ZSTD  → monotonic timestamps
--   Delta+ZSTD        → numeric metrics (tokens, latency, cost)
--   ZSTD(3)           → large variable-length text (detail, user_agent, error)
--
-- Skip indices (per-granule):
--   bloom_filter → high-cardinality ID lookups  (user_id, api_key_id …)
--   set(N)       → low-cardinality exact match  (action, provider, status …)
--   tokenbf_v1   → ILIKE '%keyword%' substring search (Strings only;
--                  Nullable columns are wrapped in ifNull(col,'') in
--                  02_index_backfills.sql so the index expression is non-null)
--
-- Partitioning: monthly. ttl_only_drop_parts = 1 → whole-part TTL drops.

CREATE DATABASE IF NOT EXISTS think_watch;
USE think_watch;

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
    -- that produced it. Set by the originating handler's middleware;
    -- NULL for standalone admin actions.
    trace_id         Nullable(String),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_user_id   user_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_api_key   api_key_id TYPE bloom_filter GRANULARITY 4,
    INDEX idx_action    action     TYPE set(100)     GRANULARITY 2,
    INDEX idx_resource  resource   TYPE set(100)     GRANULARITY 2,
    INDEX idx_ip        ip_address TYPE bloom_filter GRANULARITY 4,
    INDEX idx_trace     trace_id   TYPE bloom_filter GRANULARITY 4,
    INDEX idx_search    id         TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

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
    -- trace_id: shared ID across gateway / mcp / audit rows for one request.
    trace_id         Nullable(String),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_user_id    user_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_api_key    api_key_id TYPE bloom_filter GRANULARITY 4,
    INDEX idx_model      model_id   TYPE set(200)     GRANULARITY 2,
    INDEX idx_provider   provider   TYPE set(50)      GRANULARITY 2,
    INDEX idx_status     status_code TYPE set(50)     GRANULARITY 2,
    INDEX idx_trace      trace_id   TYPE bloom_filter GRANULARITY 4,
    INDEX idx_search     id         TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

ALTER TABLE gateway_logs ADD PROJECTION IF NOT EXISTS proj_by_cost (
    SELECT * ORDER BY cost_usd, created_at
);

ALTER TABLE gateway_logs ADD PROJECTION IF NOT EXISTS proj_by_latency (
    SELECT * ORDER BY latency_ms, created_at
);

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
    trace_id         Nullable(String),
    created_at       DateTime64(3, 'UTC') DEFAULT now64(3) CODEC(DoubleDelta, ZSTD(1)),

    INDEX idx_user_id    user_id    TYPE bloom_filter GRANULARITY 4,
    INDEX idx_server_id  server_id  TYPE bloom_filter GRANULARITY 4,
    INDEX idx_tool       tool_name  TYPE set(200)     GRANULARITY 2,
    INDEX idx_status     status     TYPE set(20)      GRANULARITY 2,
    INDEX idx_trace      trace_id   TYPE bloom_filter GRANULARITY 4,
    INDEX idx_search     id         TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(created_at)
ORDER BY (created_at, id)
TTL toDateTime(created_at) + INTERVAL 90 DAY
SETTINGS index_granularity = 8192,
         ttl_only_drop_parts = 1;

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
