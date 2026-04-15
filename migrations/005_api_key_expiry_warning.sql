-- api_keys.last_expiry_warning_day — idempotency guard for the
-- `key.expiry_warning` background task. The task runs hourly; without
-- a dedupe column every warning threshold (7 / 3 / 1 day remaining)
-- would fire 24 times per day per key.
--
-- We store the *remaining-days bucket* most recently warned about
-- (7 / 3 / 1), and only emit when the key enters a lower bucket than
-- the one already recorded. Null means "never warned".
ALTER TABLE api_keys
    ADD COLUMN last_expiry_warning_days INTEGER;
