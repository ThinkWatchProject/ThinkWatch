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

ThinkWatch's MCP gateway is built on a single design choice that most MCP proxies skip: **the upstream server sees the real end user, not a shared service account.** Every other capability follows from that. See [MCP Gateway: how we compare](#mcp-gateway-how-we-compare) below.

- **Per-user upstream identity** — every MCP request carries the calling user's own OAuth token or PAT to GitHub / Notion / Linear / Slack / Atlassian / Feishu / GitLab / Cloudflare / Google / Discord / etc. Tokens are AES-256-GCM encrypted in `mcp_user_credentials`. Most "MCP gateways" pin one shared admin token to the server config — so the upstream's audit log shows every action as the same service account. ThinkWatch propagates real identity end to end.
- **Multi-account per user** — bind work and personal GitHub accounts to the same server, label them, mark one as default. The same physical user can have multiple credential rows per server.
- **API-key → account override** — pin different `tw-` keys to different upstream accounts on the same server. Your Cursor key uses your personal GitHub; the CI key uses the service-bot. One user, multiple agents, multiple identities — without re-issuing credentials.
- **One-paste OAuth onboarding** — paste an MCP URL, click 一键发现. The probe walks the full RFC 9728 → RFC 8414 → RFC 7591 chain: triggers `WWW-Authenticate` from a JSON-RPC `initialize`, follows the `resource_metadata` hint, fetches AS metadata at the path-aware well-known location, and runs Dynamic Client Registration if the upstream advertises it. When DCR isn't supported the UI shows three concrete next steps (copy callback URL → register app upstream → paste Client ID back) with no protocol jargon.
- **Public-client support** — detects `token_endpoint_auth_methods_supported: ["none"]` and propagates `is_public_client` end to end. The Client Secret input is hidden for issuers like Feishu that don't use one.
- **Static-token vault** — for upstreams that only speak PATs / API keys (GitHub PATs, Notion integration tokens). Same per-user surface, same encrypted storage, same /connections UX. Static tokens are verified at paste time so users find out immediately if the token is wrong.
- **Per-user tool catalogs** — when an upstream filters tool visibility by scope or role (Atlassian, enterprise IDPs), the user-authenticated `tools/list` is cached in `mcp_user_tools` and **only ever returned to that user**. The system-level `mcp_tools` catalog only stores anonymous-discoverable tools. No cross-user leakage; auth-required servers are no longer "0 tools" until someone manually fixes it.
- **Three-tier upstream subject resolution** — `/connections` shows real upstream identities (`@octocat`, `alice@acme.com`, Slack `Bob`). Resolver tries JWT decode (free) → userinfo endpoint (priority-ranked extractor: `preferred_username` → `sub` → `accountId` → `login` → `email`) → `.well-known` discovery. Pre-seeded for GitHub, Notion, Slack, Atlassian, Cloudflare, GitLab, Discord, Google.
- **MCP Store with 23+ curated templates** — GitHub, Notion, Linear, Slack, Atlassian, Cloudflare, GitLab, Discord, Google, Feishu and more, pre-seeded with the right OAuth scopes, userinfo endpoints, and PAT help URLs. One-click install. Daily catalog refresh from the registry.
- **Generic MCP client UX** — for users who haven't authorized yet, the gateway still serves the tool catalog but tags every entry with `_meta: { requires_user_auth: true, server_id, server_name, authorize_url }`. `tools/call` against an unauthorized server returns JSON-RPC error code `-32050` with the authorize URL, so Cursor / Claude Desktop / any compliant MCP client can prompt the user to authorize without the gateway hiding the catalog.
- **Tool-level RBAC** — per-role tool grants on the server side, per-key `allowed_mcp_tools` allowlist on the API-key side (bounded by the issuing role's grants). A locked-down service key can hold exactly two tools and nothing else.
- **`mcp:connect` permission** — gates the /connections page and authorize/revoke flow. Granted to admin / team_manager / developer by default.
- **Cache scoped by `(user, account_label)`** — MCP response cache never serves Alice's authorized response to Bob. Direct-mode (no per-user creds) servers still get global caching.
- **Race-free token refresh** — OAuth refresh holds a `pg_advisory_xact_lock` keyed by `(server, user, label)` so concurrent tool calls don't race two refresh attempts. Terminal refresh failure purges the row so the next call cleanly surfaces `NeedsUserCredentials`.
- **Health probe robustness** — 401/403 from an anonymous probe is *expected* on auth-required MCPs; the server is marked `auth_required` (amber), not `disconnected` (red). The /mcp/servers list shows "—" tool count with a hover tooltip for that state.
- **Step-by-step registration wizard** — auth-mode-aware edit form, per-credential Test Connection button on /connections, admin foot-gun guards (verify static tokens at paste time, no silent fall-through to default account).
- **SSRF hardening** — discovery and OAuth probe URLs are validated through an injected URL validator; private IP ranges, link-local, and metadata-service hosts are rejected.
- **Namespace isolation** — `github__create_issue`, `postgres__query` — no tool name collisions across upstreams.
- **Connection pooling & health monitoring** — automatic reconnection, periodic background probes, per-server health surfaced on the dashboard.
- **Full audit trail** — every tool invocation logged with user, account label, parameters, response, latency, and error in ClickHouse alongside the AI gateway logs.
- **Rate limits + budgets apply to MCP** — the same engine that meters AI tokens also meters MCP tool calls; per-user, per-API-key, per-server subjects all stack. See [Rate limits & budgets](#rate-limits--budgets).
- **One key, two surfaces** — the same `tw-` virtual key works on both `/v1/chat/completions` and `/mcp` via a per-key `surfaces` allowlist (`ai_gateway`, `mcp_gateway`, or both).

### MCP Gateway: how we compare

Most "MCP gateways" available today are thin reverse proxies: one shared admin token per upstream, no end-user identity, and "auth" means "did this user pass the gateway's bearer token". That model works for hobby setups and breaks the moment a real organization plugs it into GitHub / Atlassian / Linear / Slack — every tool call shows up as the same service account, scopes can't differ per user, and there's no honest answer to "who renamed this Linear ticket?".

ThinkWatch is built for the second case.

| Capability | Typical MCP proxy | ThinkWatch |
|---|---|---|
| **Upstream sees the real user** | ❌ shared admin token / env var | ✅ per-user OAuth tokens + PAT vault, AES-256-GCM encrypted at rest |
| **Multi-account per user** | ❌ one config = one identity | ✅ work + personal accounts, labelled, default + named |
| **API key → account binding** | ❌ keys are opaque | ✅ Cursor → personal, cron → service-bot, all on the same user |
| **OAuth onboarding** | ❌ hand-edit JSON / env | ✅ paste URL, one-click DCR (RFC 9728 → 8414 → 7591), public-client support |
| **Per-user tool visibility** | ❌ assumes uniform catalog (privilege-escalation if cached) | ✅ separate `mcp_user_tools` per user, system catalog only holds anonymous-discoverable tools |
| **Generic MCP client UX (Cursor/Claude Desktop)** | ❌ unauthorized = blank list | ✅ catalog returned with `_meta.requires_user_auth` markers + `-32050` with `authorize_url` |
| **Tool-level RBAC** | ❌ all-or-nothing | ✅ per-role grants + per-key `allowed_mcp_tools` allowlist bounded by role |
| **Built-in catalog** | ❌ DIY everything | ✅ 23+ templates seeded (GitHub / Notion / Linear / Slack / Atlassian / Cloudflare / GitLab / Discord / Google / Feishu …) |
| **Audit / rate limits / budgets** | ❌ LLM-only or absent | ✅ same engine meters AI tokens AND MCP tool calls |
| **Response cache safety** | ❌ shared cache leaks across users | ✅ scoped by `(user, account_label)` for OAuth/PAT servers |
| **OAuth refresh races** | ❌ duplicate refresh attempts under concurrency | ✅ `pg_advisory_xact_lock` per `(server, user, label)` |
| **Health classification** | ❌ 401/403 = "unhealthy" (false alarms) | ✅ `auth_required` is a first-class amber state |
| **SSRF protection** | ❌ raw fetcher | ✅ injected URL validator, private/link-local/metadata IPs rejected |
| **One key, two surfaces** | ❌ separate stacks for AI vs MCP | ✅ single `tw-` key, per-key `surfaces` allowlist |

If your only requirement is "expose a few public MCP servers to a small team", the simple proxies do fine. The moment you need *who did what, on whose behalf, with what scopes, billed to which cost center* — ThinkWatch is the design point.

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
| **Raw tokens** | `gateway_logs.input_tokens / output_tokens` | Real provider-billed token counts | Analytics, cost reports |
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

# 2. Generate dev secrets + start backend (gateway :3000 + console :3001)
make dev-secrets       # writes .env from .env.example with random secrets
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
├── db/                  # Declarative PostgreSQL schema (schema.sql + seeds.sql)
├── web/                 # React frontend — ~20 page components
├── deploy/
│   ├── docker/          # Dockerfile.server (distroless), Dockerfile.web (nginx)
│   ├── docker-compose.yml       # Production deployment
│   ├── docker-compose.dev.yml   # Development (PG + Redis + ClickHouse + Zitadel)
│   └── helm/think-watch/      # Kubernetes Helm chart
└── ...
```

> Documentation: **[thinkwat.ch/docs](https://thinkwat.ch/docs)**

## Related projects

| Repository | What it is |
|---|---|
| [ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite) | A desktop app for individual developers. It puts a local AI API gateway in your menu bar, so you can see what Claude Code and Codex cost, where each request was routed, and what was redacted. In development, macOS first. MIT. |
| [ThinkWatch Core](https://github.com/ThinkWatchProject/ThinkWatch-Core) | Rust crates for a local AI API gateway: routing, failover, cost accounting, and redaction. ThinkWatch Lite is built on it, and this server edition depends on five of its crates: shared types, protocol handling, providers, envelope encryption, and retry and failover. MIT. |

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
