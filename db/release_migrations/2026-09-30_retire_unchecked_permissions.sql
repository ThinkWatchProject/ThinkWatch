-- 2026-09-30: team_manager gets teams:read; retire permissions nothing checks
--
-- What changed:
--   * The seeded `team_manager` role granted `team:read` / `team:write`,
--     but the team handlers check `teams:read` (list a team, its roster,
--     its roles). A team manager could not open the team they manage.
--     The seed now grants `teams:read` instead.
--   * `team:read`, `team:write`, `logs:read_own`, `logs:read_team`,
--     `audit_logs:read_own`, `audit_logs:read_team` and
--     `audit_logs:read_all` were in the permission catalog and in the
--     seeded roles, but no handler ever checked them (every log endpoint,
--     audit logs included, is gated on `logs:read_all` at global scope).
--     They are gone from the catalog and from the seeds.
--
-- Why this file:
--   `db/seeds.sql` only inserts missing roles, so an existing database
--   keeps the old system-role policies. The server still boots with them
--   (the startup check logs a warning for retired keys instead of
--   failing), but team managers stay unable to read their team until the
--   policy is updated. This file:
--     1. adds `teams:read` to `team_manager`'s Allow statements that still
--        carry the legacy `team:read` (a role an operator already edited
--        to drop `team:read` is left alone);
--     2. removes the retired keys from every role, system and custom.
--   Removing them changes no access — nothing checked them.
--   Clicking "Reset to defaults" on a system role in the console has the
--   same effect for that role.
--
--   Re-running is a no-op: after step 2 no role names `team:read`, so
--   step 1 matches nothing, and step 2 matches nothing.
--
--   The rewrite does not add rows to rbac_role_history. Each user's
--   permission set is cached in Redis for 60 seconds, so team managers
--   see `teams:read` within a minute of the commit.
--
-- Applied to:
--   - dev:   pending
--   - stage: pending
--   - prod:  pending

BEGIN;

-- Step 1. team_manager: team:read -> teams:read.
UPDATE rbac_roles r
   SET policy_document = jsonb_set(
           r.policy_document,
           '{Statement}',
           (SELECT jsonb_agg(
                       CASE
                           WHEN stmt->>'Effect' = 'Allow'
                            AND jsonb_typeof(stmt->'Action') = 'array'
                            AND stmt->'Action' ? 'team:read'
                            AND NOT stmt->'Action' ? 'teams:read'
                           THEN jsonb_set(stmt, '{Action}', (stmt->'Action') || '["teams:read"]'::jsonb)
                           ELSE stmt
                       END
                       ORDER BY ord)
              FROM jsonb_array_elements(r.policy_document->'Statement')
                   WITH ORDINALITY AS s(stmt, ord))),
       updated_at = now()
 WHERE r.name = 'team_manager'
   AND r.is_system
   AND jsonb_typeof(r.policy_document->'Statement') = 'array'
   AND jsonb_path_exists(r.policy_document, '$.Statement[*].Action[*] ? (@ == "team:read")');

-- Step 2. Strip the retired keys from every role's Action arrays.
UPDATE rbac_roles r
   SET policy_document = jsonb_set(
           r.policy_document,
           '{Statement}',
           (SELECT jsonb_agg(
                       CASE
                           WHEN jsonb_typeof(stmt->'Action') = 'array'
                           THEN jsonb_set(
                                    stmt,
                                    '{Action}',
                                    (SELECT COALESCE(jsonb_agg(a ORDER BY o), '[]'::jsonb)
                                       FROM jsonb_array_elements(stmt->'Action')
                                            WITH ORDINALITY AS x(a, o)
                                      WHERE a #>> '{}' NOT IN (
                                                'team:read', 'team:write',
                                                'logs:read_own', 'logs:read_team',
                                                'audit_logs:read_own', 'audit_logs:read_team',
                                                'audit_logs:read_all')))
                           ELSE stmt
                       END
                       ORDER BY ord)
              FROM jsonb_array_elements(r.policy_document->'Statement')
                   WITH ORDINALITY AS s(stmt, ord))),
       updated_at = now()
 WHERE jsonb_typeof(r.policy_document->'Statement') = 'array'
   AND jsonb_path_exists(
           r.policy_document,
           '$.Statement[*].Action[*] ? (@ == "team:read" || @ == "team:write"
                                        || @ == "logs:read_own" || @ == "logs:read_team"
                                        || @ == "audit_logs:read_own" || @ == "audit_logs:read_team"
                                        || @ == "audit_logs:read_all")');

-- Check: no role should list here.
SELECT name, stmt->'Action' AS actions
  FROM rbac_roles, jsonb_array_elements(policy_document->'Statement') AS stmt
 WHERE jsonb_path_exists(stmt, '$.Action[*] ? (@ like_regex "^(team:(read|write)|logs:read_(own|team)|audit_logs:)")');

COMMIT;
