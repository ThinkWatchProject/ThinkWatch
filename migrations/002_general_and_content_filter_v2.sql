-- 002_general_and_content_filter_v2.sql
--
-- Adds two related changes:
-- 1) `general` category settings for the public gateway URL
--    (host / port / protocol shown in the configuration guide)
-- 2) Upgrades the content filter rule schema from the legacy
--    {pattern, severity, category} format to the new
--    {name, pattern, match_type, action} format. The runtime
--    deserializer accepts both, so this migration is optional —
--    we run it to give existing installs the friendlier UI labels.

-- ---------------------------------------------------------------------------
-- 1. General — public gateway URL components
-- ---------------------------------------------------------------------------

INSERT INTO system_settings (key, value, category, description) VALUES
    ('general.public_protocol', '""', 'general', 'Public gateway protocol: "http", "https", or empty for auto-detect from browser'),
    ('general.public_host',     '""', 'general', 'Public gateway host (empty = auto-detect from browser)'),
    ('general.public_port',     '0',  'general', 'Public gateway port (0 = use the gateway listening port)')
ON CONFLICT (key) DO NOTHING;

-- ---------------------------------------------------------------------------
-- 2. Content filter rules — upgrade legacy schema in place
-- ---------------------------------------------------------------------------
-- Only rewrite if the value still uses the old `severity` field.
-- This is safe to re-run: subsequent migrations or admin edits won't be
-- clobbered because the WHERE clause filters them out.

UPDATE system_settings
SET value = '[
    {"name": "Ignore Previous Instructions", "pattern": "ignore previous instructions", "match_type": "contains", "action": "block"},
    {"name": "Ignore All Previous",          "pattern": "ignore all previous",          "match_type": "contains", "action": "block"},
    {"name": "Disregard Instructions",       "pattern": "disregard your instructions",  "match_type": "contains", "action": "block"},
    {"name": "Jailbreak",                    "pattern": "jailbreak",                    "match_type": "contains", "action": "block"},
    {"name": "DAN",                          "pattern": " dan ",                        "match_type": "contains", "action": "block"},
    {"name": "Developer Mode",               "pattern": "developer mode",               "match_type": "contains", "action": "block"},
    {"name": "Persona Manipulation",         "pattern": "you are now",                  "match_type": "contains", "action": "warn"},
    {"name": "Act As",                       "pattern": "act as",                       "match_type": "contains", "action": "warn"},
    {"name": "System Prompt Extraction",     "pattern": "system prompt",                "match_type": "contains", "action": "warn"},
    {"name": "Reveal Instructions",          "pattern": "reveal your instructions",     "match_type": "contains", "action": "warn"}
]'::jsonb,
    description = 'Content filter rules (JSON array of {name, pattern, match_type, action})'
WHERE key = 'security.content_filter_patterns'
  AND value @> '[{"severity": "critical"}]'::jsonb;
