-- trace_id column backfill for installs that pre-date M2-8.
--
-- Correlates a single request's events across gateway_logs / mcp_logs /
-- audit_logs. `GET /api/admin/trace/{trace_id}` reads from all three.
-- Fresh installs already have the column from 01_tables.sql; these
-- idempotent ALTERs make the migration a no-op on upgrade.

USE think_watch;

ALTER TABLE audit_logs    ADD COLUMN IF NOT EXISTS trace_id Nullable(String);
ALTER TABLE gateway_logs  ADD COLUMN IF NOT EXISTS trace_id Nullable(String);
ALTER TABLE mcp_logs      ADD COLUMN IF NOT EXISTS trace_id Nullable(String);

ALTER TABLE audit_logs    ADD INDEX  IF NOT EXISTS idx_trace trace_id TYPE bloom_filter GRANULARITY 4;
ALTER TABLE gateway_logs  ADD INDEX  IF NOT EXISTS idx_trace trace_id TYPE bloom_filter GRANULARITY 4;
ALTER TABLE mcp_logs      ADD INDEX  IF NOT EXISTS idx_trace trace_id TYPE bloom_filter GRANULARITY 4;
