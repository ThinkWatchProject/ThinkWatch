# ThinkWatch - Claude Code Instructions

## Pre-commit

Always run `make precommit` before committing. It mirrors the CI pipeline exactly:

```
cargo check --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
cd web && pnpm build          # tsc -b && vite build
```

**Do NOT use `tsc --noEmit` alone** as the frontend check. `tsc -b` (inside `pnpm build`) is stricter and catches unused imports (TS6133) that `--noEmit` misses.

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
