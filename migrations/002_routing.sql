-- Routing strategy + circuit-breaker schema
--
-- Adds per-model strategy/affinity overrides and per-route capacity
-- caps. Global defaults + circuit-breaker tunables live in
-- system_settings (gateway category) so they're editable in the
-- admin UI without a deploy.
--
-- Strategy semantics — see crates/gateway/src/strategy.rs:
--   weighted      — operator-set `weight` column = traffic ratios
--                   (the wizard's "manual mode")
--   latency       — weight ∝ 1/latency_ms^k (EWMA), k from
--                   gateway.latency_strategy_k
--   cost          — weight ∝ 1/effective_cost_per_token
--   latency_cost  — combined latency × cost score (default — closest
--                   to "do the right thing" with zero configuration)
--
-- Affinity modes — see crates/gateway/src/proxy.rs:
--   none      — stateless selection per request
--   provider  — sticky to provider_id (preserves prompt-cache hit
--               rate when a provider serves multiple upstream models)
--   route     — sticky to a specific route_id (forces strict A/B
--               adherence to one upstream within a session)
--
-- All three model-level overrides are NULLABLE — NULL means "fall
-- through to gateway.default_*". The "global default + Model
-- override" pattern; per-model rows only diverge from the global
-- when an operator explicitly sets them.

ALTER TABLE models
    ADD COLUMN routing_strategy   TEXT,
    ADD COLUMN affinity_mode      TEXT,
    ADD COLUMN affinity_ttl_secs  INT,
    -- Free-form admin tags. Surfaced as chip badges in the model
    -- detail UI; ignored at the routing layer.
    ADD COLUMN tags               TEXT[],
    ADD CONSTRAINT models_routing_strategy_chk
        CHECK (routing_strategy IS NULL OR routing_strategy IN
              ('weighted', 'latency', 'cost', 'latency_cost')),
    ADD CONSTRAINT models_affinity_mode_chk
        CHECK (affinity_mode IS NULL OR affinity_mode IN ('none', 'provider', 'route')),
    ADD CONSTRAINT models_affinity_ttl_chk
        CHECK (affinity_ttl_secs IS NULL OR affinity_ttl_secs BETWEEN 0 AND 86400);

-- Per-route capacity caps. NULL = unlimited. Enforced in the gateway
-- selection path: routes at cap are filtered out of the candidate set
-- the same way circuit-broken routes are, then the strategy picks
-- among the remainder.
ALTER TABLE model_routes
    ADD COLUMN rpm_cap INT,
    ADD COLUMN tpm_cap INT,
    ADD CONSTRAINT model_routes_rpm_cap_chk CHECK (rpm_cap IS NULL OR rpm_cap > 0),
    ADD CONSTRAINT model_routes_tpm_cap_chk CHECK (tpm_cap IS NULL OR tpm_cap > 0);

-- Global defaults + circuit-breaker tunables. Idempotent ON
-- CONFLICT keeps re-runs of the init script harmless on upgraded
-- deployments where an operator may have edited values already.
INSERT INTO system_settings (key, value, category, description) VALUES
    ('gateway.default_routing_strategy', '"latency_cost"', 'gateway',
     'Default routing strategy for models that do not override (weighted/latency/cost/latency_cost)'),
    ('gateway.default_affinity_mode',    '"provider"', 'gateway',
     'Default session affinity mode (none/provider/route)'),
    ('gateway.default_affinity_ttl_secs','300',        'gateway',
     'Default affinity key TTL in seconds (0-86400)'),
    ('gateway.latency_strategy_k',       '2.0',        'gateway',
     'Exponent for the latency-strategy weighting (higher = more aggressive). Default 2.0 (aggressive).'),
    ('gateway.cb_enabled',               'true',       'gateway',
     'Enable circuit-breaker: routes exceeding the error threshold are temporarily excluded from selection'),
    ('gateway.cb_error_pct',             '50',         'gateway',
     'Circuit-breaker error rate threshold (percent, 1-100). Routes above this rate trip open'),
    ('gateway.cb_min_samples',           '10',         'gateway',
     'Minimum sample count in the rolling window before the circuit-breaker can trip (avoids tripping on a handful of errors)'),
    ('gateway.cb_window_secs',           '60',         'gateway',
     'Rolling window length in seconds for circuit-breaker error-rate computation'),
    ('gateway.cb_open_secs',             '30',         'gateway',
     'How long a tripped (open) circuit stays open before transitioning to half-open (probe) state')
ON CONFLICT (key) DO NOTHING;
