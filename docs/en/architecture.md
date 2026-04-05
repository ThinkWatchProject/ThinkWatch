**[English](../en/architecture.md) | [中文](../zh-CN/architecture.md)**

# AgentBastion Architecture

## 1. Overview

AgentBastion is an enterprise AI API gateway and MCP (Model Context Protocol) management platform built in Rust. It acts as a **bastion host** for all AI access within an organization -- every request from developer tools, automated agents, and internal applications flows through a single controlled gateway before reaching upstream AI providers or MCP servers.

This architecture provides centralized:

- **Authentication and authorization** -- API key validation, JWT-based sessions, OIDC SSO, and role-based access control.
- **Cost management** -- per-key and per-team budgets, token-level usage tracking, and real-time cost calculation.
- **Rate limiting** -- requests-per-minute (RPM) and tokens-per-minute (TPM) enforcement per API key.
- **Audit logging** -- every request is recorded with full context for compliance and debugging.
- **Model access governance** -- fine-grained permissions controlling which users, teams, and roles can access which AI models and MCP tools.
- **Multi-provider abstraction** -- a unified API (OpenAI, Anthropic, and Responses formats) that proxies to OpenAI, Anthropic, Google, Azure OpenAI, AWS Bedrock, and custom providers with automatic request/response format translation.

---

## 2. System Architecture Diagram

```
                          Downstream Clients
            +----------------+  +----------------+  +----------------+
            |  Claude Code   |  |    Cursor      |  | Custom Agents  |
            |  (MCP client)  |  | (OpenAI-compat)|  | (API / MCP)    |
            +-------+--------+  +-------+--------+  +-------+--------+
                    |                    |                    |
                    |   ab-xxx API key   |   JWT / API key   |
                    +--------------------+--------------------+
                                         |
                              +----------+-----------+
                              |   Reverse Proxy      |
                              |  (nginx / traefik)   |
                              +----+------------+----+
                                   |            |
                    +--------------+--+    +----+----------------+
                    |  Gateway :3000  |    |   Console :3001     |
                    |  +-----------+  |    |  +--------------+   |
                    |  | /v1/*     |  |    |  | /api/*       |   |
                    |  | AI Proxy  |  |    |  | Mgmt REST API|   |
                    |  +-----------+  |    |  +--------------+   |
                    |  +-----------+  |    |  +--------------+   |
                    |  | /mcp      |  |    |  | Web UI (SPA) |   |
                    |  | MCP Proxy |  |    |  | React 19     |   |
                    |  +-----------+  |    |  +--------------+   |
                    |  +-----------+  |    +---------------------+
                    |  +-----------+  |
                    |  | /health/* |  |
                    |  | /metrics  |  |
                    |  +-----------+  |
                    +---------+-------+
                              |
          +-------------------+-------------------+
          |                   |                   |
    +-----+------+    +------+------+    +-------+-------+
    | PostgreSQL  |    |    Redis    |    |  ClickHouse   |
    | users, keys |    | rate limits |    | audit logs    |
    | providers   |    | sessions    |    |               |
    | settings    |    | config sync |    |               |
    | usage, RBAC |    | OIDC state  |    +---------------+
    +-------------+    +-------------+

          +-------------------+-------------------+
          |                   |                   |
    +-----+------+    +------+------+    +-------+-------+
    |   OpenAI   |    |  Anthropic  |    |    Google     |
    |  API       |    |  API        |    |  Gemini API   |
    +------------+    +-------------+    +---------------+
    +-----+------+    +------+------+
    | Azure      |    |    AWS      |
    | OpenAI     |    |  Bedrock    |
    +------------+    +-------------+
          Upstream AI Providers

    +---------------------------------------------+
    | Upstream MCP Servers (Streamable HTTP)       |
    |  e.g. filesystem, github, database, etc.    |
    +---------------------------------------------+

    +---------------------------------------------+
    |   Zitadel (OIDC SSO Provider)               |
    |   External identity provider for SSO login  |
    +---------------------------------------------+
```

---

## 3. Dual-Port Architecture

AgentBastion binds two separate TCP listeners on a single process:

| Port | Name    | Purpose                                   | Audience           |
|------|---------|-------------------------------------------|--------------------|
| 3000 | Gateway | AI API proxy (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1/models`), MCP proxy (`/mcp`), health checks (`/health/live`, `/health/ready`), Prometheus metrics (`/metrics`) | AI clients, agents, monitoring |
| 3001 | Console | Management REST API (`/api/*`), Web UI    | Administrators     |

### Why Two Ports?

**Security isolation.** The gateway port is the only port that should be exposed to the public network or developer workstations. It accepts API key authentication and serves the high-throughput proxy path. The console port serves the administrative interface and should be restricted to an internal network, VPN, or localhost.

**Network topology recommendations:**

- **Production:** Place a reverse proxy (nginx, traefik, or a cloud load balancer) in front of both ports. Expose only port 3000 to the public or developer network. Restrict port 3001 to an internal network segment or require VPN access.
- **Kubernetes:** Create two separate Ingress resources -- a public Ingress for the gateway host and an internal Ingress (or no Ingress at all, using port-forward) for the console host.
- **Development:** Both ports are accessible on localhost. The React dev server (pnpm dev) proxies API calls to `localhost:3001`.

---

## 4. AI API Gateway Data Flow

When a client sends a request through the gateway (via `/v1/chat/completions`, `/v1/messages`, or `/v1/responses`), the following steps occur:

```
Client                Gateway :3000                        Upstream Provider
  |                        |                                      |
  |  POST /v1/chat/completions                                    |
  |  Authorization: Bearer ab-xxxx                                |
  |----------------------->|                                      |
  |                        |                                      |
  |               1. API Key Auth Middleware                       |
  |                  - Extract key prefix (ab-xxxx)               |
  |                  - Hash key, lookup in PostgreSQL              |
  |                  - Validate: active, not expired, budget ok    |
  |                  - Attach user_id, team_id, scopes to request |
  |                        |                                      |
  |               2. Model Router                                 |
  |                  - Extract model from request body             |
  |                  - Find provider + model in registry           |
  |                  - Check model_permissions for user/team/role  |
  |                  - Check allowed_models on the API key         |
  |                        |                                      |
  |               3. Request Transform                            |
  |                  - If provider is Anthropic: -> Claude format  |
  |                  - If provider is Google: -> Gemini format     |
  |                  - If provider is Azure: -> Azure OpenAI format|
  |                  - If provider is Bedrock: -> Converse API     |
  |                  - If provider is OpenAI: pass through         |
  |                        |                                      |
  |               4. Rate Limiter                                 |
  |                  - Check RPM counter in Redis                  |
  |                  - Estimate tokens, check TPM counter          |
  |                  - If exceeded: return 429 Too Many Requests   |
  |                        |                                      |
  |               5. Upstream Proxy                                |
  |                  - Build request with provider API key         |
  |                    (decrypted from providers.api_key_encrypted)|
  |                  - Forward to provider base_url                |
  |                        |--- POST provider.base_url ---------->|
  |                        |                                      |
  |                        |<--- SSE stream / JSON response ------|
  |                        |                                      |
  |               6. Response Transform                           |
  |                  - Convert upstream response to client format  |
  |                  - Bedrock: decode binary event-stream         |
  |                  - Stream SSE chunks back to client            |
  |                        |                                      |
  |<-- SSE stream ---------|                                      |
  |                        |                                      |
  |               7. Async Post-Processing (spawned task)         |
  |                  - Count input/output tokens (tiktoken)        |
  |                  - Calculate cost from model pricing           |
  |                  - Insert usage_record into PostgreSQL         |
  |                  - Push audit log to ClickHouse                |
  |                  - Forward to log forwarders (syslog/kafka/webhook) |
  |                  - Update rate limit counters in Redis         |
```

### Key Design Decisions

- **Streaming-first:** The proxy uses `eventsource-stream` and `async-stream` to forward SSE chunks in real time with minimal buffering. For AWS Bedrock, native binary event-stream decoding is used. Token counting happens after the stream completes.
- **Provider API keys are encrypted at rest** using AES-256-GCM. The `ENCRYPTION_KEY` environment variable provides the 256-bit key. For AWS Bedrock, credentials (`ACCESS_KEY_ID:SECRET_ACCESS_KEY`) are encrypted with the same scheme and used for SigV4 request signing via the official `aws-sigv4` crate.
- **Multi-format support:** The gateway accepts three API formats: OpenAI Chat Completions (`/v1/chat/completions`), Anthropic Messages (`/v1/messages`), and OpenAI Responses (`/v1/responses`). Regardless of the inbound format, the request is routed through the same model router and translated to the upstream provider's native format. This means a single `ab-` API key can be used with any client tool.

---

## 5. MCP Gateway Data Flow

The MCP gateway allows clients (such as Claude Code or Cursor) to access remote MCP tool servers through a single authenticated endpoint:

```
MCP Client              MCP Gateway :3000/mcp            Upstream MCP Server
  |                           |                                  |
  |  POST /mcp                |                                  |
  |  Authorization: Bearer <JWT>                                 |
  |  (or ab-xxx API key)      |                                  |
  |-------------------------->|                                  |
  |                           |                                  |
  |               1. Authentication                              |
  |                  - Validate JWT or API key                   |
  |                  - Extract user identity + roles             |
  |                           |                                  |
  |               2. Session Management                          |
  |                  - Mcp-Session-Id header check               |
  |                  - Create new session or resume existing     |
  |                  - Session state stored in Redis             |
  |                           |                                  |
  |               3. JSON-RPC Dispatch                           |
  |                  Parse method:                               |
  |                  - "initialize" -> return capabilities       |
  |                  - "tools/list" -> aggregate from registry   |
  |                  - "tools/call" -> route to server           |
  |                           |                                  |
  |               4. Tool Namespace Resolution (tools/call)      |
  |                  - Tool name: "github__create_issue"         |
  |                  - Split on "__" -> server="github",         |
  |                    tool="create_issue"                       |
  |                  - Look up server in mcp_servers registry    |
  |                           |                                  |
  |               5. Access Control Check                        |
  |                  - Check mcp_tool_permissions for            |
  |                    user/team/role against this tool           |
  |                  - Deny if not permitted                     |
  |                           |                                  |
  |               6. Connection Pool -> Upstream                 |
  |                  - Get/create connection from pool            |
  |                  - Forward JSON-RPC call to upstream server   |
  |                           |--- POST server.endpoint_url ---->|
  |                           |<-- JSON-RPC response ------------|
  |                           |                                  |
  |               7. Response Forwarding + Audit                 |
  |                  - Return JSON-RPC response to client        |
  |                  - Log tool invocation to audit_logs         |
  |                           |                                  |
  |<-- JSON-RPC response -----|                                  |
```

### Key Design Decisions

- **Streamable HTTP transport** is the primary MCP transport. The gateway connects to upstream servers via `streamable_http`.
- **Tool aggregation:** When a client calls `tools/list`, the gateway aggregates tools from all registered MCP servers that the user has access to, prefixing each tool name with its server name and `__` to avoid collisions.
- **Connection pooling** reuses upstream MCP connections to avoid repeated handshakes.
- **Health checking** runs periodically (configurable via `health_check_interval`) to mark servers as healthy or unhealthy.

---

## 6. Crate Architecture

AgentBastion is structured as a Cargo workspace with five crates:

```
crates/
  server/           # Binary crate -- HTTP servers, routes, handlers, middleware
  gateway/          # Library -- AI API proxy engine
  mcp-gateway/      # Library -- MCP proxy engine
  auth/             # Library -- authentication and authorization
  common/           # Library -- shared infrastructure
```

### server

The application entry point. Contains:

- **`main.rs`** -- Initializes config, database, Redis, runs startup validation (dependency checks, JWT secret entropy), and starts both the gateway and console Axum servers.
- **`app.rs`** -- Builds the Axum router trees for both ports.
- **`background_tasks/`** -- Periodic background jobs:
  - `api_key_lifecycle.rs` -- Runs hourly to enforce key rotation periods, inactivity timeouts, and expiry policies.
  - `data_retention.rs` -- Runs daily to purge expired usage records, audit logs, and soft-deleted records past the 30-day retention window.
- **`handlers/`** -- Request handlers organized by domain:
  - `auth.rs`, `sso.rs` -- Login, registration, OIDC callbacks
  - `api_keys.rs` -- CRUD for virtual API keys
  - `providers.rs` -- AI provider and model management
  - `mcp_servers.rs`, `mcp_tools.rs` -- MCP server registry management
  - `analytics.rs`, `audit.rs` -- Usage dashboards, audit log queries
  - `admin.rs` -- User management, role assignment, system settings
  - `settings.rs` -- Dynamic configuration CRUD (`GET/PATCH /api/admin/settings`, category filtering)
  - `setup.rs` -- First-run setup wizard (`GET /api/setup/status`, `POST /api/setup/initialize`)
  - `health.rs` -- Health check endpoints (`/health/live`, `/health/ready`, `/api/health`)
  - `metrics.rs` -- Prometheus metrics endpoint (`GET /metrics`)
- **`middleware/`** -- Axum middleware layers:
  - `api_key_auth.rs` -- Extracts and validates `ab-` API keys for gateway routes
  - `auth_guard.rs` -- Validates JWT tokens for console routes
  - `require_role.rs` -- RBAC enforcement middleware

### gateway

The AI API proxy engine. Contains:

- **`providers/`** -- Provider implementations behind a common trait:
  - `traits.rs` -- `AiProvider` trait defining the proxy interface
  - `openai.rs` -- OpenAI proxy (passthrough)
  - `anthropic.rs` -- Anthropic Claude proxy with format translation
  - `google.rs` -- Google Gemini proxy with format translation
  - `azure.rs` -- Azure OpenAI proxy with `api-key` header auth and `api_version` query parameter
  - `bedrock.rs` -- AWS Bedrock proxy with SigV4 signing (official `aws-sigv4` crate), Converse API, and native binary event-stream streaming
  - `custom.rs` -- Generic OpenAI-compatible provider proxy
- **`proxy.rs`** -- Core proxy logic: receives request, selects provider, forwards, returns response.
- **`router.rs`** -- Model-to-provider routing and permission checks.
- **`streaming.rs`** -- SSE stream forwarding and chunk processing.
- **`transform/`** -- Request/response format conversion between OpenAI, Anthropic, Google, Azure, and Bedrock formats.
- **`rate_limiter.rs`** -- Redis-backed RPM/TPM rate limiting with sliding window counters.
- **`token_counter.rs`** -- Token counting using `tiktoken-rs` for usage tracking and TPM enforcement.
- **`cost_tracker.rs`** -- Real-time cost calculation based on model pricing from the database.
- **`circuit_breaker.rs`** -- Three-state circuit breaker (Closed/Open/HalfOpen) per provider, with configurable `failure_threshold` and `recovery_secs`.
- **`retry.rs`** -- Retry with exponential backoff for transient failures (NetworkError, UpstreamRateLimited), with configurable `max_retries`, `initial_delay_ms`, `max_delay_ms`, and `jitter`.

### mcp-gateway

The MCP proxy engine. Contains:

- **`proxy.rs`** -- Core MCP proxy: receives JSON-RPC requests, dispatches to the correct upstream server.
- **`registry.rs`** -- In-memory registry of MCP servers and their tools, synced from PostgreSQL.
- **`pool.rs`** -- Connection pool for upstream MCP server connections.
- **`session.rs`** -- Session management for stateful MCP interactions (Redis-backed).
- **`access_control.rs`** -- Permission checks for tool invocations against `mcp_tool_permissions`.
- **`health.rs`** -- Periodic health checking of registered MCP servers.
- **`transport/`** -- Transport layer implementations:
  - `streamable_http.rs` -- Streamable HTTP transport client for upstream MCP servers.

### auth

Authentication and authorization library. Contains:

- **`jwt.rs`** -- JWT token creation and validation using `jsonwebtoken`.
- **`api_key.rs`** -- API key generation (`ab-` prefixed), hashing, and validation.
- **`password.rs`** -- Password hashing and verification using Argon2.
- **`oidc.rs`** -- OpenID Connect client for SSO with Zitadel or any OIDC provider. Handles authorization URL generation, callback processing, and user provisioning.
- **`rbac.rs`** -- Role-based access control: permission loading, role hierarchy, and authorization checks.

### common

Shared infrastructure used by all other crates. Contains:

- **`config.rs`** -- `AppConfig` struct loaded from environment variables.
- **`dynamic_config.rs`** -- `DynamicConfig` system that loads settings from the `system_settings` database table. Supports multi-instance sync via Redis Pub/Sub and in-memory caching. Covers JWT TTLs, cache TTL, content filter patterns, PII patterns, budget thresholds, API key policies, and data retention settings.
- **`db.rs`** -- PostgreSQL connection pool setup using `sqlx`.
- **`models/`** -- Database model structs (one per domain entity): `user.rs`, `team.rs`, `api_key.rs`, `provider.rs`, `mcp_server.rs`, `usage.rs`, `audit_log.rs`.
- **`dto/`** -- Data transfer objects for API request/response serialization.
- **`errors.rs`** -- Unified error type with HTTP status code mapping.
- **`crypto.rs`** -- AES-256-GCM encryption/decryption for provider API keys.
- **`audit.rs`** -- Audit log writer (PostgreSQL + optional ClickHouse + optional syslog forwarding).

---

## 7. Database Schema Overview

The database schema is defined across twelve migration files applied in order on startup:

### 001_init_users -- User Accounts

| Table   | Purpose |
|---------|---------|
| `users` | Core user accounts with email, display name, password hash (for local auth), and OIDC subject/issuer (for SSO). |

### 002_init_teams -- Team Organization

| Table          | Purpose |
|----------------|---------|
| `teams`        | Organizational teams with optional monthly budget limits. |
| `team_members` | Many-to-many join between users and teams, with a role field (owner/member). |

### 003_init_rbac -- Role-Based Access Control

| Table              | Purpose |
|--------------------|---------|
| `roles`            | Named roles. Five system roles are seeded: `super_admin`, `admin`, `team_manager`, `developer`, `viewer`. |
| `permissions`      | Fine-grained permissions as (resource, action) pairs. |
| `role_permissions`  | Maps roles to their granted permissions. |
| `user_roles`       | Assigns roles to users with an optional scope (global or team-scoped). |

### 004_init_api_keys -- Virtual API Keys

| Table      | Purpose |
|------------|---------|
| `api_keys` | Virtual API keys (`ab-` prefixed) issued to users or teams. Each key stores: hashed key, allowed models, rate limits (RPM/TPM), monthly budget, expiration, and scopes. |

### 005_init_providers -- AI Provider Configuration

| Table               | Purpose |
|---------------------|---------|
| `providers`         | Upstream AI provider configuration: name, type (openai/anthropic/google/azure/bedrock/custom), base URL, AES-encrypted API key, and optional `config_json` (e.g. `api_version` for Azure). |
| `models`            | AI models registered under a provider, with input/output token pricing. |
| `model_permissions` | Access control rules for models, grantable by role, team, or individual user. |

### 006_init_mcp_servers -- MCP Server Registry

| Table                  | Purpose |
|------------------------|---------|
| `mcp_servers`          | Registered upstream MCP servers: endpoint URL, transport type, auth configuration, health status, and check interval. |
| `mcp_tools`            | Tools discovered from each MCP server, with their JSON Schema input definitions. |
| `mcp_tool_permissions` | Access control rules for individual MCP tools, grantable by role, team, or user. |

### 007_init_usage_audit -- Usage Tracking and Audit

| Table           | Purpose |
|-----------------|---------|
| `usage_records` | Per-request usage data: API key, user, team, provider, model, token counts, cost in USD, latency, and HTTP status. Indexed by time and by user/team/key for dashboard queries. |
| `audit_logs`    | Security audit trail: user, API key, action, resource, detail JSON, IP address, and user agent. |
| `budget_alerts` | Records of budget threshold notifications for teams and API keys. |

### 008--012 -- Additional Migrations

The remaining migrations add:

| Table / Change | Purpose |
|----------------|---------|
| `system_settings` | Dynamic configuration key-value store with category, description, and type metadata. Queried by the `DynamicConfig` system and editable via the Admin Settings UI. |
| `api_keys.rotation_period_days` | Per-key automatic rotation interval. |
| `api_keys.inactivity_timeout_days` | Auto-disable key after N days of inactivity. |
| `api_keys.last_used_at` | Timestamp of last key usage, used by the inactivity timeout policy. |
| `api_keys.grace_period_expires_at` | Allows the old key to remain valid during rotation until the grace period expires. |
| `users.deleted_at` | Soft-delete column for user accounts. |
| `providers.deleted_at` | Soft-delete column for provider records. |
| `api_keys.deleted_at` | Soft-delete column for API keys. |
| New indexes | Performance indexes on hot query paths (usage records by time range, audit logs by user, API keys by team). |

---

## 8. Frontend Architecture

The web console is a single-page application located in the `web/` directory.

### Technology Stack

- **React 19** with TypeScript
- **shadcn/ui** component library (Tailwind CSS-based)
- **Simple pushState routing** (no external router library)
- **Fetch API** for all HTTP requests, with JWT from `localStorage`

### Page Structure

The UI consists of approximately 20 pages organized into six groups:

| Group       | Pages                             | Description |
|-------------|-----------------------------------|-------------|
| Setup       | `setup.tsx`                       | First-run setup wizard (super_admin creation, site config, optional provider setup) |
| Auth        | `login.tsx`                       | Login form (local + OIDC SSO) |
| Dashboard   | `dashboard.tsx`                   | Overview: usage charts, cost summary, recent activity |
| Gateway     | `providers.tsx`, `models.tsx`, `api-keys.tsx`, `logs.tsx` | AI provider management, model registry, API key CRUD, request logs |
| MCP         | `servers.tsx`, `tools.tsx`, `logs.tsx` | MCP server registry, tool discovery/permissions, MCP request logs |
| Analytics   | `usage.tsx`, `costs.tsx`, `audit.tsx` | Token usage dashboards, cost breakdowns, audit log search |
| Admin       | `users.tsx`, `roles.tsx`, `settings.tsx` | User management, RBAC role configuration, system settings |

### API Client Pattern

All pages communicate with the console server (`/api/*`) using a shared fetch wrapper that:

1. Reads the JWT from `localStorage`.
2. Attaches it as `Authorization: Bearer <token>`.
3. Handles 401 responses by redirecting to the login page.
4. Parses JSON responses and surfaces errors to the UI.

The web UI is built as a static SPA and served either by the development server (Vite with `pnpm dev`) or by an nginx container in production (see `deploy/docker/Dockerfile.web` and `deploy/docker/nginx.conf`).
