-- 013: Add missing indexes and API key lifecycle constraints
--
-- Fixes:
-- 1. Missing indexes on frequently queried columns
-- 2. API key lifecycle state machine consistency CHECK constraints

-- Missing indexes for hot query paths
CREATE INDEX IF NOT EXISTS idx_team_members_user_id ON team_members (user_id);
CREATE INDEX IF NOT EXISTS idx_role_permissions_permission_id ON role_permissions (permission_id);
CREATE INDEX IF NOT EXISTS idx_user_roles_scope ON user_roles (scope);

-- API key lifecycle: ensure state machine consistency
-- If rotated_from_id is set, grace_period_ends_at must also be set
ALTER TABLE api_keys
    ADD CONSTRAINT chk_api_key_rotation_consistency
    CHECK (
        (rotated_from_id IS NULL)
        OR (rotated_from_id IS NOT NULL AND grace_period_ends_at IS NOT NULL)
    );

-- Prevent negative rotation/grace period values
ALTER TABLE api_keys
    ADD CONSTRAINT chk_api_key_rotation_period_positive
    CHECK (rotation_period_days IS NULL OR rotation_period_days > 0);

ALTER TABLE api_keys
    ADD CONSTRAINT chk_api_key_inactivity_timeout_positive
    CHECK (inactivity_timeout_days IS NULL OR inactivity_timeout_days >= 0);
