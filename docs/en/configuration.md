**[English](../en/configuration.md) | [中文](../zh-CN/configuration.md)**

# ThinkWatch Configuration Reference

This document provides a complete reference for all configuration options in ThinkWatch. Configuration is managed through a combination of environment variables (for infrastructure and secrets) and database-backed dynamic settings (configurable via the admin Web UI).

---

## Environment Variables

### Database

#### `DATABASE_URL`

| Property  | Value                                                      |
| --------- | ---------------------------------------------------------- |
| Required  | Yes                                                        |
| Default   | —                                                          |
| Example   | `postgres://thinkwatch:password@localhost:5432/thinkwatch` |

PostgreSQL connection string. ThinkWatch requires PostgreSQL 15 or later.

**Security notes:**
- In production, use `sslmode=require` to enforce encrypted connections: `postgres://user:pass@host:5432/db?sslmode=require`
- Avoid embedding passwords in URLs checked into version control. Use a secrets manager.
- The database user needs permissions to create tables (for migrations) or should have migrations applied separately.

---

#### `REDIS_URL`

| Property  | Value                                    |
| --------- | ---------------------------------------- |
| Required  | Yes                                      |
| Default   | —                                        |
| Example   | `redis://localhost:6379`                 |

Redis connection string. Used for rate limiting, OIDC state/nonce storage, and session management.

**Security notes:**
- In production, enable Redis authentication: `redis://:yourpassword@host:6379`
- For TLS-enabled Redis, use the `rediss://` scheme: `rediss://:password@host:6380`

---

### Cryptography

#### `JWT_SECRET`

| Property  | Value                                              |
| --------- | -------------------------------------------------- |
| Required  | Yes                                                |
| Default   | —                                                  |
| Example   | `a3f8c1e0b9d74...` (64-character hex string)       |

Shared secret used for HS256 JWT signing and verification. Must be at least 32 characters long, and at least 256 bits (32 bytes / 64 hex characters) is recommended for adequate security. At startup, ThinkWatch performs an entropy check on this value and will refuse to start if it does not meet the minimum length requirement.

**Security notes:**
- Generate with: `openssl rand -hex 32`
- **Minimum 32 characters required.** Shorter values will cause a startup failure.
- Changing this value invalidates all active access and refresh tokens, forcing all users to re-authenticate.
- Never reuse this value across environments (dev/staging/prod).
- Store in a secrets manager, not in plaintext files.

---

#### `ENCRYPTION_KEY`

| Property  | Value                                              |
| --------- | -------------------------------------------------- |
| Required  | Yes                                                |
| Default   | —                                                  |
| Example   | `b7e4d219f0c83...` (64-character hex string)       |

256-bit key (32 bytes, encoded as 64 hex characters) used for AES-256-GCM encryption of sensitive data at rest (provider API keys, MCP server auth secrets).

**Security notes:**
- Generate with: `openssl rand -hex 32`
- Changing this value renders all previously encrypted data (provider API keys, MCP auth secrets) unreadable. You must re-enter those values after rotation.
- This is the most critical secret in the system. Compromise of this key exposes all stored provider credentials.
- Store in a hardware security module (HSM) or secrets manager in production.

---

### Server

#### `SERVER_HOST`

| Property  | Value           |
| --------- | --------------- |
| Required  | No              |
| Default   | `0.0.0.0`      |
| Example   | `127.0.0.1`    |

IP address the servers bind to. Use `0.0.0.0` to listen on all interfaces, or `127.0.0.1` to restrict to localhost only.

---

#### `GATEWAY_PORT`

| Property  | Value  |
| --------- | ------ |
| Required  | No     |
| Default   | `3000` |
| Example   | `8080` |

TCP port for the gateway server (OpenAI-compatible API and MCP transport).

---

#### `CONSOLE_PORT`

| Property  | Value  |
| --------- | ------ |
| Required  | No     |
| Default   | `3001` |
| Example   | `8081` |

TCP port for the console server (management API and admin endpoints).

---

#### `CORS_ORIGINS`

| Property  | Value                                    |
| --------- | ---------------------------------------- |
| Required  | No                                       |
| Default   | `http://localhost:5173`                  |
| Example   | `https://console.example.com,https://admin.example.com` |

Comma-separated list of allowed CORS origins. Each origin must include the scheme (`http://` or `https://`) and must not include a trailing slash.

**Security notes:**
- In production, restrict to only the domain(s) that host your console frontend.
- Do not use `*` as this disables CORS protection entirely.

---

### Observability

#### `CLICKHOUSE_URL`

| Property  | Value                          |
| --------- | ------------------------------ |
| Required  | No                             |
| Default   | —                              |
| Example   | `http://clickhouse:8123`       |

Base URL of the ClickHouse HTTP interface for audit log storage and search. If not set, audit log ingestion to ClickHouse will be unavailable (entries are still logged to stdout and forwarded to configured log forwarders).

---

#### `CLICKHOUSE_DB`

| Property  | Value                       |
| --------- | --------------------------- |
| Required  | No                          |
| Default   | `think_watch`             |
| Example   | `think_watch_prod`        |

Name of the ClickHouse database where audit log tables are stored. ThinkWatch automatically creates the database and tables on startup if they do not exist.

---

#### `CLICKHOUSE_USER`

| Property  | Value                       |
| --------- | --------------------------- |
| Required  | No                          |
| Default   | `default`                   |
| Example   | `thinkwatch`              |

ClickHouse user for authentication.

---

#### `CLICKHOUSE_PASSWORD`

| Property  | Value                       |
| --------- | --------------------------- |
| Required  | No                          |
| Default   | —                           |
| Example   | `your-clickhouse-password`  |

ClickHouse password for authentication.

**Security notes:**
- In production, always set a strong password for ClickHouse.
- Store in a secrets manager, not in plaintext files.

---

#### `RUST_LOG`

| Property  | Value                                                    |
| --------- | -------------------------------------------------------- |
| Required  | No                                                       |
| Default   | `info`                                                   |
| Example   | `thinkwatch=debug,tower_http=debug,sqlx=warn`         |

Controls log verbosity using the [`tracing-subscriber` `EnvFilter` syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html). Accepts comma-separated directives of the form `target=level`.

Common configurations:

| Use Case              | Value                                              |
| --------------------- | -------------------------------------------------- |
| Production            | `info` or `thinkwatch=info,tower_http=warn`      |
| Debugging HTTP        | `thinkwatch=debug,tower_http=debug`              |
| Debugging SQL         | `thinkwatch=debug,sqlx=debug`                    |
| Minimal output        | `warn`                                             |

---

### OIDC / SSO

All four OIDC variables must be set together to enable SSO. If any are missing, OIDC is disabled and `GET /api/auth/sso/authorize` returns 500.

#### `OIDC_ISSUER_URL`

| Property  | Value                                                            |
| --------- | ---------------------------------------------------------------- |
| Required  | No (required if SSO is desired)                                  |
| Default   | —                                                                |
| Example   | `https://login.microsoftonline.com/tenant-id/v2.0`              |

The OIDC issuer URL. ThinkWatch fetches the `.well-known/openid-configuration` document from this URL at startup to discover endpoints.

---

#### `OIDC_CLIENT_ID`

| Property  | Value                        |
| --------- | ---------------------------- |
| Required  | No (required if SSO)         |
| Default   | —                            |
| Example   | `abcdef12-3456-7890-abcd-ef1234567890` |

The client ID registered with your OIDC identity provider.

---

#### `OIDC_CLIENT_SECRET`

| Property  | Value                              |
| --------- | ---------------------------------- |
| Required  | No (required if SSO)               |
| Default   | —                                  |
| Example   | `your-client-secret-value`         |

The client secret for the OIDC application.

**Security notes:**
- This is a sensitive credential. Store it in a secrets manager.
- Rotate periodically according to your IdP's recommendations.

---

#### `OIDC_REDIRECT_URL`

| Property  | Value                                                          |
| --------- | -------------------------------------------------------------- |
| Required  | No (required if SSO)                                           |
| Default   | —                                                              |
| Example   | `https://console.example.com/api/auth/sso/callback`           |

The full URL that the OIDC provider redirects to after authentication. This must exactly match the redirect URI registered in your IdP configuration and must be reachable by the user's browser.

---

## Provider Configuration

Providers are configured through the admin Web UI (Admin > Providers) or the `POST /api/admin/providers` API. Each provider type has specific configuration requirements:

### OpenAI

| Field           | Value                              |
| --------------- | ---------------------------------- |
| `provider_type` | `openai`                           |
| `base_url`      | `https://api.openai.com`           |
| `api_key`       | `sk-...` (OpenAI API key)          |

Models with prefixes `gpt-`, `o1-`, `o3-`, `o4-` are automatically routed to this provider.

### Anthropic

| Field           | Value                              |
| --------------- | ---------------------------------- |
| `provider_type` | `anthropic`                        |
| `base_url`      | `https://api.anthropic.com`        |
| `api_key`       | `sk-ant-...` (Anthropic API key)   |

Models with prefix `claude-` are automatically routed to this provider.

### Google Gemini

| Field           | Value                                          |
| --------------- | ---------------------------------------------- |
| `provider_type` | `google`                                       |
| `base_url`      | `https://generativelanguage.googleapis.com`    |
| `api_key`       | Google AI API key                              |

Models with prefix `gemini-` are automatically routed to this provider.

### Azure OpenAI

| Field           | Value                                          |
| --------------- | ---------------------------------------------- |
| `provider_type` | `azure`                                        |
| `base_url`      | `https://{resource}.openai.azure.com`          |
| `api_key`       | Azure API key                                  |
| `config_json`   | `{"api_version": "2024-12-01-preview"}`        |

Azure OpenAI uses the `api-key` header (not Bearer token) for authentication. The `api_version` is passed as a query parameter. Azure does not support automatic model routing -- you must explicitly register models via Admin > Models.

### AWS Bedrock

| Field           | Value                                          |
| --------------- | ---------------------------------------------- |
| `provider_type` | `bedrock`                                      |
| `base_url`      | AWS region (e.g. `us-east-1`)                  |
| `api_key`       | `ACCESS_KEY_ID:SECRET_ACCESS_KEY`              |

AWS Bedrock uses SigV4 request signing via the official `aws-sigv4` Rust crate. The `base_url` field specifies the AWS region, and the `api_key` field contains the IAM credentials in `ACCESS_KEY_ID:SECRET_ACCESS_KEY` format. Streaming uses the native Bedrock binary event-stream protocol (Converse API). Like Azure, Bedrock does not support automatic model routing -- you must explicitly register models via Admin > Models.

### Custom (OpenAI-compatible)

| Field           | Value                                          |
| --------------- | ---------------------------------------------- |
| `provider_type` | `custom`                                       |
| `base_url`      | Any OpenAI-compatible endpoint URL             |
| `api_key`       | Bearer token for the endpoint                  |

Use this type for self-hosted models (vLLM, Ollama, LiteLLM, etc.) or any third-party service that implements the OpenAI API format.

### Provider Auto-Loading

On startup, ThinkWatch loads all active providers from the database and registers them in the model router. Models are routed based on:

1. **Prefix matching** (for OpenAI, Anthropic, Google): Models matching the provider's default prefix are automatically routed.
2. **Explicit registration** (for Azure, Bedrock, Custom): Models must be registered via the Admin > Models page with a specific provider assignment.

---

## Configuration Patterns

### Development vs Production

| Setting           | Development                                  | Production                                  |
| ----------------- | -------------------------------------------- | ------------------------------------------- |
| `DATABASE_URL`    | Local PostgreSQL without SSL                 | Managed PostgreSQL with `sslmode=require`   |
| `REDIS_URL`       | Local Redis without auth                     | Redis with `requirepass` or managed Redis   |
| `JWT_SECRET`      | Any stable value for dev convenience         | Cryptographically random 256-bit key        |
| `ENCRYPTION_KEY`  | Any stable 32-byte hex for dev convenience   | Cryptographically random, stored in HSM     |
| `CORS_ORIGINS`    | `http://localhost:5173`                      | `https://console.yourdomain.com`            |
| `RUST_LOG`        | `thinkwatch=debug,tower_http=debug`        | `info` or `thinkwatch=info`               |
| OIDC variables    | _(unset unless testing SSO)_                 | Fully configured                            |

### Using .env Files

For local development, create a `.env` file in the project root:

```bash
# .env (DO NOT commit this file)
DATABASE_URL=postgres://thinkwatch:devpass@localhost:5432/thinkwatch
REDIS_URL=redis://localhost:6379
JWT_SECRET=dev-only-jwt-secret-do-not-use-in-production-000000
ENCRYPTION_KEY=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
CORS_ORIGINS=http://localhost:5173
RUST_LOG=thinkwatch=debug,tower_http=debug
```

ThinkWatch loads `.env` automatically via the `dotenvy` crate. The `.env` file should be listed in `.gitignore`.

### Environment File

ThinkWatch uses a single `.env` file at the project root for all environments:

- **Development**: `cargo run` loads via `dotenvy`, `docker compose` loads via `--env-file .env` (configured in Makefile)
- **Production**: Uses `.env.production` at the project root, loaded via `make deploy`

There is no separate `deploy/.env` file. Copy `.env.example` to get started:

```bash
cp .env.example .env
# Edit .env with your values, or run:
# deploy/generate-secrets.sh
```

### Docker / Docker Compose

Pass environment variables through `docker-compose.yml`:

```yaml
services:
  thinkwatch:
    image: thinkwatch:latest
    environment:
      DATABASE_URL: postgres://thinkwatch:${DB_PASSWORD}@postgres:5432/thinkwatch
      REDIS_URL: redis://redis:6379
      JWT_SECRET: ${JWT_SECRET}
      ENCRYPTION_KEY: ${ENCRYPTION_KEY}
      CORS_ORIGINS: https://console.example.com
      CLICKHOUSE_URL: http://clickhouse:8123
      RUST_LOG: info
    ports:
      - "3000:3000"
      - "3001:3001"
```

Use a `.env` file or shell exports to provide `${DB_PASSWORD}`, `${JWT_SECRET}`, and `${ENCRYPTION_KEY}` without hardcoding them in the compose file.

### Kubernetes Secrets

Create a Kubernetes Secret for sensitive values:

```bash
kubectl create secret generic thinkwatch-secrets \
  --from-literal=JWT_SECRET="$(openssl rand -hex 32)" \
  --from-literal=ENCRYPTION_KEY="$(openssl rand -hex 32)" \
  --from-literal=DATABASE_URL="postgres://user:pass@pg-host:5432/thinkwatch?sslmode=require" \
  --from-literal=REDIS_URL="rediss://:password@redis-host:6380" \
  --from-literal=OIDC_CLIENT_SECRET="your-oidc-secret"
```

Reference the secret in your Deployment:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: thinkwatch
spec:
  template:
    spec:
      containers:
        - name: thinkwatch
          image: thinkwatch:latest
          envFrom:
            - secretRef:
                name: thinkwatch-secrets
          env:
            - name: CORS_ORIGINS
              value: "https://console.example.com"
            - name: GATEWAY_PORT
              value: "3000"
            - name: CONSOLE_PORT
              value: "3001"
            - name: CLICKHOUSE_URL
              value: "http://clickhouse:8123"
            - name: RUST_LOG
              value: "info"
```

### Generating Secure Random Keys

Use `openssl` to generate cryptographically secure random values:

```bash
# Generate a 256-bit key (for JWT_SECRET or ENCRYPTION_KEY)
openssl rand -hex 32

# Generate a 128-bit key (for less critical uses)
openssl rand -hex 16

# Generate a base64-encoded key (alternative format)
openssl rand -base64 32
```

Both `JWT_SECRET` and `ENCRYPTION_KEY` expect a 64-character hex string (representing 32 bytes / 256 bits).

---

## Dynamic Configuration (Web UI)

Many settings that previously required environment variables are now stored in the database and configurable through the admin Web UI at runtime. These settings can also be managed via the `GET/PATCH /api/admin/settings` API endpoints.

Changes to dynamic settings take effect immediately without requiring a server restart.

### Authentication & JWT

| Setting                      | Default  | Description                                          |
| ---------------------------- | -------- | ---------------------------------------------------- |
| `jwt_access_ttl_seconds`     | `900`    | Access token lifetime in seconds (15 minutes)        |
| `jwt_refresh_ttl_seconds`    | `604800` | Refresh token lifetime in seconds (7 days)           |

### Cache

| Setting              | Default | Description                              |
| -------------------- | ------- | ---------------------------------------- |
| `cache_ttl_seconds`  | `300`   | Default cache TTL for various caches     |

### Security

| Setting                      | Default | Description                                          |
| ---------------------------- | ------- | ---------------------------------------------------- |
| `signature_drift_seconds`    | `300`   | Maximum allowed clock drift for signed requests      |
| `nonce_ttl_seconds`          | `300`   | TTL for nonce values used in replay protection       |
| `content_filter_patterns`    | `[]`    | Content filter patterns (max 500; severity enum: `low`, `medium`, `high`, `critical`) |
| `pii_patterns`               | `[]`    | PII detection regex patterns (max 100; max 1000 chars each; validated at save time) |

### Budget

| Setting                      | Default | Description                                          |
| ---------------------------- | ------- | ---------------------------------------------------- |
| `budget_warning_threshold`   | `0.8`   | Budget utilization ratio that triggers a warning     |
| `budget_critical_threshold`  | `0.95`  | Budget utilization ratio that triggers a critical alert |

### API Key Policies

| Setting                        | Default     | Description                                    |
| ------------------------------ | ----------- | ---------------------------------------------- |
| `api_key_max_expiry_days`      | `365`       | Maximum allowed expiry for new API keys        |
| `api_key_default_rate_limit_rpm` | `60`      | Default RPM limit for new API keys             |

### General

| Setting                | Default          | Description                                      |
| ---------------------- | ---------------- | ------------------------------------------------ |
| `data_retention_days`  | `30`             | Days to retain soft-deleted records before purge |
| `site_name`            | `ThinkWatch`   | Site name displayed in the Web UI                |

---

## Startup Validation

ThinkWatch validates all configuration and dependencies at startup and will refuse to start if:

- `DATABASE_URL` is missing or the database is unreachable
- `REDIS_URL` is missing or Redis is unreachable
- `JWT_SECRET` is missing or shorter than 32 characters
- `JWT_SECRET` fails the entropy check (e.g., all identical characters)
- `ENCRYPTION_KEY` is missing or not a valid 64-character hex string
- `OIDC_*` variables are partially configured (all four must be set, or none)

Additionally, the server performs dependency health checks at startup:

- **PostgreSQL:** Verifies the connection and runs a test query
- **Redis:** Verifies the connection with a PING command
- **ClickHouse:** If configured, verifies the connection and tables exist (non-blocking -- logs a warning if unavailable)

Check the application logs if the server fails to start. Missing or invalid configuration will be reported with clear error messages.
