-- Drop `model_routes.input_price_override` / `output_price_override`.
--
-- These were added in 003_simplify_pricing as an escape hatch for
-- cases where a specific provider's rate didn't match `baseline × weight`.
-- They were never wired up: the CostTracker ignores them, no endpoint
-- reads/writes them, and no UI surfaces them.
--
-- Re-add when there's a real caller. For now, fewer columns = less
-- cognitive load in CRUD handlers.

ALTER TABLE model_routes
    DROP COLUMN input_price_override,
    DROP COLUMN output_price_override;
