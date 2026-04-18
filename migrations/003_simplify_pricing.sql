-- Pricing model v2: platform baseline × per-model weight.
--
-- v1 put concrete $/token on every `models` row alongside the budget
-- multiplier — two fields expressing the same idea in different units.
-- v1 was also never wired up: the gateway cost tracker had a hardcoded
-- price table and ignored `models.input_price` / `output_price` entirely.
--
-- v2:
--   * `platform_pricing` holds a single baseline (input + output $/token).
--   * `models.input_weight` / `output_weight` is the relative factor
--     (replaces `input_multiplier` / `output_multiplier`).
--   * `model_routes.{input,output}_price_override` is a rare escape
--     hatch when a specific provider offers a different absolute rate.
--
-- cost($) = tokens × weight × platform_baseline (+ route override)

-- -- Drop dead per-model prices ------------------------------------------
ALTER TABLE models DROP COLUMN input_price;
ALTER TABLE models DROP COLUMN output_price;

-- -- Rename multiplier → weight (same value, clearer name) ---------------
ALTER TABLE models RENAME COLUMN input_multiplier  TO input_weight;
ALTER TABLE models RENAME COLUMN output_multiplier TO output_weight;

-- -- Platform-wide baseline singleton -----------------------------------
CREATE TABLE platform_pricing (
    id                     SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    input_price_per_token  NUMERIC(20, 10) NOT NULL DEFAULT 0.0000020,
    output_price_per_token NUMERIC(20, 10) NOT NULL DEFAULT 0.0000080,
    currency               TEXT NOT NULL DEFAULT 'USD',
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO platform_pricing (id) VALUES (1);

-- -- Per-route override (null = use baseline × weight) ------------------
ALTER TABLE model_routes
    ADD COLUMN input_price_override  NUMERIC(20, 10),
    ADD COLUMN output_price_override NUMERIC(20, 10);
