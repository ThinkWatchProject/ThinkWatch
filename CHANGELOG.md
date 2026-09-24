# Changelog

All notable changes to ThinkWatch are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); from `1.0.0`
onwards versioning follows [SemVer](https://semver.org/spec/v2.0.0.html).

Until `1.0.0`, any `0.y` bump may include breaking changes — see the
notes under each release. Operators upgrading inside the `0.x` line
should read the section for every intermediate version, not just the
target.

## [Unreleased]

## [1.1.0] — 2026-09-24

The gateway stops rebuilding every request as a chat-shaped message. A
request whose route speaks the caller's own format goes out as the
caller sent it; one that crosses formats is converted by
[ThinkWatch-Core](https://github.com/ThinkWatchProject/ThinkWatch-Core),
the same layer the desktop edition uses. Tools, tool choice, system
prompt blocks, `metadata` and `cache_control` now reach the upstream,
where they used to be dropped. Two new checks guard what goes in and
out: tool calls an upstream returns, and invisible characters in what
a caller sends.

### Read before upgrading

- **Anthropic routes record more prompt tokens for the same work.**
  Prompt tokens now count the same for every upstream: plain input plus
  cache reads and cache writes, which is OpenAI's definition.
  Anthropic's own `input_tokens` leaves the cached part out, so on those
  routes prompt tokens, cost and budget use all go up. The price model
  still charges every prompt token at one rate.
- **Requests in the upstream's own format are forwarded as sent.** Every
  field the caller sends reaches the upstream, along with the caller's
  `anthropic-beta` and `anthropic-version` headers. Only the model name
  changes, and PII is swapped for placeholders. An OpenAI-compatible
  upstream that rejects fields it does not know may now refuse requests
  that used to succeed, because those fields were stripped before. Try
  your upstreams with the clients you actually run.
- **Two checks are on by default, and neither blocks anything yet.**
  - Tool-call inspection starts in `observe` mode.
  - The hidden-character check starts in `warn` mode.
  - Both write audit events, so expect new entries in the audit log and
    in anything subscribed to it.
  - Nothing is refused until you switch to `enforce` or `block`.
- **The response cache starts cold.** The cache key now covers the whole
  request, so entries written by 1.0.2 are never hit again. They expire
  on their own.
- **One PII value gets one placeholder.** Within a request, the same
  e-mail address is `{{EMAIL_1}}` wherever it appears. It used to get a
  new number each time, so a model saw one person as several. Saving a
  PII pattern now also requires the placeholder prefix to be letters,
  digits or underscores.

### Added

- **Tool-call inspection** (`security.tool_inspection`).
  - **Why:** an upstream writes the response, so it can hand the caller
    a tool call the model never made, such as `bash("curl … | sh")`
    appended to an ordinary answer. An agent set to auto-approve then
    runs it.
  - **Rules:** a built-in set of dangerous-command rules. Each can be
    switched off or given a different action, and you can add your own.
  - **Modes:** `off`, `observe` (records hits and changes nothing on the
    wire) and `enforce`.
  - **Enforce on a stream:** the stream is cut at the frame that would
    complete a matching call, and the refusal arrives in the caller's
    format.
  - **Enforce on a whole response or a cache hit:** refused with 403
    (`policy_blocked`). A refused answer is neither cached nor billed.
  - **Audit and metrics:** every hit is an audit event
    (`gateway.tool_call_flagged` or `gateway.tool_call_blocked`) and
    counts in `gateway_tool_call_flagged_total`.
  - **Admin API:** `GET /api/admin/settings/tool-inspection/rules` and
    `POST /api/admin/settings/tool-inspection/test`.
  - **Console:** a card on the security page, plus a sandbox tab.
- **Hidden-character check** (`security.hidden_text`: `off`, `log`,
  `warn` or `block`).
  - **What it looks for:**
    - Unicode tag characters, which carry an instruction invisibly into
      the model's context;
    - bidirectional overrides, which make text read differently on
      screen than it is.
  - **Where:** the caller's messages and the tool results inside them.
    The system prompt and the model's own turns are not checked.
  - **Not flagged:** zero-width joiners (emoji), the zero-width
    non-joiner (Persian) and Cyrillic.
  - **Actions:** `warn` writes `gateway.hidden_text_flagged`; `block`
    refuses with 403 and writes `gateway.hidden_text_blocked`.
  - **Console:** a card on the security page.
- **Tool-call arguments get their PII back.** A model asked to e-mail
  `a@example.com` used to call the tool with `{{EMAIL_1}}` as the
  address.

### Changed

- **One pipeline for `/v1/chat/completions`, `/v1/messages` and
  `/v1/responses`.** A same-format request is forwarded as sent. A
  cross-format request is converted, and the gateway logs what the
  target format cannot carry.
- **Content filtering and PII detection read tool results too.** That is
  where an injected instruction, or customer data pulled in by a tool,
  usually sits.
- **Usage is read off the upstream's own bytes.** A streamed response is
  no longer held in memory for an accounting pass at the end.
- **Chat streams are billed on the upstream's actual usage.** They are
  always sent asking for it. A caller who did not ask for the usage
  chunk still does not get one. 1.0.2 estimated the count for these.
- **Streams send their headers at once.** A caller who leaves while the
  upstream is still thinking is recorded as cancelled.
- **Connectivity tests use the live encoder.** A route's test request is
  built by the same encoder as real traffic, so a passing test means
  forwarding works.
- **The web console loads data through TanStack Query.**
  - Signing out, including from another tab, clears everything cached.
  - After a change, screens refresh in place.
  - Polling pauses while the tab is hidden.
- **Core crates come from one pinned tag** (ThinkWatch-Core v0.40.0),
  declared once at the workspace root.

### Fixed

- **Requests lost their tools, tool choice and non-text content** on the
  way upstream (ThinkWatch-Core#50). Claude Code's system prompt, sent
  as an array, was dropped whole, and so was every `cache_control`
  breakpoint. Each cached prefix was billed as full-price input.
- **The response cache could serve the wrong answer.** Its key covered
  only model, messages and `max_tokens`, so two requests that differed
  only in tools, `response_format`, `seed` and so on shared one entry.
- **A tripped route stayed out until its Redis key expired**, roughly
  four cooldowns. It now gets probed once the cooldown is over, and a
  success closes it.
- **The dashboard showed every AI provider's breaker as `Closed`.** It
  now shows the real state of the provider's routes, reporting the
  worst one.
- **The PII "try patterns" endpoint misreported labels.** A pattern
  named `CUSTOM_EMAIL` was reported as `CUSTOM`.
- **`:latest` could point at a `main` build rather than the release**,
  which is what happened for v1.0.2's server image. Only the release
  workflow sets `:latest` now.
- **The web image hung for six hours.** Its frontend was built under
  QEMU for arm64, where Node crashed and the step never returned. It is
  now built once, natively. The image contents are unchanged.

### Security

- Refreshed the web console's lockfile to clear 56 Dependabot alerts
  (1 critical, 23 high). All were transitive, and none of them reached
  the shipped bundle.

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

[Unreleased]: https://github.com/ThinkWatchProject/ThinkWatch/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.1.0
[1.0.2]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.0.2
[1.0.1]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.0.1
[1.0.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v1.0.0
[0.5.0]: https://github.com/ThinkWatchProject/ThinkWatch/releases/tag/v0.5.0
