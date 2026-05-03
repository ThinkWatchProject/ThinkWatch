-- Optional human-friendly label for an MCP server instance. Distinct
-- from `name` (system identifier, must be unique, used in audit logs)
-- and `namespace_prefix` (tool routing key). When two installs of the
-- same template land as `linear` + `linear_2`, the operator can label
-- them "Linear (Acme prod)" / "Linear (BBQ corp)" so users on
-- /connections see something meaningful rather than `linear_2`.
--
-- Frontend falls back to `name` when this column is NULL — no breakage
-- for existing rows.
ALTER TABLE mcp_servers ADD COLUMN IF NOT EXISTS display_label TEXT;
