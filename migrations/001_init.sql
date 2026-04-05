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
    monthly_budget  DECIMAL(12, 4),
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
-- RBAC — System Roles
-- --------------------------------------------------------------------------

CREATE TABLE roles (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        VARCHAR(100) NOT NULL UNIQUE,
    description TEXT,
    is_system   BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE permissions (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    resource    VARCHAR(100) NOT NULL,
    action      VARCHAR(100) NOT NULL,
    UNIQUE(resource, action)
);

CREATE TABLE role_permissions (
    role_id       UUID NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    permission_id UUID NOT NULL REFERENCES permissions(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, permission_id)
);

CREATE INDEX idx_role_permissions_permission_id ON role_permissions(permission_id);

CREATE TABLE user_roles (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role_id UUID NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    scope   VARCHAR(255) NOT NULL DEFAULT 'global',
    PRIMARY KEY (user_id, role_id, scope)
);

CREATE INDEX idx_user_roles_scope ON user_roles(scope);

INSERT INTO roles (name, description, is_system) VALUES
    ('super_admin',   'Full system access',        TRUE),
    ('admin',         'Administrative access',      TRUE),
    ('team_manager',  'Team management access',     TRUE),
    ('developer',     'Standard developer access',  TRUE),
    ('viewer',        'Read-only access',           TRUE);

-- --------------------------------------------------------------------------
-- RBAC — Custom Roles with IAM Policies
-- --------------------------------------------------------------------------

CREATE TABLE custom_roles (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                VARCHAR(100) NOT NULL UNIQUE,
    description         TEXT,
    is_system           BOOLEAN NOT NULL DEFAULT FALSE,
    allowed_models      TEXT[],
    allowed_mcp_servers UUID[],
    policy_document     JSONB,
    created_by          UUID REFERENCES users(id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE custom_role_permissions (
    custom_role_id UUID NOT NULL REFERENCES custom_roles(id) ON DELETE CASCADE,
    permission     VARCHAR(100) NOT NULL,
    PRIMARY KEY (custom_role_id, permission)
);

CREATE INDEX idx_custom_role_permissions_role ON custom_role_permissions(custom_role_id);

CREATE TABLE user_custom_roles (
    user_id        UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    custom_role_id UUID NOT NULL REFERENCES custom_roles(id) ON DELETE CASCADE,
    assigned_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, custom_role_id)
);

CREATE INDEX idx_user_custom_roles_user ON user_custom_roles(user_id);

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
    scopes                  JSONB NOT NULL DEFAULT '[]',
    allowed_models          TEXT[],
    rate_limit_rpm          INTEGER,
    rate_limit_tpm          INTEGER,
    monthly_budget          DECIMAL(12, 4),
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
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    provider_id   UUID NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id      VARCHAR(255) NOT NULL,
    display_name  VARCHAR(255) NOT NULL,
    input_price   DECIMAL(10, 6),
    output_price  DECIMAL(10, 6),
    is_active     BOOLEAN NOT NULL DEFAULT TRUE,
    UNIQUE(provider_id, model_id)
);

CREATE TABLE model_permissions (
    id        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    model_id  UUID NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    role_id   UUID REFERENCES roles(id) ON DELETE CASCADE,
    team_id   UUID REFERENCES teams(id) ON DELETE CASCADE,
    user_id   UUID REFERENCES users(id) ON DELETE CASCADE,
    allowed   BOOLEAN NOT NULL DEFAULT TRUE,
    CHECK (role_id IS NOT NULL OR team_id IS NOT NULL OR user_id IS NOT NULL)
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

CREATE TABLE mcp_tool_permissions (
    id        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_id   UUID NOT NULL REFERENCES mcp_tools(id) ON DELETE CASCADE,
    role_id   UUID REFERENCES roles(id) ON DELETE CASCADE,
    team_id   UUID REFERENCES teams(id) ON DELETE CASCADE,
    user_id   UUID REFERENCES users(id) ON DELETE CASCADE,
    allowed   BOOLEAN NOT NULL DEFAULT TRUE,
    CHECK (role_id IS NOT NULL OR team_id IS NOT NULL OR user_id IS NOT NULL)
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

CREATE TABLE budget_alerts (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_type   VARCHAR(50)  NOT NULL,
    target_id     UUID NOT NULL,
    threshold     DECIMAL(5, 2) NOT NULL,
    current_spend DECIMAL(12, 4),
    budget_limit  DECIMAL(12, 4),
    notified_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

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
    {"pattern": "ignore previous instructions", "severity": "critical", "category": "instruction_override"},
    {"pattern": "ignore all previous",          "severity": "critical", "category": "instruction_override"},
    {"pattern": "disregard your instructions",  "severity": "critical", "category": "instruction_override"},
    {"pattern": "jailbreak",                    "severity": "critical", "category": "jailbreak"},
    {"pattern": " dan ",                        "severity": "critical", "category": "jailbreak"},
    {"pattern": "developer mode",              "severity": "critical", "category": "jailbreak"},
    {"pattern": "you are now",                 "severity": "high",     "category": "persona_manipulation"},
    {"pattern": "new persona",                 "severity": "high",     "category": "persona_manipulation"},
    {"pattern": "act as",                      "severity": "high",     "category": "persona_manipulation"},
    {"pattern": "pretend to be",               "severity": "high",     "category": "persona_manipulation"},
    {"pattern": "system prompt",               "severity": "medium",   "category": "prompt_extraction"},
    {"pattern": "reveal your instructions",    "severity": "medium",   "category": "prompt_extraction"},
    {"pattern": "what are your rules",         "severity": "medium",   "category": "prompt_extraction"}
]', 'security', 'Content filter deny patterns (JSON array)'),
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

-- Budget
INSERT INTO system_settings (key, value, category, description) VALUES
('budget.alert_thresholds', '[0.50, 0.80, 0.95]', 'budget', 'Budget alert threshold percentages'),
('budget.webhook_url',      'null',                'budget', 'Budget alert webhook URL');

-- API Keys
INSERT INTO system_settings (key, value, category, description) VALUES
('api_keys.default_expiry_days',         '90', 'api_keys', 'Default API key expiration in days (0 = no expiry)'),
('api_keys.inactivity_timeout_days',     '0',  'api_keys', 'Auto-disable after N days of inactivity (0 = disabled)'),
('api_keys.rotation_period_days',        '0',  'api_keys', 'Auto-rotation period in days (0 = disabled)'),
('api_keys.rotation_grace_period_hours', '24', 'api_keys', 'Grace period for old key after rotation');

-- Data retention
INSERT INTO system_settings (key, value, category, description) VALUES
('data.retention_days_usage', '90',  'data', 'Days to keep usage records (0 = forever)'),
('data.retention_days_audit', '365', 'data', 'Days to keep audit logs (0 = forever)');

-- Setup
INSERT INTO system_settings (key, value, category, description) VALUES
('setup.initialized', 'false',         'setup', 'Whether initial setup has been completed'),
('setup.site_name',   '"ThinkWatch"', 'setup', 'Site display name');
