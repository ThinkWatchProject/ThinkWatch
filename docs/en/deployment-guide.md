**[English](../en/deployment-guide.md) | [中文](../zh-CN/deployment-guide.md)**

# AgentBastion Deployment Guide

## 1. Prerequisites

### Hardware Requirements

| Resource | Minimum          | Recommended          |
|----------|------------------|----------------------|
| CPU      | 2 cores          | 4+ cores             |
| RAM      | 4 GB             | 8+ GB                |
| Disk     | 20 GB SSD        | 50+ GB SSD           |
| Network  | 100 Mbps         | 1 Gbps               |

The server process itself is lightweight (Rust binary, ~50 MB RSS typical). Most resource consumption comes from PostgreSQL, Redis, and Quickwit.

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
git clone <repository-url> AgentBastion
cd AgentBastion

# Install Rust toolchain (reads from rust-toolchain.toml)
rustup show

# Install web UI dependencies
cd web && pnpm install && cd ..
```

### 2.2 Start Infrastructure Services

```bash
docker compose -f deploy/docker-compose.dev.yml up -d
```

This starts:
- **PostgreSQL** on port 5432 (user: `postgres`, password: `postgres`, db: `agent_bastion`)
- **Redis** on port 6379
- **RustFS** (S3-compatible storage) on ports 9000/9001
- **Quickwit** (audit log search) on port 7280
- **Zitadel** (OIDC SSO) on port 8080

Wait for all services to become healthy:

```bash
docker compose -f deploy/docker-compose.dev.yml ps
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

### 2.5 Creating the First Admin User

On a fresh database, register the first user via the console API:

```bash
curl -X POST http://localhost:3001/api/auth/register \
  -H "Content-Type: application/json" \
  -d '{
    "email": "admin@example.com",
    "display_name": "Admin",
    "password": "your-secure-password"
  }'
```

Then assign the `super_admin` role. Connect to PostgreSQL and run:

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
DB_USER=bastion
DB_PASSWORD=<generate-a-strong-password>
DB_NAME=agent_bastion

# Authentication
JWT_SECRET=<generate-a-64-char-hex-string>
ENCRYPTION_KEY=<generate-a-64-char-hex-string>

# Ports
GATEWAY_PORT=3000
WEB_PORT=80

# CORS — set to your actual console domain
CORS_ORIGINS=https://console.yourdomain.com

# RustFS (S3 storage for Quickwit)
RUSTFS_USER=rustfs
RUSTFS_PASSWORD=<generate-a-strong-password>

# Redis
REDIS_PASSWORD=<generate-a-strong-password>

# OIDC SSO (optional)
OIDC_ISSUER_URL=https://auth.yourdomain.com
OIDC_CLIENT_ID=<client-id>
OIDC_CLIENT_SECRET=<client-secret>
OIDC_REDIRECT_URL=https://console.yourdomain.com/api/auth/oidc/callback

# Logging
RUST_LOG=info,agent_bastion=info
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
- **server** -- The AgentBastion Rust binary (gateway on port 3000, console on port 3001 internal only)
- **web** -- nginx serving the built React SPA (port 80)
- **postgres** -- PostgreSQL 18 with persistent volume
- **redis** -- Redis 8 with persistent volume
- **rustfs** -- S3-compatible storage with persistent volume
- **quickwit** -- Audit log search engine

### 3.4 Verify Health

```bash
# Gateway health
curl http://localhost:3000/health

# Console health (from within the Docker network, or via the web container)
docker compose -f deploy/docker-compose.yml exec server curl http://localhost:3001/api/health
```

### 3.5 Set Up a Reverse Proxy

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

A Helm chart is provided at `deploy/helm/agent-bastion/`.

### 4.1 Images

Pre-built images are published automatically to GitHub Container Registry on every push to `main`:

```
ghcr.io/agentbastion/agent-bastion-server:latest
ghcr.io/agentbastion/agent-bastion-server:<git-sha>

ghcr.io/agentbastion/agent-bastion-web:latest
ghcr.io/agentbastion/agent-bastion-web:<git-sha>
```

No authentication is needed — the packages are public.

### 4.2 Install with Helm

```bash
helm install agent-bastion deploy/helm/agent-bastion \
  --set secrets.jwtSecret=$(openssl rand -hex 32) \
  --set secrets.encryptionKey=$(openssl rand -hex 32) \
  --set secrets.databaseUrl="postgres://bastion:password@postgres:5432/agent_bastion" \
  --set secrets.redisUrl="redis://:password@redis:6379" \
  --set config.corsOrigins="https://console.internal.example.com"
```

To deploy a specific image tag:

```bash
helm install agent-bastion deploy/helm/agent-bastion \
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
  name: agent-bastion
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: vault-backend  # or aws-secrets-manager, etc.
    kind: SecretStore
  target:
    name: agent-bastion-secrets
  data:
    - secretKey: jwt-secret
      remoteRef:
        key: agent-bastion/jwt-secret
    - secretKey: encryption-key
      remoteRef:
        key: agent-bastion/encryption-key
    - secretKey: database-url
      remoteRef:
        key: agent-bastion/database-url
    - secretKey: redis-url
      remoteRef:
        key: agent-bastion/redis-url
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

The recommended approach is to terminate TLS at the reverse proxy layer. AgentBastion itself serves plain HTTP on ports 3000 and 3001.

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
pg_dump -h localhost -U bastion -d agent_bastion -Fc > backup_$(date +%Y%m%d).dump

# Restore
pg_restore -h localhost -U bastion -d agent_bastion -c backup_20260401.dump
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

### 6.3 Quickwit / RustFS

Quickwit stores indexed audit logs, backed by RustFS (S3-compatible storage). The Docker Compose files mount persistent volumes for both.

For disaster recovery:
- Back up the RustFS data volume, which contains the S3 bucket used by Quickwit.
- Alternatively, since audit logs are also written to PostgreSQL (`audit_logs` table), Quickwit data can be reindexed from the primary database if needed.

---

## 7. Monitoring

### 7.1 Health Endpoints

| Endpoint                  | Port | Purpose                          |
|---------------------------|------|----------------------------------|
| `GET /health`             | 3000 | Gateway health (database + Redis connectivity) |
| `GET /api/health`         | 3001 | Console health                   |

Use these endpoints for load balancer health checks, Kubernetes liveness/readiness probes, and uptime monitoring.

### 7.2 Structured Logging

AgentBastion uses `tracing` with configurable output via the `RUST_LOG` environment variable:

```bash
# Default: info level
RUST_LOG=info,agent_bastion=info

# Debug for AgentBastion crates, info for dependencies
RUST_LOG=info,agent_bastion=debug

# Trace-level for detailed request/response debugging
RUST_LOG=info,agent_bastion=trace

# JSON-formatted logs (configured in tracing-subscriber setup)
```

Logs are written to stdout and can be collected by any log aggregation system (Datadog, Loki, CloudWatch, etc.).

### 7.3 Syslog Forwarding

For environments that use centralized syslog (e.g., Splunk, Graylog), configure the `SYSLOG_ADDR` environment variable:

```bash
SYSLOG_ADDR=udp://syslog.internal:514
```

Audit log events will be forwarded to the syslog endpoint in addition to being written to PostgreSQL and Quickwit.

### 7.4 Prometheus Metrics (Future)

The codebase includes OpenTelemetry dependencies (`opentelemetry`, `opentelemetry-otlp`, `tracing-opentelemetry`), laying the groundwork for Prometheus-compatible metrics export. This will include:
- Request latency histograms
- Token throughput counters
- Active connection gauges
- Error rate counters

---

## 8. Upgrading

### 8.1 Database Migrations

Migrations are run automatically when the server starts. The `sqlx` migrate system tracks which migrations have been applied in a `_sqlx_migrations` table and only runs new ones.

**Important:** Always back up your database before upgrading to a new version that includes schema changes.

### 8.2 Rolling Updates (Kubernetes)

The server is stateless, so rolling updates work out of the box:

```bash
# Update the image tag
helm upgrade agent-bastion deploy/helm/agent-bastion \
  --set image.server.tag=0.2.0 \
  --reuse-values
```

Kubernetes will perform a rolling update, starting new pods before terminating old ones. The first new pod to start will run any pending database migrations.

**Tip:** Set `maxSurge: 1` and `maxUnavailable: 0` in your deployment strategy to ensure zero downtime during upgrades.

### 8.3 Docker Compose Updates

```bash
# Pull new images or rebuild
docker compose -f deploy/docker-compose.yml --env-file .env.production build

# Restart with new images (database migrations run on startup)
docker compose -f deploy/docker-compose.yml --env-file .env.production up -d
```

### 8.4 Breaking Changes Policy

- **Patch versions** (0.1.x): Bug fixes only. No migration changes. Safe to upgrade without review.
- **Minor versions** (0.x.0): May include new migrations that add tables or columns. Always backward-compatible. Review the changelog.
- **Major versions** (x.0.0): May include breaking API changes, destructive migrations, or configuration changes. Read the upgrade guide carefully before proceeding.

Check the project changelog or release notes for migration details before upgrading.
