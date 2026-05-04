-- ============================================================================
-- ThinkWatch — Seed data (idempotent)
--
-- Applied after `db/schema.sql` on every boot. Every INSERT carries
-- `ON CONFLICT ... DO NOTHING` so re-runs leave existing rows alone —
-- if you want to *update* a seed (e.g. tweak rbac_roles policy_document),
-- write a one-off in `db/release_migrations/` instead so the rewrite is
-- explicit.
-- ============================================================================


-- Seed system roles. The policy_document is the single source of truth
-- for permissions, model scope, tool scope, and constraints. Permission
-- catalog must stay in lockstep with the backend PERMISSION_CATALOG
-- (crates/server/src/handlers/roles.rs).
INSERT INTO rbac_roles (name, description, is_system, policy_document) VALUES
('super_admin',
 'Full system access. Can manage every resource and inspect every log.',
 TRUE,
 '{"Version":"2024-01-01","Statement":[{"Sid":"FullAccess","Effect":"Allow","Action":"*","Resource":"*"}]}'
),
('admin',
 'Administrative access. Manages providers, MCP servers, API keys, and users.',
 TRUE,
 '{"Version":"2024-01-01","Statement":[{"Sid":"AdminAccess","Effect":"Allow","Action":["ai_gateway:use","mcp_gateway:use","mcp:connect","api_keys:read","api_keys:create","api_keys:update","api_keys:rotate","api_keys:delete","api_keys:admin","providers:read","providers:create","providers:update","providers:delete","providers:rotate_key","models:read","models:write","mcp_servers:read","mcp_servers:create","mcp_servers:update","mcp_servers:delete","users:read","users:create","users:update","teams:read","teams:create","teams:update","teams:delete","team_members:write","team:read","team:write","sessions:revoke","roles:read","roles:create","roles:update","roles:delete","analytics:read_all","audit_logs:read_all","logs:read_all","log_forwarders:read","log_forwarders:write","webhooks:read","webhooks:write","content_filter:read","content_filter:write","pii_redactor:read","pii_redactor:write","rate_limits:read","rate_limits:write","settings:read","settings:write"],"Resource":"*"}]}'
),
('team_manager',
 'Team-level management. Manages members, API keys, and rate limits for the team it''s assigned to. Intended to be granted with scope_kind = team.',
 TRUE,
 '{"Version":"2024-01-01","Statement":[{"Sid":"TeamManagement","Effect":"Allow","Action":["ai_gateway:use","mcp_gateway:use","mcp:connect","api_keys:read","api_keys:create","api_keys:update","api_keys:rotate","providers:read","models:read","mcp_servers:read","users:read","users:update","team_members:write","team:read","team:write","analytics:read_team","audit_logs:read_team","logs:read_team","rate_limits:read","rate_limits:write"],"Resource":"*"}]}'
),
('developer',
 'Standard developer. Uses the gateway, manages own API keys, sees own usage.',
 TRUE,
 '{"Version":"2024-01-01","Statement":[{"Sid":"DeveloperAccess","Effect":"Allow","Action":["ai_gateway:use","mcp_gateway:use","mcp:connect","api_keys:read","api_keys:create","api_keys:update","providers:read","models:read","mcp_servers:read","analytics:read_own","audit_logs:read_own","logs:read_own"],"Resource":"*"}]}'
),
('viewer',
 'Read-only access. Can browse providers and analytics but not modify anything.',
 TRUE,
 '{"Version":"2024-01-01","Statement":[{"Sid":"ViewerAccess","Effect":"Allow","Action":["api_keys:read","providers:read","models:read","mcp_servers:read","analytics:read_own","audit_logs:read_own","logs:read_own"],"Resource":"*"}]}'
)
ON CONFLICT (name) DO NOTHING;
INSERT INTO api_key_surface_kinds (name, display_name, description) VALUES
    ('ai_gateway',  'AI Gateway',  'LLM proxy endpoints (/v1/chat/completions, /v1/messages, …).'),
    ('mcp_gateway', 'MCP Gateway', 'Model Context Protocol tool-call proxy.'),
    ('console',     'Console API', 'Admin console (users, keys, providers, logs, settings).')
ON CONFLICT (name) DO NOTHING;
-- Idempotent singleton bootstrap: a re-run of the init script (e.g.
-- the binary's in-memory re-apply on an upgraded deployment) must not
-- clobber operator-edited pricing. Touching updated_at is harmless and
-- lets the UI show "last reconciled".
INSERT INTO platform_pricing (id) VALUES (1)
    ON CONFLICT (id) DO UPDATE SET updated_at = platform_pricing.updated_at;

-- Auth
INSERT INTO system_settings (key, value, category, description) VALUES
('auth.jwt_access_ttl_secs',   '900',   'auth', 'JWT access token lifetime in seconds'),
('auth.jwt_refresh_ttl_days',  '7',     'auth', 'JWT refresh token lifetime in days'),
('auth.allow_registration',    'false', 'auth', 'Whether public user self-registration is allowed')
ON CONFLICT (key) DO NOTHING;

-- Gateway
INSERT INTO system_settings (key, value, category, description) VALUES
('gateway.cache_ttl_secs',       '3600',     'gateway', 'Response cache TTL in seconds'),
('gateway.request_timeout_secs', '120',      'gateway', 'Gateway request timeout (requires restart)'),
('gateway.body_limit_bytes',     '10485760', 'gateway', 'Gateway max request body size (requires restart)')
ON CONFLICT (key) DO NOTHING;

-- Console
INSERT INTO system_settings (key, value, category, description) VALUES
('console.request_timeout_secs', '30',      'console', 'Console API request timeout (requires restart)'),
('console.body_limit_bytes',     '1048576', 'console', 'Console API max request body size (requires restart)')
ON CONFLICT (key) DO NOTHING;

-- Security
INSERT INTO system_settings (key, value, category, description) VALUES
('security.signature_nonce_ttl_secs', '600',    'security', 'Request signature nonce TTL in seconds'),
('security.signature_drift_secs',    '300',     'security', 'Maximum allowed clock skew for signatures'),
('security.totp_required',          'false',    'security', 'Require all users to enable TOTP two-factor authentication'),
('security.rate_limit_fail_closed', 'false',    'security', 'When true the rate-limit engine refuses requests on Redis outage instead of failing open'),
('security.client_ip_source',       '"connection"',    'security', 'Client IP source: "connection", "xff", or "x-real-ip"'),
('security.client_ip_xff_position', '"left"',   'security', 'XFF pick direction: "left" (first) or "right" (last)'),
('security.client_ip_xff_depth',    '1',        'security', 'Position depth (1-based) from chosen XFF direction'),
('security.content_filter_patterns', '[
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
]', 'security', 'Content filter rules (JSON array of {name, pattern, match_type, action})'),
('security.pii_redactor_patterns', '[
    {"name": "email",       "regex": "[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}",           "placeholder_prefix": "EMAIL"},
    {"name": "id_card_cn",  "regex": "\\b\\d{17}[\\dXx]\\b",                                       "placeholder_prefix": "ID"},
    {"name": "credit_card", "regex": "\\b\\d{4}[-\\s]?\\d{4}[-\\s]?\\d{4}[-\\s]?\\d{4}\\b",        "placeholder_prefix": "CARD"},
    {"name": "phone_cn",    "regex": "1[3-9]\\d{9}",                                                "placeholder_prefix": "PHONE"},
    {"name": "phone_us",    "regex": "\\b\\d{3}[-.]?\\d{3}[-.]?\\d{4}\\b",                          "placeholder_prefix": "PHONE"},
    {"name": "ipv4",        "regex": "\\b\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}\\.\\d{1,3}\\b",             "placeholder_prefix": "IP"}
]', 'security', 'PII redactor patterns (JSON array)'),
('security.budget_alert_webhook_url', '""', 'security', 'Webhook URL for budget cap alerts'),
('security.trusted_proxies', '[]', 'security', 'JSON array of trusted reverse proxy IPs')
ON CONFLICT (key) DO NOTHING;

-- Audit
INSERT INTO system_settings (key, value, category, description) VALUES
('audit.batch_size',          '50',    'audit', 'Quickwit batch flush size'),
('audit.flush_interval_secs', '2',     'audit', 'Quickwit batch flush interval in seconds'),
('audit.channel_capacity',    '10000', 'audit', 'Audit log channel buffer capacity'),
('audit.sample_rate',         '1.0',   'audit', 'Fraction of audit events to keep (0.0-1.0). Lower on high-volume deployments to spare ClickHouse; sampled-out entries increment audit_log_sampled_out_total.')
ON CONFLICT (key) DO NOTHING;

-- API Keys
INSERT INTO system_settings (key, value, category, description) VALUES
('api_keys.default_expiry_days',         '90', 'api_keys', 'Default API key expiration in days (0 = no expiry)'),
('api_keys.inactivity_timeout_days',     '0',  'api_keys', 'Auto-disable after N days of inactivity (0 = disabled)'),
('api_keys.rotation_period_days',        '0',  'api_keys', 'Auto-rotation period in days (0 = disabled)'),
('api_keys.rotation_grace_period_hours', '24', 'api_keys', 'Grace period for old key after rotation')
ON CONFLICT (key) DO NOTHING;

-- Data retention — per-log-type ClickHouse TTL seeds.
-- Audit/Gateway/MCP/Platform default to 90 days; Access/App default to 30 days.
-- Changing these via the admin UI issues `ALTER TABLE ... MODIFY TTL` against
-- the corresponding ClickHouse table, so the value here is the seed default only.
INSERT INTO system_settings (key, value, category, description) VALUES
('data.retention_days_audit',    '90', 'data', 'Days to keep audit logs in ClickHouse'),
('data.retention_days_gateway',  '90', 'data', 'Days to keep AI gateway request logs in ClickHouse'),
('data.retention_days_mcp',      '90', 'data', 'Days to keep MCP tool invocation logs in ClickHouse'),
('data.retention_days_access',   '30', 'data', 'Days to keep HTTP access logs in ClickHouse'),
('data.retention_days_app',      '30', 'data', 'Days to keep application runtime logs in ClickHouse')
ON CONFLICT (key) DO NOTHING;

-- Setup
INSERT INTO system_settings (key, value, category, description) VALUES
('setup.initialized', 'false',         'setup', 'Whether initial setup has been completed'),
('setup.site_name',   '"ThinkWatch"', 'setup', 'Site display name')
ON CONFLICT (key) DO NOTHING;

-- General — gateway public URL components (used by configuration guide).
-- Empty/zero values mean "auto-detect from the user's browser request".
INSERT INTO system_settings (key, value, category, description) VALUES
('general.public_protocol', '""', 'general', 'Public gateway protocol: "http", "https", or empty for auto-detect from browser'),
('general.public_host',     '""', 'general', 'Public gateway host (empty = auto-detect from browser)'),
('general.public_port',     '0',  'general', 'Public gateway port (0 = use the gateway listening port)')
ON CONFLICT (key) DO NOTHING;

-- MCP
INSERT INTO system_settings (key, value, category, description) VALUES
('mcp.health_interval_secs', '300', 'mcp',
 'How often (in seconds) to background-probe each registered MCP server. Default 300 = every 5 minutes.')
ON CONFLICT (key) DO NOTHING;

-- Performance tuning — all live-adjustable from Admin > Settings.
INSERT INTO system_settings (key, value, category, description) VALUES
('perf.http_client_secs',        '15', 'perf', 'Outbound HTTP client timeout in seconds (MCP discovery, OIDC, etc.)'),
('perf.mcp_pool_secs',           '30', 'perf', 'MCP connection pool per-request timeout in seconds'),
('perf.console_request_secs',    '30', 'perf', 'Console-side request timeout in seconds'),
('perf.dashboard_ws_io_secs',    '5',  'perf', 'Dashboard WebSocket per-frame read/write timeout in seconds'),
('perf.dashboard_ws_tick_secs',  '4',  'perf', 'Dashboard WebSocket push interval in seconds'),
('perf.dashboard_ws_max_per_user', '4', 'perf', 'Max concurrent dashboard WebSocket connections per user')
ON CONFLICT (key) DO NOTHING;

-- Seed: built-in MCP store templates.
-- Auth shape semantics:
--   * upstream supports OAuth → fill `oauth_*`. Admin still needs to
--     paste a `client_id` / `client_secret` once on install (these
--     are app-level, not user-level).
--   * upstream accepts PAT / API key → set `allow_static_token=TRUE`
--     and surface the help URL via `static_token_help_url`.
--   * anonymous service (databases, public docs APIs) → both empty.
INSERT INTO mcp_store_templates
    (slug, name, description, category, tags, endpoint_template,
     oauth_userinfo_endpoint,
     allow_static_token, static_token_help_url, auth_instructions,
     deploy_type, featured)
VALUES
('github',         'GitHub',           'Manage repositories, issues, pull requests, and code search', 'developer',     '{"git","code","vcs"}',           'https://api.githubcopilot.com/mcp/',                            'https://api.github.com/user',                          TRUE,  'https://github.com/settings/tokens',                            'Personal access token (classic) or fine-grained PAT.',                                                  'hosted', true),
('gitlab',         'GitLab',           'Manage projects, merge requests, and CI/CD pipelines',        'developer',     '{"git","code","cicd"}',          '',                                                              'https://gitlab.com/api/v4/user',                       TRUE,  'https://gitlab.com/-/profile/personal_access_tokens',           'Create a personal access token in GitLab.',                                                              'manual', false),
('linear',         'Linear',           'Project management — issues, projects, and cycles',           'developer',     '{"project","agile"}',            'https://mcp.linear.app/sse',                                    NULL,                                                   TRUE,  'https://linear.app/settings/api',                               'Create a personal API key in Linear.',                                                                   'hosted', false),
('sentry',         'Sentry',           'Error tracking and performance monitoring',                   'developer',     '{"monitoring","errors"}',        '',                                                              'https://sentry.io/api/0/auth/',                        TRUE,  'https://sentry.io/settings/account/api/auth-tokens/',           'Create an auth token in Sentry.',                                                                        'manual', false),
('postgresql',     'PostgreSQL',       'Query databases, browse schemas, and manage tables',          'database',      '{"sql","relational"}',           '',                                                              NULL,                                                   FALSE, NULL,                                                            'Deploy the PostgreSQL MCP server with your connection string.',                                          'docker', true),
('mysql',          'MySQL',            'Query databases, browse schemas, and manage tables',          'database',      '{"sql","relational"}',           '',                                                              NULL,                                                   FALSE, NULL,                                                            'Deploy the MySQL MCP server with your connection string.',                                               'docker', false),
('redis',          'Redis',            'Key-value operations, pub/sub, and data inspection',          'database',     '{"cache","nosql"}',               '',                                                              NULL,                                                   FALSE, NULL,                                                            'Deploy the Redis MCP server pointing at your instance.',                                                 'docker', false),
('mongodb',        'MongoDB',          'Document operations, aggregation, and collection management','database',      '{"nosql","document"}',           '',                                                              NULL,                                                   FALSE, NULL,                                                            'Deploy the MongoDB MCP server with your connection URI.',                                                'docker', false),
('slack',          'Slack',            'Send messages, manage channels, and search workspace',       'communication', '{"chat","messaging"}',           '',                                                              'https://slack.com/api/users.identity',                 TRUE,  'https://api.slack.com/apps',                                    'Create a Slack app, enable bot scopes, install to workspace, and copy the bot token.',                  'docker', true),
('discord',        'Discord',          'Send messages, manage channels, and moderate servers',       'communication', '{"chat","gaming"}',              '',                                                              'https://discord.com/api/v10/users/@me',                TRUE,  'https://discord.com/developers/applications',                   'Create an application + bot in the Discord Developer Portal and copy the bot token.',                   'manual', false),
('aws',            'AWS',              'Manage S3 buckets, Lambda functions, EC2 instances, and more','cloud',         '{"infrastructure","devops"}',    '',                                                              NULL,                                                   TRUE,  'https://console.aws.amazon.com/iam/home#/security_credentials', 'Provide an AWS access-key pair (or assume-role token) for the MCP server.',                              'docker', false),
('cloudflare',     'Cloudflare',       'Manage DNS records, Workers, and edge configuration',         'cloud',         '{"cdn","dns","edge"}',           'https://mcp.cloudflare.com',                                    'https://api.cloudflare.com/client/v4/user',            TRUE,  'https://dash.cloudflare.com/profile/api-tokens',                'Create an API token in Cloudflare.',                                                                     'hosted', false),
('filesystem',     'Filesystem',       'Read and write local files, browse directories',              'utility',       '{"files","local"}',              '',                                                              NULL,                                                   FALSE, NULL,                                                            'Deploys locally — grants access to the configured directory.',                                           'docker', false),
('web-search',     'Web Search',       'Search the web and fetch page content',                       'utility',       '{"search","web"}',               '',                                                              NULL,                                                   TRUE,  NULL,                                                            'Requires a search API key (Google, Bing, or Brave).',                                                    'docker', true),
('puppeteer',      'Puppeteer',        'Browser automation — navigate, screenshot, and extract data','utility',       '{"browser","scraping"}',         '',                                                              NULL,                                                   FALSE, NULL,                                                            'Deploy the Puppeteer MCP server with a headless Chrome instance.',                                       'docker', false),
('microsoft-docs', 'Microsoft Docs',   'Search and browse Microsoft Learn documentation',             'knowledge',     '{"docs","microsoft","azure"}',   'https://learn.microsoft.com/api/mcp',                           NULL,                                                   FALSE, NULL,                                                            NULL,                                                                                                     'hosted', false),
('aws-docs',       'AWS Documentation','Search and browse AWS service documentation',                  'knowledge',     '{"docs","aws","cloud"}',         'https://knowledge-mcp.global.api.aws',                          NULL,                                                   FALSE, NULL,                                                            NULL,                                                                                                     'hosted', false),
('mdn-web-docs',   'MDN Web Docs',     'Search MDN for HTML, CSS, JavaScript, and Web API references','knowledge',     '{"docs","web","frontend"}',      '',                                                              NULL,                                                   FALSE, NULL,                                                            NULL,                                                                                                     'docker', false),
('wikipedia',      'Wikipedia',        'Search and read Wikipedia articles in any language',          'knowledge',     '{"docs","encyclopedia"}',        '',                                                              NULL,                                                   FALSE, NULL,                                                            NULL,                                                                                                     'docker', false),
('arxiv',          'arXiv',            'Search and read academic papers from arXiv',                  'knowledge',     '{"docs","research","papers"}',   '',                                                              NULL,                                                   FALSE, NULL,                                                            NULL,                                                                                                     'docker', false),
('notion',         'Notion',           'Read and write Notion pages, databases, and blocks',          'productivity',  '{"notes","wiki","docs"}',        'https://mcp.notion.com/sse',                                    'https://api.notion.com/v1/users/me',                   TRUE,  'https://www.notion.so/my-integrations',                         'Create an internal integration in Notion and copy its token.',                                           'hosted', false),
('google-drive',   'Google Drive',     'Search, read, and manage files in Google Drive',              'productivity',  '{"files","google","storage"}',   '',                                                              'https://www.googleapis.com/oauth2/v3/userinfo',        TRUE,  'https://console.cloud.google.com/apis/credentials',             'Create a Google Cloud OAuth2 credential and authorize Drive access.',                                    'manual', false),
('jira',           'Jira',             'Manage Jira issues, sprints, and project boards',             'developer',     '{"project","agile","atlassian"}','',                                                              'https://api.atlassian.com/me',                         TRUE,  'https://id.atlassian.com/manage-profile/security/api-tokens',   'Create an API token at id.atlassian.com.',                                                               'manual', false)
ON CONFLICT (slug) DO NOTHING;

-- OAuth-shaped templates: distinct INSERT because they fill the
-- oauth_issuer / oauth_authorization_endpoint / oauth_token_endpoint /
-- oauth_default_scopes columns the static-token templates above leave
-- NULL. Keeping a separate row keeps the catalog catalogues both shapes
-- of the same upstream so admins can pick (e.g. `linear` for PAT,
-- `linear-oauth` for org SSO).
INSERT INTO mcp_store_templates
    (slug, name, description, category, tags, endpoint_template,
     oauth_issuer, oauth_authorization_endpoint, oauth_token_endpoint,
     oauth_userinfo_endpoint, oauth_default_scopes,
     allow_static_token, static_token_help_url, auth_instructions,
     deploy_type, featured)
VALUES (
    'linear-oauth',
    'Linear (OAuth)',
    'Project management — issues, projects, and cycles. OAuth flow for org SSO.',
    'developer',
    '{"project","agile","oauth"}',
    'https://mcp.linear.app/sse',
    'https://linear.app',
    'https://linear.app/oauth/authorize',
    'https://api.linear.app/oauth/token',
    NULL, -- Linear has no OIDC userinfo; resolver falls back to JWT decode (also unavailable for opaque tokens — upstream_subject ends up NULL, acceptable)
    '{"read"}',
    FALSE,
    NULL,
    'Register an OAuth application at https://linear.app/settings/api/applications, copy the client ID and secret, and paste them into the install dialog. Linear''s OAuth uses opaque tokens so per-user display names will fall back to email from the access cookie.',
    'hosted',
    false
)
ON CONFLICT (slug) DO NOTHING;

-- MCP Store
INSERT INTO system_settings (key, value, category, description) VALUES
('mcp_store.registry_url', '"https://thinkwat.ch/registry/mcp-templates.json"', 'mcp_store', 'Remote registry URL for syncing MCP store templates')
ON CONFLICT (key) DO NOTHING;

-- Gateway routing + circuit-breaker tunables. Editable in the admin
-- UI without a deploy.
INSERT INTO system_settings (key, value, category, description) VALUES
    ('gateway.default_routing_strategy', '"latency_health"', 'gateway',
     'Default routing strategy for models that do not override (weighted/latency/health/latency_health)'),
    ('gateway.default_affinity_mode',    '"provider"', 'gateway',
     'Default session affinity mode (none/provider/route)'),
    ('gateway.default_affinity_ttl_secs','300',        'gateway',
     'Default affinity key TTL in seconds (0-86400)'),
    ('gateway.latency_strategy_k',       '2.0',        'gateway',
     'Exponent for the latency-strategy weighting (higher = more aggressive). Default 2.0 (aggressive).'),
    ('gateway.cb_enabled',               'true',       'gateway',
     'Enable circuit-breaker: routes exceeding the error threshold are temporarily excluded from selection'),
    ('gateway.cb_error_pct',             '50',         'gateway',
     'Circuit-breaker error rate threshold (percent, 1-100). Routes above this rate trip open'),
    ('gateway.cb_min_samples',           '10',         'gateway',
     'Minimum sample count in the rolling window before the circuit-breaker can trip (avoids tripping on a handful of errors)'),
    ('gateway.cb_window_secs',           '60',         'gateway',
     'Rolling window length in seconds for circuit-breaker error-rate computation'),
    ('gateway.cb_open_secs',             '30',         'gateway',
     'How long a tripped (open) circuit stays open before transitioning to half-open (probe) state')
ON CONFLICT (key) DO NOTHING;
