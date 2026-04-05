**[English](../en/deployment-guide.md) | [中文](../zh-CN/deployment-guide.md)**

# AgentBastion 部署指南

## 1. 前提条件

### 硬件要求

| 资源   | 最低要求         | 推荐配置             |
|--------|------------------|----------------------|
| CPU    | 2 核             | 4+ 核                |
| RAM    | 4 GB             | 8+ GB                |
| 磁盘   | 20 GB SSD        | 50+ GB SSD           |
| 网络   | 100 Mbps         | 1 Gbps               |

服务器进程本身非常轻量（Rust 二进制文件，典型 RSS 约 50 MB）。大部分资源消耗来自 PostgreSQL、Redis 和 ClickHouse。

### 软件要求

**开发环境：**

| 软件       | 版本    | 用途                             |
|------------|---------|----------------------------------|
| Rust       | Edition 2024（见 `rust-toolchain.toml`） | 构建服务器         |
| Node.js    | 20+     | 构建 Web UI                      |
| pnpm       | 9+      | Web UI 包管理器                  |
| Docker     | 24+     | 运行基础设施服务                 |
| Docker Compose | v2+ | 编排开发服务                     |

**生产环境（仅 Docker）：**

| 软件            | 版本    |
|-----------------|---------|
| Docker          | 24+     |
| Docker Compose  | v2+     |

---

## 2. 开发环境搭建

### 2.1 克隆并安装依赖

```bash
git clone <repository-url> AgentBastion
cd AgentBastion

# Install Rust toolchain (reads from rust-toolchain.toml)
rustup show

# Install web UI dependencies
cd web && pnpm install && cd ..
```

### 2.2 启动基础设施服务

```bash
docker compose -f deploy/docker-compose.dev.yml up -d
```

这将启动：
- **PostgreSQL**，端口 5432（用户：`postgres`，密码：`postgres`，数据库：`agent_bastion`）
- **Redis**，端口 6379
- **ClickHouse**（审计日志存储），端口 8123 (HTTP) / 9000（原生 TCP）
- **Zitadel**（OIDC SSO），端口 8080

等待所有服务变为健康状态：

```bash
docker compose -f deploy/docker-compose.dev.yml ps
```

### 2.3 启动服务器

```bash
# From the project root
cargo run
```

服务器从环境变量读取配置（带有合理的开发默认值）。它将：
1. 连接到 PostgreSQL 并自动运行迁移。
2. 连接到 Redis。
3. 在 `http://localhost:3000` 启动网关。
4. 在 `http://localhost:3001` 启动控制台。

### 2.4 启动 Web UI（开发模式）

```bash
cd web
pnpm dev
```

Vite 开发服务器在 `http://localhost:5173` 启动，并将 API 请求代理到 `localhost:3001` 的控制台服务器。

### 2.5 创建首个管理员用户（设置向导）

在全新数据库上，AgentBastion 提供了可通过 Web UI 或 API 访问的设置向导。设置向导可在一步中创建首个管理员用户并可选配置首个 AI 提供商。

**通过 Web UI：**

在浏览器中打开 `http://localhost:5173`。如果系统尚未初始化，UI 将自动显示设置向导。

**通过 API：**

检查是否需要设置：

```bash
curl http://localhost:3001/api/setup/status
# 返回：{"initialized": false, "needs_setup": true}
```

初始化系统：

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

`provider` 字段是可选的。您可以稍后通过管理员 UI 添加提供商。

> **注意：** 设置端点有速率限制（每分钟 5 次请求），并在首个管理员用户创建后禁用。详情请参阅安全文档。

**传统方法（手动）：**

或者，通过 `POST /api/auth/register` 注册，然后直接在 PostgreSQL 中分配 `super_admin` 角色：

```sql
INSERT INTO user_roles (user_id, role_id, scope)
SELECT u.id, r.id, 'global'
FROM users u, roles r
WHERE u.email = 'admin@example.com' AND r.name = 'super_admin';
```

后续用户可以由超级管理员通过 Web UI 提升为管理员角色。

### 2.6 配置 Zitadel SSO（开发环境）

开发 compose 文件会在 `http://localhost:8080` 启动一个 Zitadel 实例。

1. 使用用户名 `admin` 和密码 `Admin1234!` 登录 Zitadel（`http://localhost:8080`）。
2. 创建一个新项目和一个类型为 "Web" 的 OIDC 应用，配置如下：
   - 重定向 URI：`http://localhost:3001/api/auth/oidc/callback`
   - 注销后重定向：`http://localhost:5173`
3. 复制客户端 ID 和客户端密钥。
4. 在启动服务器前设置环境变量：

```bash
export OIDC_ISSUER_URL=http://localhost:8080
export OIDC_CLIENT_ID=<your-client-id>
export OIDC_CLIENT_SECRET=<your-client-secret>
export OIDC_REDIRECT_URL=http://localhost:3001/api/auth/oidc/callback
```

或者将它们添加到项目根目录的 `.env` 文件中（由 `dotenvy` 自动加载）。

---

## 3. Docker Compose 生产部署

### 3.1 创建生产环境文件

创建一个 `.env.production` 文件，填入真实密钥。**切勿将此文件提交到版本控制系统。**

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
RUST_LOG=info,agent_bastion=info
```

### 3.2 生成安全密钥

```bash
# JWT_SECRET (64 hex characters = 256 bits)
openssl rand -hex 32

# ENCRYPTION_KEY (64 hex characters = 256 bits, used for AES-256-GCM)
openssl rand -hex 32

# Database and Redis passwords
openssl rand -base64 24
```

### 3.3 部署

从 GitHub Container Registry 拉取预构建镜像并启动所有服务：

```bash
docker compose -f deploy/docker-compose.yml --env-file .env.production pull
docker compose -f deploy/docker-compose.yml --env-file .env.production up -d
```

如需固定特定版本而非 `latest`：

```bash
IMAGE_TAG=<git-sha> docker compose -f deploy/docker-compose.yml --env-file .env.production up -d
```

这将启动：
- **server** —— AgentBastion Rust 二进制文件（网关端口 3000，控制台端口 3001 仅内部访问）
- **web** —— nginx 提供构建好的 React SPA（端口 80）
- **postgres** —— PostgreSQL 18，带持久化卷
- **redis** —— Redis 8，带持久化卷
- **clickhouse** —— ClickHouse 列式数据库，用于审计日志

### 3.4 验证健康状态

```bash
# Gateway health
curl http://localhost:3000/health

# Console health (from within the Docker network, or via the web container)
docker compose -f deploy/docker-compose.yml exec server curl http://localhost:3001/api/health
```

### 3.5 ClickHouse 配置

ClickHouse 以列式格式存储审计日志，针对分析查询进行优化。在 `.env.production` 中配置连接：

```bash
# ClickHouse 连接
CLICKHOUSE_URL=http://clickhouse:8123
CLICKHOUSE_DB=agent_bastion
CLICKHOUSE_USER=default
CLICKHOUSE_PASSWORD=<your-clickhouse-password>
```

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `CLICKHOUSE_URL` | `http://clickhouse:8123` | ClickHouse HTTP 接口 URL。 |
| `CLICKHOUSE_DB` | `agent_bastion` | 审计日志表所在的数据库名称。 |
| `CLICKHOUSE_USER` | `default` | ClickHouse 认证用户。 |
| `CLICKHOUSE_PASSWORD` | — | ClickHouse 密码。 |

AgentBastion 在启动时会自动创建所需的表（如果不存在）。审计日志保留策略可通过管理员 Web UI 中的 `data_retention_days` 设置进行配置。

### 3.6 设置反向代理

在生产环境中，应在 Docker 服务前放置反向代理来处理 TLS 终止。只有网关端口（3000）和 Web UI 端口（80）需要对终端用户可达。

**重要：** 控制台端口（3001）不应暴露给公共互联网。仅通过内部网络、VPN 或通过 Web 容器的 nginx（将 `/api/*` 反向代理到端口 3001）访问。

#### 示例：nginx 反向代理

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

#### 示例：traefik（通过 Docker 标签）

在 compose 覆盖文件中为 `server` 和 `web` 服务添加标签，以配置 Traefik 路由和使用 Let's Encrypt 自动配置 TLS。

---

## 4. Kubernetes 部署

在 `deploy/helm/agent-bastion/` 提供了 Helm chart。

### 4.1 镜像

预构建镜像在每次推送到 `main` 分支时自动发布到 GitHub Container Registry：

```
ghcr.io/agentbastion/agent-bastion-server:latest
ghcr.io/agentbastion/agent-bastion-server:<git-sha>

ghcr.io/agentbastion/agent-bastion-web:latest
ghcr.io/agentbastion/agent-bastion-web:<git-sha>
```

镜像为公开可访问，无需认证即可拉取。

### 4.2 使用 Helm 安装

```bash
helm install agent-bastion deploy/helm/agent-bastion \
  --set secrets.jwtSecret=$(openssl rand -hex 32) \
  --set secrets.encryptionKey=$(openssl rand -hex 32) \
  --set secrets.databaseUrl="postgres://bastion:password@postgres:5432/agent_bastion" \
  --set secrets.redisUrl="redis://:password@redis:6379" \
  --set config.corsOrigins="https://console.internal.example.com"
```

如需部署特定版本：

```bash
helm install agent-bastion deploy/helm/agent-bastion \
  --set image.server.tag=<git-sha> \
  --set image.web.tag=<git-sha> \
  ...
```

### 4.3 配置 Ingress

Helm chart 支持双主机 Ingress。在 `values.yaml` 中或通过 `--set` 启用和配置：

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

对于控制台 Ingress，使用内部 Ingress 类或添加注解以限制访问：

```yaml
ingress:
  console:
    annotations:
      nginx.ingress.kubernetes.io/whitelist-source-range: "10.0.0.0/8,172.16.0.0/12"
```

### 4.4 外部密钥管理

在生产环境中，使用 External Secrets Operator 而非通过 `--set` 传递密钥：

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

### 4.5 水平 Pod 自动伸缩

在 `values.yaml` 中启用 HPA：

```yaml
autoscaling:
  enabled: true
  minReplicas: 2
  maxReplicas: 10
  targetCPUUtilizationPercentage: 70
```

服务器是无状态的（所有状态存储在 PostgreSQL 和 Redis 中），因此可以无障碍地水平扩展。

---

## 5. SSL/TLS

### 使用 Let's Encrypt 配合反向代理

推荐的方式是在反向代理层终止 TLS。AgentBastion 本身在端口 3000 和 3001 上提供纯 HTTP 服务。

**使用 certbot（独立 nginx）：**

```bash
certbot certonly --nginx -d gateway.yourdomain.com -d console.yourdomain.com
```

**使用 Kubernetes cert-manager：**

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

然后在 Ingress 注解中引用该 issuer：

```yaml
annotations:
  cert-manager.io/cluster-issuer: letsencrypt-prod
```

### 为 HTTPS 配置 CORS

当控制台通过 HTTPS 提供服务时，更新 `CORS_ORIGINS` 环境变量：

```bash
CORS_ORIGINS=https://console.yourdomain.com
```

如果需要，可以用逗号分隔多个来源。

---

## 6. 备份与恢复

### 6.1 PostgreSQL

PostgreSQL 包含所有关键的应用状态：用户、API 密钥、提供商配置、用量记录和审计日志。

**定时 pg_dump（简单方式）：**

```bash
# Daily backup
pg_dump -h localhost -U bastion -d agent_bastion -Fc > backup_$(date +%Y%m%d).dump

# Restore
pg_restore -h localhost -U bastion -d agent_bastion -c backup_20260401.dump
```

**WAL 归档（时间点恢复）：**

对于生产部署，配置 PostgreSQL WAL 归档到 S3 存储桶或网络存储。这允许恢复到任意时间点，而不仅仅是最后一次转储。使用 `pgBackRest` 或 `wal-g` 等工具进行自动化 WAL 管理。

**托管数据库：** 如果使用托管 PostgreSQL 服务（AWS RDS、Google Cloud SQL 等），自动备份和时间点恢复通常已包含在内。

### 6.2 Redis

Redis 存储临时数据（速率限制计数器、会话状态、OIDC 流程状态）。丢失 Redis 数据不会造成灾难性后果 —— 速率限制会重置，用户需要重新登录。

不过，为了在重启后保留速率限制状态，建议启用 Redis 持久化：

```
# redis.conf
appendonly yes
appendfsync everysec
```

Docker Compose 文件默认为 Redis 数据挂载持久化卷。

### 6.3 ClickHouse

ClickHouse 以列式表存储审计日志。Docker Compose 文件为 ClickHouse 数据挂载了持久化卷。

灾难恢复：
- 备份 ClickHouse 数据卷，或使用 `clickhouse-client` 导出数据。
- 由于审计日志也写入 PostgreSQL（`audit_logs` 表），如有需要，ClickHouse 数据可以从主数据库重新插入。

---

## 7. 监控

### 7.1 健康检查端点

| 端点                      | 端口 | 用途                                         |
|---------------------------|------|----------------------------------------------|
| `GET /health`             | 3000 | 网关基本健康检查                             |
| `GET /health/live`        | 3000 | 存活探针（进程是否存活？）                   |
| `GET /health/ready`       | 3000 | 就绪探针（PG 或 Redis 不可用时返回 503）     |
| `GET /api/health`         | 3001 | 控制台详细健康检查（延迟、连接池、运行时间） |
| `GET /metrics`            | 3000 | Prometheus 指标（无需认证）                  |

**Kubernetes 推荐的探针配置：**

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

使用 `/health/live` 作为存活探针（如果进程卡住则重启 Pod），使用 `/health/ready` 作为就绪探针（如果依赖不可用则从负载均衡器中移除 Pod）。

### 7.2 结构化日志

AgentBastion 使用 `tracing`，通过 `RUST_LOG` 环境变量配置可选的输出：

```bash
# Default: info level
RUST_LOG=info,agent_bastion=info

# Debug for AgentBastion crates, info for dependencies
RUST_LOG=info,agent_bastion=debug

# Trace-level for detailed request/response debugging
RUST_LOG=info,agent_bastion=trace

# JSON-formatted logs (configured in tracing-subscriber setup)
```

日志写入标准输出，可由任何日志聚合系统收集（Datadog、Loki、CloudWatch 等）。

### 7.3 审计日志转发

AgentBastion 支持将审计事件实时转发至外部系统，支持四种通道：

| 通道 | 协议 | 适用场景 |
|------|------|----------|
| UDP Syslog | RFC 5424 | Splunk、Graylog 等传统 SIEM |
| TCP Syslog | RFC 5424 | 需要可靠传输的 SIEM 环境 |
| Kafka | REST Proxy (JSON) | 数据湖、流处理管道 |
| HTTP Webhook | POST JSON | 自定义告警、Slack/Teams 集成 |

通过管理控制台 **Admin > Log Forwarding** 配置，支持：
- 多通道并行投递
- 每通道独立启停
- 连接测试与统计监控
- 自动重试与错误计数

审计事件始终写入 PostgreSQL 和 ClickHouse（如已配置），转发通道为附加投递渠道。

### 7.4 Prometheus 指标

AgentBastion 在网关端口（3000）的 `GET /metrics` 暴露 Prometheus 兼容的指标。此端点无需认证，以便标准 Prometheus 抓取。

可用指标包括：
- **请求延迟直方图**（`http_request_duration_seconds`）-- 按端点、方法和状态码分组
- **Token 吞吐量计数器**（`llm_tokens_total`）-- 按模型分组的提示词和补全 Token
- **活跃连接数仪表**（`http_active_connections`）-- 当前打开的连接数
- **错误率计数器**（`http_requests_errors_total`）-- 按端点和错误类型分组

**Prometheus 抓取配置：**

```yaml
scrape_configs:
  - job_name: 'agentbastion'
    scrape_interval: 15s
    static_configs:
      - targets: ['agentbastion:3000']
    metrics_path: /metrics
```

> **安全：** `/metrics` 端点无需认证。在生产环境中，请通过网络策略或反向代理限制访问，以防止向不受信任的网络暴露内部指标。

### 7.5 后台任务

AgentBastion 运行多个自动执行的后台任务：

| 任务                          | 间隔     | 描述                                                 |
|-------------------------------|----------|------------------------------------------------------|
| API 密钥生命周期管理          | 定期     | 禁用超过不活跃超时的密钥                             |
| 数据保留清除                  | 每日     | 永久删除超过 `data_retention_days` 的软删除记录      |
| 密钥过期通知                  | 每日     | 为 7 天内即将过期的密钥记录警告                      |

这些任务在服务器进程内运行，不需要外部调度器（如 cron）。服务器启动时会自动激活。

---

## 8. 客户端配置指南

AgentBastion 在 Web 控制台中内置了**配置指南**页面，位于 `/gateway/guide`。该页面提供了将各种 AI 客户端工具连接到网关的一键复制配置说明，并自动检测网关 URL。

### 支持的客户端工具

配置指南提供了以下工具的现成配置说明：

- **Claude Code** -- 将 `ANTHROPIC_BASE_URL` 设置为网关的 `/v1` 端点（使用 `/v1/messages` 的 Anthropic Messages API）
- **Cursor** -- 在 Cursor 设置中配置 OpenAI 兼容端点（使用 `/v1/chat/completions`）
- **Continue** -- 在 `~/.continue/config.json` 中配置提供商（使用 `/v1/chat/completions`）
- **Cline** -- 在 Cline 设置中配置 API 基础 URL（使用 `/v1/chat/completions`）
- **OpenAI SDK** -- 设置 `OPENAI_BASE_URL` 和 `OPENAI_API_KEY` 环境变量（使用 `/v1/chat/completions` 或 `/v1/responses`）
- **Anthropic SDK** -- 设置 `ANTHROPIC_BASE_URL` 和 `ANTHROPIC_API_KEY` 环境变量（使用 `/v1/messages`）
- **cURL** -- 三种 API 格式的示例命令

### 网关 API 端点

网关（端口 3000）在单一端口上提供三种 API 格式：

| 端点                        | 格式                    | 典型客户端                                   |
| --------------------------- | ----------------------- | -------------------------------------------- |
| `POST /v1/chat/completions` | OpenAI Chat Completions | Cursor、Continue、Cline、OpenAI SDK          |
| `POST /v1/messages`         | Anthropic Messages API  | Claude Code、Anthropic SDK                   |
| `POST /v1/responses`        | OpenAI Responses API    | OpenAI SDK（2025 格式）                      |
| `GET /v1/models`            | OpenAI Models 列表      | 所有客户端                                   |

所有端点通过 `Authorization: Bearer` 请求头接受 `ab-` API 密钥。

---

## 9. 升级

### 9.1 数据库迁移

迁移在服务器启动时自动运行。`sqlx` 迁移系统在 `_sqlx_migrations` 表中跟踪已应用的迁移，仅运行新的迁移。

**重要：** 升级到包含 Schema 变更的新版本前，务必备份数据库。

### 9.2 滚动更新（Kubernetes）

服务器是无状态的，因此滚动更新可以直接使用：

```bash
# Update the image tag
helm upgrade agent-bastion deploy/helm/agent-bastion \
  --set image.server.tag=0.2.0 \
  --reuse-values
```

Kubernetes 将执行滚动更新，在终止旧 Pod 之前启动新 Pod。第一个启动的新 Pod 将运行所有待处理的数据库迁移。

**提示：** 在部署策略中设置 `maxSurge: 1` 和 `maxUnavailable: 0`，以确保升级期间零停机。

### 9.3 Docker Compose 更新

```bash
# Pull new images or rebuild
docker compose -f deploy/docker-compose.yml --env-file .env.production build

# Restart with new images (database migrations run on startup)
docker compose -f deploy/docker-compose.yml --env-file .env.production up -d
```

### 9.4 破坏性变更策略

- **补丁版本**（0.1.x）：仅修复 Bug。无迁移变更。可安全升级，无需审查。
- **次要版本**（0.x.0）：可能包含新增表或列的迁移。始终向后兼容。请查阅变更日志。
- **主要版本**（x.0.0）：可能包含破坏性 API 变更、破坏性迁移或配置变更。升级前请仔细阅读升级指南。

升级前请查看项目变更日志或发布说明以了解迁移详情。
