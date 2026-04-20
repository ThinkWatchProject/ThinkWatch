-- ============================================================================
-- 002_limit_overrides.sql
--
-- Adds per-subject temporary override metadata to the existing
-- `rate_limit_rules` and `budget_caps` side tables so operators can
-- tighten or relax a single user's (or api key's) limits with an
-- explicit expiry and audit justification.
--
-- Semantics at request time: for each (subject=user|api_key, surface,
-- metric, window_secs) or (subject, period) key, an active override
-- REPLACES the role-derived value — it can be higher or lower than
-- what roles alone would produce. See `crates/common/src/limits/mod.rs`
-- for the merge function.
--
-- Columns are all NULL-able so the existing role-merge-only behavior
-- keeps working during and after this migration; override read path is
-- opt-in per row (`expires_at > now()` filter does the lifecycle work).
-- ============================================================================

ALTER TABLE rate_limit_rules
    -- Optional expiry. NULL = permanent override (rare, discouraged —
    -- the UI nudges operators toward bounded expiries).
    ADD COLUMN expires_at TIMESTAMPTZ NULL,
    -- Justification text — required by the handler layer whenever
    -- either expires_at OR override fields are supplied. Kept NULL-able
    -- at the DB so existing rows (pre-override) don't need a backfill.
    ADD COLUMN reason     TEXT        NULL,
    -- User who most recently touched the row. ON DELETE SET NULL so
    -- deleting a user doesn't cascade-delete their audit history.
    ADD COLUMN created_by UUID        NULL REFERENCES users(id) ON DELETE SET NULL;

ALTER TABLE budget_caps
    ADD COLUMN expires_at TIMESTAMPTZ NULL,
    ADD COLUMN reason     TEXT        NULL,
    ADD COLUMN created_by UUID        NULL REFERENCES users(id) ON DELETE SET NULL;

-- Partial index feeds the nightly sweep job — only rows that can
-- actually expire are worth scanning.
CREATE INDEX idx_rlr_expires_at
    ON rate_limit_rules(expires_at)
    WHERE expires_at IS NOT NULL;
CREATE INDEX idx_budget_caps_expires_at
    ON budget_caps(expires_at)
    WHERE expires_at IS NOT NULL;
