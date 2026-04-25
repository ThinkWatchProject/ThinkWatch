# ThinkWatch - Claude Code Instructions

## Pre-commit

Always run `make precommit` before committing. It mirrors the CI pipeline exactly:

```
cargo check --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cd web && pnpm check:i18n
cd web && pnpm test           # vitest run
cd web && pnpm build          # tsc -b && vite build
```

**Do NOT use `tsc --noEmit` alone** as the frontend check. `tsc -b` (inside `pnpm build`) is stricter and catches unused imports (TS6133) that `--noEmit` misses.

## Integration tests

Cross-cutting integration tests live in `crates/test-support/tests/`. They boot the full server in-process against a fresh per-test Postgres database and a dedicated Redis logical DB (1 by default). Each test fn carries `#[ignore]` so `cargo test --workspace` skips them — `make test-it` opts in.

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
