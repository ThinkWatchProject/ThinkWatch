**[English](../en/configuration.md) | [中文](../zh-CN/configuration.md)**

# ThinkWatch 配置参考

本文档提供 ThinkWatch 所有配置选项的完整参考。配置通过环境变量（用于基础设施和密钥）和数据库存储的动态设置（可通过管理员 Web UI 配置）相结合进行管理。

---

## 环境变量

### 数据库

#### `DATABASE_URL`

| 属性      | 值                                                         |
| --------- | ---------------------------------------------------------- |
| 必填      | 是                                                         |
| 默认值    | —                                                          |
| 示例      | `postgres://thinkwatch:password@localhost:5432/thinkwatch` |

PostgreSQL 连接字符串。ThinkWatch 需要 PostgreSQL 15 或更高版本。

**安全说明：**
- 在生产环境中，使用 `sslmode=require` 强制加密连接：`postgres://user:pass@host:5432/db?sslmode=require`
- 避免在提交到版本控制的 URL 中嵌入密码。请使用密钥管理器。
- 数据库用户需要创建表的权限（用于迁移），或者应单独应用迁移。

---

#### `REDIS_URL`

| 属性      | 值                                       |
| --------- | ---------------------------------------- |
| 必填      | 是                                       |
| 默认值    | —                                        |
| 示例      | `redis://localhost:6379`                 |

Redis 连接字符串。用于速率限制、OIDC 状态/nonce 存储和会话管理。

**安全说明：**
- 在生产环境中，启用 Redis 认证：`redis://:yourpassword@host:6379`
- 对于启用了 TLS 的 Redis，使用 `rediss://` 协议：`rediss://:password@host:6380`

---

### 密码学

#### `JWT_SECRET`

| 属性      | 值                                                 |
| --------- | -------------------------------------------------- |
| 必填      | 是                                                 |
| 默认值    | —                                                  |
| 示例      | `a3f8c1e0b9d74...`（64 字符十六进制字符串）        |

用于 HS256 JWT 签名和验证的共享密钥。必须至少 32 个字符长，建议至少为 256 位（32 字节 / 64 个十六进制字符）以确保足够的安全性。启动时，ThinkWatch 会对此值执行熵检查，如果不满足最低长度要求将拒绝启动。

**安全说明：**
- 使用以下命令生成：`openssl rand -hex 32`
- **最少需要 32 个字符。** 更短的值将导致启动失败。
- 更改此值会使所有活跃的访问令牌和刷新令牌失效，迫使所有用户重新认证。
- 切勿在不同环境（开发/预发布/生产）之间重用此值。
- 存储在密钥管理器中，而非明文文件。

---

#### `ENCRYPTION_KEY`

| 属性      | 值                                                 |
| --------- | -------------------------------------------------- |
| 必填      | 是                                                 |
| 默认值    | —                                                  |
| 示例      | `b7e4d219f0c83...`（64 字符十六进制字符串）        |

256 位密钥（32 字节，编码为 64 个十六进制字符），用于敏感数据的 AES-256-GCM 静态加密（提供商 API 密钥、MCP 服务器认证密钥）。

**安全说明：**
- 使用以下命令生成：`openssl rand -hex 32`
- 更改此值会使所有先前加密的数据（提供商 API 密钥、MCP 认证密钥）不可读。轮换后必须重新输入这些值。
- 这是系统中最关键的密钥。此密钥泄露将暴露所有存储的提供商凭证。
- 在生产环境中存储在硬件安全模块（HSM）或密钥管理器中。

---

### 服务器

#### `SERVER_HOST`

| 属性      | 值              |
| --------- | --------------- |
| 必填      | 否              |
| 默认值    | `0.0.0.0`      |
| 示例      | `127.0.0.1`    |

服务器绑定的 IP 地址。使用 `0.0.0.0` 监听所有接口，或使用 `127.0.0.1` 仅限本地访问。

---

#### `GATEWAY_PORT`

| 属性      | 值     |
| --------- | ------ |
| 必填      | 否     |
| 默认值    | `3000` |
| 示例      | `8080` |

网关服务器的 TCP 端口（OpenAI 兼容 API 和 MCP 传输）。

---

#### `CONSOLE_PORT`

| 属性      | 值     |
| --------- | ------ |
| 必填      | 否     |
| 默认值    | `3001` |
| 示例      | `8081` |

控制台服务器的 TCP 端口（管理 API 和管理员端点）。

---

#### `CORS_ORIGINS`

| 属性      | 值                                       |
| --------- | ---------------------------------------- |
| 必填      | 否                                       |
| 默认值    | `http://localhost:5173`                  |
| 示例      | `https://console.example.com,https://admin.example.com` |

逗号分隔的允许 CORS 来源列表。每个来源必须包含协议（`http://` 或 `https://`），且不能包含尾部斜杠。

**安全说明：**
- 在生产环境中，仅限制为托管控制台前端的域名。
- 不要使用 `*`，这会完全禁用 CORS 保护。

---

### 可观测性

#### `CLICKHOUSE_URL`

| 属性      | 值                             |
| --------- | ------------------------------ |
| 必填      | 否                             |
| 默认值    | —                              |
| 示例      | `http://clickhouse:8123`       |

ClickHouse HTTP 接口的基础 URL，用于审计日志存储和搜索。如果未设置，审计日志写入 ClickHouse 将不可用（条目仍会记录到标准输出并转发至已配置的日志转发器）。

---

#### `CLICKHOUSE_DB`

| 属性      | 值                          |
| --------- | --------------------------- |
| 必填      | 否                          |
| 默认值    | `think_watch`             |
| 示例      | `think_watch_prod`        |

存储审计日志表的 ClickHouse 数据库名称。ThinkWatch 会在启动时自动创建数据库和表（如果不存在）。

---

#### `CLICKHOUSE_USER`

| 属性      | 值                          |
| --------- | --------------------------- |
| 必填      | 否                          |
| 默认值    | `default`                   |
| 示例      | `thinkwatch`              |

ClickHouse 认证用户。

---

#### `CLICKHOUSE_PASSWORD`

| 属性      | 值                          |
| --------- | --------------------------- |
| 必填      | 否                          |
| 默认值    | —                           |
| 示例      | `your-clickhouse-password`  |

ClickHouse 认证密码。

**安全说明：**
- 在生产环境中，务必为 ClickHouse 设置强密码。
- 存储在密钥管理器中，而非明文文件。

---

#### `RUST_LOG`

| 属性      | 值                                                       |
| --------- | -------------------------------------------------------- |
| 必填      | 否                                                       |
| 默认值    | `info`                                                   |
| 示例      | `thinkwatch=debug,tower_http=debug,sqlx=warn`         |

使用 [`tracing-subscriber` `EnvFilter` 语法](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) 控制日志详细程度。接受逗号分隔的 `target=level` 形式的指令。

常用配置：

| 用例                  | 值                                                 |
| --------------------- | -------------------------------------------------- |
| 生产环境              | `info` 或 `thinkwatch=info,tower_http=warn`      |
| 调试 HTTP             | `thinkwatch=debug,tower_http=debug`              |
| 调试 SQL              | `thinkwatch=debug,sqlx=debug`                    |
| 最少输出              | `warn`                                             |

---

### OIDC / SSO

要启用 SSO，必须同时设置所有四个 OIDC 变量。如果缺少任何一个，OIDC 将被禁用，`GET /api/auth/sso/authorize` 将返回 500。

#### `OIDC_ISSUER_URL`

| 属性      | 值                                                               |
| --------- | ---------------------------------------------------------------- |
| 必填      | 否（如需 SSO 则必填）                                           |
| 默认值    | —                                                                |
| 示例      | `https://login.microsoftonline.com/tenant-id/v2.0`              |

OIDC 发行者 URL。ThinkWatch 在启动时从此 URL 获取 `.well-known/openid-configuration` 文档以发现端点。

---

#### `OIDC_CLIENT_ID`

| 属性      | 值                           |
| --------- | ---------------------------- |
| 必填      | 否（如需 SSO 则必填）       |
| 默认值    | —                            |
| 示例      | `abcdef12-3456-7890-abcd-ef1234567890` |

在 OIDC 身份提供商注册的客户端 ID。

---

#### `OIDC_CLIENT_SECRET`

| 属性      | 值                                 |
| --------- | ---------------------------------- |
| 必填      | 否（如需 SSO 则必填）             |
| 默认值    | —                                  |
| 示例      | `your-client-secret-value`         |

OIDC 应用的客户端密钥。

**安全说明：**
- 这是敏感凭证。请存储在密钥管理器中。
- 根据身份提供商的建议定期轮换。

---

#### `OIDC_REDIRECT_URL`

| 属性      | 值                                                             |
| --------- | -------------------------------------------------------------- |
| 必填      | 否（如需 SSO 则必填）                                         |
| 默认值    | —                                                              |
| 示例      | `https://console.example.com/api/auth/sso/callback`           |

OIDC 提供商在认证后重定向到的完整 URL。此 URL 必须与在身份提供商配置中注册的重定向 URI 完全匹配，并且必须可被用户浏览器访问。

---

## 提供商配置

提供商通过管理员 Web UI（管理 > 提供商）或 `POST /api/admin/providers` API 进行配置。每种提供商类型有特定的配置要求：

### OpenAI

| 字段            | 值                                 |
| --------------- | ---------------------------------- |
| `provider_type` | `openai`                           |
| `base_url`      | `https://api.openai.com`           |
| `api_key`       | `sk-...`（OpenAI API 密钥）        |

前缀为 `gpt-`、`o1-`、`o3-`、`o4-` 的模型会自动路由到此提供商。

### Anthropic

| 字段            | 值                                 |
| --------------- | ---------------------------------- |
| `provider_type` | `anthropic`                        |
| `base_url`      | `https://api.anthropic.com`        |
| `api_key`       | `sk-ant-...`（Anthropic API 密钥） |

前缀为 `claude-` 的模型会自动路由到此提供商。

### Google Gemini

| 字段            | 值                                             |
| --------------- | ---------------------------------------------- |
| `provider_type` | `google`                                       |
| `base_url`      | `https://generativelanguage.googleapis.com`    |
| `api_key`       | Google AI API 密钥                             |

前缀为 `gemini-` 的模型会自动路由到此提供商。

### Azure OpenAI

| 字段            | 值                                             |
| --------------- | ---------------------------------------------- |
| `provider_type` | `azure`                                        |
| `base_url`      | `https://{resource}.openai.azure.com`          |
| `api_key`       | Azure API 密钥                                 |
| `config_json`   | `{"api_version": "2024-12-01-preview"}`        |

Azure OpenAI 使用 `api-key` 请求头（非 Bearer 令牌）进行认证。`api_version` 作为查询参数传递。Azure 不支持自动模型路由——必须通过管理 > 模型页面显式注册模型。

### AWS Bedrock

| 字段            | 值                                             |
| --------------- | ---------------------------------------------- |
| `provider_type` | `bedrock`                                      |
| `base_url`      | AWS 区域（如 `us-east-1`）                     |
| `api_key`       | `ACCESS_KEY_ID:SECRET_ACCESS_KEY`              |

AWS Bedrock 通过官方 `aws-sigv4` Rust crate 使用 SigV4 请求签名。`base_url` 字段指定 AWS 区域，`api_key` 字段包含格式为 `ACCESS_KEY_ID:SECRET_ACCESS_KEY` 的 IAM 凭证。流式传输使用原生 Bedrock 二进制 event-stream 协议（Converse API）。与 Azure 类似，Bedrock 不支持自动模型路由——必须通过管理 > 模型页面显式注册模型。

### 自定义（OpenAI 兼容）

| 字段            | 值                                             |
| --------------- | ---------------------------------------------- |
| `provider_type` | `custom`                                       |
| `base_url`      | 任何 OpenAI 兼容端点 URL                       |
| `api_key`       | 端点的 Bearer 令牌                             |

用于自托管模型（vLLM、Ollama、LiteLLM 等）或任何实现了 OpenAI API 格式的第三方服务。

### 提供商自动加载

启动时，ThinkWatch 从数据库加载所有活跃提供商并注册到模型路由器。模型路由基于：

1. **前缀匹配**（OpenAI、Anthropic、Google）：匹配提供商默认前缀的模型会自动路由。
2. **显式注册**（Azure、Bedrock、自定义）：模型必须通过管理 > 模型页面注册，并指定提供商分配。

---

## 配置模式

### 开发环境与生产环境对比

| 设置              | 开发环境                                     | 生产环境                                    |
| ----------------- | -------------------------------------------- | ------------------------------------------- |
| `DATABASE_URL`    | 本地 PostgreSQL，不使用 SSL                  | 托管 PostgreSQL，使用 `sslmode=require`     |
| `REDIS_URL`       | 本地 Redis，无认证                           | Redis，使用 `requirepass` 或托管 Redis      |
| `JWT_SECRET`      | 任意稳定值，方便开发                         | 密码学随机 256 位密钥                       |
| `ENCRYPTION_KEY`  | 任意稳定的 32 字节十六进制值，方便开发       | 密码学随机，存储在 HSM 中                   |
| `CORS_ORIGINS`    | `http://localhost:5173`                      | `https://console.yourdomain.com`            |
| `RUST_LOG`        | `thinkwatch=debug,tower_http=debug`        | `info` 或 `thinkwatch=info`               |
| OIDC 变量         | _（除非测试 SSO 否则未设置）_                | 完整配置                                    |

### 环境文件

ThinkWatch 在项目根目录使用单一 `.env` 文件：

- **开发环境**：`cargo run` 通过 `dotenvy` 自动加载，`docker compose` 通过 Makefile 中的 `--env-file .env` 加载
- **生产环境**：使用项目根目录的 `.env.production`，通过 `make deploy` 加载

`deploy/` 目录下没有单独的 `.env` 文件。复制 `.env.example` 开始：

```bash
cp .env.example .env
# 编辑 .env 填入实际值，或运行：
# deploy/generate-secrets.sh
```

### 使用 .env 文件

本地开发时，在项目根目录创建 `.env` 文件：

```bash
# .env (DO NOT commit this file)
DATABASE_URL=postgres://thinkwatch:devpass@localhost:5432/thinkwatch
REDIS_URL=redis://localhost:6379
JWT_SECRET=dev-only-jwt-secret-do-not-use-in-production-000000
ENCRYPTION_KEY=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
CORS_ORIGINS=http://localhost:5173
RUST_LOG=thinkwatch=debug,tower_http=debug
```

ThinkWatch 通过 `dotenvy` crate 自动加载 `.env` 文件。`.env` 文件应列在 `.gitignore` 中。

### Docker / Docker Compose

通过 `docker-compose.yml` 传递环境变量：

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

使用 `.env` 文件或 shell 导出来提供 `${DB_PASSWORD}`、`${JWT_SECRET}` 和 `${ENCRYPTION_KEY}`，避免在 compose 文件中硬编码。

### Kubernetes Secrets

为敏感值创建 Kubernetes Secret：

```bash
kubectl create secret generic thinkwatch-secrets \
  --from-literal=JWT_SECRET="$(openssl rand -hex 32)" \
  --from-literal=ENCRYPTION_KEY="$(openssl rand -hex 32)" \
  --from-literal=DATABASE_URL="postgres://user:pass@pg-host:5432/thinkwatch?sslmode=require" \
  --from-literal=REDIS_URL="rediss://:password@redis-host:6380" \
  --from-literal=OIDC_CLIENT_SECRET="your-oidc-secret"
```

在 Deployment 中引用该 Secret：

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

### 生成安全随机密钥

使用 `openssl` 生成密码学安全的随机值：

```bash
# Generate a 256-bit key (for JWT_SECRET or ENCRYPTION_KEY)
openssl rand -hex 32

# Generate a 128-bit key (for less critical uses)
openssl rand -hex 16

# Generate a base64-encoded key (alternative format)
openssl rand -base64 32
```

`JWT_SECRET` 和 `ENCRYPTION_KEY` 都期望 64 字符的十六进制字符串（代表 32 字节 / 256 位）。

---

## 动态配置（Web UI）

许多之前需要通过环境变量配置的设置现在存储在数据库中，可通过管理员 Web UI 在运行时配置。这些设置也可通过 `GET/PATCH /api/admin/settings` API 端点管理。

动态设置的更改立即生效，无需重启服务器。

### 认证与 JWT

| 设置                         | 默认值   | 描述                                         |
| ---------------------------- | -------- | -------------------------------------------- |
| `jwt_access_ttl_seconds`     | `900`    | 访问令牌有效期（秒，默认 15 分钟）          |
| `jwt_refresh_ttl_seconds`    | `604800` | 刷新令牌有效期（秒，默认 7 天）             |

### 缓存

| 设置                 | 默认值  | 描述                             |
| -------------------- | ------- | -------------------------------- |
| `cache_ttl_seconds`  | `300`   | 各种缓存的默认 TTL              |

### 安全

| 设置                         | 默认值  | 描述                                         |
| ---------------------------- | ------- | -------------------------------------------- |
| `signature_drift_seconds`    | `300`   | 签名请求允许的最大时钟偏差                   |
| `nonce_ttl_seconds`          | `300`   | 用于重放保护的 nonce 值 TTL                  |
| `content_filter_patterns`    | `[]`    | 内容过滤模式（最多 500 个；严重级别枚举：`low`、`medium`、`high`、`critical`） |
| `pii_patterns`               | `[]`    | PII 检测正则表达式模式（最多 100 个；每个最长 1000 字符；保存时验证） |

### 预算

| 设置                         | 默认值  | 描述                                         |
| ---------------------------- | ------- | -------------------------------------------- |
| `budget_warning_threshold`   | `0.8`   | 触发警告的预算使用率                         |
| `budget_critical_threshold`  | `0.95`  | 触发严重告警的预算使用率                     |

### API 密钥策略

| 设置                           | 默认值      | 描述                                   |
| ------------------------------ | ----------- | -------------------------------------- |
| `api_key_max_expiry_days`      | `365`       | 新 API 密钥允许的最大过期天数          |
| `api_key_default_rate_limit_rpm` | `60`      | 新 API 密钥的默认 RPM 限制            |

### 通用

| 设置                   | 默认值           | 描述                                     |
| ---------------------- | ---------------- | ---------------------------------------- |
| `data_retention_days`  | `30`             | 软删除记录在清除前保留的天数             |
| `site_name`            | `ThinkWatch`   | Web UI 中显示的站点名称                  |

---

## 启动验证

ThinkWatch 在启动时验证所有配置和依赖，如果出现以下情况将拒绝启动：

- `DATABASE_URL` 缺失或数据库不可达
- `REDIS_URL` 缺失或 Redis 不可达
- `JWT_SECRET` 缺失或少于 32 个字符
- `JWT_SECRET` 未通过熵检查（例如全部相同字符）
- `ENCRYPTION_KEY` 缺失或不是有效的 64 字符十六进制字符串
- `OIDC_*` 变量部分配置（必须全部设置或全部不设置）

此外，服务器在启动时执行依赖健康检查：

- **PostgreSQL：** 验证连接并运行测试查询
- **Redis：** 通过 PING 命令验证连接
- **ClickHouse：** 如已配置，验证连接和表是否存在（非阻塞 -- 不可用时记录警告）

如果服务器启动失败，请检查应用日志。缺失或无效的配置将以清晰的错误消息报告。
