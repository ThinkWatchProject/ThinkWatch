.PHONY: dev dev-backend dev-frontend infra infra-down check precommit precommit-rust precommit-frontend \
        precommit-strict test test-it test-e2e build clean \
        tools deploy deploy-down secrets helm-deploy helm-deploy-down helm-template helm-lint

# Start full dev environment
dev: infra dev-backend dev-frontend

# Start infrastructure (PG, Redis, ClickHouse, Zitadel)
infra:
	docker compose -f deploy/docker-compose.dev.yml --env-file .env up -d

infra-down:
	docker compose -f deploy/docker-compose.dev.yml --env-file .env down

# Start backend (gateway :3000 + console :3001)
dev-backend:
	cargo run -p think-watch-server

# Start frontend dev server (:5173)
dev-frontend:
	cd web && pnpm dev

# Quick compile check
check:
	cargo check --workspace
	cd web && pnpm exec tsc --noEmit

# Pre-commit: clippy + unit tests + fmt. Integration-test *linking*
# (`nextest --no-run --tests`) is deferred to `make precommit-strict`
# / CI — that step only catches link errors (extern symbols etc.),
# which are rare, while costing ~80s for the 40+ test binaries on
# macOS Sequoia's per-binary dyld provenance scan.
#
# Speedup levers:
#   1. `cargo check` is dropped — `cargo clippy` is a strict superset.
#   2. `cargo test` is replaced by `cargo nextest run` so each test
#      binary runs in its own process; on a touched-common-types diff
#      this stops the workspace from blocking on the slowest binary.
#   3. The Rust and frontend pipelines run in parallel via a recursive
#      `make -j2` — wall-clock is `max(rust, frontend)` instead of sum.
#   4. **VSCode rust-analyzer must use a separate `target/` subtree** —
#      see `.vscode/settings.json` (`rust-analyzer.cargo.targetDir = true`).
#      Without that setting, RA's `cargo check` holds `target/debug/.cargo-lock`
#      and any terminal cargo blocks idle on it; symptom is `time` showing
#      `real >> user+sys` (we observed 1109s real / 22s CPU before the fix).
#
# Run `make tools` once to install cargo-nextest if it's missing.
precommit:
	@$(MAKE) -j2 precommit-rust precommit-frontend

precommit-rust:
	cargo clippy --workspace --lib --bins -- -D warnings
	cargo clippy --workspace --tests -- -D warnings
	cargo nextest run --workspace --lib --bins
	cargo fmt --all -- --check

# Stricter precommit — also LINKS the integration test binaries to
# catch the rare link-time errors (missing extern symbols, ABI drift).
# ~+80s on top of `precommit`. Use before pushing changes to a `pub`
# API consumed by `crates/test-support/tests/` if you don't want CI
# to bounce them.
precommit-strict: precommit
	cargo nextest run --workspace --tests --no-run

precommit-frontend:
	cd web && pnpm check:i18n
	cd web && pnpm test
	cd web && pnpm build

# Run unit tests (same pattern as precommit's test step). Integration
# tests live in crates/test-support/tests/ and run via `make test-it`.
test:
	cargo nextest run --workspace --lib --bins

# Install dev tools that precommit + CI assume are available locally.
# Idempotent: cargo install is a no-op when the binary is already
# present at the same version.
tools:
	cargo install cargo-nextest --locked

# Run the full integration test suite (test-support crate). Tests
# are #[ignore]-marked so the default `cargo test --workspace` skips
# them — this target opts in. Required infra: Postgres + Redis from
# `make infra` (or any deploy/docker-compose.dev.yml stack).
#
# Tests share a single Redis logical DB (1 by default) and run
# serially via --test-threads=1 to keep nonce / lockout counters
# from cross-pollinating.
#
# Override the infra targets when running against a different stack:
#   TEST_DATABASE_BASE_URL=postgres://user:pwd@host:5432 \
#   TEST_REDIS_URL=redis://:pwd@host:6379/1 \
#   make test-it
test-it:
	cargo test -p think-watch-test-support -- --ignored --test-threads=1

# Browser E2E via Playwright. Requires the backend to already be
# running (`make dev-backend`); the vite dev server is started by
# the playwright config's `webServer` block automatically. Override
# `PW_BASE_URL` to point at a deployed environment.
test-e2e:
	cd web && pnpm test:e2e

# Build release
build:
	cargo build --release -p think-watch-server
	cd web && pnpm build

# Lint
lint:
	cargo clippy --workspace -- -D warnings
	cargo fmt --all -- --check

# Format code
fmt:
	cargo fmt --all

# Docker build (production)
docker-build:
	docker build -f deploy/docker/Dockerfile.server -t think-watch-server .
	docker build -f deploy/docker/Dockerfile.web -t think-watch-web .

# Production deploy — auto-generates .env.production on first run
deploy: .env.production
	docker compose -f deploy/docker-compose.yml --env-file .env.production up -d

deploy-down:
	docker compose -f deploy/docker-compose.yml --env-file .env.production down

# Explicit secrets (re)generation; respects FORCE=1 to overwrite
secrets:
	bash deploy/generate-secrets.sh

.env.production:
	@echo "→ .env.production not found — generating secrets…"
	@bash deploy/generate-secrets.sh

# ---- Kubernetes / Helm ----
HELM_RELEASE    ?= thinkwatch
HELM_NAMESPACE  ?= thinkwatch
HELM_CHART      := deploy/helm/think-watch
HELM_VALUES     ?=

HELM_VALUES_FLAG := $(if $(HELM_VALUES),-f $(HELM_VALUES),)

# One-click cluster install. Pulls chart deps, generates random secrets
# on first install, preserves them on upgrade via the chart's lookup pattern.
helm-deploy:
	helm dependency update $(HELM_CHART)
	helm upgrade --install $(HELM_RELEASE) $(HELM_CHART) \
	  --namespace $(HELM_NAMESPACE) --create-namespace \
	  $(HELM_VALUES_FLAG)

helm-deploy-down:
	helm uninstall $(HELM_RELEASE) --namespace $(HELM_NAMESPACE)

helm-template:
	helm dependency update $(HELM_CHART) >/dev/null
	helm template $(HELM_RELEASE) $(HELM_CHART) --namespace $(HELM_NAMESPACE) $(HELM_VALUES_FLAG)

helm-lint:
	helm dependency update $(HELM_CHART) >/dev/null
	helm lint $(HELM_CHART) $(HELM_VALUES_FLAG)

# Clean build artifacts
clean:
	cargo clean
	rm -rf web/dist web/node_modules/.tmp
