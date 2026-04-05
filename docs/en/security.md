**[English](../en/security.md) | [中文](../zh-CN/security.md)**

# AgentBastion Security Documentation

This document describes the security architecture, mechanisms, and hardening practices for AgentBastion.

---

## 1. Authentication

AgentBastion supports three authentication mechanisms. The gateway accepts any of them; the console management API requires JWT.

### 1.1 JWT Tokens

AgentBastion issues two JWT tokens upon successful authentication:

| Token          | Default Lifetime | Purpose                                      |
| -------------- | ---------------- | -------------------------------------------- |
| Access token   | 900s (15 min)    | Short-lived credential for API authorization |
| Refresh token  | 604800s (7 days) | Used to obtain new access/refresh token pair |

Both TTL values are configurable via the admin Web UI (`jwt_access_ttl_seconds` and `jwt_refresh_ttl_seconds` settings) without requiring a server restart.

**Signing algorithm:** HS256 with a shared secret (`JWT_SECRET` environment variable). A migration to RS256 with asymmetric key pairs is planned for a future release.

**Secret requirements:**
- `JWT_SECRET` must be at least 32 characters long.
- At startup, AgentBastion performs an entropy check and rejects secrets that are trivially weak (e.g., all identical characters, common patterns).
- Generate a strong secret with: `openssl rand -hex 32`

**Clock skew tolerance:** A 30-second leeway is applied when validating token expiration (`exp`) and not-before (`nbf`) claims, accommodating minor clock differences between distributed services.

**Token claims:**

```json
{
  "sub": "user-uuid",
  "email": "user@example.com",
  "role": "admin",
  "exp": 1711930500,
  "iat": 1711929600,
  "iss": "agentbastion"
}
```

**Refresh flow:** The client sends the refresh token to `POST /api/auth/refresh`. The server validates the token, issues a new access/refresh pair, and invalidates the old refresh token. Refresh token reuse is detected and causes all tokens for the user to be invalidated (rotation with replay detection).

### 1.2 API Keys

API keys provide long-lived authentication for programmatic access to the gateway.

| Property        | Detail                                           |
| --------------- | ------------------------------------------------ |
| Format          | Prefixed with `ab-` (e.g. `ab-sk-a1b2c3d4...`)  |
| Storage         | SHA-256 hash stored in PostgreSQL                |
| Validation      | Middleware hashes the incoming key and looks it up (filters `deleted_at IS NULL`) |
| Scoping         | Optional `allowed_models` restriction            |
| Rate limiting   | Optional per-key RPM limit, enforced via Redis   |
| Expiration      | Optional TTL set at creation time                |

The raw key value is returned exactly once at creation time. It cannot be retrieved afterward because only the hash is stored.

**Lifecycle management:**

API keys now support a full lifecycle with the following capabilities:

- **Rotation:** Keys can be rotated via `POST /api/keys/{id}/rotate`. A new key is generated and returned; the old key enters a configurable grace period during which both old and new keys are accepted. The `grace_period_ends_at` timestamp indicates when the old key will stop working.
- **Grace period:** After rotation, the API key auth middleware accepts both the old and new key hashes until `grace_period_ends_at` expires, allowing clients to transition without downtime.
- **Inactivity timeout:** Keys can be configured with an `inactivity_timeout_days` value. If a key is not used within that period, it is automatically disabled by the background lifecycle task.
- **Auto-disable:** Keys that exceed their inactivity timeout are soft-disabled (not deleted), allowing administrators to re-enable them if needed.
- **Expiry monitoring:** Use `GET /api/keys/expiring?days=N` to list keys approaching their expiration date.
- **Team validation:** API key creation validates team membership -- users can only create keys within teams they belong to.

### 1.3 OIDC / SSO

AgentBastion supports OpenID Connect for single sign-on with enterprise identity providers (Entra ID, Okta, Keycloak, Auth0, etc.).

**Flow: Authorization Code**

1. The user's browser is directed to `GET /api/auth/sso/authorize`.
2. The server generates a cryptographically random `state` and `nonce`, stores both in Redis with a 10-minute TTL, and redirects the browser to the OIDC provider's authorization endpoint.
3. After authentication, the IdP redirects back to `GET /api/auth/sso/callback?code=...&state=...`.
4. The server validates the `state` against Redis (CSRF protection), exchanges the authorization code for tokens, verifies the ID token signature and `nonce`, and provisions or updates the local user record.
5. AgentBastion JWT tokens are returned to the client.

**CSRF protection:** The `state` parameter is a one-time random value stored in Redis. It is consumed (deleted) on callback, preventing replay attacks.

### 1.4 Dual Authentication on Gateway

The gateway middleware attempts authentication in the following order:

1. If the `Authorization` header contains a token starting with `ab-`, validate it as an API key.
2. Otherwise, validate it as a JWT Bearer token.

This allows both programmatic clients (API keys) and browser-based/console users (JWT) to access the gateway seamlessly.

---

## 2. Authorization (RBAC)

AgentBastion implements role-based access control with five system roles.

### 2.1 Role Hierarchy

| Role      | Description                                                        |
| --------- | ------------------------------------------------------------------ |
| `admin`   | Full system access. Can manage providers, users, MCP servers, settings, and view audit logs. |
| `operator`| Can manage providers and MCP servers. Cannot manage users or system settings. |
| `user`    | Standard user. Can create/manage own API keys, view analytics, and use the gateway. |
| `viewer`  | Read-only access. Can view analytics and models but cannot create keys or modify anything. |
| `service` | Machine-to-machine identity. API key access to the gateway only; no console access. |

### 2.2 Access Control Matrix

| Resource                | admin | operator | user | viewer | service |
| ----------------------- | ----- | -------- | ---- | ------ | ------- |
| Gateway (chat, models)  | Yes   | Yes      | Yes  | Yes    | Yes     |
| Gateway (MCP)           | Yes   | Yes      | Yes  | Yes    | Yes     |
| Own API keys (CRUD)     | Yes   | Yes      | Yes  | No     | No      |
| Analytics (read)        | Yes   | Yes      | Yes  | Yes    | No      |
| Providers (CRUD)        | Yes   | Yes      | No   | No     | No      |
| MCP servers (CRUD)      | Yes   | Yes      | No   | No     | No      |
| Users (CRUD)            | Yes   | No       | No   | No     | No      |
| Audit logs              | Yes   | No       | No   | No     | No      |
| System settings         | Yes   | No       | No   | No     | No      |

### 2.3 Middleware Enforcement

- **`require_auth`** — Extracts and validates JWT from the `Authorization` header. Rejects unauthenticated requests with 401.
- **`require_admin`** — Chains after `require_auth`. Checks that the JWT `role` claim is `admin`. Rejects non-admin users with 403.
- **API key middleware** — Gateway-specific. Validates the API key hash, checks expiration, enforces `allowed_models`, and applies rate limiting via Redis.

### 2.4 MCP Tool-Level Access Control

MCP tools can be restricted at the API key or user level. When an API key has `allowed_models` set, MCP tool invocations are also gated: the gateway checks that the calling identity has permission to invoke the specific tool before proxying the request to the upstream MCP server.

---

## 3. Encryption

### 3.1 Encryption at Rest

**Provider API Keys**

Provider API keys (credentials for upstream AI services like OpenAI, Anthropic, Google, Azure OpenAI, and AWS Bedrock) are encrypted before storage:

- Algorithm: AES-256-GCM
- Nonce: 12 bytes, cryptographically random, generated per encryption operation
- Key: Derived from the `ENCRYPTION_KEY` environment variable (32-byte hex string)
- Storage format: `nonce || ciphertext || tag` (base64-encoded)

**AWS Bedrock Credentials**

AWS Bedrock credentials (stored in `ACCESS_KEY_ID:SECRET_ACCESS_KEY` format) are encrypted at rest using the same AES-256-GCM scheme. At request time, credentials are decrypted and used for AWS SigV4 request signing via the official `aws-sigv4` Rust crate. The signing process is performed in-memory and credentials are never written to disk in plaintext.

**MCP Server Auth Secrets**

Authentication credentials for upstream MCP servers use the same AES-256-GCM scheme as provider API keys.

### 3.2 Password Hashing

User passwords are hashed using Argon2id with the following parameters:

| Parameter       | Value       |
| --------------- | ----------- |
| Algorithm       | Argon2id    |
| Memory cost     | 19 MiB      |
| Time cost       | 2 iterations|
| Parallelism     | 1           |
| Salt            | 16 bytes, random |
| Output hash     | 32 bytes    |

Argon2id is the recommended algorithm per OWASP guidelines, providing resistance against both GPU and side-channel attacks.

### 3.3 API Key Hashing

API keys are hashed with SHA-256 before storage. This is a one-way operation; the original key cannot be derived from the hash. The raw key is returned to the user exactly once at creation time.

### 3.4 Encryption in Transit

AgentBastion itself does not terminate TLS. TLS termination should be handled by a reverse proxy (Nginx, Caddy, Traefik, cloud load balancer). See the hardening checklist in Section 6.

---

## 4. Network Security

### 4.1 Dual-Port Architecture

AgentBastion separates concerns across two ports:

| Port | Server  | Exposure                               |
| ---- | ------- | -------------------------------------- |
| 3000 | Gateway | Intended for application/client access |
| 3001 | Console | Intended for admin/internal access     |

This separation allows network-level isolation: the gateway port can be exposed to application traffic while the console port is restricted to a VPN or internal network.

### 4.2 CORS

Cross-Origin Resource Sharing is configured via the `CORS_ORIGINS` environment variable:

- Accepts a comma-separated list of allowed origins.
- Credentials are allowed (`Access-Control-Allow-Credentials: true`).
- Only the specified origins are reflected in `Access-Control-Allow-Origin`.
- In development, `http://localhost:5173` (Vite dev server) is typically allowed.
- In production, restrict to your actual console domain.

### 4.3 Security Headers

The following headers are set on all responses:

| Header                    | Value              | Purpose                                   |
| ------------------------- | ------------------ | ----------------------------------------- |
| `X-Content-Type-Options`  | `nosniff`          | Prevents MIME type sniffing               |
| `X-Frame-Options`         | `DENY`             | Prevents clickjacking via iframes         |

**Content Security Policy (console port only):**

The console port (3001) includes a `Content-Security-Policy` header to mitigate XSS and data injection attacks. The policy restricts script sources, style sources, and connection endpoints to known origins.

### 4.4 Request Timeouts

| Server  | Timeout | Rationale                                        |
| ------- | ------- | ------------------------------------------------ |
| Gateway | 120s    | LLM completions (especially streaming) can be slow |
| Console | 30s     | Management operations should complete quickly    |

### 4.5 Container Security

Production Docker images use distroless base images:

- No shell (`/bin/sh`, `/bin/bash`)
- No package manager
- No unnecessary system utilities
- Minimal attack surface: only the compiled Rust binary and its runtime dependencies

---

## 5. Audit Trail

### 5.1 What Is Logged

Every security-relevant action generates an audit entry:

| Category         | Actions Logged                                             |
| ---------------- | ---------------------------------------------------------- |
| Authentication   | Login success, login failure, registration, token refresh  |
| API Keys         | Creation, revocation, usage (rate limit hits)              |
| Providers        | Create, update, delete                                     |
| MCP Servers      | Create, delete, tool discovery                             |
| Users            | Create, role change, delete                                |
| Settings         | Any configuration change                                   |

### 5.2 Audit Entry Schema

Each audit log entry contains:

```json
{
  "id": "uuid",
  "timestamp": "2026-03-28T09:15:00.000Z",
  "user_id": "uuid",
  "user_email": "admin@example.com",
  "action": "provider.create",
  "resource_type": "provider",
  "resource_id": "uuid",
  "details": {},
  "ip_address": "10.0.1.50",
  "user_agent": "Mozilla/5.0..."
}
```

### 5.3 ClickHouse Integration

Audit entries are inserted into ClickHouse for SQL-based search and analytics:

- Database: Configurable via `CLICKHOUSE_DB` (default: `agent_bastion`)
- Entries are sent asynchronously to avoid blocking request processing.
- The console provides a search UI at `/api/audit/logs` with support for time-range filtering and SQL queries.

### 5.4 Log Forwarder / SIEM Integration

For enterprise environments, AgentBastion can forward audit logs to external systems via the admin Web UI (Admin > Log Forwarders):

- **Supported transports:** UDP Syslog, TCP Syslog (RFC 5424), Kafka, HTTP Webhook
- **Configuration:** Managed dynamically through the database — no restart required
- **Format:** RFC 5424 structured data for syslog transports:

```
<14>1 2026-03-28T09:15:00.000Z agentbastion - - - [agentbastion@0 action="provider.create" user="admin@example.com" resource_type="provider" resource_id="uuid"] Provider created: openai-prod
```

This allows integration with SIEM platforms such as Splunk, Elastic SIEM, Microsoft Sentinel, and others.

---

## 6. Startup Validation

AgentBastion validates all secrets and dependencies before starting the server. If any critical requirement is not met, the process exits with a clear error message rather than running in a degraded state.

Validated at startup:
- `JWT_SECRET` is present, at least 32 characters, and passes an entropy check
- `ENCRYPTION_KEY` is present and a valid 64-character hex string
- PostgreSQL is reachable and responds to a test query
- Redis is reachable and responds to a PING
- OIDC variables are either all set or all absent (partial configuration is rejected)
- ClickHouse connectivity (if configured; logs a warning but does not block startup)

---

## 7. Setup Endpoint Security

The `POST /api/setup/initialize` endpoint allows creating the first admin user without authentication. To prevent abuse:

- **Rate limiting:** The endpoint is limited to 5 requests per minute per IP address.
- **Double-check:** Before creating the admin user, the endpoint performs a database query to verify no admin user exists. This prevents race conditions where two concurrent requests could both create admin accounts.
- **Disabled after use:** Once the system is initialized, the endpoint returns `400 Bad Request` for all subsequent calls, regardless of rate limit status.

---

## 8. Soft-Delete and Data Retention

AgentBastion uses soft-delete for critical resources:

| Resource     | Behavior                                                                          |
| ------------ | --------------------------------------------------------------------------------- |
| Users        | `deleted_at` is set; all sessions are revoked; the user cannot log in             |
| Providers    | `deleted_at` is set; the provider's models become unavailable for new requests    |
| API Keys     | `deleted_at` is set; the key is immediately rejected by the auth middleware       |

**Data retention policy:**
- Soft-deleted records are retained for a configurable period (default: 30 days, controlled by the `data_retention_days` setting).
- A background task periodically purges records older than the retention period.
- Account deletion via the API is always a soft-delete operation: the user's sessions are revoked, and `deleted_at` is marked. The record is permanently removed only after the retention period expires.

---

## 9. Hardening Checklist

Use this checklist when preparing AgentBastion for production deployment.

### Secrets and Cryptography

- [ ] Set `JWT_SECRET` to a cryptographically random value (minimum 32 characters, recommended 64 hex characters / 256 bits):
  ```bash
  openssl rand -hex 32
  ```
- [ ] Set `ENCRYPTION_KEY` to a random 32-byte hex string:
  ```bash
  openssl rand -hex 32
  ```
- [ ] Rotate `JWT_SECRET` periodically (note: rotation invalidates all active tokens)
- [ ] Store all secrets in a proper secrets manager (Vault, AWS Secrets Manager, K8s Secrets) rather than plaintext `.env` files
- [ ] Verify startup validation passes without warnings (check logs for entropy check results)

### Network

- [ ] Set `CORS_ORIGINS` to your actual console domain (e.g. `https://console.example.com`)
- [ ] Deploy the console (port 3001) behind a VPN or corporate firewall; do not expose it to the public internet
- [ ] Enable TLS termination on the reverse proxy (Nginx, Caddy, Traefik, or cloud LB)
- [ ] Configure HSTS headers on the reverse proxy
- [ ] Restrict gateway (port 3000) access to known CIDR ranges if possible

### Authentication

- [ ] Configure OIDC for SSO with your corporate identity provider
- [ ] Disable password-based registration in production (use admin-provisioned accounts or SSO)
- [ ] Enforce strong password policies if password auth is enabled
- [ ] Set up rate limiting on login endpoints to mitigate brute-force attacks

### Database and Infrastructure

- [ ] Configure PostgreSQL to require TLS (`sslmode=require` in `DATABASE_URL`)
- [ ] Enable Redis authentication (`requirepass` directive)
- [ ] Use Redis TLS if available (`rediss://` scheme)
- [ ] Restrict PostgreSQL access to only the AgentBastion service account
- [ ] Run database migrations only from a privileged CI/CD pipeline, not from the application at runtime

### API Key Lifecycle

- [ ] Set appropriate `inactivity_timeout_days` on API keys to auto-disable unused keys
- [ ] Establish a key rotation schedule and use `POST /api/keys/{id}/rotate` for zero-downtime rotation
- [ ] Periodically review expiring keys via `GET /api/keys/expiring?days=30`
- [ ] Configure `data_retention_days` to comply with your data retention policy

### Audit and Monitoring

- [ ] Set up log rotation for application logs
- [ ] Verify ClickHouse audit tables are being populated
- [ ] Configure log forwarders to your SIEM via Admin > Log Forwarders if applicable
- [ ] Set up alerts for `auth.login_failed` spikes (potential brute-force)
- [ ] Monitor the `/api/health`, `/health/live`, and `/health/ready` endpoints with your infrastructure monitoring system
- [ ] Configure Prometheus scraping of the `/metrics` endpoint (gateway port 3000)
- [ ] Set up alerts for API key inactivity and expiration events

### Container and Runtime

- [ ] Use the distroless production image (no shell, no package manager)
- [ ] Run the container as a non-root user
- [ ] Set read-only filesystem where possible
- [ ] Apply resource limits (CPU, memory) to prevent runaway processes
- [ ] Scan container images for CVEs in CI/CD

### RBAC

- [ ] Review all user roles and ensure least-privilege assignment
- [ ] Audit admin accounts regularly
- [ ] Use `service` role for machine-to-machine integrations
- [ ] Restrict `allowed_models` on API keys to only the models each consumer needs
