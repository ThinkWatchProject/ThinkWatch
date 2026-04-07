-- Add a `last_error` column to mcp_servers so the dashboard / admin UI
-- can surface why the last tool discovery (or health check) failed,
-- without forcing the user to dig through server logs. NULL means
-- "the last attempt succeeded".
ALTER TABLE mcp_servers ADD COLUMN IF NOT EXISTS last_error TEXT;
