# ThinkWatch - Claude Code Instructions

## Pre-commit

Always run `make precommit` before committing. The two pipelines run in parallel via a recursive `make -j2`:

```
# pipeline 1 (rust)                   | # pipeline 2 (frontend)
cargo nextest run --workspace ...     | cd web && pnpm check:i18n
cargo clippy --workspace -- -D warn   |          && pnpm test
cargo fmt --all -- --check            |          && pnpm build
```

Wall-clock is `max(rust, frontend)` instead of their sum, since the pipelines never share build artifacts. `cargo check` is intentionally absent — clippy is a strict superset, and re-running check separately just doubles the Rust compile time on touched-common-types diffs. Clippy runs *without* `--all-targets` so the test sources nextest already compiled don't get re-walked under the lint pass.

`cargo nextest` is required (replaces `cargo test` for ~3× faster cross-binary parallelism). Install once via `make tools` (idempotent — runs `cargo install cargo-nextest --locked`).

The Rust pipeline runs `nextest --no-run` over `--lib --bins --tests` (so a syntax error in an `#[ignore]`-marked integration test still fails the build), then runs `nextest` over `--lib --bins` only. Every test in `crates/test-support/tests/` is `#[ignore]`-marked and only runs via `make test-it`; launching those 40+ binaries during precommit does nothing but pay macOS Sequoia 26.x's per-binary dyld provenance scan (~25-36s each on first launch). Skipping them brings precommit's test phase from minutes to seconds on macOS.

**Do NOT use `tsc --noEmit` alone** as the frontend check. `tsc -b` (inside `pnpm build`) is stricter and catches unused imports (TS6133) that `--noEmit` misses.

## Integration tests

Cross-cutting integration tests live in `crates/test-support/tests/`. They boot the full server in-process against a fresh per-test Postgres database and a dedicated Redis logical DB (1 by default). Each test fn carries `#[ignore]` so `cargo nextest run --workspace` skips them — `make test-it` opts in.

Required infra: `make infra` (Postgres + Redis). Tests run serially via `--test-threads=1` because they share the Redis instance.

Add a new test by dropping a function into the appropriate file (`auth.rs`, `gateway_proxy.rs`, `console_admin.rs`, …) — fixtures and the `TestApp` harness handle DB / Redis isolation, signed-client wiring, and wiremock for upstream AI providers. The full prelude is `use think_watch_test_support::prelude::*;`.

ClickHouse-dependent tests (analytics, gateway_logs cost) call `TestApp::spawn_with_clickhouse()` instead — that creates a per-test CH database, loads the production schema into it, and drops it on teardown. They live in `tests/analytics_clickhouse.rs`.

## Frontend E2E (Playwright)

Browser E2E specs live in `web/e2e/`. Run via `make test-e2e` (requires `make dev-backend` in another terminal — the vite dev server is started by the playwright config). Tests use accessibility-friendly selectors (`getByRole`, `getByLabel`) so the i18n bundle can change without breaking them.

## Project structure

- `crates/server` — Dual-port Axum server (gateway :3000 + console :3001)
- `crates/gateway` — AI API proxy logic (OpenAI, Anthropic, Gemini, Azure, Bedrock)
- `crates/mcp-gateway` — MCP tool proxy
- `crates/auth` — JWT, API key, OIDC, RBAC, TOTP
- `crates/common` — Shared models, config, crypto, validation, audit logging
- `web/` — React 19 + TypeScript + Vite + shadcn/ui + i18next (en/zh)
- `deploy/` — Docker Compose, Helm charts, ClickHouse config

## Environment

Single `.env` at project root. Both `cargo run` (dotenvy) and `docker compose` (via `--env-file .env` in Makefile) read from it. No duplicate `.env` files in subdirectories.

## i18n

Keys in `web/src/i18n/en.json` and `zh.json` must stay in sync (perfect parity). Some keys are referenced dynamically via template literals (e.g. `t(\`setup.steps.${step}\`)`) — don't remove those.
