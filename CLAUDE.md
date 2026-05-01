# ThinkWatch - Claude Code Instructions

## Pre-commit

Always run `make precommit` before committing. The two pipelines run in parallel via a recursive `make -j2`:

```
# pipeline 1 (rust)                            | # pipeline 2 (frontend)
cargo clippy --workspace --lib --bins ...      | cd web && pnpm check:i18n
cargo clippy --workspace --tests ...           |          && pnpm test
cargo nextest run --workspace --lib --bins     |          && pnpm build
cargo fmt --all -- --check                     |
```

Integration tests are type-checked (`clippy --tests`) but **not linked** at precommit. Linking 40+ test binaries pays the macOS Sequoia dyld provenance scan (~80s) just to catch the rare missing-symbol error — `make precommit-strict` adds `nextest --tests --no-run` for that case. CI runs the full integration suite.

Wall-clock is `max(rust, frontend)` instead of their sum. `cargo check` is intentionally absent — clippy is a strict superset.

**rust-analyzer MUST use a separate `target/` subtree** — see `.vscode/settings.json` (`rust-analyzer.cargo.targetDir = true`). Without this, RA's continuous `cargo check` holds `target/debug/.cargo-lock` and any terminal cargo blocks idle on it; the symptom is `time` reporting `real ≫ user+sys` (we observed 1109s wall / 22s CPU before isolating). Reload VSCode after editing the file.

`cargo nextest` is required for `make test` / `make test-it` (cross-binary parallelism). Install once via `make tools` (idempotent — runs `cargo install cargo-nextest --locked`).

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
