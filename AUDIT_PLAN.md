# ThinkWatch — Audit Remediation Plan

Date: 2026-04-07
Source: 35 findings from full-project security/backend/frontend audit
Goal: Resolve every finding across grouped rounds. Each round is a coherent
unit, runs `make precommit` clean, and lands as one commit.

Status legend: `[ ]` pending · `[~]` in progress · `[x]` done

---

## Round 1 — Critical bugs / data integrity

Focus: things that are actively wrong in production today. All low-risk,
mechanical fixes.

- [x] **R1.1** TOTP recovery code consumption made atomic (UPDATE … WHERE
  totp_recovery_codes = $old → 0 rows affected on race) + audit
  `auth.totp_recovery_used`. The audit was wrong about the code not being
  removed at all (it was), but the consumption wasn't atomic.
- [x] **R1.2** `discover_and_persist_tools` now wraps deactivate + upsert
  + status update in a single `state.db.begin()` transaction. Also fixed
  the related `unwrap_or(&json)` bug that silently accepted malformed
  MCP responses.
- [x] **R1.3** MCP circuit breaker state machine now lives in one
  `Mutex<BreakerInner>`. Added two concurrency regression tests.
- [x] **R1.4** Dashboard `build_live_snapshot` parallelizes 4 ClickHouse
  queries via `tokio::try_join!`.
- [x] **R1.5** Auth handler — lockout TTL check + lockout SET both now
  fail-closed on Redis errors.
- [x] **R1.6** Dashboard PG queries (max_rpm, providers, mcp_servers) now
  parallelize via `tokio::try_join!` AND propagate errors instead of
  swallowing them with `unwrap_or_default`.

---

## Round 2 — Auth / authorization hardening

- [ ] **R2.1** MCP tool default-allow → default-deny
  - File: [crates/mcp-gateway/src/access_control.rs](crates/mcp-gateway/src/access_control.rs#L44)
  - Fix: when no policy exists for a `(server, tool)` pair, deny unless
    the user has admin role.
- [ ] **R2.2** JWT missing `aud` / `iss` + explicit algorithm pinning
  - File: [crates/auth/src/jwt.rs](crates/auth/src/jwt.rs#L7)
  - Fix: add `aud` + `iss` to `Claims`, set them to a configurable value
    (env: `THINKWATCH_JWT_AUDIENCE`), pin `Validation` to `HS256` only.
- [ ] **R2.3** Dashboard WS doesn't recheck token revocation after upgrade
  - File: [crates/server/src/handlers/dashboard.rs](crates/server/src/handlers/dashboard.rs#L312)
  - Fix: every N ticks (every 30s), re-verify `is_revoked` and close the
    socket if the token has been blacklisted.
- [ ] **R2.4** Login rate-limit IP extraction trusts XFF without proxy check
  - File: [crates/server/src/handlers/auth.rs](crates/server/src/handlers/auth.rs#L31)
  - Fix: extract login IP via the same trusted-proxy validation as
    `auth_guard::require_auth`.
- [ ] **R2.5** Session IP binding fails open when bound_ip is `None`
  - File: [crates/server/src/middleware/verify_signature.rs](crates/server/src/middleware/verify_signature.rs#L196)
  - Fix: when `client_ip_source != "connection"` is configured, require
    sessions to have a bound IP and reject when missing.
- [ ] **R2.6** Unbounded `offset` parameter on log/analytics queries
  - File: [crates/server/src/handlers/gateway_logs.rs](crates/server/src/handlers/gateway_logs.rs)
    + similar handlers
  - Fix: cap `offset` at `100_000` (or use cursor pagination, but cap is
    enough for now).

---

## Round 3 — WS token leak fix (ticket pattern)

- [ ] **R3.1** Replace `?token=` upgrade with one-shot ticket
  - Files:
    - [crates/server/src/handlers/dashboard.rs](crates/server/src/handlers/dashboard.rs#L266)
    - [crates/server/src/app.rs](crates/server/src/app.rs)
    - [web/src/routes/dashboard.tsx](web/src/routes/dashboard.tsx#L114)
  - Fix:
    1. New endpoint `POST /api/dashboard/ws-ticket` (auth required) →
       returns short-lived (30s) one-shot ticket stored in Redis.
    2. WS handler accepts `?ticket=` instead of `?token=`, validates +
       atomically deletes from Redis.
    3. Frontend `useLiveDashboard` does the POST then opens the WS.

---

## Round 4 — Backend code quality / correctness

- [ ] **R4.1** Encryption key versioning envelope
  - File: [crates/common/src/crypto.rs](crates/common/src/crypto.rs)
  - Fix: prepend a `u8` key-version byte to the ciphertext envelope so we
    can rotate keys later. Read path tries current then previous key.
- [ ] **R4.2** `mcp_servers::create_server` discovery error visibility
  - File: [crates/server/src/handlers/mcp_servers.rs](crates/server/src/handlers/mcp_servers.rs#L84)
  - Fix: persist last discovery error onto `mcp_servers.last_error` (new
    column) and surface in admin UI list.
- [ ] **R4.3** Reuse a single `reqwest::Client` for tool discovery
  - File: [crates/server/src/app.rs](crates/server/src/app.rs#L857)
  - Fix: hoist a `reqwest::Client` into `AppState` (or a top-level
    `OnceLock`) and reuse it.
- [ ] **R4.4** Admin role-creation transaction missing rollback semantics
  - File: [crates/server/src/handlers/admin.rs](crates/server/src/handlers/admin.rs#L134)
  - Fix: wrap in `state.db.begin()` and let the `Drop` handle rollback;
    explicit `tx.commit()` only on the happy path.
- [ ] **R4.5** Hardcoded timeouts and intervals scattered across the code
  - Files:
    - [crates/server/src/app.rs](crates/server/src/app.rs#L264)
    - [crates/mcp-gateway/src/pool.rs](crates/mcp-gateway/src/pool.rs#L67)
    - [crates/mcp-gateway/src/circuit_breaker.rs](crates/mcp-gateway/src/circuit_breaker.rs#L31)
  - Fix: move all to `AppConfig` with sensible defaults.
- [ ] **R4.6** `validate_url` SSRF defence audit + IPv6 / decimal IP / DNS
  rebinding hardening (existing helper, never re-checked)
  - File: search `crates/server/src/handlers/providers.rs` for
    `validate_url`
  - Fix: add IPv6 link-local + ULA + IPv4-mapped checks, decimal/octal IP
    rejection, and use a single resolver pass cached for the connection.

---

## Round 5 — Frontend bug fixes

- [ ] **R5.1** Multi-tab token-refresh race
  - File: [web/src/lib/api.ts](web/src/lib/api.ts#L106)
  - Fix: use `BroadcastChannel('auth')` so only one tab refreshes; others
    wait for the broadcast.
- [ ] **R5.2** `cachedSetupStatus` never invalidated after setup completes
  - File: [web/src/router.tsx](web/src/router.tsx#L46)
  - Fix: clear the cache when setup-initialize succeeds + re-fetch on
    visibility-change.
- [ ] **R5.3** `Intl.NumberFormat` hardcoded to `en-US`
  - File: [web/src/routes/dashboard.tsx](web/src/routes/dashboard.tsx#L65)
  - Fix: derive locale from i18next current language.
- [ ] **R5.4** Dashboard WS stale-closure fallback fetch never fires
  - File: [web/src/routes/dashboard.tsx](web/src/routes/dashboard.tsx#L155)
  - Fix: read `live` via a ref instead of closure capture, OR move the
    fallback fetch out of `onclose` into a separate first-paint effect.
- [ ] **R5.5** WS frame parse errors silently dropped
  - File: [web/src/routes/dashboard.tsx](web/src/routes/dashboard.tsx#L156)
  - Fix: `console.error('dashboard ws parse failed', err, ev.data)` so
    debugging is possible.
- [ ] **R5.6** Logs page search input loses focus on each refetch
  - File: [web/src/routes/logs.tsx](web/src/routes/logs.tsx#L364)
  - Fix: debounce the query, key the input by category not by query.
- [ ] **R5.7** Destructive actions missing confirm dialogs
  - Files:
    - [web/src/routes/admin/settings.tsx](web/src/routes/admin/settings.tsx#L1000)
    - [web/src/routes/gateway/providers.tsx](web/src/routes/gateway/providers.tsx#L340)
  - Fix: route through existing `confirm-dialog.tsx` for content-filter,
    pii-pattern, provider, key, MCP server delete.

---

## Round 6 — Frontend a11y + virtualization

- [ ] **R6.1** Unified logs list virtualization
  - File: [web/src/routes/logs.tsx](web/src/routes/logs.tsx#L553)
  - Fix: integrate `@tanstack/react-virtual` (or write a windowed
    scroller) for the row list.
- [ ] **R6.2** `ProviderFilterTabs` keyboard support + ARIA
  - File: [web/src/routes/dashboard.tsx](web/src/routes/dashboard.tsx#L498)
  - Fix: `role="tablist"` + arrow-key handler.
- [ ] **R6.3** Status dots have text alternative
  - File: [web/src/routes/dashboard.tsx](web/src/routes/dashboard.tsx#L654)
  - Fix: visually-hidden `<span>` with the status label, or `aria-label`
    on the dot wrapper.
- [ ] **R6.4** Filter buttons missing `aria-label`
  - File: [web/src/routes/logs.tsx](web/src/routes/logs.tsx#L640)
  - Fix: add aria labels to `+` / `−` exclude buttons.

---

## Round 7 — i18n parity + runtime validation

- [ ] **R7.1** i18n key parity check
  - Files: [web/src/i18n/en.json](web/src/i18n/en.json),
    [web/src/i18n/zh.json](web/src/i18n/zh.json)
  - Fix: a small Node script under `web/scripts/check-i18n.mjs` that
    fails the build if keys mismatch; add to `pnpm build` step.
- [ ] **R7.2** Runtime validation of key API responses with zod
  - File: [web/src/lib/api.ts](web/src/lib/api.ts)
  - Fix: introduce optional `schema` parameter on `api<T>(...)` and
    validate critical endpoints (`/api/auth/me`, `/api/dashboard/live`,
    `/api/setup/status`).

---

## Round 8 — Architecture / refactor

- [ ] **R8.1** Split [crates/server/src/app.rs](crates/server/src/app.rs)
  - Extract into `app/` module: `state.rs`, `gateway_app.rs`,
    `console_app.rs`, `providers_loader.rs`, `mcp_loader.rs`,
    `mcp_health.rs`.
- [ ] **R8.2** Sub-state structs for `AppState`
  - Reduce 18 fields → 4 logical sub-states (`Core`, `Mcp`, `Filters`,
    `Clickhouse`). Handlers extract only the sub-state they need.
- [ ] **R8.3** Move tracing of `format!()`-built error chains to
  redact-aware helper to prevent secret leaks
  - File: [crates/common/src/errors.rs](crates/common/src/errors.rs#L50)

---

## Round 9 — Polish / observability

- [ ] **R9.1** Audit log `detail` rendering hardening (defensive — current
  code is text-only but enforce it explicitly)
  - File: [web/src/routes/logs.tsx](web/src/routes/logs.tsx)
  - Fix: never use `dangerouslySetInnerHTML` on detail; render via `<pre>`.
- [ ] **R9.2** Sliding-window nonce limiter
  - File: [crates/server/src/middleware/verify_signature.rs](crates/server/src/middleware/verify_signature.rs#L143)
  - Fix: replace fixed-window counter with a small sorted-set sliding
    window in Redis.
- [ ] **R9.3** WebSocket per-user concurrent connection cap + per-tick
  send timeout
  - File: [crates/server/src/handlers/dashboard.rs](crates/server/src/handlers/dashboard.rs#L312)
  - Fix: track `(user_id, count)` in a `DashMap`, reject upgrade above
    limit; wrap each `send` in a 5s timeout.

---

## Out of scope for this remediation (acknowledged risk)

- KMS-backed encryption-key management (R4.1 only adds versioning, not
  rotation infrastructure).
- Migration of refresh tokens from `localStorage` to httpOnly cookie —
  requires a fuller session model rework.
- Per-MCP-server connection-pool quotas — not part of the audit findings.

---

## Process

After every round:

1. `cargo check --workspace`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo fmt --all`
4. `cargo test --workspace`
5. `cd web && pnpm build`
6. Commit with `fix(audit-rN): <summary>` message

Update the checkboxes in this file as rounds complete.
