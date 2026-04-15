-- MCP health-check cadence is now a runtime setting instead of an
-- env var (THINKWATCH_MCP_HEALTH_INTERVAL_SECS). The background loop
-- reads this on every iteration via DynamicConfig, so the admin can
-- change the cadence from the settings UI without a redeploy.
--
-- Default 300 = every 5 minutes (was 60s as a hard-coded env default).

INSERT INTO system_settings (key, value, category, description) VALUES
('mcp.health_interval_secs', '300', 'mcp',
 'How often (in seconds) to background-probe each registered MCP server. Default 300 = every 5 minutes.')
ON CONFLICT (key) DO NOTHING;
