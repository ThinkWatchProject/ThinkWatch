-- 2026-05-24: case-insensitive email enforcement
--
-- What changed:
--   `db/schema.sql` adds `CREATE UNIQUE INDEX IF NOT EXISTS
--   idx_users_email_lower ON users (LOWER(email))`, and all
--   application writers (`register`, `setup_initialize`,
--   `admin/users::create_user`, SSO provisioning) now normalize
--   email to ASCII-lowercase before INSERT.
--
-- Why:
--   The previous case-sensitive `UNIQUE (email)` let `Alice@x.com`
--   and `alice@x.com` coexist as two distinct rows, splitting the
--   per-email lockout / rate-limit / audit identity. An attacker
--   could rotate case to brute-force the same human-facing
--   "account" while presenting clean per-bucket counters.
--
-- Risk to existing deployments:
--   If any deployment already has case-only-duplicate emails, the
--   functional index creation in schema.sql will FAIL on the next
--   boot with a unique-violation error and the cluster won't come
--   up. Run THIS file FIRST against the live database, confirm
--   `dedup_candidates` is empty, then deploy the schema change.
--
-- Applied to:
--   - dev:   pending
--   - stage: pending
--   - prod:  pending
--
-- =========================================================
-- Step 1. Detect collisions. If this returns rows, do NOT
-- proceed to the schema deploy until they're resolved by hand.
-- =========================================================

WITH dedup_candidates AS (
    SELECT LOWER(email) AS canonical, array_agg(id ORDER BY created_at) AS ids,
           array_agg(email ORDER BY created_at) AS variants
    FROM users
    WHERE deleted_at IS NULL
    GROUP BY LOWER(email)
    HAVING COUNT(*) > 1
)
SELECT canonical, variants, ids FROM dedup_candidates;

-- =========================================================
-- Step 2. Manual resolution.
--
-- For each row returned by Step 1, the operator must decide
-- which row is the "real" account. Then either:
--
--   (a) Soft-delete the duplicates:
--         UPDATE users SET deleted_at = now(), is_active = false
--         WHERE id IN ('<dup1>', '<dup2>', ...);
--
--   (b) Merge the duplicates into the canonical row first
--       (move api_keys, teams, role_assignments, audit history
--       to the canonical user_id), THEN soft-delete the rest.
--
-- We deliberately do NOT automate merges here — picking the
-- canonical row and re-parenting child records is a business
-- decision (which login did the user actually use?), not a
-- mechanical one.
-- =========================================================

-- =========================================================
-- Step 3. After Step 1 returns 0 rows, lowercase all live
-- emails so writes from the new code don't accidentally
-- create a NEW row for a user whose existing row has
-- mixed-case email. This is safe because the functional
-- index hasn't been created yet — there can't be conflicts.
-- =========================================================

UPDATE users
SET email = LOWER(TRIM(email))
WHERE email <> LOWER(TRIM(email))
  AND deleted_at IS NULL;

-- =========================================================
-- Step 4. Now deploy the schema change. The
-- `CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email_lower`
-- in schema.sql will succeed because every live row is
-- already canonical and Step 1 caught any collisions.
-- =========================================================
