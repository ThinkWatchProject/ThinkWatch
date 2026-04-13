<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.png">
    <img src="assets/logo.png" alt="ThinkWatch" width="480">
  </picture>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" />
  <img src="https://img.shields.io/badge/React-20232A?style=for-the-badge&logo=react&logoColor=61DAFB" />
  <img src="https://img.shields.io/badge/PostgreSQL-316192?style=for-the-badge&logo=postgresql&logoColor=white" />
  <img src="https://img.shields.io/badge/Redis-DC382D?style=for-the-badge&logo=redis&logoColor=white" />
  <img src="https://img.shields.io/badge/Docker-2496ED?style=for-the-badge&logo=docker&logoColor=white" />
  <img src="https://img.shields.io/badge/Kubernetes-326CE5?style=for-the-badge&logo=kubernetes&logoColor=white" />
[![SafeSkill 88/100](https://img.shields.io/badge/SafeSkill-88%2F100_Passes%20with%20Notes-yellow)](https://safeskill.dev/scan/thinkwatchproject-thinkwatch)
</p>

# ThinkWatch

**[English](README.md) | [中文](README.zh-CN.md)**

**The enterprise-grade secure gateway for AI.** Secure, audit, and govern every AI API call and MCP tool invocation across your organization — from a single control plane.

Just as an SSH secure gateway is the single gateway through which all server access must flow, ThinkWatch is the single gateway through which all AI access must flow. Every model request. Every tool call. Every token. Authenticated, authorized, rate-limited, logged, and accounted for.

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
- **Virtual API keys** — issue scoped `tw-` keys; the same `tw-` token works on both the AI gateway and the MCP gateway via a per-key `surfaces` allowlist
- **API key lifecycle management** — automatic rotation with grace periods, per-key inactivity timeout, expiry warnings, and background policy enforcement
- **Composable rate limits & budgets** — multi-window sliding limits (1m / 5m / 1h / 5h / 1d / 1w) and natural-period token budgets (daily / weekly / monthly), keyed per user, per API key, per provider, or per MCP server. See [Rate limits & budgets](#rate-limits--budgets) below
- **Per-model token weighting** — gpt-4o tokens can count more than gpt-3.5 tokens against the same quota via configurable `input_multiplier` / `output_multiplier`
- **Circuit breaker** — three-state (Closed/Open/HalfOpen) circuit breaker with configurable failure threshold and recovery period
- **Retry with exponential backoff** — configurable retries with jitter for network errors and upstream rate limits
- **Real-time cost tracking** — per-model pricing with team attribution

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
- **Password complexity** — minimum 8 characters with required uppercase, lowercase, and digit
- **Session IP binding** — admin sessions bound to client IP; stolen tokens cannot be replayed from a different network
- **Distroless containers** — minimal attack surface in production (2MB runtime image, no shell)

### Operations & Configuration
- **Dynamic configuration** — most settings stored in database (`system_settings` table), configurable via Web UI (Admin > Settings with 7 category tabs)
- **First-run setup wizard** — guided `/setup` wizard creates the super_admin account, configures the site, and optionally adds the first provider and API key
- **Configuration Guide** — built-in `/gateway/guide` page in the web console with copy-paste setup instructions for Claude Code, Cursor, Continue, Cline, OpenAI SDK, Anthropic SDK, and cURL; auto-detects the gateway URL
- **Multi-instance sync** — configuration changes propagated across instances via Redis Pub/Sub
- **Data retention policies** — configurable retention periods for usage records and audit logs with automatic daily purge

### Observability
- **Prometheus metrics** — `GET /metrics` endpoint on the gateway port (3000) exposing `gateway_requests_total`, `gateway_request_duration_seconds`, `gateway_tokens_total`, `gateway_rate_limited_total`, `circuit_breaker_state`, `gateway_stream_completion_total`, `audit_log_dropped_total`, and more. **Disabled by default** — set `METRICS_BEARER_TOKEN` (the secret-generation script populates it automatically) to mount the route, then pass the same value as `Authorization: Bearer <token>` from your scraper. When unset, the route returns 404 and the recorder isn't even installed (zero memory / CPU cost).
- **Enhanced health checks** — `/health/live` (liveness probe), `/health/ready` (readiness probe verifying PostgreSQL, Redis, **and at least one active provider** — so K8s won't route AI traffic to a fresh pod with an empty router), `/api/health` (detailed latency and pool statistics)
- **ClickHouse-powered audit logs** — SQL-queryable audit logs across all API calls and tool invocations, stored in ClickHouse for high-performance columnar analytics
- **Audit log forwarding** — multi-channel delivery: UDP/TCP Syslog (RFC 5424), Kafka, and HTTP webhooks — route audit events to any SIEM, data lake, or alerting pipeline
- **Usage analytics** — token consumption by user, team, model, and time period
- **Cost analytics** — MTD spend, budget utilization, per-model cost breakdown
- **Health dashboard** — real-time status of PostgreSQL, Redis, ClickHouse, and all MCP servers
- **Unified log explorer** — search across audit, gateway, MCP, access, and platform logs from a single page with structured query syntax

## Rate limits & budgets

ThinkWatch enforces two parallel kinds of quota at every gateway request,
both managed from the same admin UI:

| | Sliding-window rate limits | Natural-period budget caps |
|---|---|---|
| **What it counts** | Requests OR weighted tokens, depending on the rule's `metric` | Weighted tokens only |
| **Window shape** | Rolling 60-bucket window: `1m / 5m / 1h / 5h / 1d / 1w` | Calendar-aligned: `daily / weekly / monthly` (resets on the period boundary) |
| **Backing store** | Redis ZSET-style buckets | Redis INCR counters keyed by `subject:period:bucket_id` |
| **When it fires** | Pre-flight (requests metric) AND post-flight (tokens metric) | Post-flight only |
| **Hard or soft?** | Hard for requests metric, soft for tokens metric | Soft cap — exactly one request can push you over before subsequent calls in the same period are rejected |

### Subjects

A single request can be subject to multiple rules and budgets at once. The
engine resolves the request to a set of `(subject_kind, subject_id)` tuples
and runs every enabled rule against all of them in one atomic Lua check.
**Any rule rejecting → the request is rejected. All-or-nothing INCR.**

| Subject | Rate limit rules | Budget caps |
|---|---|---|
| `user`        | ✅ ai_gateway / mcp_gateway | ✅ |
| `api_key`     | ✅ ai_gateway / mcp_gateway | ✅ |
| `provider`    | ✅ ai_gateway only          | ✅ |
| `mcp_server`  | ✅ mcp_gateway only         | ❌ (no token cost concept) |
| `team`        | (use user / api_key)        | ✅ |

For an AI request the engine resolves: `api_key + user + provider`. For an
MCP request: `user + mcp_server`. Per-subject limits stack — a developer
can have a personal cap, AND their API key can have a tighter cap, AND the
provider can have a global cap, all enforced simultaneously.

### Three flavors of "tokens"

Three numbers float around the system. Don't confuse them.

| Number | Source | Used for | Where it shows up |
|---|---|---|---|
| **Raw tokens** | `usage_records.input_tokens / output_tokens` | Real provider-billed token counts | Analytics, cost reports |
| **Weighted tokens** | `raw × models.input_multiplier / output_multiplier` | Quota accounting (rate limits + budgets) | Limits panel "X / Y used" |
| **USD cost** | `raw × models.input_price / output_price` | Billing | Costs page |

The two `models` columns are independent. Weighted tokens are a *relative*
unit (gpt-3.5-turbo = 1.0 by convention); they have no global USD value.
USD always comes from the real per-token price. By default every model has
multiplier `1.0`, which means quotas count raw tokens. Tune the multipliers
on the model management page to make a 1M-token monthly cap actually
survive a single gpt-4o burst.

### Example

> *Operator goal:* "developers get 60 requests/minute on the AI gateway,
> 1M weighted tokens/day, and 20M weighted tokens/month — but the entire
> OpenAI provider has a 100k requests/hour ceiling."

```
On the developer USER subject:
  rate_limit_rule  ai_gateway / requests / 60s   → 60
  rate_limit_rule  ai_gateway / tokens   / 1d    → 1_000_000
  budget_cap       monthly                       → 20_000_000

On the OpenAI PROVIDER subject:
  rate_limit_rule  ai_gateway / requests / 1h    → 100_000
```

A request from any developer key against gpt-4o then has to clear:
1. Developer's per-minute request rule
2. OpenAI provider's per-hour request rule
3. After the response: developer's per-day token rule
4. After the response: developer's monthly token budget

Any one of those failing → 429 with the rule label in the body
(`user:requests/1m`, `provider:requests/1h`, etc).

### Failure mode

When Redis is unavailable the engine defaults to **fail open** and bumps
the `gateway_rate_limiter_fail_open_total` / `gateway_budget_fail_open_total`
metrics so the AI control plane keeps running through a Redis blip.
Operators who would rather refuse traffic than miss accounting can flip
`security.rate_limit_fail_closed = true` on the Settings page; the
gateway then returns 429 (`rate_limiter_unavailable`) for any request
the engine couldn't check, and bumps `gateway_rate_limiter_fail_closed_total`.

### Budget alerts

Crossing 50% / 80% / 95% / 100% of any budget cap fires a structured
`budget threshold crossed` warn log and bumps
`gateway_budget_alert_total{subject_kind, period, threshold_pct}`.
Each threshold fires at most once per period bucket — if a request
takes you from 60% straight past 100% the 80 / 95 / 100 lines all
fire on that single response, but the next request in the same
period won't re-fire any of them.

### Streaming token accounting

Token-metric rules and budget caps fire on streaming responses too,
provided the upstream actually surfaces usage on the SSE stream:

- **OpenAI**: requires the client to set
  `stream_options.include_usage = true` on the request body.
- **Anthropic**: cumulative usage on the final `message_delta` event
  is captured automatically.

If neither upstream surfaces usage on the stream the post-flight
accounting silently no-ops for that request — the rate-limit and
budget counters stay accurate within the limits of what the
upstream is willing to tell us.

### PII redaction and streaming responses

The PII redactor (configured at Admin > Settings > PII patterns)
runs on every prompt before it's forwarded upstream — emails,
phone numbers, ID cards etc. are replaced with `{{EMAIL_xxx_1}}`
style placeholders so the upstream never sees the original. On
**non-streaming** responses the gateway then runs `restore_response`
on the way back, so the client sees the original PII the model
would have echoed.

On **streaming** (SSE) responses the gateway does NOT restore the
placeholders — re-stitching them across chunk boundaries is its own
project. As a result, streaming clients see the placeholder text
verbatim if the model echoes user PII back in its answer. The
prompt-side redaction still happens, so the upstream provider
never sees the original PII either way; this is purely a
client-side cosmetic gap on streaming responses. Switch the client
to non-streaming if it needs the original text restored.

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
make infra

# 2. Start backend (gateway :3000 + console :3001)
cp .env.example .env
make dev-backend

# 3. Start frontend dev server
cd web && pnpm install && pnpm dev

# 4. Complete the setup wizard at http://localhost:5173/setup
```

See the **[Deployment Guide](https://thinkwat.ch/docs/deployment-guide)** for production setup with Docker Compose or Kubernetes.

## Documentation

Full documentation: **[thinkwat.ch/docs](https://thinkwat.ch/docs)**

| Document | Description |
|----------|-------------|
| **[Architecture](https://thinkwat.ch/docs/architecture)** | System design, dual-port model, data flow diagrams |
| **[Deployment Guide](https://thinkwat.ch/docs/deployment-guide)** | Docker Compose, Kubernetes Helm, SSL, production hardening |
| **[Configuration](https://thinkwat.ch/docs/configuration)** | All environment variables and their effects |
| **[API Reference](https://thinkwat.ch/docs/api-reference)** | Complete endpoint documentation for Gateway and Console |
| **[Security](https://thinkwat.ch/docs/security)** | Auth model, encryption, RBAC, threat model, hardening checklist |
| **[Secret Rotation](https://thinkwat.ch/docs/secret-rotation)** | Rotating provider keys, JWT secrets, and admin credentials |

## Port Architecture

| Port | Server | Exposure | Purpose |
|------|--------|----------|---------|
| `3000` | Gateway | **Public** — expose to AI clients | `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1/models`, `/mcp`, `/metrics`†, `/health/*` |
| `3001` | Console | **Internal** — behind VPN/firewall | `/api/*` management endpoints, Web UI |

† `/metrics` is only mounted when `METRICS_BEARER_TOKEN` is set. Without the env var the route returns 404 and the Prometheus recorder isn't installed.

> In production, **only port 3000** should be reachable from the internet. Port 3001 should be restricted to your admin network.

## Project Structure

```
ThinkWatch/
├── crates/
│   ├── server/          # Dual-port Axum server (gateway + console)
│   ├── gateway/         # AI API proxy: routing, streaming, rate limiting, cost tracking
│   ├── mcp-gateway/     # MCP proxy: JSON-RPC, tool aggregation, access control
│   ├── auth/            # JWT, OIDC, API key, password hashing, RBAC
│   └── common/          # Config, DB, models, crypto, validation, audit logger
├── migrations/          # 12 PostgreSQL migration files
├── web/                 # React frontend — ~20 page components
├── deploy/
│   ├── docker/          # Dockerfile.server (distroless), Dockerfile.web (nginx)
│   ├── docker-compose.yml       # Production deployment
│   ├── docker-compose.dev.yml   # Development (PG + Redis + ClickHouse + Zitadel)
│   └── helm/think-watch/      # Kubernetes Helm chart
└── ...
```

> Documentation: **[thinkwat.ch/docs](https://thinkwat.ch/docs)**

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

<a href="https://www.star-history.com/#ThinkWatchProject/ThinkWatch&Date">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=ThinkWatchProject/ThinkWatch&type=Date&theme=dark" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=ThinkWatchProject/ThinkWatch&type=Date" />
   <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=ThinkWatchProject/ThinkWatch&type=Date" />
 </picture>
</a>
