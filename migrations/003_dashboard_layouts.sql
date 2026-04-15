-- Dashboard layouts — server-side persistence of per-user stat-card ordering
-- and (future) widget visibility. Previously stored in localStorage, which
-- meant no cross-device sync and no portability to new browsers.
--
-- Single layout per user for now; the `name` column is reserved for future
-- multi-layout support (personal / team / "wide screen" variants) so we
-- don't have to migrate again when that lands.

CREATE TABLE user_dashboard_layouts (
    user_id     UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    name        TEXT NOT NULL DEFAULT 'default',
    layout_json JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
