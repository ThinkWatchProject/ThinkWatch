-- Add scope to user_custom_roles, mirroring the legacy `user_roles.scope`
-- column. Default `'global'` so existing rows continue to mean "all".
--
-- Why: the previous design allowed "user X has role Y", full stop. With
-- a real multi-tenant gateway you need "user X has role Y in scope Z"
-- (e.g. team_manager for the engineering team but viewer for marketing).
-- The legacy `user_roles` table already had this column; the modern
-- `user_custom_roles` did not. This migration brings them in line.
--
-- Migration is safe to re-run: ADD COLUMN IF NOT EXISTS, and the PK
-- swap is wrapped in a CASE so it only runs if the old PK is still in
-- place.

ALTER TABLE user_custom_roles
    ADD COLUMN IF NOT EXISTS scope VARCHAR(255) NOT NULL DEFAULT 'global';

-- Replace the (user_id, custom_role_id) PK with a (user_id, custom_role_id, scope) PK.
-- This is idempotent because we DROP CONSTRAINT IF EXISTS first.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.table_constraints
         WHERE table_name = 'user_custom_roles'
           AND constraint_name = 'user_custom_roles_pkey'
    ) THEN
        ALTER TABLE user_custom_roles DROP CONSTRAINT user_custom_roles_pkey;
        ALTER TABLE user_custom_roles
            ADD CONSTRAINT user_custom_roles_pkey
            PRIMARY KEY (user_id, custom_role_id, scope);
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_user_custom_roles_scope
    ON user_custom_roles(scope);
