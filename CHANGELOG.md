# Changelog

All notable changes to ThinkWatch are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); from `1.0.0`
onwards versioning follows [SemVer](https://semver.org/spec/v2.0.0.html).

Until `1.0.0`, any `0.y` bump may include breaking changes — see the
notes under each release. Operators upgrading inside the `0.x` line
should read the section for every intermediate version, not just the
target.

## [Unreleased]

### Added
- _(nothing yet)_

### Changed
- _(nothing yet)_

### Fixed
- _(nothing yet)_

### Removed
- _(nothing yet)_

### Security
- _(nothing yet)_

## [0.5.0] — 2026-05-26

First public beta. The product surface is stable enough to deploy
against, but the API contract is **not yet committed** — expect
breaking changes in `0.6.x` and beyond as the run-up to `1.0.0`
narrows the surface. Operators running this version against real
traffic should pin the image digest and read every subsequent
release note.

### Highlights

- **AI gateway** (`:3000`) — OpenAI / Anthropic / Google / Azure
  OpenAI / AWS Bedrock with weighted multi-route failover,
  circuit breakers, semantic response cache (Redis-backed), SSE
  streaming with PII restore on the wire.
- **MCP gateway** — per-user OAuth + static-token + admin-shared
  credential modes, response cache with prefix-based invalidation,
  per-server circuit breakers keyed by UUID (so a rename or
  recreate doesn't inherit stale state).
- **Audit pipeline** — bodies up to `audit.body_max_bytes` land
  inline in ClickHouse; oversize bodies offload to S3-compatible
  object storage (RustFS / MinIO / AWS S3). Request and response
  bodies are PII-redacted before write; the bucket lifecycle rule
  is administered from the admin UI
  (`audit.body_s3_lifecycle_days`).
- **RBAC + identity** — JWT access tokens, refresh tokens, OIDC
  SSO, TOTP, recovery codes. Permissions evaluated on every
  request from a Redis-cached + DB-backed policy. Per-API-key
  limits + budgets enforced on the gateway hot path.
- **Observability** — `/metrics` (Prometheus exposition, bearer-
  protected), `/api/health` (PG + Redis + ClickHouse + S3 deep
  probe), per-request `x-trace-id`, structured tracing.
- **Admin console** (`:3001`) — React 19 + TypeScript + i18n
  (en/zh, perfect parity). Dashboard, traces, cost analytics,
  RBAC editor, MCP server CRUD, settings PATCH.

### Quality

- 675 unit + integration tests, gated on `make precommit`.
- Five rounds of systematic bug audits (≈ 45 bugs fixed, ≈ 800
  lines of legacy compat scrubbed) preceded this tag — see
  commits `b0b5820 → dbfe9da` for the full series.

### Known limitations for `0.5.x`

- **Backup / restore runbook not yet shipped** — PG, ClickHouse,
  and S3 backups are the operator's responsibility for now.
- **No bundled Prometheus `ServiceMonitor` or Grafana dashboards**
  — `/metrics` is exposed, but operators wire the scrape config
  themselves.
- **No `JWT_SECRET` / `ENCRYPTION_KEY` rotation runbook** — both
  can be rotated but require a planned-downtime sequence that
  isn't documented yet.
- **API surface NOT frozen** — REST routes, MCP wire shapes,
  audit-row JSON keys, and database schema may change in any
  `0.x` bump.

### Upgrade path from `0.1.0`

The `0.1.0` series was never published; deployments running
unreleased builds should: stop the gateway, run `db/schema.sql`
against PostgreSQL, restart against this tag. The schema is
idempotent end-to-end, so the apply is safe to repeat.

[Unreleased]: https://github.com/ThinkWatchProject/ThinkWatch/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v0.5.0
