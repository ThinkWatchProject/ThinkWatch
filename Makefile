.PHONY: dev dev-backend dev-frontend infra infra-down check precommit test test-it test-e2e build clean \
        deploy deploy-down secrets helm-deploy helm-deploy-down helm-template helm-lint

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

# Pre-commit: mirrors CI exactly (cargo check + test + clippy + fmt +
# i18n parity + pnpm test + pnpm build)
precommit:
	cargo check --workspace
	cargo test --workspace
	cargo clippy --workspace -- -D warnings
	cargo fmt --all -- --check
	cd web && pnpm check:i18n
	cd web && pnpm test
	cd web && pnpm build

# Run all tests
test:
	cargo test --workspace

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
