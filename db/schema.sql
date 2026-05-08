-- ============================================================================
-- ThinkWatch — Database Schema (declarative, idempotent)
--
-- This file is the SOURCE OF TRUTH for the database structure. Edit it
-- in place when the schema changes; the application calls
-- `sqlx::raw_sql(include_str!("../../../db/schema.sql"))` on every
-- boot, and every statement here is wrapped in `IF NOT EXISTS` /
-- `OR REPLACE` so a re-run is a no-op on an up-to-date DB.
--
-- Limits of declarative apply:
--   * column rename, type narrowing, or DROP COLUMN need an explicit
--     one-off SQL kept in `db/release_migrations/` and run by hand.
--   * data backfills (UPDATE ... SET ...) are never idempotent in a
--     useful way; same escape hatch.
--
-- Time convention: every TIMESTAMPTZ written/read in UTC. ClickHouse
-- side mirrors as `DateTime64(3, 'UTC')` (deploy/clickhouse/initdb.d/
-- 01_init.sql). chrono::Utc::now() is the canonical source.
-- ============================================================================

-- ============================================================================
-- ThinkWatch — Consolidated Schema
--
-- Time convention: every timestamp column is TIMESTAMPTZ and the server
-- always writes and reads in UTC. The ClickHouse side mirrors this
-- explicitly as `DateTime64(3, 'UTC')` (see deploy/clickhouse/initdb.d/
-- 01_init.sql). The application layer MUST NOT assume the session or
-- OS timezone — chrono::Utc::now() is the canonical source, and all
-- formatted output converts to UTC before display unless the UI opts
-- into a user-local offset.
-- ============================================================================

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

-- --------------------------------------------------------------------------
-- Users & Teams
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS users (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email                   VARCHAR(255) NOT NULL UNIQUE,
    display_name            VARCHAR(255) NOT NULL,
    password_hash           VARCHAR(255),
    oidc_subject            VARCHAR(255),
    oidc_issuer             VARCHAR(512),
    avatar_url              TEXT,
    is_active               BOOLEAN NOT NULL DEFAULT TRUE,
    totp_secret             TEXT,
    totp_enabled            BOOLEAN NOT NULL DEFAULT FALSE,
    totp_recovery_codes     TEXT,
    password_change_required BOOLEAN NOT NULL DEFAULT FALSE,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at              TIMESTAMPTZ,
    UNIQUE(oidc_subject, oidc_issuer),
    -- Every user must have at least one auth method: a password hash or
    -- an OIDC identity. Without this check, an API misuse (or handler
    -- bug) could create a row that can never log in — visible in the
    -- admin list but silently unusable.
    CONSTRAINT chk_users_auth_method
        CHECK (password_hash IS NOT NULL OR oidc_subject IS NOT NULL),
    -- OIDC identity is either fully present or fully absent. Half-set
    -- rows ({issuer='x', subject=NULL}) wouldn't be identifiable under
    -- the OIDC spec and would slip through the UNIQUE(subject, issuer)
    -- constraint because Postgres treats NULLs as distinct — allowing
    -- unlimited half-configured "users" against the same issuer.
    CONSTRAINT chk_users_oidc_pair
        CHECK ((oidc_subject IS NULL) = (oidc_issuer IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_users_not_deleted ON users(created_at) WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS teams (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name            VARCHAR(255) NOT NULL UNIQUE,
    description     TEXT,
    -- Budget caps live in `budget_caps` (subject_kind = 'team').
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS team_members (
    user_id   UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    team_id   UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, team_id)
);

CREATE INDEX IF NOT EXISTS idx_team_members_user_id ON team_members(user_id);
CREATE INDEX IF NOT EXISTS idx_team_members_team_id ON team_members(team_id);

-- --------------------------------------------------------------------------
-- RBAC — Unified roles + assignments
--
-- One table for the role catalog (system + custom), one table for
-- (user, role, scope) memberships. Permission strings live directly on
-- the role row as TEXT[] — at this scale (~50 perms × ~10 roles) the
-- join table buys nothing.
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS rbac_roles (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                VARCHAR(100) NOT NULL UNIQUE,
    description         TEXT,
    is_system           BOOLEAN NOT NULL DEFAULT FALSE,
    policy_document     JSONB NOT NULL DEFAULT '{"Version":"2024-01-01","Statement":[]}',
    created_by          UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_rbac_roles_is_system ON rbac_roles(is_system);

-- Scope is a (kind, id) twople. `scope_marker` collapses the
-- (kind, NULL id) case into a deterministic UUID so the primary key
-- treats two global assignments as duplicates (PostgreSQL would
-- otherwise consider multiple NULLs distinct). It is never read by
-- application code.
--
-- Two scope kinds:
--   * 'global'  — applies platform-wide. scope_id IS NULL.
--   * 'team'    — applies only when the target subject (user, api_key,
--                 limits row, ...) belongs to that team. scope_id is
--                 the team UUID.
CREATE TABLE IF NOT EXISTS rbac_role_assignments (
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role_id      UUID NOT NULL REFERENCES rbac_roles(id) ON DELETE CASCADE,
    scope_kind   VARCHAR(16) NOT NULL DEFAULT 'global'
        CHECK (scope_kind IN ('global', 'team')),
    scope_id     UUID REFERENCES teams(id) ON DELETE CASCADE,
    scope_marker UUID GENERATED ALWAYS AS (COALESCE(scope_id, '00000000-0000-0000-0000-000000000000'::uuid)) STORED,
    assigned_by  UUID REFERENCES users(id) ON DELETE SET NULL,
    assigned_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, role_id, scope_kind, scope_marker),
    CONSTRAINT chk_scope_consistency
        CHECK ((scope_kind = 'global' AND scope_id IS NULL)
            OR (scope_kind = 'team'   AND scope_id IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS idx_rbac_role_assignments_user  ON rbac_role_assignments(user_id);
CREATE INDEX IF NOT EXISTS idx_rbac_role_assignments_role  ON rbac_role_assignments(role_id);
CREATE INDEX IF NOT EXISTS idx_rbac_role_assignments_scope ON rbac_role_assignments(scope_kind, scope_id);

-- Roles assigned to a team. All team members automatically inherit
-- the permissions of these roles. Works like permission groups —
-- adding a role here grants it to every current and future member.
CREATE TABLE IF NOT EXISTS team_role_assignments (
    team_id     UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    role_id     UUID NOT NULL REFERENCES rbac_roles(id) ON DELETE CASCADE,
    assigned_by UUID REFERENCES users(id) ON DELETE SET NULL,
    assigned_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (team_id, role_id)
);

CREATE INDEX IF NOT EXISTS idx_team_role_assignments_role ON team_role_assignments(role_id);
-- The PK on (team_id, role_id) already supports a team_id-prefix
-- lookup, but an explicit single-column index makes the planner
-- consistently prefer it for the common "list roles on team X"
-- read path and keeps it available after any PK reshuffle.
CREATE INDEX IF NOT EXISTS idx_team_role_assignments_team ON team_role_assignments(team_id);

-- --------------------------------------------------------------------------
-- API Keys
-- --------------------------------------------------------------------------

-- Canonical list of surfaces an API key can be authorised for. Lifted
-- out of the inline ARRAY literal so adding a new surface is one
-- INSERT row instead of a CHECK rewrite + handler edit; the trigger
-- below enforces api_keys.surfaces against it.
CREATE TABLE IF NOT EXISTS api_key_surface_kinds (
    name         VARCHAR(20)  PRIMARY KEY,
    display_name VARCHAR(100) NOT NULL,
    description  TEXT         NOT NULL
);

CREATE TABLE IF NOT EXISTS api_keys (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    key_prefix              VARCHAR(16)  NOT NULL,
    key_hash                VARCHAR(255) NOT NULL,
    name                    VARCHAR(255) NOT NULL,
    user_id                 UUID REFERENCES users(id) ON DELETE SET NULL,
    -- Which gateways this key can call. Non-empty subset of rows in
    -- `api_key_surface_kinds`. Both gateways share the same `tw-`
    -- token format and the same auth middleware; `surfaces` is what
    -- determines which one a given key is allowed to hit at request
    -- time. The trigger `trg_api_keys_surfaces_valid` validates each
    -- element against the lookup table below.
    surfaces                TEXT[] NOT NULL
        CHECK (cardinality(surfaces) > 0),
    allowed_models          TEXT[],
    -- MCP tool allow-list. Parallel to `allowed_models` but for the
    -- MCP gateway surface. Entries are namespaced tool keys (e.g.
    -- `github__list_issues`) or per-server wildcards (`github__*`).
    -- Request-time policy = intersection(api_key.allowed_mcp_tools,
    -- role_merged.allowed_mcp_tools) — the key can only narrow what
    -- the bearer's roles already grant.
    allowed_mcp_tools       TEXT[],
    -- Per-server account override for MCP per-user credentials. JSON
    -- map of `{ "<mcp_server_uuid>": "<account_label>" }`. When this
    -- key calls an MCP tool the gateway uses the override (instead of
    -- the user's `is_default` credential) to pick which connected
    -- account's token to inject upstream. Empty `{}` ⇒ always default.
    mcp_account_overrides   JSONB NOT NULL DEFAULT '{}',
    -- Rate limits and budget caps live in `rate_limit_rules` /
    -- `budget_caps` (subject_kind = 'api_key_lineage', subject_id =
    -- this row's `lineage_id`) so they survive rotation — every
    -- generation in the chain shares one lineage_id and is bound by
    -- the same rules without copy-forward.
    cost_center             VARCHAR(64),
    expires_at              TIMESTAMPTZ,
    last_expiry_warning_days INTEGER,
    is_active               BOOLEAN NOT NULL DEFAULT TRUE,
    last_used_at            TIMESTAMPTZ,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at              TIMESTAMPTZ,
    -- Lifecycle
    rotation_period_days    INTEGER,
    rotated_from_id         UUID REFERENCES api_keys(id) ON DELETE SET NULL,
    grace_period_ends_at    TIMESTAMPTZ,
    inactivity_timeout_days INTEGER,
    disabled_reason         VARCHAR(100),
    last_rotation_at        TIMESTAMPTZ,
    -- Stable identity that survives rotation. On INSERT, brand-new
    -- keys self-reference (lineage_id = id); rotated keys inherit
    -- the parent's lineage_id. Every row in a rotation chain shares
    -- the same value, so analytics can roll up "this logical key"
    -- across generations without recursively walking
    -- `rotated_from_id`.
    lineage_id              UUID NOT NULL DEFAULT gen_random_uuid(),
    -- Constraints
    -- Note: grace_period_ends_at lives on the OLD key (to schedule its
    -- retirement), while rotated_from_id lives on the NEW key (to record
    -- its lineage). They're never set on the same row, so no cross-column
    -- consistency constraint applies here.
    CONSTRAINT chk_api_key_rotation_period_positive
        CHECK (rotation_period_days IS NULL OR rotation_period_days > 0),
    CONSTRAINT chk_api_key_inactivity_timeout_positive
        CHECK (inactivity_timeout_days IS NULL OR inactivity_timeout_days >= 0)
);

-- UNIQUE: key_hash is the SHA-256 hash of the plaintext API key. A
-- collision would collapse two distinct credentials into a single
-- auth row (identity confusion + non-deterministic revoke). Enforce
-- uniqueness at the DB level so a bug in the key generator can't
-- silently create duplicates.
CREATE UNIQUE INDEX IF NOT EXISTS idx_api_keys_key_hash ON api_keys(key_hash);
CREATE INDEX IF NOT EXISTS idx_api_keys_key_prefix  ON api_keys(key_prefix);
CREATE INDEX IF NOT EXISTS idx_api_keys_is_active   ON api_keys(is_active)  WHERE is_active = true;
CREATE INDEX IF NOT EXISTS idx_api_keys_expires_at  ON api_keys(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_api_keys_cost_center ON api_keys(cost_center) WHERE cost_center IS NOT NULL;
-- lineage roll-up: "show me every key in the same rotation chain"
-- runs as a single index lookup. Without it the analytics
-- WHERE-clause scans the table for every per-key view.
CREATE INDEX IF NOT EXISTS idx_api_keys_lineage_id  ON api_keys(lineage_id);
-- Covers the per-user listing predicate used by list_keys: filters on
-- user_id + deleted_at IS NULL in one range lookup, with created_at DESC
-- preserving the paginated scan order the handlers emit.
CREATE INDEX IF NOT EXISTS idx_api_keys_user_not_deleted
    ON api_keys (user_id, created_at DESC)
    WHERE deleted_at IS NULL;
DROP TRIGGER IF EXISTS trg_api_keys_surfaces_valid ON api_keys;

-- Validate api_keys.surfaces against the api_key_surface_kinds lookup.
-- Done via trigger rather than FK because PG doesn't support FKs on
-- array elements; the trigger only runs on insert/update, so the cost
-- is limited to key lifecycle operations (not the hot auth path).
CREATE OR REPLACE FUNCTION validate_api_key_surfaces() RETURNS trigger AS $$
DECLARE
    unknown TEXT;
BEGIN
    SELECT s INTO unknown
    FROM unnest(NEW.surfaces) s
    WHERE s NOT IN (SELECT name FROM api_key_surface_kinds)
    LIMIT 1;
    IF unknown IS NOT NULL THEN
        RAISE EXCEPTION 'Unknown api_keys.surfaces value: %', unknown
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_api_keys_surfaces_valid
    BEFORE INSERT OR UPDATE OF surfaces ON api_keys
    FOR EACH ROW
    EXECUTE FUNCTION validate_api_key_surfaces();

-- --------------------------------------------------------------------------
-- Providers & Models
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS providers (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name              VARCHAR(100) NOT NULL,
    display_name      VARCHAR(255) NOT NULL,
    provider_type     VARCHAR(50)  NOT NULL,
    base_url          VARCHAR(512) NOT NULL,
    is_active         BOOLEAN NOT NULL DEFAULT TRUE,
    config_json       JSONB DEFAULT '{}',
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at        TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_providers_not_deleted ON providers(created_at) WHERE deleted_at IS NULL;

-- Exposed catalog of model IDs clients can call via `/v1/models`.
-- Standalone entities — not tied to a single provider; routing to
-- providers happens in `model_routes`. Per-model `input_weight` /
-- `output_weight` scale the platform-wide baseline (`platform_pricing`)
-- for both cost reporting and weighted-token quota accounting.
CREATE TABLE IF NOT EXISTS models (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    model_id          VARCHAR(255) NOT NULL UNIQUE,
    display_name      VARCHAR(255) NOT NULL,
    input_weight      DECIMAL(8, 4) NOT NULL DEFAULT 1.0 CHECK (input_weight  > 0),
    output_weight     DECIMAL(8, 4) NOT NULL DEFAULT 1.0 CHECK (output_weight > 0),
    -- Per-model overrides. NULL ⇒ "fall through to gateway.default_*".
    -- Strategy semantics — see crates/gateway/src/strategy.rs:
    --   weighted       — operator-set weight = traffic ratio (manual)
    --   latency        — w ∝ 1/latency_ms^k (EWMA from health.rs)
    --   health         — w ∝ success_rate^k (1 − error_pct/100)
    --   latency_health — combined latency × health (default)
    -- All `^k` use gateway.latency_strategy_k.
    routing_strategy  TEXT
        CHECK (routing_strategy IS NULL OR routing_strategy IN
              ('weighted', 'latency', 'health', 'latency_health')),
    -- Affinity modes — see crates/gateway/src/proxy.rs:
    --   none      — stateless; strategy decides every request
    --   provider  — sticky to provider_id (preserves prompt-cache)
    --   route     — sticky to a specific route_id (strict A/B)
    affinity_mode     TEXT
        CHECK (affinity_mode IS NULL OR affinity_mode IN ('none', 'provider', 'route')),
    affinity_ttl_secs INT
        CHECK (affinity_ttl_secs IS NULL OR affinity_ttl_secs BETWEEN 0 AND 86400),
    -- Free-form admin tags. Surfaced as chip badges in the model
    -- detail UI; ignored at the routing layer.
    tags              TEXT[],
    -- Model-level kill switch. FALSE ⇒ all routes for this model are
    -- skipped at router-bootstrap time (the gateway behaves as if no
    -- routes exist for the model_id). Per-route `enabled` toggles are
    -- preserved across model disable/re-enable, so flipping this back
    -- on restores the previous traffic split exactly.
    enabled           BOOLEAN NOT NULL DEFAULT TRUE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Platform-wide per-token pricing baseline. Single-row singleton
-- (PK pinned to 1 via CHECK). `cost($) = tokens × weight × baseline`.
CREATE TABLE IF NOT EXISTS platform_pricing (
    id                     SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    input_price_per_token  NUMERIC(20, 10) NOT NULL DEFAULT 0.0000020,
    output_price_per_token NUMERIC(20, 10) NOT NULL DEFAULT 0.0000080,
    currency               TEXT NOT NULL DEFAULT 'USD',
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Routes map models to providers with traffic splitting + failover.
-- A single (model_id, provider_id) pair may have multiple routes
-- distinguished by upstream_model — e.g. one catalog entry served by
-- two different upstream models from the same aggregator.
CREATE TABLE IF NOT EXISTS model_routes (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    model_id        VARCHAR(255) NOT NULL REFERENCES models(model_id) ON DELETE CASCADE,
    provider_id     UUID NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    -- Upstream model name sent to the provider. Defaults to model_id
    -- (the exposed name) when the admin doesn't override; aggregator
    -- routes set it to the provider's catalog name (e.g. exposing
    -- "gpt-4o" via OpenRouter where the upstream is "openai/gpt-4o").
    upstream_model  VARCHAR(255) NOT NULL,
    -- Traffic weight; meaning depends on the model's routing strategy.
    -- Under `weighted` it's a direct ratio; under the auto strategies
    -- it's a multiplicative bias on the strategy-derived score (so an
    -- operator can still skew 2:1 toward Provider A even with health
    -- /latency tuning). Failover is handled implicitly by the circuit
    -- breaker — there is no priority tier.
    weight          INTEGER NOT NULL DEFAULT 100 CHECK (weight >= 0),
    -- Optional human-readable identifiers shown in the admin UI
    -- (e.g. "EU-primary", "GPU-cluster-A"). Pure metadata.
    label           VARCHAR(64),
    notes           TEXT,
    -- Per-route capacity caps. NULL = unlimited. Enforced in the
    -- gateway selection path: routes at cap are filtered out the
    -- same way circuit-broken routes are.
    rpm_cap         INTEGER CHECK (rpm_cap IS NULL OR rpm_cap > 0),
    tpm_cap         INTEGER CHECK (tpm_cap IS NULL OR tpm_cap > 0),
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (model_id, provider_id, upstream_model)
);

CREATE INDEX IF NOT EXISTS idx_model_routes_model ON model_routes(model_id);
CREATE INDEX IF NOT EXISTS idx_model_routes_provider ON model_routes(provider_id);

-- --------------------------------------------------------------------------
-- MCP Servers & Tools
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS mcp_servers (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name                  VARCHAR(255) NOT NULL UNIQUE,
    -- Short identifier used as the tool namespace prefix. Tools are exposed
    -- to clients as `<namespace_prefix>__<tool_name>`. Must match
    -- [a-z0-9_]{1,32}. Unique so we never collide two servers' prefixes.
    namespace_prefix      VARCHAR(32) NOT NULL UNIQUE,
    -- Optional human-friendly label shown to end users on /connections
    -- and in the tool catalog. Distinct from `name` (system identifier,
    -- audit log key) and `namespace_prefix` (tool routing key). When
    -- two installs of the same template land as `linear` + `linear_2`,
    -- admins can label them "Linear (Acme prod)" / "Linear (BBQ corp)"
    -- so users see something meaningful. Frontend falls back to `name`
    -- when NULL.
    display_label         TEXT,
    description           TEXT,
    endpoint_url          VARCHAR(512) NOT NULL,
    transport_type        VARCHAR(50)  NOT NULL DEFAULT 'streamable_http',
    -- ----- per-user OAuth config -------------------------------------------
    -- Filled when the upstream supports OAuth. The gateway acts as the
    -- OAuth client; per-user access/refresh tokens land in
    -- `mcp_user_credentials`. NULL => OAuth not available for this server.
    oauth_issuer                  VARCHAR(512),
    oauth_authorization_endpoint  VARCHAR(512),
    oauth_token_endpoint          VARCHAR(512),
    oauth_revocation_endpoint     VARCHAR(512),
    oauth_client_id               TEXT,
    oauth_client_secret_encrypted BYTEA,
    oauth_scopes                  TEXT[] NOT NULL DEFAULT '{}',
    -- Userinfo endpoint used to populate `upstream_subject` after a
    -- successful OAuth callback. Best-effort:
    --   1. If the access_token is a JWT, decode the payload locally
    --      and read `sub` / `preferred_username` / `email`. No network.
    --   2. Else, if this column is set, GET it with the access_token
    --      and walk the JSON for the first non-empty subject-like
    --      field (sub, id, accountId, login, username, email).
    --   3. Else give up — `upstream_subject` stays NULL and the UI
    --      falls back to the user-supplied account_label.
    oauth_userinfo_endpoint       VARCHAR(512),
    -- ----- authentication shape (single-valued, no "OAuth + PAT" combo) --
    -- `anonymous` — public service, no credential forwarded.
    -- `oauth`     — per-server OAuth client config in the columns above
    --               drives the authorize/token flow; resulting tokens land
    --               either in mcp_user_credentials or
    --               mcp_server_shared_credentials depending on
    --               credential_owner.
    -- `static`    — credential is a PAT / API key (per-user paste in
    --               /connections, or admin-pasted shared token).
    --
    -- Single value per server: PAT vs OAuth is a different auth shape,
    -- not a fallback. Admins who need both register the upstream twice
    -- with different namespace prefixes.
    auth_shape                    TEXT NOT NULL DEFAULT 'anonymous'
        CHECK (auth_shape IN ('anonymous', 'oauth', 'static')),
    -- Optional link shown next to the "paste token" UI so the user knows
    -- where to generate one. Only meaningful when `auth_shape='static'`.
    static_token_help_url         VARCHAR(512),
    -- ----- HTTP auth header injection ------------------------------------
    -- How the resolved upstream credential is injected into the proxied
    -- request. Resolver produces a token (OAuth access_token or static
    -- PAT); the proxy applies `auth_value_template.replace("{{token}}", t)`
    -- and sends it under `auth_header_name`. Defaults match the most
    -- common Bearer pattern; servers using `X-API-Key`, `api-key`,
    -- `Authorization: token …` set these explicitly.
    auth_header_name              TEXT NOT NULL DEFAULT 'Authorization',
    auth_value_template           TEXT NOT NULL DEFAULT 'Bearer {{token}}',
    -- ----- credential ownership -----------------------------------------
    -- 'per_user'    ⇒ each user authorizes / pastes their own token in
    --                  the connections UI; rows live in mcp_user_credentials.
    -- 'admin_shared'⇒ one credential configured by an admin in the server
    --                  edit form is used for every caller; row lives in
    --                  mcp_server_shared_credentials. Per-user audit/quota
    --                  attribution is unchanged — callers are still
    --                  identified by their own user_id, only the upstream
    --                  bearer is shared.
    credential_owner              TEXT NOT NULL DEFAULT 'per_user'
        CHECK (credential_owner IN ('per_user', 'admin_shared')),
    -- ----- tool catalog cache ---------------------------------------------
    -- Snapshot from the most recent admin / probe-time tools/list call,
    -- shown to users that haven't authorized yet so the catalog isn't
    -- silently empty. Per-user calls hit the upstream live with the
    -- caller's token and bypass this cache.
    cached_tools_jsonb    JSONB,
    cached_tools_at       TIMESTAMPTZ,
    status                VARCHAR(50) NOT NULL DEFAULT 'pending',
    health_check_interval INTEGER DEFAULT 60,
    last_health_check     TIMESTAMPTZ,
    last_error            TEXT,
    config_json           JSONB DEFAULT '{}',
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Per-user upstream credentials. One row per (server, user, label) so
-- a single user can connect a "work" GitHub and a "personal" GitHub to
-- the same server. `is_default` picks the credential when the calling
-- API key has no `mcp_account_overrides` entry for the server.
CREATE TABLE IF NOT EXISTS mcp_user_credentials (
    mcp_server_id            UUID NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    user_id                  UUID NOT NULL REFERENCES users(id)       ON DELETE CASCADE,
    account_label            TEXT NOT NULL,
    credential_type          TEXT NOT NULL
        CHECK (credential_type IN ('oauth_authcode', 'static_token')),
    is_default               BOOLEAN NOT NULL DEFAULT FALSE,
    access_token_encrypted   BYTEA NOT NULL,
    refresh_token_encrypted  BYTEA,
    -- NULL ⇒ never expires (typical for static_token / PAT). For
    -- oauth_authcode this is set from the upstream's token response.
    expires_at               TIMESTAMPTZ,
    scopes                   TEXT[] NOT NULL DEFAULT '{}',
    -- Whatever the upstream calls "the connected identity" — surfaced in
    -- the UI as "@octocat" / "user@example.com" so users can tell their
    -- accounts apart at a glance. For OAuth we read this from `userinfo`
    -- on callback; for static tokens it stays NULL.
    upstream_subject         TEXT,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (mcp_server_id, user_id, account_label)
);

-- One default credential per (server, user). Partial unique index so
-- non-default rows aren't constrained.
CREATE UNIQUE INDEX IF NOT EXISTS uq_mcp_user_credentials_default
    ON mcp_user_credentials(mcp_server_id, user_id) WHERE is_default;
CREATE INDEX IF NOT EXISTS idx_mcp_user_credentials_user
    ON mcp_user_credentials(user_id);

-- Server-level shared credentials. Used when `mcp_servers.credential_owner`
-- = 'admin_shared'. One row per server (PK on mcp_server_id), so when
-- the admin re-pastes a token the upsert replaces the previous row.
-- Field shape mirrors `mcp_user_credentials` so the OAuth refresh logic
-- can be unified across the two storage backends.
CREATE TABLE IF NOT EXISTS mcp_server_shared_credentials (
    mcp_server_id            UUID PRIMARY KEY REFERENCES mcp_servers(id) ON DELETE CASCADE,
    credential_type          TEXT NOT NULL
        CHECK (credential_type IN ('oauth_authcode', 'static_token')),
    access_token_encrypted   BYTEA NOT NULL,
    refresh_token_encrypted  BYTEA,
    expires_at               TIMESTAMPTZ,
    scopes                   TEXT[] NOT NULL DEFAULT '{}',
    upstream_subject         TEXT,
    -- Audit pointer: which admin configured this credential. Kept for
    -- the admin UI's "configured by" line; never used for caller
    -- attribution (per-user audit/quota always uses the calling user).
    configured_by            UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS mcp_tools (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    server_id     UUID NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    tool_name     VARCHAR(255) NOT NULL,
    description   TEXT,
    input_schema  JSONB,
    is_active     BOOLEAN NOT NULL DEFAULT TRUE,
    discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(server_id, tool_name)
);

-- Per-user tool catalog. Populated when a user's authenticated
-- `tools/list` returns — either at credential-write time (oauth_callback,
-- paste_static_token) or lazily on first proxy request. Lives separately
-- from `mcp_tools` because:
--
--   1. Tool catalogs *can* differ per user (Atlassian-style filtering by
--      role / scope), so caching at the (server) level would cross-leak
--      one user's filtered view to another's.
--   2. Auth-required servers MUST NOT have `mcp_tools` rows — that table
--      is the "system-level" catalog (admin / store visible) and only gets
--      populated when anonymous discovery succeeds. Writing user-specific
--      tools to `mcp_tools` would be a privilege-escalation surface.
--
-- Refresh strategy: write-through on each authenticated `tools/list` call
-- (the gateway `tools/list` proxy handler). No background refresh loop —
-- a user who hasn't called the server in days will pick up the freshest
-- catalog the next time they do.
CREATE TABLE IF NOT EXISTS mcp_user_tools (
    mcp_server_id UUID NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tool_name     VARCHAR(255) NOT NULL,
    description   TEXT,
    input_schema  JSONB,
    discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (mcp_server_id, user_id, tool_name)
);
CREATE INDEX IF NOT EXISTS mcp_user_tools_lookup
    ON mcp_user_tools (user_id, mcp_server_id);

-- --------------------------------------------------------------------------
-- Per-request usage / analytics data no longer lives in Postgres.
-- Every gateway call writes a row into ClickHouse `gateway_logs`
-- (see deploy/clickhouse/initdb.d/01_init.sql); all cost / token /
-- model / provider dashboards query CH directly. The legacy
-- `usage_records` table + its five indexes were dropped once the
-- readers (analytics, dashboard, chargeback, cost_forecast,
-- usage_license) migrated to CH.
-- --------------------------------------------------------------------------
-- Rate limit rules + budget caps
--
-- Generic rule storage for sliding-window rate limits and natural-period
-- budget caps. Role-level constraints are inline in
-- rbac_roles.policy_document (Constraints field); these tables are for
-- user / api_key_lineage subjects only.
--
-- `api_key_lineage` (NOT `api_key`) means the rule is keyed by the key's
-- `lineage_id` rather than its current `id`. Rotating an api_key mints a
-- new id but reuses the lineage_id, so a rule attached to the lineage
-- automatically applies to every generation. Admin handlers translate
-- `…/limits/api_key/{api_key_id}` URLs to `(api_key_lineage, lineage_id)`
-- before persisting / reading.
--
-- Team scope is DELIBERATELY not supported. Teams in ThinkWatch are
-- IAM-group-style permission containers (they grant roles, they don't
-- own resources or spend). Limits and budgets attach to the actor that
-- actually spends tokens — the user or the API key. To express "this
-- team's members should share a budget", grant the team a role whose
-- policy carries the per-user constraint, or set per-user caps via
-- bulk-override. A `subject_kind='team'` variant would imply either
-- shared counters (hard to keep consistent without a distributed
-- locking story) or team-wide sums (what's already achievable through
-- role policy) — neither is worth the complexity today.
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS rate_limit_rules (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Who the rule applies to.
    subject_kind VARCHAR(20) NOT NULL
        CHECK (subject_kind IN ('user', 'api_key_lineage')),
    subject_id   UUID NOT NULL,
    -- Which gateway this rule guards.
    surface      VARCHAR(20) NOT NULL
        CHECK (surface IN ('ai_gateway', 'mcp_gateway')),
    -- What we count: requests (1 per call) or weighted tokens (computed
    -- by `weight.rs` from raw token counts × model weights).
    metric       VARCHAR(20) NOT NULL CHECK (metric IN ('requests', 'tokens')),
    -- Sliding window length in seconds. Validated at startup against
    -- the `[60, 60*60*24*7*4]` range — anything outside that is
    -- either too coarse for the bucket scheme or too long to be a
    -- "rate" rather than a budget.
    window_secs  INTEGER NOT NULL CHECK (window_secs > 0),
    -- Threshold inside the window. requests-metric counts whole calls;
    -- tokens-metric counts weighted tokens.
    max_count    BIGINT  NOT NULL CHECK (max_count > 0),
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Temporary override metadata. NULL expires_at = permanent; set
    -- expires_at to bound the override and the read path filters stale
    -- rows automatically. `reason` is operator justification (validated
    -- to ≥10 chars at the handler layer whenever expires_at is set).
    -- `created_by` records the actor who wrote the row for the audit
    -- trail; ON DELETE SET NULL so removing a user doesn't cascade-
    -- delete their historical override log.
    expires_at   TIMESTAMPTZ NULL,
    reason       TEXT        NULL,
    -- NULL means the original author was deleted (the FK sets NULL on
    -- cascade so their historical overrides aren't silently purged).
    -- It does NOT mean "system-created" — those rows either carry a
    -- real user id or wouldn't hit this column. The index below
    -- supports audit queries ("show me everything this admin set")
    -- but skips the NULL tombstones, which aren't actionable.
    created_by   UUID        NULL REFERENCES users(id) ON DELETE SET NULL,
    -- One row per (subject, surface, metric, window) — admins enable /
    -- disable via the `enabled` flag rather than re-creating rows.
    UNIQUE(subject_kind, subject_id, surface, metric, window_secs)
);
CREATE INDEX IF NOT EXISTS idx_rlr_subject    ON rate_limit_rules(subject_kind, subject_id);
CREATE INDEX IF NOT EXISTS idx_rlr_enabled    ON rate_limit_rules(enabled) WHERE enabled = TRUE;
CREATE INDEX IF NOT EXISTS idx_rlr_expires_at ON rate_limit_rules(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_rlr_created_by ON rate_limit_rules(created_by, created_at DESC)
    WHERE created_by IS NOT NULL;

CREATE TABLE IF NOT EXISTS budget_caps (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subject_kind VARCHAR(20) NOT NULL
        CHECK (subject_kind IN ('user', 'api_key_lineage')),
    subject_id   UUID NOT NULL,
    -- Natural calendar period — counters reset on the period boundary
    -- (system TZ). NOT a sliding window; that's what rate_limit_rules
    -- is for.
    period       VARCHAR(20) NOT NULL
        CHECK (period IN ('daily', 'weekly', 'monthly')),
    -- Threshold in weighted tokens. The UI may display "≈ $X" by
    -- aggregating real `gateway_logs.cost_usd` for the same period,
    -- but the cap itself is unitless tokens.
    limit_tokens BIGINT  NOT NULL CHECK (limit_tokens > 0),
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Same override metadata as rate_limit_rules. See that table for the
    -- full rationale.
    expires_at   TIMESTAMPTZ NULL,
    reason       TEXT        NULL,
    -- See rate_limit_rules.created_by for NULL semantics.
    created_by   UUID        NULL REFERENCES users(id) ON DELETE SET NULL,
    UNIQUE(subject_kind, subject_id, period)
);
CREATE INDEX IF NOT EXISTS idx_budget_caps_subject    ON budget_caps(subject_kind, subject_id);
CREATE INDEX IF NOT EXISTS idx_budget_caps_enabled    ON budget_caps(enabled) WHERE enabled = TRUE;
CREATE INDEX IF NOT EXISTS idx_budget_caps_expires_at ON budget_caps(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_budget_caps_created_by ON budget_caps(created_by, created_at DESC)
    WHERE created_by IS NOT NULL;

-- --------------------------------------------------------------------------
-- MCP Call Logs
--
-- Retention policy: when an MCP server is removed, the historical call
-- logs STAY — server_id is set NULL via the FK, the row is kept as a
-- permanent audit record. Deliberate choice over CASCADE so that
-- deleting a compromised or misconfigured server doesn't erase the
-- evidence of what was called through it.
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS mcp_call_logs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    server_id       UUID REFERENCES mcp_servers(id) ON DELETE SET NULL,
    tool_name       VARCHAR(255) NOT NULL,
    user_id         UUID REFERENCES users(id) ON DELETE SET NULL,
    duration_ms     INTEGER,
    status          VARCHAR(50) NOT NULL DEFAULT 'success',
    error_message   TEXT,
    request_payload JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_mcp_call_logs_created_at ON mcp_call_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_mcp_call_logs_server_id  ON mcp_call_logs(server_id, created_at);

-- --------------------------------------------------------------------------
-- Log Forwarders
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS log_forwarders (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name           VARCHAR(255) NOT NULL,
    forwarder_type VARCHAR(50)  NOT NULL,
    config         JSONB NOT NULL DEFAULT '{}',
    enabled        BOOLEAN NOT NULL DEFAULT TRUE,
    sent_count     BIGINT NOT NULL DEFAULT 0,
    error_count    BIGINT NOT NULL DEFAULT 0,
    last_sent_at   TIMESTAMPTZ,
    last_error     TEXT,
    log_types      TEXT[] NOT NULL DEFAULT '{audit}',
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_log_forwarders_enabled ON log_forwarders(enabled);

-- --------------------------------------------------------------------------
-- System Settings (key-value store, managed via Web UI)
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS system_settings (
    key         VARCHAR(255) PRIMARY KEY,
    value       JSONB NOT NULL,
    category    VARCHAR(100) NOT NULL,
    description TEXT,
    updated_by  UUID REFERENCES users(id) ON DELETE SET NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_system_settings_category ON system_settings(category);

-- --------------------------------------------------------------------------
-- Dashboard layouts — per-user stat-card ordering (server-side persistence)
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS user_dashboard_layouts (
    user_id     UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    name        TEXT NOT NULL DEFAULT 'default',
    layout_json JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- --------------------------------------------------------------------------
-- Webhook outbox — durable retry queue for webhook deliveries
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS webhook_outbox (
    id              UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
    forwarder_id    UUID         NOT NULL
                                 REFERENCES log_forwarders(id) ON DELETE CASCADE,
    payload         JSONB        NOT NULL,
    attempts        INTEGER      NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    last_error      TEXT,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_webhook_outbox_next_attempt ON webhook_outbox(next_attempt_at);

-- --------------------------------------------------------------------------
-- MCP Store — template marketplace for one-click MCP server installation
-- --------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS mcp_store_templates (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    slug                VARCHAR(100) NOT NULL UNIQUE,
    name                VARCHAR(255) NOT NULL,
    description         TEXT,
    icon_url            VARCHAR(512),
    author              VARCHAR(255) DEFAULT 'Community',
    category            VARCHAR(100),
    tags                TEXT[] DEFAULT ARRAY[]::TEXT[],
    endpoint_template   VARCHAR(512),
    -- Per-user auth shape for the upstream described by this template.
    -- Mirrors the columns on `mcp_servers` so installation is a
    -- straight copy. Both can coexist (e.g. GitHub MCP supports OAuth
    -- AND PATs); both can be empty (anonymous service).
    oauth_issuer                  VARCHAR(512),
    oauth_authorization_endpoint  VARCHAR(512),
    oauth_token_endpoint          VARCHAR(512),
    oauth_revocation_endpoint     VARCHAR(512),
    oauth_userinfo_endpoint       VARCHAR(512),
    oauth_default_scopes          TEXT[] NOT NULL DEFAULT '{}',
    -- Authentication shape — see comment on mcp_servers.auth_shape.
    -- Templates pre-declare the shape so installation pre-populates the
    -- new server row's auth_shape.
    auth_shape                    TEXT NOT NULL DEFAULT 'anonymous'
        CHECK (auth_shape IN ('anonymous', 'oauth', 'static')),
    static_token_help_url         VARCHAR(512),
    -- Header / template defaults for upstreams that don't use
    -- `Authorization: Bearer …`. When a template ships e.g. an Anthropic
    -- API the install handler copies these into mcp_servers so the
    -- resolver injects the right header out-of-the-box.
    auth_header_name              TEXT NOT NULL DEFAULT 'Authorization',
    auth_value_template           TEXT NOT NULL DEFAULT 'Bearer {{token}}',
    -- Free-form text shown in the install / connect dialogs to point
    -- the user at where to generate a token / set up an OAuth app.
    auth_instructions   TEXT,
    deploy_type         VARCHAR(50) DEFAULT 'hosted',
    deploy_command      TEXT,
    deploy_docs_url     VARCHAR(512),
    homepage_url        VARCHAR(512),
    repo_url            VARCHAR(512),
    featured            BOOLEAN DEFAULT FALSE,
    install_count       INTEGER DEFAULT 0,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_mcp_store_category ON mcp_store_templates(category);

CREATE TABLE IF NOT EXISTS mcp_store_installs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    template_id     UUID NOT NULL REFERENCES mcp_store_templates(id),
    server_id       UUID NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    installed_by    UUID REFERENCES users(id),
    installed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(server_id)
);
