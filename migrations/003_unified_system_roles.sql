-- Unify system roles into the `custom_roles` table.
--
-- Background: the audit found that system roles were defined in TWO places:
--   1. The legacy `roles` table (5 hardcoded rows, never read by the
--      handlers).
--   2. A hardcoded array on the React frontend, with permission strings
--      that did not match anything the backend actually enforced.
--
-- This migration moves the system roles into `custom_roles` (where the
-- handlers already look) with `is_system = TRUE` so:
--   - The unified `/api/admin/roles` endpoint returns them alongside
--     custom roles.
--   - The permission strings finally match the backend catalog.
--   - Operators can SEE what's defined without reading source code.
--   - The handler enforces "can't delete / modify is_system" rules.

INSERT INTO custom_roles (name, description, is_system, created_at, updated_at)
VALUES
    ('super_admin',
     'Full system access. Can manage every resource and inspect every log.',
     TRUE, now(), now()),
    ('admin',
     'Administrative access. Manages providers, MCP servers, API keys, and users.',
     TRUE, now(), now()),
    ('team_manager',
     'Team-level management. Creates API keys for the team, sees the team''s usage and audit trail.',
     TRUE, now(), now()),
    ('developer',
     'Standard developer. Uses the gateway, manages own API keys, sees own usage.',
     TRUE, now(), now()),
    ('viewer',
     'Read-only access. Can browse providers and analytics but not modify anything.',
     TRUE, now(), now())
ON CONFLICT (name) DO UPDATE
    SET description = EXCLUDED.description,
        is_system   = TRUE,
        updated_at  = now();

-- Wipe and re-seed permissions for system roles so the catalog stays in
-- lockstep with the backend code on every migration run.
DELETE FROM custom_role_permissions
 WHERE custom_role_id IN (SELECT id FROM custom_roles WHERE is_system = TRUE);

-- super_admin → everything
INSERT INTO custom_role_permissions (custom_role_id, permission)
SELECT r.id, p.permission
  FROM custom_roles r
  CROSS JOIN (VALUES
    ('ai_gateway:use'), ('mcp_gateway:use'),
    ('api_keys:read'), ('api_keys:create'), ('api_keys:update'), ('api_keys:rotate'), ('api_keys:delete'),
    ('providers:read'), ('providers:create'), ('providers:update'), ('providers:delete'), ('providers:rotate_key'),
    ('mcp_servers:read'), ('mcp_servers:create'), ('mcp_servers:update'), ('mcp_servers:delete'),
    ('users:read'), ('users:create'), ('users:update'), ('users:delete'),
    ('team:read'), ('team:write'),
    ('sessions:revoke'),
    ('roles:read'), ('roles:create'), ('roles:update'), ('roles:delete'),
    ('analytics:read_own'), ('analytics:read_team'), ('analytics:read_all'),
    ('audit_logs:read_own'), ('audit_logs:read_team'), ('audit_logs:read_all'),
    ('logs:read_own'), ('logs:read_team'), ('logs:read_all'),
    ('log_forwarders:read'), ('log_forwarders:write'),
    ('webhooks:read'), ('webhooks:write'),
    ('content_filter:read'), ('content_filter:write'),
    ('pii_redactor:read'), ('pii_redactor:write'),
    ('settings:read'), ('settings:write'),
    ('system:configure_oidc')
  ) AS p(permission)
 WHERE r.name = 'super_admin';

-- admin → everything except dangerous OIDC reconfiguration
INSERT INTO custom_role_permissions (custom_role_id, permission)
SELECT r.id, p.permission
  FROM custom_roles r
  CROSS JOIN (VALUES
    ('ai_gateway:use'), ('mcp_gateway:use'),
    ('api_keys:read'), ('api_keys:create'), ('api_keys:update'), ('api_keys:rotate'), ('api_keys:delete'),
    ('providers:read'), ('providers:create'), ('providers:update'), ('providers:delete'), ('providers:rotate_key'),
    ('mcp_servers:read'), ('mcp_servers:create'), ('mcp_servers:update'), ('mcp_servers:delete'),
    ('users:read'), ('users:create'), ('users:update'),
    ('team:read'), ('team:write'),
    ('sessions:revoke'),
    ('roles:read'), ('roles:create'), ('roles:update'), ('roles:delete'),
    ('analytics:read_all'),
    ('audit_logs:read_all'),
    ('logs:read_all'),
    ('log_forwarders:read'), ('log_forwarders:write'),
    ('webhooks:read'), ('webhooks:write'),
    ('content_filter:read'), ('content_filter:write'),
    ('pii_redactor:read'), ('pii_redactor:write'),
    ('settings:read'), ('settings:write')
  ) AS p(permission)
 WHERE r.name = 'admin';

-- team_manager → team-scoped management; can read team audit/logs (audit
-- finding R6: team managers couldn't see their team's audit log)
INSERT INTO custom_role_permissions (custom_role_id, permission)
SELECT r.id, p.permission
  FROM custom_roles r
  CROSS JOIN (VALUES
    ('ai_gateway:use'), ('mcp_gateway:use'),
    ('api_keys:read'), ('api_keys:create'), ('api_keys:update'), ('api_keys:rotate'),
    ('providers:read'),
    ('mcp_servers:read'),
    ('users:read'),
    ('team:read'), ('team:write'),
    ('analytics:read_team'),
    ('audit_logs:read_team'),
    ('logs:read_team')
  ) AS p(permission)
 WHERE r.name = 'team_manager';

-- developer → use gateway, manage own keys, read own usage. Audit found
-- developer was missing api_keys:create — they couldn't self-serve.
INSERT INTO custom_role_permissions (custom_role_id, permission)
SELECT r.id, p.permission
  FROM custom_roles r
  CROSS JOIN (VALUES
    ('ai_gateway:use'), ('mcp_gateway:use'),
    ('api_keys:read'), ('api_keys:create'), ('api_keys:update'),
    ('providers:read'),
    ('mcp_servers:read'),
    ('analytics:read_own'),
    ('audit_logs:read_own'),
    ('logs:read_own')
  ) AS p(permission)
 WHERE r.name = 'developer';

-- viewer → strict read-only across the surface a non-admin can see.
-- Audit found viewer was missing mcp_servers:read while developer had it,
-- which was inconsistent with "viewer is read-only over everything
-- developer can interact with".
INSERT INTO custom_role_permissions (custom_role_id, permission)
SELECT r.id, p.permission
  FROM custom_roles r
  CROSS JOIN (VALUES
    ('api_keys:read'),
    ('providers:read'),
    ('mcp_servers:read'),
    ('analytics:read_own'),
    ('audit_logs:read_own'),
    ('logs:read_own')
  ) AS p(permission)
 WHERE r.name = 'viewer';
