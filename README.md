<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" />
  <img src="https://img.shields.io/badge/React-20232A?style=for-the-badge&logo=react&logoColor=61DAFB" />
  <img src="https://img.shields.io/badge/PostgreSQL-316192?style=for-the-badge&logo=postgresql&logoColor=white" />
  <img src="https://img.shields.io/badge/Redis-DC382D?style=for-the-badge&logo=redis&logoColor=white" />
  <img src="https://img.shields.io/badge/Docker-2496ED?style=for-the-badge&logo=docker&logoColor=white" />
  <img src="https://img.shields.io/badge/Kubernetes-326CE5?style=for-the-badge&logo=kubernetes&logoColor=white" />
</p>

# AgentBastion

**[English](README.md) | [中文](README.zh-CN.md)**

**The enterprise-grade bastion host for AI.** Secure, audit, and govern every AI API call and MCP tool invocation across your organization — from a single control plane.

Just as an SSH bastion host is the single gateway through which all server access must flow, AgentBastion is the single gateway through which all AI access must flow. Every model request. Every tool call. Every token. Authenticated, authorized, rate-limited, logged, and accounted for.

```
                    ┌──────────────────────────────────────┐
 Claude Code ──────>│                                      │──> OpenAI
 Cursor ───────────>│    Gateway  :3000                    │──> Anthropic
 Custom Agent ─────>│    AI API + MCP Unified Proxy        │──> Google Gemini
 CI/CD Pipeline ───>│                                      │──> Self-hosted LLMs
                    └──────────────────────────────────────┘
                    ┌──────────────────────────────────────┐
 Admin Browser ────>│    Console  :3001                    │
                    │    Management UI + Admin API          │
                    └──────────────────────────────────────┘
```

## Why AgentBastion?

As AI agents proliferate across engineering teams, organizations face a growing governance challenge:

- **API keys scattered everywhere** — hardcoded in `.env` files, shared in Slack, rotated never
- **Zero visibility** — who used which model, how many tokens, at what cost?
- **No access control** — every developer has direct access to every model and every MCP tool
- **Compliance gaps** — no audit trail for AI-assisted code generation or data access
- **Cost surprises** — monthly AI bills that nobody can explain or attribute

AgentBastion solves all of this with a single deployment.

## Key Features

### AI API Gateway
- **OpenAI-compatible proxy** — drop-in replacement; point `OPENAI_BASE_URL` at AgentBastion and go
- **Multi-provider routing** — OpenAI, Anthropic, Google Gemini, Azure, or any OpenAI-compatible endpoint
- **Automatic format conversion** — Anthropic Messages API, Google Gemini, and more, all behind a single OpenAI-compatible interface
- **Streaming SSE pass-through** — zero-overhead forwarding with real-time token counting
- **Virtual API keys** — issue scoped `ab-` keys per team, per project, per developer; revoke in one click
- **Sliding-window rate limiting** — RPM and TPM limits via Redis, per key or per user
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
- **Security headers** — X-Content-Type-Options, X-Frame-Options, CORS whitelisting, request timeouts
- **Distroless containers** — minimal attack surface in production (2MB runtime image, no shell)

### Observability
- **Quickwit-powered audit logs** — full-text search across all API calls and tool invocations
- **Syslog forwarding** — RFC 5424 compliant; integrate with your existing SIEM
- **Usage analytics** — token consumption by user, team, model, and time period
- **Cost analytics** — MTD spend, budget utilization, per-model cost breakdown
- **Health dashboard** — real-time status of PostgreSQL, Redis, Quickwit, and all MCP servers

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Backend | Rust, Axum 0.8, SQLx 0.8, fred 10 (Redis), OpenTelemetry |
| Frontend | React 19, TypeScript 6, Vite 8, shadcn/ui, Tailwind CSS 4 |
| Database | PostgreSQL 18 |
| Cache & Rate Limiting | Redis 8 |
| Audit Log Search | Quickwit 0.8 |
| Object Storage | RustFS (S3-compatible) |
| SSO | Zitadel (or any OIDC provider) |
| Containers | Distroless (2MB runtime), Helm Chart for K8s |

## Quick Start

```bash
# 1. Start infrastructure
docker compose -f deploy/docker-compose.dev.yml up -d

# 2. Start backend (gateway :3000 + console :3001)
cp .env.example .env
cargo run -p agent-bastion-server

# 3. Start frontend dev server
cd web && pnpm install && pnpm dev

# 4. Open http://localhost:5173
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
| `3000` | Gateway | **Public** — expose to AI clients | `/v1/chat/completions`, `/v1/models`, `/mcp` |
| `3001` | Console | **Internal** — behind VPN/firewall | `/api/*` management endpoints, Web UI |

> In production, **only port 3000** should be reachable from the internet. Port 3001 should be restricted to your admin network.

## Project Structure

```
AgentBastion/
├── crates/
│   ├── server/          # Dual-port Axum server (gateway + console)
│   ├── gateway/         # AI API proxy: routing, streaming, rate limiting, cost tracking
│   ├── mcp-gateway/     # MCP proxy: JSON-RPC, tool aggregation, access control
│   ├── auth/            # JWT, OIDC, API key, password hashing, RBAC
│   └── common/          # Config, DB, models, crypto, audit logger
├── migrations/          # 7 PostgreSQL migration files
├── web/                 # React frontend — 15 page components
├── deploy/
│   ├── docker/          # Dockerfile.server (distroless), Dockerfile.web (nginx)
│   ├── docker-compose.yml       # Production deployment
│   ├── docker-compose.dev.yml   # Development (PG + Redis + RustFS + Quickwit + Zitadel)
│   └── helm/agent-bastion/      # Kubernetes Helm chart
└── docs/                # Detailed documentation
```

## Contributing

Contributions are welcome. Please open an issue to discuss before submitting a PR for major changes.

## License

AgentBastion is source-available under the [Business Source License 1.1](LICENSE).
Non-production use is free. Production use is free up to `10,000,000`
Billable Tokens per UTC calendar month; above that threshold, a commercial
license is required and priced by usage volume.

See [LICENSING.md](LICENSING.md) for the production-use threshold, the Billable
Token definition, and the changeover to `GPL-2.0-or-later`.
