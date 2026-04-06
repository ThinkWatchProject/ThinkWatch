.PHONY: dev dev-backend dev-frontend infra infra-down check precommit test build clean

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
# pnpm build + i18n parity check)
precommit:
	cargo check --workspace
	cargo test --workspace
	cargo clippy --workspace -- -D warnings
	cargo fmt --all -- --check
	cd web && pnpm check:i18n
	cd web && pnpm build

# Run all tests
test:
	cargo test --workspace

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

# Production deploy
deploy:
	docker compose -f deploy/docker-compose.yml --env-file .env.production up -d

# Clean build artifacts
clean:
	cargo clean
	rm -rf web/dist web/node_modules/.tmp
