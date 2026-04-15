-- Skip-index backfills.
--
-- Idempotent ALTERs that safely apply on every boot. Each index is
-- justified by an actual or planned query that was otherwise forcing
-- a full-granule scan.
--
-- ClickHouse 26+ rejects tokenbf_v1 on Nullable(String) columns, so
-- the substring-search indices on Nullable text columns are built on
-- the expression `ifNull(col, '')` instead. Queries must reference
-- the same expression (e.g. `ifNull(error_message,'') ILIKE ?`) to
-- benefit from the skip index — `NULL ILIKE` never matches anyway,
-- so the rewrite is semantically equivalent.

USE think_watch;

ALTER TABLE audit_logs    ADD INDEX IF NOT EXISTS idx_user_email user_email TYPE bloom_filter GRANULARITY 4;
ALTER TABLE platform_logs ADD INDEX IF NOT EXISTS idx_user_email user_email TYPE bloom_filter GRANULARITY 4;

ALTER TABLE mcp_logs      ADD INDEX IF NOT EXISTS idx_error_msg ifNull(error_message, '') TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4;
ALTER TABLE mcp_logs      ADD INDEX IF NOT EXISTS idx_detail    ifNull(detail, '')        TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4;
ALTER TABLE gateway_logs  ADD INDEX IF NOT EXISTS idx_detail    ifNull(detail, '')        TYPE tokenbf_v1(512, 3, 0) GRANULARITY 4;
