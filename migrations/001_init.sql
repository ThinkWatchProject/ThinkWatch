-- ============================================================================
-- ThinkWatch — Consolidated Schema
-- ============================================================================

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

-- --------------------------------------------------------------------------
-- Users & Teams
-- --------------------------------------------------------------------------

CREATE TABLE users (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email                   VARCHAR(255) NOT NULL UNIQUE,
    display_name            VARCHAR(255) NOT NULL,
    password_hash           VARCHAR(255),
    oidc_subject            VARCHAR(255),
    oidc_issuer             VARCHAR(512),
    avatar_url              TEXT,
    is_active               BOOLEAN NOT NULL DEFAULT TRUE,
    totp_secret             TEXT,
    totp_enabled            BOOLEAN NOT NULL DEFAULT FALSE,
    totp_recovery_codes     TEXT,
    password_change_required BOOLEAN NOT NULL DEFAULT FALSE,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at              TIMESTAMPTZ,
    UNIQUE(oidc_subject, oidc_issuer)
);

CREATE TABLE teams (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name            VARCHAR(255) NOT NULL UNIQUE,
    description     TEXT,
    -- Budget caps live in `budget_caps` (subject_kind = 'team').
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE team_members (
    user_id   UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    team_id   UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    role      VARCHAR(50) NOT NULL DEFAULT 'member',
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, team_id)
);

CREATE INDEX idx_team_members_user_id ON team_members(user_id);

-- --------------------------------------------------------------------------
-- RBAC — Unified roles + assignments
--
-- One table for the role catalog (system + custom), one table for
-- (user, role, scope) memberships. Permission strings live directly on
-- the role row as TEXT[] — at this scale (~50 perms × ~10 roles) the
-- join table buys nothing.
-- --------------------------------------------------------------------------

CREATE TABLE rbac_roles (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                VARCHAR(100) NOT NULL UNIQUE,
    description         TEXT,
    is_system           BOOLEAN NOT NULL DEFAULT FALSE,
    permissions         TEXT[]   NOT NULL DEFAULT ARRAY[]::TEXT[],
    allowed_models      TEXT[],
    allowed_mcp_servers UUID[],
    policy_document     JSONB,
    created_by          UUID REFERENCES users(id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_rbac_roles_is_system ON rbac_roles(is_system);

-- Scope is a (kind, id) twople. `scope_marker` collapses the
-- (kind, NULL id) case into a deterministic UUID so the primary key
-- treats two global assignments as duplicates (PostgreSQL would
-- otherwise consider multiple NULLs distinct). It is never read by
-- application code.
CREATE TABLE rbac_role_assignments (
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role_id      UUID NOT NULL REFERENCES rbac_roles(id) ON DELETE CASCADE,
    scope_kind   VARCHAR(16) NOT NULL DEFAULT 'global'
        CHECK (scope_kind IN ('global', 'team', 'project')),
    scope_id     UUID,
    scope_marker UUID GENERATED ALWAYS AS (COALESCE(scope_id, '00000000-0000-0000-0000-000000000000'::uuid)) STORED,
    assigned_by  UUID REFERENCES users(id),
    assigned_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, role_id, scope_kind, scope_marker),
    CONSTRAINT chk_scope_consistency
        CHECK ((scope_kind = 'global' AND scope_id IS NULL)
            OR (scope_kind <> 'global' AND scope_id IS NOT NULL))
);

CREATE INDEX idx_rbac_role_assignments_user  ON rbac_role_assignments(user_id);
CREATE INDEX idx_rbac_role_assignments_role  ON rbac_role_assignments(role_id);
CREATE INDEX idx_rbac_role_assignments_scope ON rbac_role_assignments(scope_kind, scope_id);

-- Seed system roles. Permission catalog must stay in lockstep with the
-- backend `PERMISSION_CATALOG` (crates/server/src/handlers/roles.rs).
INSERT INTO rbac_roles (name, description, is_system, permissions) VALUES
('super_admin',
 'Full system access. Can manage every resource and inspect every log.',
 TRUE,
 ARRAY[
    'ai_gateway:use', 'mcp_gateway:use',
    'api_keys:read', 'api_keys:create', 'api_keys:update', 'api_keys:rotate', 'api_keys:delete',
    'providers:read', 'providers:create', 'providers:update', 'providers:delete', 'providers:rotate_key',
    'mcp_servers:read', 'mcp_servers:create', 'mcp_servers:update', 'mcp_servers:delete',
    'users:read', 'users:create', 'users:update', 'users:delete',
    'team:read', 'team:write',
    'sessions:revoke',
    'roles:read', 'roles:create', 'roles:update', 'roles:delete', 'roles:edit_system',
    'analytics:read_own', 'analytics:read_team', 'analytics:read_all',
    'audit_logs:read_own', 'audit_logs:read_team', 'audit_logs:read_all',
    'logs:read_own', 'logs:read_team', 'logs:read_all',
    'log_forwarders:read', 'log_forwarders:write',
    'webhooks:read', 'webhooks:write',
    'content_filter:read', 'content_filter:write',
    'pii_redactor:read', 'pii_redactor:write',
    'rate_limits:read', 'rate_limits:write',
    'settings:read', 'settings:write',
    'system:configure_oidc'
 ]),
('admin',
 'Administrative access. Manages providers, MCP servers, API keys, and users.',
 TRUE,
 ARRAY[
    'ai_gateway:use', 'mcp_gateway:use',
    'api_keys:read', 'api_keys:create', 'api_keys:update', 'api_keys:rotate', 'api_keys:delete',
    'providers:read', 'providers:create', 'providers:update', 'providers:delete', 'providers:rotate_key',
    'mcp_servers:read', 'mcp_servers:create', 'mcp_servers:update', 'mcp_servers:delete',
    'users:read', 'users:create', 'users:update',
    'team:read', 'team:write',
    'sessions:revoke',
    'roles:read', 'roles:create', 'roles:update', 'roles:delete',
    'analytics:read_all',
    'audit_logs:read_all',
    'logs:read_all',
    'log_forwarders:read', 'log_forwarders:write',
    'webhooks:read', 'webhooks:write',
    'content_filter:read', 'content_filter:write',
    'pii_redactor:read', 'pii_redactor:write',
    'rate_limits:read', 'rate_limits:write',
    'settings:read', 'settings:write'
 ]),
('team_manager',
 'Team-level management. Creates API keys for the team, sees the team''s usage and audit trail.',
 TRUE,
 ARRAY[
    'ai_gateway:use', 'mcp_gateway:use',
    'api_keys:read', 'api_keys:create', 'api_keys:update', 'api_keys:rotate',
    'providers:read',
    'mcp_servers:read',
    'users:read',
    'team:read', 'team:write',
    'analytics:read_team',
    'audit_logs:read_team',
    'logs:read_team',
    'rate_limits:read'
 ]),
('developer',
 'Standard developer. Uses the gateway, manages own API keys, sees own usage.',
 TRUE,
 ARRAY[
    'ai_gateway:use', 'mcp_gateway:use',
    'api_keys:read', 'api_keys:create', 'api_keys:update',
    'providers:read',
    'mcp_servers:read',
    'analytics:read_own',
    'audit_logs:read_own',
    'logs:read_own'
 ]),
('viewer',
 'Read-only access. Can browse providers and analytics but not modify anything.',
 TRUE,
 ARRAY[
    'api_keys:read',
    'providers:read',
    'mcp_servers:read',
    'analytics:read_own',
    'audit_logs:read_own',
    'logs:read_own'
 ]);

-- --------------------------------------------------------------------------
-- API Keys
-- --------------------------------------------------------------------------

CREATE TABLE api_keys (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    key_prefix              VARCHAR(16)  NOT NULL,
    key_hash                VARCHAR(255) NOT NULL,
    name                    VARCHAR(255) NOT NULL,
    user_id                 UUID REFERENCES users(id) ON DELETE SET NULL,
    team_id                 UUID REFERENCES teams(id) ON DELETE SET NULL,
    -- Which gateways this key can call. Subset of
    -- {'ai_gateway', 'mcp_gateway'}; the CHECK enforces non-empty
    -- so a key always has at least one surface. Both gateways
    -- share the same `tw-` token format and the same auth
    -- middleware — `surfaces` is what determines which one a
    -- given key is allowed to hit at request time. The legacy
    -- `scopes` JSONB column is gone.
    surfaces                TEXT[] NOT NULL
        CHECK (cardinality(surfaces) > 0
               AND surfaces <@ ARRAY['ai_gateway', 'mcp_gateway']),
    allowed_models          TEXT[],
    -- Rate limits and budget caps live in `rate_limit_rules` /
    -- `budget_caps` (subject_kind = 'api_key'). The previous fixed
    -- columns (rate_limit_rpm / rate_limit_tpm / monthly_budget)
    -- have been removed.
    expires_at              TIMESTAMPTZ,
    is_active               BOOLEAN NOT NULL DEFAULT TRUE,
    last_used_at            TIMESTAMPTZ,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at              TIMESTAMPTZ,
    -- Lifecycle
    rotation_period_days    INTEGER,
    rotated_from_id         UUID REFERENCES api_keys(id),
    grace_period_ends_at    TIMESTAMPTZ,
    inactivity_timeout_days INTEGER,
    disabled_reason         VARCHAR(100),
    last_rotation_at        TIMESTAMPTZ,
    -- Constraints
    CONSTRAINT chk_api_key_rotation_consistency
        CHECK (rotated_from_id IS NULL OR grace_period_ends_at IS NOT NULL),
    CONSTRAINT chk_api_key_rotation_period_positive
        CHECK (rotation_period_days IS NULL OR rotation_period_days > 0),
    CONSTRAINT chk_api_key_inactivity_timeout_positive
        CHECK (inactivity_timeout_days IS NULL OR inactivity_timeout_days >= 0)
);

CREATE INDEX idx_api_keys_key_hash    ON api_keys(key_hash);
CREATE INDEX idx_api_keys_key_prefix  ON api_keys(key_prefix);
CREATE INDEX idx_api_keys_user_id     ON api_keys(user_id);
CREATE INDEX idx_api_keys_is_active   ON api_keys(is_active)  WHERE is_active = true;
CREATE INDEX idx_api_keys_expires_at  ON api_keys(expires_at) WHERE expires_at IS NOT NULL;

-- --------------------------------------------------------------------------
-- Providers & Models
-- --------------------------------------------------------------------------

CREATE TABLE providers (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name              VARCHAR(100) NOT NULL,
    display_name      VARCHAR(255) NOT NULL,
    provider_type     VARCHAR(50)  NOT NULL,
    base_url          VARCHAR(512) NOT NULL,
    api_key_encrypted BYTEA NOT NULL,
    is_active         BOOLEAN NOT NULL DEFAULT TRUE,
    config_json       JSONB DEFAULT '{}',
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at        TIMESTAMPTZ
);

CREATE TABLE models (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    provider_id       UUID NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id          VARCHAR(255) NOT NULL,
    display_name      VARCHAR(255) NOT NULL,
    -- Real USD/token cost — drives billing reports.
    input_price       DECIMAL(10, 6),
    output_price      DECIMAL(10, 6),
    -- Quota multipliers — relative to a virtual baseline (1.0).
    -- Used by the rate-limit / budget-cap engine to convert raw
    -- token counts into "weighted tokens" so a quota of 1M tokens
    -- can't be drained by a single gpt-4o burst. Defaults to 1.0
    -- so existing models behave as raw-token quotas until an admin
    -- tunes them.
    input_multiplier  DECIMAL(8, 4) NOT NULL DEFAULT 1.0
        CHECK (input_multiplier > 0),
    output_multiplier DECIMAL(8, 4) NOT NULL DEFAULT 1.0
        CHECK (output_multiplier > 0),
    is_active         BOOLEAN NOT NULL DEFAULT TRUE,
    UNIQUE(provider_id, model_id)
);

-- --------------------------------------------------------------------------
-- MCP Servers & Tools
-- --------------------------------------------------------------------------

CREATE TABLE mcp_servers (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                  VARCHAR(255) NOT NULL UNIQUE,
    description           TEXT,
    endpoint_url          VARCHAR(512) NOT NULL,
    transport_type        VARCHAR(50)  NOT NULL DEFAULT 'streamable_http',
    auth_type             VARCHAR(50),
    auth_secret_encrypted BYTEA,
    status                VARCHAR(50) NOT NULL DEFAULT 'pending',
    health_check_interval INTEGER DEFAULT 60,
    last_health_check     TIMESTAMPTZ,
    last_error            TEXT,
    config_json           JSONB DEFAULT '{}',
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE mcp_tools (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    server_id     UUID NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    tool_name     VARCHAR(255) NOT NULL,
    description   TEXT,
    input_schema  JSONB,
    is_active     BOOLEAN NOT NULL DEFAULT TRUE,
    discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(server_id, tool_name)
);

-- --------------------------------------------------------------------------
-- Usage, Analytics & Budget
-- --------------------------------------------------------------------------

CREATE TABLE usage_records (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    api_key_id      UUID REFERENCES api_keys(id),
    user_id         UUID REFERENCES users(id),
    team_id         UUID REFERENCES teams(id),
    provider_id     UUID REFERENCES providers(id),
    model_id        VARCHAR(255) NOT NULL,
    request_type    VARCHAR(50)  NOT NULL,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    total_tokens    INTEGER NOT NULL DEFAULT 0,
    cost_usd        DECIMAL(12, 8) NOT NULL DEFAULT 0,
    latency_ms      INTEGER,
    status_code     INTEGER,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_usage_records_created_at  ON usage_records(created_at);
CREATE INDEX idx_usage_records_user_id     ON usage_records(user_id, created_at);
CREATE INDEX idx_usage_records_team_id     ON usage_records(team_id, created_at);
CREATE INDEX idx_usage_records_api_key_id  ON usage_records(api_key_id, created_at);
CREATE INDEX idx_usage_records_model_id    ON usage_records(model_id, created_at);

-- The legacy `budget_alerts` table and its `BudgetAlertManager`
-- runtime were removed in the limits/quotas refactor — alerts now
-- come from cap crossings on `budget_caps` (or, today, from raw
-- usage_records aggregations on the dashboard). The table is gone
-- because nothing reads it anymore.

-- --------------------------------------------------------------------------
-- Rate limit rules + budget caps
--
-- Generic rule storage for sliding-window rate limits and natural-period
-- budget caps. Both tables are subject-polymorphic — the same engine
-- enforces limits on users, API keys, providers, and MCP servers via
-- the (subject_kind, subject_id) tuple. See plan.md for the full design.
-- --------------------------------------------------------------------------

CREATE TABLE rate_limit_rules (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Who the rule applies to.
    subject_kind VARCHAR(20) NOT NULL
        CHECK (subject_kind IN ('user', 'api_key', 'provider', 'mcp_server')),
    subject_id   UUID NOT NULL,
    -- Which gateway this rule guards. A user can have separate rules
    -- for the AI gateway and the MCP gateway.
    surface      VARCHAR(20) NOT NULL
        CHECK (surface IN ('ai_gateway', 'mcp_gateway')),
    -- What we count: requests (1 per call) or weighted tokens (computed
    -- by `weight.rs` from raw token counts × model multipliers).
    metric       VARCHAR(20) NOT NULL CHECK (metric IN ('requests', 'tokens')),
    -- Sliding window length in seconds. Validated at startup against
    -- the `[60, 60*60*24*7*4]` range — anything outside that is
    -- either too coarse for the bucket scheme or too long to be a
    -- "rate" rather than a budget.
    window_secs  INTEGER NOT NULL CHECK (window_secs > 0),
    -- Threshold inside the window. requests-metric counts whole calls;
    -- tokens-metric counts weighted tokens.
    max_count    BIGINT  NOT NULL CHECK (max_count > 0),
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- One row per (subject, surface, metric, window) — admins enable /
    -- disable via the `enabled` flag rather than re-creating rows.
    UNIQUE(subject_kind, subject_id, surface, metric, window_secs)
);
CREATE INDEX idx_rlr_subject  ON rate_limit_rules(subject_kind, subject_id);
CREATE INDEX idx_rlr_enabled  ON rate_limit_rules(enabled) WHERE enabled = TRUE;

CREATE TABLE budget_caps (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subject_kind VARCHAR(20) NOT NULL
        CHECK (subject_kind IN ('user', 'api_key', 'team', 'provider')),
    subject_id   UUID NOT NULL,
    -- Natural calendar period — counters reset on the period boundary
    -- (system TZ). NOT a sliding window; that's what rate_limit_rules
    -- is for.
    period       VARCHAR(20) NOT NULL
        CHECK (period IN ('daily', 'weekly', 'monthly')),
    -- Threshold in weighted tokens. The UI may display "≈ $X" by
    -- aggregating real `usage_records.cost_usd` for the same period,
    -- but the cap itself is unitless tokens.
    limit_tokens BIGINT  NOT NULL CHECK (limit_tokens > 0),
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(subject_kind, subject_id, period)
);
CREATE INDEX idx_budget_caps_subject ON budget_caps(subject_kind, subject_id);
CREATE INDEX idx_budget_caps_enabled ON budget_caps(enabled) WHERE enabled = TRUE;

-- --------------------------------------------------------------------------
-- MCP Call Logs
-- --------------------------------------------------------------------------

CREATE TABLE mcp_call_logs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    server_id       UUID REFERENCES mcp_servers(id) ON DELETE SET NULL,
    tool_name       VARCHAR(255) NOT NULL,
    user_id         UUID REFERENCES users(id) ON DELETE SET NULL,
    duration_ms     INTEGER,
    status          VARCHAR(50) NOT NULL DEFAULT 'success',
    error_message   TEXT,
    request_payload JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_mcp_call_logs_created_at ON mcp_call_logs(created_at);
CREATE INDEX idx_mcp_call_logs_server_id  ON mcp_call_logs(server_id, created_at);

-- --------------------------------------------------------------------------
-- Log Forwarders
-- --------------------------------------------------------------------------

CREATE TABLE log_forwarders (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name           VARCHAR(255) NOT NULL,
    forwarder_type VARCHAR(50)  NOT NULL,
    config         JSONB NOT NULL DEFAULT '{}',
    enabled        BOOLEAN NOT NULL DEFAULT TRUE,
    sent_count     BIGINT NOT NULL DEFAULT 0,
    error_count    BIGINT NOT NULL DEFAULT 0,
    last_sent_at   TIMESTAMPTZ,
    last_error     TEXT,
    log_types      TEXT[] NOT NULL DEFAULT '{audit}',
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_log_forwarders_enabled ON log_forwarders(enabled);

-- --------------------------------------------------------------------------
-- System Settings (key-value store, managed via Web UI)
-- --------------------------------------------------------------------------

CREATE TABLE system_settings (
    key         VARCHAR(255) PRIMARY KEY,
    value       JSONB NOT NULL,
    category    VARCHAR(100) NOT NULL,
    description TEXT,
    updated_by  UUID REFERENCES users(id),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_system_settings_category ON system_settings(category);

-- Auth
INSERT INTO system_settings (key, value, category, description) VALUES
('auth.jwt_access_ttl_secs',   '900',   'auth', 'JWT access token lifetime in seconds'),
('auth.jwt_refresh_ttl_days',  '7',     'auth', 'JWT refresh token lifetime in days'),
('auth.allow_registration',    'false', 'auth', 'Whether public user self-registration is allowed');

-- Gateway
INSERT INTO system_settings (key, value, category, description) VALUES
('gateway.cache_ttl_secs',       '3600',     'gateway', 'Response cache TTL in seconds'),
('gateway.request_timeout_secs', '120',      'gateway', 'Gateway request timeout (requires restart)'),
('gateway.body_limit_bytes',     '10485760', 'gateway', 'Gateway max request body size (requires restart)');

-- Console
INSERT INTO system_settings (key, value, category, description) VALUES
('console.request_timeout_secs', '30',      'console', 'Console API request timeout (requires restart)'),
('console.body_limit_bytes',     '1048576', 'console', 'Console API max request body size (requires restart)');

-- Security
INSERT INTO system_settings (key, value, category, description) VALUES
('security.signature_nonce_ttl_secs', '600',    'security', 'Request signature nonce TTL in seconds'),
('security.signature_drift_secs',    '300',     'security', 'Maximum allowed clock skew for signatures'),
('security.totp_required',          'false',    'security', 'Require all users to enable TOTP two-factor authentication'),
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
]', 'security', 'PII redactor patterns (JSON array)');

-- Audit
INSERT INTO system_settings (key, value, category, description) VALUES
('audit.batch_size',          '50',    'audit', 'Quickwit batch flush size'),
('audit.flush_interval_secs', '2',     'audit', 'Quickwit batch flush interval in seconds'),
('audit.channel_capacity',    '10000', 'audit', 'Audit log channel buffer capacity');

-- API Keys
INSERT INTO system_settings (key, value, category, description) VALUES
('api_keys.default_expiry_days',         '90', 'api_keys', 'Default API key expiration in days (0 = no expiry)'),
('api_keys.inactivity_timeout_days',     '0',  'api_keys', 'Auto-disable after N days of inactivity (0 = disabled)'),
('api_keys.rotation_period_days',        '0',  'api_keys', 'Auto-rotation period in days (0 = disabled)'),
('api_keys.rotation_grace_period_hours', '24', 'api_keys', 'Grace period for old key after rotation');

-- Data retention — usage records (PostgreSQL) + per-log-type ClickHouse retention.
-- Audit/Gateway/MCP/Platform default to 90 days; Access/App default to 30 days.
-- Changing these via the admin UI issues `ALTER TABLE ... MODIFY TTL` against
-- the corresponding ClickHouse table, so the value here is the seed default only.
INSERT INTO system_settings (key, value, category, description) VALUES
('data.retention_days_usage',    '90', 'data', 'Days to keep usage records in PostgreSQL (0 = forever)'),
('data.retention_days_audit',    '90', 'data', 'Days to keep audit logs in ClickHouse'),
('data.retention_days_gateway',  '90', 'data', 'Days to keep AI gateway request logs in ClickHouse'),
('data.retention_days_mcp',      '90', 'data', 'Days to keep MCP tool invocation logs in ClickHouse'),
('data.retention_days_platform', '90', 'data', 'Days to keep platform management logs in ClickHouse'),
('data.retention_days_access',   '30', 'data', 'Days to keep HTTP access logs in ClickHouse'),
('data.retention_days_app',      '30', 'data', 'Days to keep application runtime logs in ClickHouse');

-- Setup
INSERT INTO system_settings (key, value, category, description) VALUES
('setup.initialized', 'false',         'setup', 'Whether initial setup has been completed'),
('setup.site_name',   '"ThinkWatch"', 'setup', 'Site display name');

-- General — gateway public URL components (used by configuration guide).
-- Empty/zero values mean "auto-detect from the user's browser request".
INSERT INTO system_settings (key, value, category, description) VALUES
('general.public_protocol', '""', 'general', 'Public gateway protocol: "http", "https", or empty for auto-detect from browser'),
('general.public_host',     '""', 'general', 'Public gateway host (empty = auto-detect from browser)'),
('general.public_port',     '0',  'general', 'Public gateway port (0 = use the gateway listening port)');
