.PHONY: dev dev-backend dev-frontend infra infra-down check test build clean

# Start full dev environment
dev: infra dev-backend dev-frontend

# Start infrastructure (PG, Redis, ClickHouse, Zitadel)
infra:
	docker compose -f deploy/docker-compose.dev.yml up -d

infra-down:
	docker compose -f deploy/docker-compose.dev.yml down

# Start backend (gateway :3000 + console :3001)
dev-backend:
	cargo run -p agent-bastion-server

# Start frontend dev server (:5173)
dev-frontend:
	cd web && pnpm dev

# Check everything compiles
check:
	cargo check --workspace
	cd web && pnpm exec tsc --noEmit

# Run all tests
test:
	cargo test --workspace

# Build release
build:
	cargo build --release -p agent-bastion-server
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
	docker build -f deploy/docker/Dockerfile.server -t agent-bastion-server .
	docker build -f deploy/docker/Dockerfile.web -t agent-bastion-web .

# Production deploy
deploy:
	docker compose -f deploy/docker-compose.yml --env-file .env.production up -d

# Clean build artifacts
clean:
	cargo clean
	rm -rf web/dist web/node_modules/.tmp
