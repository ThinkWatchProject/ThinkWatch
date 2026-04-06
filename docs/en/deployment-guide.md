**[English](../en/deployment-guide.md) | [中文](../zh-CN/deployment-guide.md)**

# ThinkWatch Deployment Guide

## 1. Prerequisites

### Hardware Requirements

| Resource | Minimum          | Recommended          |
|----------|------------------|----------------------|
| CPU      | 2 cores          | 4+ cores             |
| RAM      | 4 GB             | 8+ GB                |
| Disk     | 20 GB SSD        | 50+ GB SSD           |
| Network  | 100 Mbps         | 1 Gbps               |

The server process itself is lightweight (Rust binary, ~50 MB RSS typical). Most resource consumption comes from PostgreSQL, Redis, and ClickHouse.

### Software Requirements

**For development:**

| Software   | Version | Purpose                          |
|------------|---------|----------------------------------|
| Rust       | Edition 2024 (see `rust-toolchain.toml`) | Build the server          |
| Node.js    | 20+     | Build the web UI                 |
| pnpm       | 9+      | Web UI package manager           |
| Docker     | 24+     | Run infrastructure services      |
| Docker Compose | v2+ | Orchestrate dev services         |

**For production (Docker-only):**

| Software        | Version |
|-----------------|---------|
| Docker          | 24+     |
| Docker Compose  | v2+     |

---

## 2. Development Setup

### 2.1 Clone and Install Dependencies

```bash
git clone <repository-url> ThinkWatch
cd ThinkWatch

# Install Rust toolchain (reads from rust-toolchain.toml)
rustup show

# Install web UI dependencies
cd web && pnpm install && cd ..
```

### 2.2 Start Infrastructure Services

```bash
make infra
```

This starts:
- **PostgreSQL** on port 5432 (user: `postgres`, password: `postgres`, db: `think_watch`)
- **Redis** on port 6379
- **ClickHouse** (audit log storage) on port 8123 (HTTP) / 9000 (native TCP)
- **Zitadel** (OIDC SSO) on port 8080

Wait for all services to become healthy:

```bash
make infra-down   # to stop services
```

### 2.3 Start the Server

```bash
# From the project root
cargo run
```

The server reads configuration from environment variables (with sensible development defaults). It will:
1. Connect to PostgreSQL and run migrations automatically.
2. Connect to Redis.
3. Start the gateway on `http://localhost:3000`.
4. Start the console on `http://localhost:3001`.

### 2.4 Start the Web UI (Development)

```bash
cd web
pnpm dev
```

The Vite dev server starts on `http://localhost:5173` and proxies API requests to the console server at `localhost:3001`.

### 2.5 Creating the First Admin User (Setup Wizard)

On a fresh database, ThinkWatch provides a setup wizard accessible through the Web UI or the API. The setup wizard creates the first admin user and optionally configures the first AI provider in a single step.

**Via the Web UI:**

Open `http://localhost:5173` in your browser. If the system has not been initialized, the UI will automatically show the setup wizard.

**Via the API:**

Check whether setup is needed:

```bash
curl http://localhost:3001/api/setup/status
# Returns: {"initialized": false, "needs_setup": true}
```

Initialize the system:

```bash
curl -X POST http://localhost:3001/api/setup/initialize \
  -H "Content-Type: application/json" \
  -d '{
    "admin": {
      "email": "admin@example.com",
      "display_name": "Admin",
      "password": "your-secure-password"
    },
    "provider": {
      "name": "openai-prod",
      "display_name": "OpenAI",
      "provider_type": "openai",
      "base_url": "https://api.openai.com/v1",
      "api_key": "sk-..."
    }
  }'
```

The `provider` field is optional. You can add providers later through the admin UI.

> **Note:** The setup endpoint is rate-limited (5 requests per minute) and is disabled after the first admin user is created. See the security documentation for details.

**Legacy method (manual):**

Alternatively, register via `POST /api/auth/register` and then assign the `super_admin` role directly in PostgreSQL:

```sql
INSERT INTO user_roles (user_id, role_id, scope)
SELECT u.id, r.id, 'global'
FROM users u, roles r
WHERE u.email = 'admin@example.com' AND r.name = 'super_admin';
```

Subsequent users can be promoted to admin roles through the web UI by the super admin.

### 2.6 Configuring Zitadel SSO (Development)

The dev compose file starts a Zitadel instance at `http://localhost:8080`.

1. Log in to Zitadel at `http://localhost:8080` with username `admin` and password `Admin1234!`.
2. Create a new project and an OIDC application of type "Web" with:
   - Redirect URI: `http://localhost:3001/api/auth/oidc/callback`
   - Post-logout redirect: `http://localhost:5173`
3. Copy the client ID and client secret.
4. Set environment variables before starting the server:

```bash
export OIDC_ISSUER_URL=http://localhost:8080
export OIDC_CLIENT_ID=<your-client-id>
export OIDC_CLIENT_SECRET=<your-client-secret>
export OIDC_REDIRECT_URL=http://localhost:3001/api/auth/oidc/callback
```

Or add them to a `.env` file in the project root (loaded automatically by `dotenvy`).

---

## 3. Docker Compose Production Deployment

### 3.1 Create Production Environment File

Create a `.env.production` file with real secrets. **Never commit this file to version control.**

```bash
# Database
DB_USER=thinkwatch
DB_PASSWORD=<generate-a-strong-password>
DB_NAME=think_watch

# Authentication
JWT_SECRET=<generate-a-64-char-hex-string>
ENCRYPTION_KEY=<generate-a-64-char-hex-string>

# Ports
GATEWAY_PORT=3000
WEB_PORT=80

# CORS — set to your actual console domain
CORS_ORIGINS=https://console.yourdomain.com

# ClickHouse
CLICKHOUSE_USER=default
CLICKHOUSE_PASSWORD=<generate-a-strong-password>

# Redis
REDIS_PASSWORD=<generate-a-strong-password>

# OIDC SSO (optional)
OIDC_ISSUER_URL=https://auth.yourdomain.com
OIDC_CLIENT_ID=<client-id>
OIDC_CLIENT_SECRET=<client-secret>
OIDC_REDIRECT_URL=https://console.yourdomain.com/api/auth/oidc/callback

# Logging
RUST_LOG=info,think_watch=info
```

### 3.2 Generate Secure Secrets

```bash
# JWT_SECRET (64 hex characters = 256 bits)
openssl rand -hex 32

# ENCRYPTION_KEY (64 hex characters = 256 bits, used for AES-256-GCM)
openssl rand -hex 32

# Database and Redis passwords
openssl rand -base64 24
```

### 3.3 Deploy

Pull the pre-built images from GitHub Container Registry and start all services:

```bash
docker compose -f deploy/docker-compose.yml --env-file .env.production pull
docker compose -f deploy/docker-compose.yml --env-file .env.production up -d
```

To pin a specific release instead of `latest`:

```bash
IMAGE_TAG=<git-sha> docker compose -f deploy/docker-compose.yml --env-file .env.production up -d
```

This starts:
- **server** -- The ThinkWatch Rust binary (gateway on port 3000, console on port 3001 internal only)
- **web** -- nginx serving the built React SPA (port 80)
- **postgres** -- PostgreSQL 18 with persistent volume
- **redis** -- Redis 8 with persistent volume
- **clickhouse** -- ClickHouse columnar database for audit logs

### 3.4 Verify Health

```bash
# Gateway health
curl http://localhost:3000/health

# Console health (from within the Docker network, or via the web container)
docker compose -f deploy/docker-compose.yml exec server curl http://localhost:3001/api/health
```

### 3.5 ClickHouse Configuration

ClickHouse stores audit logs in a columnar format optimized for analytical queries. Configure the connection via these environment variables in `.env.production`:

```bash
# ClickHouse connection
CLICKHOUSE_URL=http://clickhouse:8123
CLICKHOUSE_DB=think_watch
CLICKHOUSE_USER=default
CLICKHOUSE_PASSWORD=<your-clickhouse-password>
```

| Variable | Default | Description |
|----------|---------|-------------|
| `CLICKHOUSE_URL` | `http://clickhouse:8123` | ClickHouse HTTP interface URL. |
| `CLICKHOUSE_DB` | `think_watch` | Database name for audit log tables. |
| `CLICKHOUSE_USER` | `default` | ClickHouse user for authentication. |
| `CLICKHOUSE_PASSWORD` | — | ClickHouse password. |

ThinkWatch creates six tables in ClickHouse:

- `audit_logs` — Security audit trail (90-day TTL)
- `gateway_logs` — AI API request logs (90-day TTL)
- `mcp_logs` — MCP tool invocation logs (90-day TTL)
- `platform_logs` — Platform management operations (90-day TTL)
- `access_logs` — HTTP access logs (30-day TTL)
- `app_logs` — Application runtime logs (30-day TTL)

Tables are automatically created on first startup via `deploy/clickhouse/init.sql`.

### 3.6 Set Up a Reverse Proxy

In production, place a reverse proxy in front of the Docker services to handle TLS termination. Only the gateway port (3000) and the web UI port (80) need to be reachable by end users.

**Important:** The console port (3001) should NOT be exposed to the public internet. Access it only through an internal network, VPN, or via the web container's nginx that reverse-proxies `/api/*` to port 3001.

#### Example: nginx reverse proxy

```nginx
# Gateway — public-facing
server {
    listen 443 ssl;
    server_name gateway.yourdomain.com;

    ssl_certificate     /etc/letsencrypt/live/gateway.yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/gateway.yourdomain.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Required for SSE streaming
        proxy_buffering off;
        proxy_cache off;
        proxy_read_timeout 300s;
    }
}

# Console Web UI — restricted access
server {
    listen 443 ssl;
    server_name console.yourdomain.com;

    ssl_certificate     /etc/letsencrypt/live/console.yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/console.yourdomain.com/privkey.pem;

    # Optionally restrict by IP
    # allow 10.0.0.0/8;
    # deny all;

    location / {
        proxy_pass http://127.0.0.1:80;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

#### Example: traefik (via Docker labels)

Add labels to the `server` and `web` services in your compose override to configure Traefik routing and TLS with Let's Encrypt automatically.

---

## 4. Kubernetes Deployment

A Helm chart is provided at `deploy/helm/think-watch/`.

### 4.1 Images

Pre-built images are published automatically to GitHub Container Registry on every push to `main`:

```
ghcr.io/thinkwatch/think-watch-server:latest
ghcr.io/thinkwatch/think-watch-server:<git-sha>

ghcr.io/thinkwatch/think-watch-web:latest
ghcr.io/thinkwatch/think-watch-web:<git-sha>
```

No authentication is needed — the packages are public.

### 4.2 Install with Helm

```bash
helm install think-watch deploy/helm/think-watch \
  --set secrets.jwtSecret=$(openssl rand -hex 32) \
  --set secrets.encryptionKey=$(openssl rand -hex 32) \
  --set secrets.databaseUrl="postgres://thinkwatch:password@postgres:5432/think_watch" \
  --set secrets.redisUrl="redis://:password@redis:6379" \
  --set config.corsOrigins="https://console.internal.example.com"
```

To deploy a specific image tag:

```bash
helm install think-watch deploy/helm/think-watch \
  --set image.server.tag=<git-sha> \
  --set image.web.tag=<git-sha> \
  ...
```

### 4.3 Configure Ingress

The Helm chart supports dual-host Ingress. Enable and configure in `values.yaml` or via `--set`:

```yaml
ingress:
  enabled: true
  className: nginx
  gateway:
    host: gateway.yourdomain.com        # Public-facing
    tls:
      - secretName: gateway-tls
        hosts:
          - gateway.yourdomain.com
  console:
    host: console.internal.yourdomain.com  # Internal only
    tls:
      - secretName: console-tls
        hosts:
          - console.internal.yourdomain.com
```

For the console Ingress, use an internal ingress class or add annotations to restrict access:

```yaml
ingress:
  console:
    annotations:
      nginx.ingress.kubernetes.io/whitelist-source-range: "10.0.0.0/8,172.16.0.0/12"
```

### 4.4 External Secrets

For production, use the External Secrets Operator instead of passing secrets via `--set`:

```yaml
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: think-watch
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: vault-backend  # or aws-secrets-manager, etc.
    kind: SecretStore
  target:
    name: think-watch-secrets
  data:
    - secretKey: jwt-secret
      remoteRef:
        key: think-watch/jwt-secret
    - secretKey: encryption-key
      remoteRef:
        key: think-watch/encryption-key
    - secretKey: database-url
      remoteRef:
        key: think-watch/database-url
    - secretKey: redis-url
      remoteRef:
        key: think-watch/redis-url
```

### 4.5 Horizontal Pod Autoscaling

Enable HPA in `values.yaml`:

```yaml
autoscaling:
  enabled: true
  minReplicas: 2
  maxReplicas: 10
  targetCPUUtilizationPercentage: 70
```

The server is stateless (all state is in PostgreSQL and Redis), so it scales horizontally without issue.

---

## 5. SSL/TLS

### Using Let's Encrypt with a Reverse Proxy

The recommended approach is to terminate TLS at the reverse proxy layer. ThinkWatch itself serves plain HTTP on ports 3000 and 3001.

**With certbot (standalone nginx):**

```bash
certbot certonly --nginx -d gateway.yourdomain.com -d console.yourdomain.com
```

**With Kubernetes cert-manager:**

```yaml
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt-prod
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: admin@yourdomain.com
    privateKeySecretRef:
      name: letsencrypt-prod
    solvers:
      - http01:
          ingress:
            class: nginx
```

Then reference the issuer in your Ingress annotations:

```yaml
annotations:
  cert-manager.io/cluster-issuer: letsencrypt-prod
```

### Configuring CORS for HTTPS

When the console is served over HTTPS, update the `CORS_ORIGINS` environment variable:

```bash
CORS_ORIGINS=https://console.yourdomain.com
```

Multiple origins can be comma-separated if needed.

---

## 6. Backup and Recovery

### 6.1 PostgreSQL

PostgreSQL contains all critical application state: users, API keys, provider configurations, usage records, and audit logs.

**Scheduled pg_dump (simple):**

```bash
# Daily backup
pg_dump -h localhost -U thinkwatch -d think_watch -Fc > backup_$(date +%Y%m%d).dump

# Restore
pg_restore -h localhost -U thinkwatch -d think_watch -c backup_20260401.dump
```

**WAL archiving (point-in-time recovery):**

For production deployments, configure PostgreSQL WAL archiving to an S3 bucket or network storage. This allows restoring to any point in time, not just the last dump. Use tools like `pgBackRest` or `wal-g` for automated WAL management.

**Managed databases:** If using a managed PostgreSQL service (AWS RDS, Google Cloud SQL, etc.), automated backups and point-in-time recovery are typically included.

### 6.2 Redis

Redis stores ephemeral data (rate limit counters, session state, OIDC flow state). Losing Redis data is not catastrophic -- rate limits reset and users need to log in again.

That said, to preserve rate limit state across restarts, enable Redis persistence:

```
# redis.conf
appendonly yes
appendfsync everysec
```

The Docker Compose files mount a persistent volume for Redis data by default.

### 6.3 ClickHouse

ClickHouse stores audit logs in columnar tables. The Docker Compose files mount a persistent volume for ClickHouse data.

For disaster recovery:
- Back up the ClickHouse data volume, or use `clickhouse-client` to export data.
- Since audit logs are also written to PostgreSQL (`audit_logs` table), ClickHouse data can be re-inserted from the primary database if needed.

---

## 7. Monitoring

### 7.1 Health Endpoints

| Endpoint                  | Port | Purpose                                              |
|---------------------------|------|------------------------------------------------------|
| `GET /health`             | 3000 | Gateway basic health check                           |
| `GET /health/live`        | 3000 | Liveness probe (is the process alive?)               |
| `GET /health/ready`       | 3000 | Readiness probe (returns 503 if PG or Redis is down) |
| `GET /api/health`         | 3001 | Console detailed health (latency, pool stats, uptime)|
| `GET /metrics`            | 3000 | Prometheus metrics (unauthenticated)                 |

**Recommended probe configuration for Kubernetes:**

```yaml
livenessProbe:
  httpGet:
    path: /health/live
    port: 3000
  initialDelaySeconds: 5
  periodSeconds: 10
readinessProbe:
  httpGet:
    path: /health/ready
    port: 3000
  initialDelaySeconds: 10
  periodSeconds: 5
```

Use `/health/live` for liveness probes (restart the pod if the process is stuck) and `/health/ready` for readiness probes (remove the pod from the load balancer if dependencies are down).

### 7.2 Structured Logging

ThinkWatch uses `tracing` with configurable output via the `RUST_LOG` environment variable:

```bash
# Default: info level
RUST_LOG=info,think_watch=info

# Debug for ThinkWatch crates, info for dependencies
RUST_LOG=info,think_watch=debug

# Trace-level for detailed request/response debugging
RUST_LOG=info,think_watch=trace

# JSON-formatted logs (configured in tracing-subscriber setup)
```

Logs are written to stdout and can be collected by any log aggregation system (Datadog, Loki, CloudWatch, etc.).

### 7.3 Audit Log Forwarding

ThinkWatch supports real-time forwarding of audit events to external systems via four channels:

| Channel | Protocol | Use Case |
|---------|----------|----------|
| UDP Syslog | RFC 5424 | Traditional SIEM (Splunk, Graylog) |
| TCP Syslog | RFC 5424 | Reliable transport for SIEM |
| Kafka | REST Proxy (JSON) | Data lakes, stream processing pipelines |
| HTTP Webhook | POST JSON | Custom alerting, Slack/Teams integration |

Configure via the admin console at **Admin > Log Forwarding**:
- Multiple channels can run in parallel
- Each channel can be independently enabled/disabled
- Built-in connection testing and delivery statistics
- Automatic retry with error counting

Audit events are always written to PostgreSQL and ClickHouse (if configured). Forwarding channels provide additional delivery to external systems.

### 7.4 Prometheus Metrics

ThinkWatch exposes Prometheus-compatible metrics at `GET /metrics` on the gateway port (3000). This endpoint is unauthenticated to allow standard Prometheus scraping.

Available metrics include:
- **Request latency histograms** (`http_request_duration_seconds`) -- by endpoint, method, and status
- **Token throughput counters** (`llm_tokens_total`) -- prompt and completion tokens by model
- **Active connection gauges** (`http_active_connections`) -- current number of open connections
- **Error rate counters** (`http_requests_errors_total`) -- by endpoint and error type

**Prometheus scrape configuration:**

```yaml
scrape_configs:
  - job_name: 'thinkwatch'
    scrape_interval: 15s
    static_configs:
      - targets: ['thinkwatch:3000']
    metrics_path: /metrics
```

> **Security:** The `/metrics` endpoint is unauthenticated. In production, restrict access via network policies or a reverse proxy to prevent exposing internal metrics to untrusted networks.

### 7.5 Background Tasks

ThinkWatch runs several background tasks that operate automatically:

| Task                          | Interval   | Description                                                  |
|-------------------------------|------------|--------------------------------------------------------------|
| API key lifecycle management  | Periodic   | Disables keys that exceed their inactivity timeout           |
| Data retention purge          | Daily      | Permanently deletes soft-deleted records older than `data_retention_days` |
| Key expiry notifications      | Daily      | Logs warnings for keys expiring within 7 days               |

These tasks run within the server process and do not require external schedulers (e.g., cron). They are automatically active when the server starts.

---

## 8. Client Configuration Guide

ThinkWatch includes a built-in **Configuration Guide** page in the web console at `/gateway/guide`. This page provides copy-paste setup instructions for connecting various AI client tools to your gateway and auto-detects the gateway URL.

### Supported Client Tools

The Configuration Guide provides ready-to-use instructions for:

- **Claude Code** -- set `ANTHROPIC_BASE_URL` to your gateway's `/v1` endpoint (uses the Anthropic Messages API at `/v1/messages`)
- **Cursor** -- configure the OpenAI-compatible endpoint in Cursor settings (uses `/v1/chat/completions`)
- **Continue** -- configure the provider in `~/.continue/config.json` (uses `/v1/chat/completions`)
- **Cline** -- configure the API base URL in Cline settings (uses `/v1/chat/completions`)
- **OpenAI SDK** -- set `OPENAI_BASE_URL` and `OPENAI_API_KEY` environment variables (uses `/v1/chat/completions` or `/v1/responses`)
- **Anthropic SDK** -- set `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` environment variables (uses `/v1/messages`)
- **cURL** -- example commands for all three API formats

### Gateway API Endpoints

The gateway (port 3000) serves three API formats on a single port:

| Endpoint                    | Format                  | Typical Clients                              |
| --------------------------- | ----------------------- | -------------------------------------------- |
| `POST /v1/chat/completions` | OpenAI Chat Completions | Cursor, Continue, Cline, OpenAI SDK          |
| `POST /v1/messages`         | Anthropic Messages API  | Claude Code, Anthropic SDK                   |
| `POST /v1/responses`        | OpenAI Responses API    | OpenAI SDK (2025 format)                     |
| `GET /v1/models`            | OpenAI Models list      | All clients                                  |

All endpoints accept `tw-` API keys via the `Authorization: Bearer` header.

---

## 9. Upgrading

### 9.1 Database Migrations

Migrations are run automatically when the server starts. The `sqlx` migrate system tracks which migrations have been applied in a `_sqlx_migrations` table and only runs new ones.

**Important:** Always back up your database before upgrading to a new version that includes schema changes.

### 9.2 Rolling Updates (Kubernetes)

The server is stateless, so rolling updates work out of the box:

```bash
# Update the image tag
helm upgrade think-watch deploy/helm/think-watch \
  --set image.server.tag=0.2.0 \
  --reuse-values
```

Kubernetes will perform a rolling update, starting new pods before terminating old ones. The first new pod to start will run any pending database migrations.

**Tip:** Set `maxSurge: 1` and `maxUnavailable: 0` in your deployment strategy to ensure zero downtime during upgrades.

### 9.3 Docker Compose Updates

```bash
# Pull new images or rebuild
docker compose -f deploy/docker-compose.yml --env-file .env.production build

# Restart with new images (database migrations run on startup)
docker compose -f deploy/docker-compose.yml --env-file .env.production up -d
```

### 9.4 Breaking Changes Policy

- **Patch versions** (0.1.x): Bug fixes only. No migration changes. Safe to upgrade without review.
- **Minor versions** (0.x.0): May include new migrations that add tables or columns. Always backward-compatible. Review the changelog.
- **Major versions** (x.0.0): May include breaking API changes, destructive migrations, or configuration changes. Read the upgrade guide carefully before proceeding.

Check the project changelog or release notes for migration details before upgrading.
