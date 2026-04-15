-- Materialized views and rollups.

USE think_watch;

-- mcp_server_call_counts (P2-10)
--
-- Pre-aggregates mcp_logs by server_id so GET /api/mcp/servers
-- doesn't have to scan up to 90 days of log rows per request. Uses
-- SummingMergeTree: the MV pushes 1-per-row, CH merges on read via
-- sum() GROUP BY.
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

-- provider_health_5m (P2-9)
--
-- 5-minute rollup of gateway_logs by provider for the dashboard
-- "provider health" widget. SummingMergeTree stores additive
-- counters; latency is stored as sum + count so callers can compute
-- a weighted average over any time window with one GROUP BY.
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
