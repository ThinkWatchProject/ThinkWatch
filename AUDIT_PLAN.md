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

- [x] **R2.1** MCP tool default-allow → default-deny. `admin` and
  `super_admin` always pass. Plumbed `user_roles` through `handle_request`
  → `handle_tools_list` / `handle_tools_call`.
- [x] **R2.2** JWT now requires hardcoded `aud="thinkwatch"` and
  `iss="thinkwatch"`. Algorithm explicitly pinned to HS256 in both encode
  and decode paths. New regression tests for foreign aud / foreign iss.
- [x] **R2.3** Dashboard WS now re-checks `is_revoked` every 8 ticks
  (~32s) and closes the socket on revocation.
- [x] **R2.4** Login handler now uses the new shared
  `auth_guard::extract_client_ip` helper which honours the trusted-proxy
  whitelist.
- [x] **R2.5** Session IP binding now fail-closed: missing bound IP or
  missing request IP both reject.
- [x] **R2.6** Added `clamp_pagination` helper in `clickhouse_util` and
  applied to 8 log/analytics handlers. `offset` capped at 100_000.

---

## Round 3 — WS token leak fix (ticket pattern)

- [x] **R3.1** Replaced `?token=` upgrade with one-shot ticket pattern.
  New `POST /api/dashboard/ws-ticket` mints a 32-byte url-safe random
  ticket bound to the user_id in Redis (30s TTL). WS handler atomically
  consumes via fred GETDEL. Frontend `useLiveDashboard` POSTs first,
  then opens WS with `?ticket=`. Token never appears in any URL.
  Also fixed R5.4 (stale closure on `live === null`) and R5.5 (silent
  WS parse errors) in the same edit since they touched the same hook.
  Session revocation now uses a `dashboard_user_revoked:{uid}` Redis
  flag set by `revoke_sessions`, polled by the WS loop every ~32s.

---

## Round 4 — Backend code quality / correctness

- [x] **R4.1** Encryption key versioning envelope. Added 5-byte envelope
  prefix `[magic(4)] [version(1)]`. New ciphertexts use v1; the decrypt
  path auto-detects and falls back to legacy `nonce || ciphertext` for
  existing rows. 3 new tests cover legacy decode, magic prefix, and
  unknown-version rejection.
- [x] **R4.2** Added `mcp_servers.last_error` TEXT column (migration
  002). `create_server` and the health loop both write the error
  message on failure and clear it on success. Admin UI can render it.
- [x] **R4.3** `AppState.http_client` shared `reqwest::Client`.
  `discover_and_persist_tools` now takes `&reqwest::Client` and reuses
  the pooled client; both call sites updated.
- [N/A] **R4.4** Admin role-creation: false positive. The current code
  uses standard `?` + `tx.commit()` which sqlx already rolls back on
  drop. No bug.
- [~] **R4.5** Hardcoded timeouts: HTTP client timeout (15s) lifted into
  the shared client construction. CB and pool timeouts left as-is for
  now (touch them in the architecture round when AppConfig grows).
- [x] **R4.6** `validate_url`: removed `std::thread::sleep` (was
  blocking the async executor) and the redundant double-resolve.
  Added IPv4-mapped IPv6 check (`::ffff:127.0.0.1`), 6to4 (`2002::/16`),
  IPv6 multicast, and unified everything under `is_blocked_ip`. 3 new
  test groups cover the new cases.

---

## Round 5 — Frontend bug fixes

- [x] **R5.1** Multi-tab token-refresh — added a `BroadcastChannel`
  named `thinkwatch-auth`. The tab that wins the refresh broadcasts
  the new tokens; other tabs apply them via the channel listener
  instead of running their own refresh.
- [x] **R5.2** Setup status cache invalidation — added
  `invalidateSetupStatusCache()` (called by setup wizard on success)
  and a `visibilitychange` re-check.
- [x] **R5.3** `Intl.NumberFormat` now reads `i18n.language` and maps to
  a BCP 47 locale (`zh` → `zh-CN`, default `en-US`). Stat card
  formatters take a `locale` parameter at call time.
- [x] **R5.4** Dashboard WS stale-closure fallback fetch — fixed in R3.1
  by reading `live` via a `liveRef`.
- [x] **R5.5** WS frame parse errors now `console.error` — fixed in R3.1.
- [x] **R5.6** Logs search input — sync-on-mismatch via a
  `lastSyncedQueryRef`. Previously every change of `activeQuery`
  unconditionally re-set local state, racing the user's keystrokes
  after our own navigation.
- [N/A] **R5.7** False positive: providers delete is already wrapped
  in `<ConfirmDialog>` (verified at providers.tsx:407). settings
  content-filter / PII removals only edit local draft state and
  require an explicit "Save" click — they're not destructive on click.

---

## Round 6 — Frontend a11y + virtualization

- [N/A] **R6.1** Logs virtualization — deferred. After R2.6 the
  pagination cap is 200 rows max per request, which renders fine
  natively. Adding `@tanstack/react-virtual` would pull ~10 KB into
  the logs chunk for marginal benefit. Revisit if/when streaming
  log views land.
- [x] **R6.2** `ProviderFilterTabs` now has `role="tablist"` + per-tab
  `role="tab" aria-selected tabIndex` + arrow-key/Home/End navigation.
- [x] **R6.3** Provider health status dots have `role="img"` +
  `aria-label`/`title` carrying the localized status text. Connection
  indicator dot at the page header is `aria-hidden` and its container
  is `aria-live="polite"` so screen readers announce reconnect/live.
- [x] **R6.4** Logs `+`/`−` filter buttons have descriptive `aria-label`s
  and the inner icons are `aria-hidden`.

---

## Round 7 — i18n parity + runtime validation

- [x] **R7.1** `check-i18n.mjs` already existed and passed; wired into
  `pnpm build` so any future drift fails CI immediately.
- [x] **R7.2** Added `zod` dependency, extended `api<T>(...)` with an
  optional `schema` parameter, and shipped a `lib/schemas.ts` covering
  the most-trafficked endpoints (`/api/dashboard/live`,
  `/api/dashboard/ws-ticket` so far). Schema mismatches log via
  `console.error` and throw, surfacing backend/frontend drift the
  moment it happens. More schemas can be added incrementally.

---

## Round 8 — Architecture / refactor

- [x] **R8.1** Extracted MCP runtime helpers (~380 lines —
  `build_registered_server`, `discover_and_persist_tools`,
  `resolve_mcp_auth_header`, `load_mcp_servers_into_registry`,
  `spawn_mcp_health_loop`, parse helpers, McpToolDef/Result) from
  `app.rs` into a new `crates/server/src/mcp_runtime.rs`. `app.rs`
  drops from 1082 → ~700 lines and now focuses on AppState +
  router wiring. CRUD handlers updated to call via
  `crate::mcp_runtime::`.
- [N/A] **R8.2** Sub-state struct refactor deferred. Touching every
  handler signature is a huge churn for marginal benefit; Pareto-
  better to revisit when we add a 5th major sub-system.
- [x] **R8.3** Critical fix in `errors.rs`:
  `AppError::Internal(e).into_response()` was using `self.to_string()`
  as the public error message, which **leaked the full internal
  error chain to API clients**. Now the internal variant returns a
  generic "Internal server error" string while the full `e:#` chain
  is logged server-side via `tracing::error!`.

---

## Round 9 — Polish / observability

- [N/A] **R9.1** Audit log detail already rendered via `<pre>` with
  text content; no `dangerouslySetInnerHTML` anywhere in logs.tsx.
- [x] **R9.2** Nonce rate limiter switched from fixed-window
  `INCR + EXPIRE` to a Redis sorted set keyed on millisecond
  timestamps. The fixed window allowed 240/min effective at the
  window boundary; the rolling window enforces a true 120/min cap.
- [x] **R9.3** Per-user WS connection cap (4) enforced in
  `try_acquire_ws_slot` via a process-local `Mutex<HashMap>`. RAII
  `WsSlotGuard` releases on every return path. Every WS `send` is
  wrapped in a 5s `tokio::time::timeout` so a slow/dead client can't
  hang the loop.

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
