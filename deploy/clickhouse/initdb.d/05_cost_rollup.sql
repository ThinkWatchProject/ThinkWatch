-- Cost rollup: hourly aggregation of gateway_logs for fast analytics.

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
ORDER BY (hour, model_id, provider, user_id, api_key_id);

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
