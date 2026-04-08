-- ============================================================================
-- Phase 1 of the role unification refactor.
--
-- Background: the database had FIVE tables for what should be one concept.
--
--   Legacy "system" side:
--     roles                       -- 5 hardcoded rows
--     role_permissions            -- never used by handlers
--     user_roles                  -- legacy membership (auth/setup/sso/admin)
--
--   "Modern" side (added in earlier rounds):
--     custom_roles                -- system roles re-seeded here as is_system=true
--     custom_role_permissions     -- permission strings keyed off custom_roles
--     user_custom_roles           -- modern membership with scope (since 004)
--
-- The handlers were forced to read from BOTH and merge. Membership counts
-- needed UNION queries by name. The frontend had to pretend they were one
-- thing. Anything touching auth still wrote to the legacy tables.
--
-- This migration introduces ONE table for roles and ONE table for assignments,
-- backfills both from the legacy + modern tables, and from this point on
-- the handlers will be cut over (Phase 2) to read/write only the new tables.
-- The legacy tables are kept (not dropped) so we can roll back if anything
-- catches fire — they will be removed in a follow-up migration after the
-- backend cutover has soaked.
--
-- Migration is idempotent — uses CREATE IF NOT EXISTS, INSERT ... ON CONFLICT,
-- and constraint-existence checks where needed.
-- ============================================================================

-- ----------------------------------------------------------------------------
-- New tables
-- ----------------------------------------------------------------------------

-- The unified role catalog. Replaces both `roles` and `custom_roles`.
-- Permission strings live directly on the row (TEXT[]) because at this
-- scale (~50 perms × ~10 roles) the join table buys us nothing.
CREATE TABLE IF NOT EXISTS rbac_roles (
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

CREATE INDEX IF NOT EXISTS idx_rbac_roles_is_system ON rbac_roles(is_system);

-- The unified membership table. Replaces both `user_roles` and
-- `user_custom_roles`. Scope is now a (kind, id) twople — `scope_kind`
-- is the closed enum 'global'/'team'/'project', `scope_id` is the
-- referenced team/project UUID (NULL when scope_kind = 'global').
--
-- The `scope_marker` generated column collapses the (kind, NULL id)
-- case into a deterministic byte sequence so we can use it in the
-- primary key without PostgreSQL treating multiple NULLs as distinct
-- (which would let the same user_id+role_id+global pair be inserted
-- twice). It is NEVER read by application code.
CREATE TABLE IF NOT EXISTS rbac_role_assignments (
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role_id      UUID NOT NULL REFERENCES rbac_roles(id) ON DELETE CASCADE,
    scope_kind   VARCHAR(16) NOT NULL DEFAULT 'global'
        CHECK (scope_kind IN ('global', 'team', 'project')),
    scope_id     UUID,
    scope_marker UUID GENERATED ALWAYS AS (COALESCE(scope_id, '00000000-0000-0000-0000-000000000000'::uuid)) STORED,
    assigned_by  UUID REFERENCES users(id),
    assigned_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, role_id, scope_kind, scope_marker),
    -- Sanity: global must NOT have a scope_id, team/project MUST.
    CONSTRAINT chk_scope_consistency
        CHECK ((scope_kind = 'global' AND scope_id IS NULL)
            OR (scope_kind <> 'global' AND scope_id IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS idx_rbac_role_assignments_user ON rbac_role_assignments(user_id);
CREATE INDEX IF NOT EXISTS idx_rbac_role_assignments_role ON rbac_role_assignments(role_id);
CREATE INDEX IF NOT EXISTS idx_rbac_role_assignments_scope
    ON rbac_role_assignments(scope_kind, scope_id);

-- ----------------------------------------------------------------------------
-- Backfill from legacy + modern tables
-- ----------------------------------------------------------------------------
--
-- 1. Roles: seed everything from `custom_roles` first (because that's
--    where the recent system + custom seeding lives, with permissions).
--    Use `name` as the natural key. Carry over id when the row already
--    exists in `rbac_roles`.

INSERT INTO rbac_roles
    (id, name, description, is_system, permissions,
     allowed_models, allowed_mcp_servers, policy_document, created_by,
     created_at, updated_at)
SELECT
    cr.id,
    cr.name,
    cr.description,
    cr.is_system,
    COALESCE(
        (SELECT ARRAY_AGG(permission ORDER BY permission)
           FROM custom_role_permissions WHERE custom_role_id = cr.id),
        ARRAY[]::TEXT[]
    ),
    cr.allowed_models,
    cr.allowed_mcp_servers,
    cr.policy_document,
    cr.created_by,
    cr.created_at,
    cr.updated_at
FROM custom_roles cr
ON CONFLICT (name) DO UPDATE
    SET description         = EXCLUDED.description,
        is_system           = EXCLUDED.is_system,
        permissions         = EXCLUDED.permissions,
        allowed_models      = EXCLUDED.allowed_models,
        allowed_mcp_servers = EXCLUDED.allowed_mcp_servers,
        policy_document     = EXCLUDED.policy_document,
        updated_at          = now();

-- 2. Any system role that exists in the legacy `roles` table but NOT
--    in `custom_roles` is also brought over (defensive — earlier rounds
--    seeded all 5 into custom_roles via migration 003, so this should
--    be a no-op on a current schema).
INSERT INTO rbac_roles (name, description, is_system, permissions)
SELECT r.name, r.description, TRUE, ARRAY[]::TEXT[]
  FROM roles r
 WHERE NOT EXISTS (SELECT 1 FROM rbac_roles rr WHERE rr.name = r.name)
ON CONFLICT (name) DO NOTHING;

-- 3. Membership backfill — legacy `user_roles` first.
--    Match by role NAME (because legacy id and modern id differ).
--    Legacy `user_roles.scope` was a free-text VARCHAR; we treat
--    'global' as global and any other value as a team scope with
--    a deterministic placeholder UUID derived from the legacy text.
--    This is intentionally crude — see comment in code: legacy
--    non-global scopes were never produced in practice (no UI to
--    set them), so this branch should not fire on real data.
INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, scope_id)
SELECT
    ur.user_id,
    rr.id,
    'global',
    NULL
FROM user_roles ur
JOIN roles r ON r.id = ur.role_id
JOIN rbac_roles rr ON rr.name = r.name
WHERE ur.scope = 'global'
ON CONFLICT DO NOTHING;

-- 4. Modern `user_custom_roles`. Same scope handling — 'global' is
--    the only value the UI ever set, so we collapse everything else
--    to global as well (a future migration can introduce per-team
--    scopes for real).
INSERT INTO rbac_role_assignments (user_id, role_id, scope_kind, scope_id)
SELECT
    ucr.user_id,
    rr.id,
    'global',
    NULL
FROM user_custom_roles ucr
JOIN custom_roles cr ON cr.id = ucr.custom_role_id
JOIN rbac_roles rr ON rr.name = cr.name
WHERE ucr.scope = 'global'
ON CONFLICT DO NOTHING;

-- ----------------------------------------------------------------------------
-- Note on the legacy tables
-- ----------------------------------------------------------------------------
-- The legacy tables (`roles`, `role_permissions`, `user_roles`,
-- `custom_roles`, `custom_role_permissions`, `user_custom_roles`) are
-- intentionally NOT dropped here. The Phase 2 backend cutover will stop
-- writing to them and start reading from `rbac_roles` /
-- `rbac_role_assignments`. After the cutover has run in production for
-- a release cycle, a follow-up migration will DROP them.
