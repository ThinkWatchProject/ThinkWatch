<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" />
  <img src="https://img.shields.io/badge/React-20232A?style=for-the-badge&logo=react&logoColor=61DAFB" />
  <img src="https://img.shields.io/badge/PostgreSQL-316192?style=for-the-badge&logo=postgresql&logoColor=white" />
  <img src="https://img.shields.io/badge/Redis-DC382D?style=for-the-badge&logo=redis&logoColor=white" />
  <img src="https://img.shields.io/badge/Docker-2496ED?style=for-the-badge&logo=docker&logoColor=white" />
  <img src="https://img.shields.io/badge/Kubernetes-326CE5?style=for-the-badge&logo=kubernetes&logoColor=white" />
</p>

# ThinkWatch

**[English](README.md) | [中文](README.zh-CN.md)**

**The enterprise-grade bastion host for AI.** Secure, audit, and govern every AI API call and MCP tool invocation across your organization — from a single control plane.

Just as an SSH bastion host is the single gateway through which all server access must flow, ThinkWatch is the single gateway through which all AI access must flow. Every model request. Every tool call. Every token. Authenticated, authorized, rate-limited, logged, and accounted for.

```
                    ┌──────────────────────────────────────┐
 Claude Code ──────>│                                      │──> OpenAI
 Cursor ───────────>│    Gateway  :3000                    │──> Anthropic
 Custom Agent ─────>│    AI API + MCP Unified Proxy        │──> Google Gemini
 CI/CD Pipeline ───>│                                      │──> Azure OpenAI / AWS Bedrock
                    └──────────────────────────────────────┘
                    ┌──────────────────────────────────────┐
 Admin Browser ────>│    Console  :3001                    │
                    │    Management UI + Admin API          │
                    └──────────────────────────────────────┘
```

## Why ThinkWatch?

As AI agents proliferate across engineering teams, organizations face a growing governance challenge:

- **API keys scattered everywhere** — hardcoded in `.env` files, shared in Slack, rotated never
- **Zero visibility** — who used which model, how many tokens, at what cost?
- **No access control** — every developer has direct access to every model and every MCP tool
- **Compliance gaps** — no audit trail for AI-assisted code generation or data access
- **Cost surprises** — monthly AI bills that nobody can explain or attribute

ThinkWatch solves all of this with a single deployment.

## Key Features

### AI API Gateway
- **Multi-format API proxy** — natively serves OpenAI Chat Completions (`/v1/chat/completions`), Anthropic Messages (`/v1/messages`), and OpenAI Responses (`/v1/responses`) APIs on a single port; works as a drop-in replacement for Cursor, Continue, Cline, Claude Code, and the OpenAI/Anthropic SDKs
- **Multi-provider routing** — OpenAI, Anthropic, Google Gemini, Azure OpenAI, AWS Bedrock, or any OpenAI-compatible endpoint
- **Automatic format conversion** — Anthropic Messages API, Google Gemini, Azure OpenAI, AWS Bedrock Converse API, and more, all behind a unified interface
- **Provider auto-loading** — active providers are loaded from the database at startup and registered in the model router; default model prefixes (`gpt-`/`o1-`/`o3-`/`o4-` for OpenAI, `claude-` for Anthropic, `gemini-` for Google) route automatically; Azure and Bedrock require explicit model registration
- **Streaming SSE pass-through** — zero-overhead forwarding with real-time token counting
- **Virtual API keys** — issue scoped `ab-` keys per team, per project, per developer; revoke in one click
- **API key lifecycle management** — automatic rotation with grace periods, per-key inactivity timeout, expiry warnings, and background policy enforcement
- **Sliding-window rate limiting** — RPM and TPM limits via Redis, per key or per user
- **Circuit breaker** — three-state (Closed/Open/HalfOpen) circuit breaker with configurable failure threshold and recovery period
- **Retry with exponential backoff** — configurable retries with jitter for network errors and upstream rate limits
- **Real-time cost tracking** — per-model pricing with budget alerts and team attribution

### MCP Gateway
- **Centralized tool proxy** — one MCP endpoint that aggregates tools from all upstream servers
- **Namespace isolation** — `github__create_issue`, `postgres__query` — no tool name collisions
- **Tool-level RBAC** — control exactly which users or roles can invoke which tools
- **Connection pooling & health monitoring** — automatic reconnection, background health checks
- **Full audit trail** — every tool invocation logged with user, parameters, and response

### Security & Compliance
- **Dual-port architecture** — gateway (public-facing) and console (internal-only) on separate ports
- **Role-based access control** — 5-tier RBAC: Super Admin, Admin, Team Manager, Developer, Viewer
- **SSO/OIDC** — plug into Zitadel, Okta, Azure AD, or any OIDC-compliant provider
- **AES-256-GCM encryption** — provider API keys and secrets encrypted at rest
- **SHA-256 key hashing** — virtual API keys stored as hashes; plaintext shown exactly once
- **Content Security Policy** — CSP headers on the console port to prevent XSS and injection attacks
- **JWT entropy enforcement** — minimum 32-character secret with entropy validation at startup
- **Startup dependency validation** — verifies PostgreSQL, Redis, and encryption key availability with clear error messages before accepting traffic
- **Security headers** — X-Content-Type-Options, X-Frame-Options, CORS whitelisting, request timeouts
- **Soft-delete** — users, providers, and API keys use soft-delete (`deleted_at` column) with automatic purge after 30 days
- **Distroless containers** — minimal attack surface in production (2MB runtime image, no shell)

### Operations & Configuration
- **Dynamic configuration** — most settings stored in database (`system_settings` table), configurable via Web UI (Admin > Settings with 7 category tabs)
- **First-run setup wizard** — guided `/setup` wizard creates the super_admin account, configures the site, and optionally adds the first provider and API key
- **Configuration Guide** — built-in `/gateway/guide` page in the web console with copy-paste setup instructions for Claude Code, Cursor, Continue, Cline, OpenAI SDK, Anthropic SDK, and cURL; auto-detects the gateway URL
- **Multi-instance sync** — configuration changes propagated across instances via Redis Pub/Sub
- **Data retention policies** — configurable retention periods for usage records and audit logs with automatic daily purge

### Observability
- **Prometheus metrics** — `GET /metrics` endpoint on the gateway port (3000) exposing `gateway_requests_total`, `gateway_request_duration_seconds`, `gateway_tokens_total`, `gateway_rate_limited_total`, `circuit_breaker_state`, and more
- **Enhanced health checks** — `/health/live` (liveness probe), `/health/ready` (readiness probe with PostgreSQL and Redis checks), `/api/health` (detailed latency and pool statistics)
- **ClickHouse-powered audit logs** — SQL-queryable audit logs across all API calls and tool invocations, stored in ClickHouse for high-performance columnar analytics
- **Audit log forwarding** — multi-channel delivery: UDP/TCP Syslog (RFC 5424), Kafka, and HTTP webhooks — route audit events to any SIEM, data lake, or alerting pipeline
- **Usage analytics** — token consumption by user, team, model, and time period
- **Cost analytics** — MTD spend, budget utilization, per-model cost breakdown
- **Health dashboard** — real-time status of PostgreSQL, Redis, ClickHouse, and all MCP servers

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Backend | Rust, Axum 0.8, SQLx 0.8, fred 10 (Redis), OpenTelemetry |
| Frontend | React 19, TypeScript 6, Vite 8, shadcn/ui, Tailwind CSS 4 |
| Database | PostgreSQL 18 |
| Cache & Rate Limiting | Redis 8 |
| Audit Log Storage | ClickHouse (columnar OLAP database) |
| SSO | Zitadel (or any OIDC provider) |
| Containers | Distroless (2MB runtime), Helm Chart for K8s |

## Quick Start

```bash
# 1. Start infrastructure
docker compose -f deploy/docker-compose.dev.yml up -d

# 2. Start backend (gateway :3000 + console :3001)
cp .env.example .env
cargo run -p think-watch-server

# 3. Start frontend dev server
cd web && pnpm install && pnpm dev

# 4. Complete the setup wizard at http://localhost:5173/setup
```

See the **[Deployment Guide](docs/en/deployment-guide.md)** for production setup with Docker Compose or Kubernetes.

## Documentation

| Document | Description |
|----------|-------------|
| **[Architecture](docs/en/architecture.md)** | System design, dual-port model, data flow diagrams |
| **[Deployment Guide](docs/en/deployment-guide.md)** | Docker Compose, Kubernetes Helm, SSL, production hardening |
| **[API Reference](docs/en/api-reference.md)** | Complete endpoint documentation for Gateway and Console |
| **[Security](docs/en/security.md)** | Auth model, encryption, RBAC, threat model, hardening checklist |
| **[Configuration](docs/en/configuration.md)** | All environment variables and their effects |

## Port Architecture

| Port | Server | Exposure | Purpose |
|------|--------|----------|---------|
| `3000` | Gateway | **Public** — expose to AI clients | `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1/models`, `/mcp`, `/metrics`, `/health/*` |
| `3001` | Console | **Internal** — behind VPN/firewall | `/api/*` management endpoints, Web UI |

> In production, **only port 3000** should be reachable from the internet. Port 3001 should be restricted to your admin network.

## Project Structure

```
ThinkWatch/
├── crates/
│   ├── server/          # Dual-port Axum server (gateway + console)
│   ├── gateway/         # AI API proxy: routing, streaming, rate limiting, cost tracking
│   ├── mcp-gateway/     # MCP proxy: JSON-RPC, tool aggregation, access control
│   ├── auth/            # JWT, OIDC, API key, password hashing, RBAC
│   └── common/          # Config, DB, models, crypto, audit logger
├── migrations/          # 12 PostgreSQL migration files
├── web/                 # React frontend — ~20 page components
├── deploy/
│   ├── docker/          # Dockerfile.server (distroless), Dockerfile.web (nginx)
│   ├── docker-compose.yml       # Production deployment
│   ├── docker-compose.dev.yml   # Development (PG + Redis + ClickHouse + Zitadel)
│   └── helm/think-watch/      # Kubernetes Helm chart
└── docs/                # Detailed documentation
```

## Contributing

Contributions are welcome. Please open an issue to discuss before submitting a PR for major changes.

## License

ThinkWatch is source-available under the [Business Source License 1.1](LICENSE).
Non-production use is free. Production use is free up to both `10,000,000`
Billable Tokens and `10,000` MCP Tool Calls per UTC calendar month; above
either threshold, a commercial license is required and priced by usage tiers.

See [LICENSING.md](LICENSING.md) for the production-use thresholds, the
Billable Token and MCP Tool Call definitions, the tiering model, and the
changeover to `GPL-2.0-or-later`.

## Star History

<a href="https://www.star-history.com/#ThinkWatch/ThinkWatch&Date">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=ThinkWatch/ThinkWatch&type=Date&theme=dark" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=ThinkWatch/ThinkWatch&type=Date" />
   <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=ThinkWatch/ThinkWatch&type=Date" />
 </picture>
</a>
