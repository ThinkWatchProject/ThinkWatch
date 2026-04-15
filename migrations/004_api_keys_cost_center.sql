-- Cost-center / project tag on API keys — used by the Costs analytics
-- page to group spend by arbitrary customer label (team / project /
-- environment), independent of the user / team membership model.
--
-- Free-form TEXT capped at 64 chars at the API layer. NULL means
-- "untagged" and gets bucketed separately in group-by reports.

ALTER TABLE api_keys
    ADD COLUMN cost_center VARCHAR(64);

-- Partial index so the autocomplete query "distinct tags currently in
-- use" doesn't have to scan every row — most keys are expected to be
-- untagged for a while after this feature ships.
CREATE INDEX idx_api_keys_cost_center
    ON api_keys(cost_center)
    WHERE cost_center IS NOT NULL;
