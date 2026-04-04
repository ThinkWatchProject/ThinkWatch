-- Phase: Platform Enhancements
-- Adds TOTP support, password change enforcement, log type filtering, and custom roles

-- 1. User TOTP & password change fields
ALTER TABLE users
    ADD COLUMN IF NOT EXISTS totp_secret TEXT,
    ADD COLUMN IF NOT EXISTS totp_enabled BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS totp_recovery_codes TEXT,
    ADD COLUMN IF NOT EXISTS password_change_required BOOLEAN NOT NULL DEFAULT false;

-- 2. Log forwarder: selectable log types
ALTER TABLE log_forwarders
    ADD COLUMN IF NOT EXISTS log_types TEXT[] NOT NULL DEFAULT '{audit}';

-- 3. Custom roles
CREATE TABLE IF NOT EXISTS custom_roles (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name VARCHAR(100) NOT NULL UNIQUE,
    description TEXT,
    is_system BOOLEAN NOT NULL DEFAULT false,
    created_by UUID REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS custom_role_permissions (
    custom_role_id UUID NOT NULL REFERENCES custom_roles(id) ON DELETE CASCADE,
    permission VARCHAR(100) NOT NULL,
    PRIMARY KEY (custom_role_id, permission)
);

CREATE TABLE IF NOT EXISTS user_custom_roles (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    custom_role_id UUID NOT NULL REFERENCES custom_roles(id) ON DELETE CASCADE,
    assigned_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, custom_role_id)
);

CREATE INDEX IF NOT EXISTS idx_user_custom_roles_user ON user_custom_roles(user_id);
CREATE INDEX IF NOT EXISTS idx_custom_role_permissions_role ON custom_role_permissions(custom_role_id);

-- 4. System setting: TOTP required policy
INSERT INTO system_settings (key, value, category, description)
VALUES ('security.totp_required', 'false', 'security', 'Require all users to enable TOTP two-factor authentication')
ON CONFLICT (key) DO NOTHING;
