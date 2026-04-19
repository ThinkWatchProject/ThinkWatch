-- Drop `models.is_active`.
--
-- The column was decorative: the gateway's route selector looks at
-- `model_routes.enabled` and `providers.is_active`, never at
-- `models.is_active`. Keeping the column around created a false
-- "master switch" for a model that nothing actually enforced.
--
-- Going forward the answer to "can clients reach this model?" is
-- purely a property of its routes. Retiring a model = disable all its
-- routes (or delete the row outright).

ALTER TABLE models DROP COLUMN is_active;
