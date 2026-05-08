-- 2026-05-08_mcp_credential_owner_refactor.sql
--
-- Why: the MCP refactor introduces three orthogonal axes
-- (auth_shape, credential_owner, auth_header injection) and removes
-- the conflated `allow_static_token` boolean. `db/schema.sql` is
-- declarative — `CREATE TABLE IF NOT EXISTS` doesn't add new columns
-- to existing rows or DROP the obsolete one — so deployed environments
-- need this one-shot migration to land the ALTERs + backfill.
--
-- Three changes per affected table:
--   1. Add new NOT NULL columns with safe defaults.
--   2. Backfill `auth_shape` from the old (oauth_issuer, allow_static_token)
--      pair so existing rows keep behaving the same.
--   3. DROP `allow_static_token`.
--
-- Plus: create the new `mcp_server_shared_credentials` table for
-- admin_shared credential storage.
--
-- Applied to: dev (2026-05-08), staging (—), prod (—).
--
-- After running this, db/schema.sql already reflects the
-- post-migration state.

BEGIN;

-- ── mcp_servers ──────────────────────────────────────────────────────
ALTER TABLE mcp_servers
  ADD COLUMN IF NOT EXISTS auth_shape          TEXT,
  ADD COLUMN IF NOT EXISTS auth_header_name    TEXT NOT NULL DEFAULT 'Authorization',
  ADD COLUMN IF NOT EXISTS auth_value_template TEXT NOT NULL DEFAULT 'Bearer {{token}}',
  ADD COLUMN IF NOT EXISTS credential_owner    TEXT NOT NULL DEFAULT 'per_user';

-- Backfill: auth_shape derived from the old fields.
UPDATE mcp_servers SET auth_shape = CASE
    WHEN oauth_issuer IS NOT NULL THEN 'oauth'
    WHEN allow_static_token       THEN 'static'
    ELSE                               'anonymous'
  END
  WHERE auth_shape IS NULL;

ALTER TABLE mcp_servers
  ALTER COLUMN auth_shape SET NOT NULL,
  ALTER COLUMN auth_shape SET DEFAULT 'anonymous',
  ADD CONSTRAINT mcp_servers_auth_shape_check
    CHECK (auth_shape IN ('anonymous', 'oauth', 'static')),
  ADD CONSTRAINT mcp_servers_credential_owner_check
    CHECK (credential_owner IN ('per_user', 'admin_shared')),
  DROP COLUMN allow_static_token;

-- ── mcp_store_templates ──────────────────────────────────────────────
ALTER TABLE mcp_store_templates
  ADD COLUMN IF NOT EXISTS auth_shape          TEXT,
  ADD COLUMN IF NOT EXISTS auth_header_name    TEXT NOT NULL DEFAULT 'Authorization',
  ADD COLUMN IF NOT EXISTS auth_value_template TEXT NOT NULL DEFAULT 'Bearer {{token}}';

UPDATE mcp_store_templates SET auth_shape = CASE
    WHEN oauth_issuer IS NOT NULL THEN 'oauth'
    WHEN allow_static_token       THEN 'static'
    ELSE                               'anonymous'
  END
  WHERE auth_shape IS NULL;

ALTER TABLE mcp_store_templates
  ALTER COLUMN auth_shape SET NOT NULL,
  ALTER COLUMN auth_shape SET DEFAULT 'anonymous',
  ADD CONSTRAINT mcp_store_templates_auth_shape_check
    CHECK (auth_shape IN ('anonymous', 'oauth', 'static')),
  DROP COLUMN allow_static_token;

-- ── mcp_server_shared_credentials (new table) ────────────────────────
CREATE TABLE IF NOT EXISTS mcp_server_shared_credentials (
    mcp_server_id            UUID PRIMARY KEY REFERENCES mcp_servers(id) ON DELETE CASCADE,
    credential_type          TEXT NOT NULL
        CHECK (credential_type IN ('oauth_authcode', 'static_token')),
    access_token_encrypted   BYTEA NOT NULL,
    refresh_token_encrypted  BYTEA,
    expires_at               TIMESTAMPTZ,
    scopes                   TEXT[] NOT NULL DEFAULT '{}',
    upstream_subject         TEXT,
    configured_by            UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMIT;
