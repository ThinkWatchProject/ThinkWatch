# Changelog

All notable changes to ThinkWatch are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); from `1.0.0`
onwards versioning follows [SemVer](https://semver.org/spec/v2.0.0.html).

Until `1.0.0`, any `0.y` bump may include breaking changes — see the
notes under each release. Operators upgrading inside the `0.x` line
should read the section for every intermediate version, not just the
target.

## [Unreleased]

### Fixed

- **CI** — the `main` push that merged #23 never produced its web image.
  `Dockerfile.web` built the frontend once per architecture, Node crashed
  with SIGILL in the QEMU-emulated arm64 build, and the build step hung
  until GitHub cancelled the job at the six-hour limit. The static files
  are identical on every architecture, so they are now built once,
  natively, and copied into each architecture's nginx image — emulated,
  `pnpm build` alone took 260s against 23s. The job also times out after
  20 minutes. Image contents are unchanged; release builds already ran on
  native runners.

## [1.0.2] — 2026-09-13

The shared gateway layer moves out into its own repository, and OIDC
learns to accept an ID token whose `aud` carries more than the client
ID. Mostly a fix release otherwise — the deploy and auth items below are
the ones worth reading before you upgrade.

### Added

- **`OIDC_ADDITIONAL_TRUSTED_AUDIENCES`** — a comma-separated allowlist
  of extra ID-token audiences to trust. Some IdPs put something besides
  the client ID in `aud`; Zitadel includes the parent project ID, and
  `openidconnect`'s stock verifier rejects every non-client-ID audience,
  so login failed outright. Leave it unset and verification is
  byte-for-byte what it was. Set, it widens exactly one check: the extra
  audiences on a multi-audience token are matched against this list as
  exact strings — no wildcards, no prefixes. The client ID must still
  appear in `aud` regardless. Documented in `.env.example`. Thanks to
  @DaniW42 for the report and the patch (#12).
- **Automatic upstream protocol detection** — the gateway learns an
  upstream's dialect and relearns it when the upstream changes, instead
  of relying on a static guess. Protocol is now a property of the route.
- **Reverse-proxy identification** — the server recognizes its own
  reverse proxy, which is what makes the dashboard WebSocket and the API
  docs work behind nginx.

### Changed

- **`azp` is validated whenever it is present**, per OIDC Core 3.1.3.7
  step 5, which has no audience-count precondition. Previously it was
  only checked when a multi-audience token made it mandatory, so a
  single-audience token naming this client with `azp` pointing at a
  *different* client was accepted — the IdP stating plainly that the
  token was authorized for somebody else. **This can reject a token that
  1.0.1 accepted.** If your IdP sets `azp` to something other than your
  client ID, logins will start failing and the message will say so;
  that is the IdP to fix, not this setting.
- **The shared gateway layer now comes from
  [ThinkWatch-Core](https://github.com/ThinkWatchProject/ThinkWatch-Core)**
  as a git dependency rather than living in this tree. Six crates —
  types, protocol, provider, resilience, crypto, and the JSON secret
  envelope — are one implementation shared with the desktop edition
  instead of two copies drifting apart. No runtime or API change; it
  affects you only if you build from source, where the build now needs
  network access to resolve that dependency. The published images are
  unaffected.
- **Stored secrets are redacted on read and decrypted on use.** Provider
  credentials no longer travel in plaintext through code paths that
  merely display or list them.

### Fixed

- **Deploy** — nginx broke the API docs three separate ways; the
  dashboard WebSocket wasn't proxied; the prod stack's healthchecks were
  wrong; and the dev stack could overwrite production's ClickHouse
  credentials. The last one is the reason to read this list.
- **Auth** — the console logged itself out after every token refresh.
  Rate-limit counters could be created without an expiry and sit in
  Redis forever.
- **Models** — the gateway no longer offers models the upstream refuses,
  no longer imports models it won't serve, and reports every dialect
  that was refused rather than only the last. Provider `base_url`
  trailing slashes are normalized.
- **Analytics** — spend is attributed to a person, not a UUID.
- **Audit** — a syslog forwarder formatting fix that had CI red on
  clippy 1.98 (#11, thanks @DaniW42). Two more instances of the same
  lint, plus a `result_large_err` false positive in the MCP lifecycle
  stages, measured rather than boxed: the `Ok` variant is 296 bytes
  against the `Err`'s 152, so boxing saves nothing and adds an
  allocation.
- **UI** — the brand mark stays inside the collapsed sidebar rail.

### Contributing

`CONTRIBUTING.md` and a PR template now state the branch contract that
had only lived in `docs/operations/release.md`: routine work targets
`dev`, and `main` is the release line. A workflow comments on PRs opened
against `main` from anything other than `dev` or a `hotfix/*` branch,
because GitHub pre-fills the base with the default branch and walks
contributors into it.

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

## [1.0.1] — 2026-05-27

Release-pipeline validation. **No product change** — the published
binary, REST surface, MCP wire shapes, and audit semantics are
identical to `v1.0.0`. Operators pinning `1.0.0` have no reason to
bump; those tracking `:latest` move forward.

### Changed

- **Release workflow** — arm64 image builds now run on a native
  arm64 runner (`ubuntu-24.04-arm`) instead of QEMU emulation.
  v1.0.0's server image build took 1h24m; this should drop to
  ~10 min. Multi-arch manifest assembled by a new merge job via
  `docker buildx imagetools create`.
- **Node 24 opt-in** — workflow sets
  `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true` so all `actions/*` +
  `docker/*` run on Node 24 ahead of GitHub's 2026-06-02 forced
  cutover.

## [1.0.0] — 2026-05-27

Stability commitment. No code delta since `0.5.0` — this tag marks
the point at which the API surface becomes a [SemVer](https://semver.org/spec/v2.0.0.html)
commitment.

### Changed

- **Versioning policy** — from this tag onwards every breaking
  change (REST routes, MCP wire shapes, audit-row JSON keys,
  database schema, public Rust APIs in published crates) requires
  a major bump. Operators chasing the `:latest` tag on the GHCR
  images can do so without surprise.

### Notes

- Docker images cut at this tag receive `:latest` for the first
  time — the release workflow suppresses `:latest` on `0.x` and
  pre-release tags. Pin the version in production rather than
  tracking `:latest` unless you have a controlled rollback path.

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

### Operations

- Helm chart ships an opt-in `ServiceMonitor`
  (`metrics.serviceMonitor.enabled=true`) gated on the
  auto-generated `METRICS_BEARER_TOKEN` secret. Pair with
  `kube-prometheus-stack` for `/metrics` scraping.
- `deploy/grafana/dashboards/` — starter overview dashboard JSON
  plus a metric reference + minimal alert rule set in the README.
- `docs/operations/secret-rotation.md` — JWT_SECRET online
  rotation and ENCRYPTION_KEY offline re-encrypt procedures.
- `docs/operations/backup-restore.md` — PG + ClickHouse + S3
  procedures with restore-order gotchas, cross-version compat
  notes, and a quarterly DR drill template.

### Known limitations for `0.5.x`

- **API surface NOT frozen** — REST routes, MCP wire shapes,
  audit-row JSON keys, and database schema may change in any
  `0.x` bump. SemVer kicks in at `1.0.0`.
- **No online ENCRYPTION_KEY rotation** — the documented
  procedure requires a brief downtime window. Online dual-key
  rotation is queued for `1.x`.

### Upgrade path from `0.1.0`

The `0.1.0` series was never published; deployments running
unreleased builds should: stop the gateway, run `db/schema.sql`
against PostgreSQL, restart against this tag. The schema is
idempotent end-to-end, so the apply is safe to repeat.

[Unreleased]: https://github.com/ThinkWatchProject/ThinkWatch/compare/v1.0.2...HEAD
[1.0.2]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.0.2
[1.0.1]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.0.1
[1.0.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.0.0
[0.5.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v0.5.0
