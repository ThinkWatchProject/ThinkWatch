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

**企业级 AI 堡垒机。** 在一个统一控制面上，对组织内每一次 AI API 调用和 MCP 工具调用进行安全管控、审计追踪和治理。

如同 SSH 堡垒机是所有服务器访问的唯一入口，ThinkWatch 是所有 AI 访问的唯一入口。每一次模型请求、每一次工具调用、每一个 Token —— 全部经过认证、授权、限流、日志记录和费用核算。

```
                    ┌──────────────────────────────────────┐
 Claude Code ──────>│                                      │──> OpenAI
 Cursor ───────────>│    Gateway  :3000                    │──> Anthropic
 自定义 Agent ─────>│    AI API + MCP 统一代理              │──> Google Gemini
 CI/CD 流水线 ─────>│                                      │──> Azure OpenAI / AWS Bedrock
                    └──────────────────────────────────────┘
                    ┌──────────────────────────────────────┐
 管理员浏览器 ─────>│    Console  :3001                    │
                    │    管理界面 + 管理 API                │
                    └──────────────────────────────────────┘
```

## 为什么需要 ThinkWatch？

随着 AI Agent 在研发团队中的普及，企业面临日益严峻的治理挑战：

- **API Key 散落各处** — 硬编码在 `.env`、在 Slack 里共享、从不轮换
- **零可见性** — 谁在用哪个模型？消耗了多少 Token？花了多少钱？
- **无访问控制** — 每个开发者都能直接访问所有模型和 MCP 工具
- **合规缺口** — AI 辅助代码生成和数据访问没有审计轨迹
- **费用失控** — 月底的 AI 账单没人能解释或归因

ThinkWatch 一次部署，全部解决。

## 核心功能

### AI API 网关
- **多格式 API 代理** — 在同一端口原生支持 OpenAI Chat Completions (`/v1/chat/completions`)、Anthropic Messages (`/v1/messages`) 和 OpenAI Responses (`/v1/responses`) API；可直接替换 Cursor、Continue、Cline、Claude Code 以及 OpenAI/Anthropic SDK
- **多 Provider 路由** — OpenAI、Anthropic、Google Gemini、Azure OpenAI、AWS Bedrock 或任何 OpenAI 兼容端点
- **自动格式转换** — Anthropic Messages API、Google Gemini、Azure OpenAI、AWS Bedrock Converse API 等，统一在同一接口之后
- **Provider 自动加载** — 启动时从数据库加载所有活跃 Provider 并注册到模型路由器；默认模型前缀（`gpt-`/`o1-`/`o3-`/`o4-` 对应 OpenAI，`claude-` 对应 Anthropic，`gemini-` 对应 Google）自动路由；Azure 和 Bedrock 需要显式注册模型
- **SSE 流式透传** — 零开销转发，实时 Token 计数
- **虚拟 API Key** — 按团队、项目、开发者签发 `ab-` 前缀的作用域密钥，一键吊销
- **API Key 生命周期管理** — 自动轮换并支持宽限期、按 Key 的闲置超时、到期预警、后台策略执行
- **滑动窗口限流** — 基于 Redis 的 RPM/TPM 限制，按 Key 或按用户
- **断路器** — 三态（Closed/Open/HalfOpen）断路器，可配置故障阈值和恢复周期
- **指数退避重试** — 可配置的重试策略，带抖动，适用于网络错误和上游限流
- **实时费用追踪** — 按模型计费，预算告警，团队费用归因

### MCP 网关
- **中心化工具代理** — 一个 MCP 端点聚合所有上游服务器的工具
- **命名空间隔离** — `github__create_issue`、`postgres__query` —— 工具名永不冲突
- **工具级 RBAC** — 精确控制哪些用户或角色可以调用哪些工具
- **连接池和健康监控** — 自动重连，后台健康检查
- **完整审计轨迹** — 每次工具调用记录用户、参数和响应

### 安全与合规
- **双端口架构** — Gateway (面向公网) 和 Console (仅限内网) 分端口部署
- **五级 RBAC** — 超级管理员、管理员、团队经理、开发者、观察者
- **SSO/OIDC** — 对接 Zitadel、Okta、Azure AD 或任何 OIDC 提供商
- **AES-256-GCM 加密** — Provider API Key 和密钥加密存储
- **SHA-256 密钥哈希** — 虚拟 API Key 仅存哈希，明文仅显示一次
- **Content Security Policy** — Console 端口的 CSP 头，防止 XSS 和注入攻击
- **JWT 熵值检测** — 启动时校验 JWT Secret 最少 32 字符并进行熵值验证
- **启动依赖验证** — 启动时校验 PostgreSQL、Redis 和加密密钥是否可用，并输出清晰的错误信息
- **安全 HTTP 头** — X-Content-Type-Options、X-Frame-Options、CORS 白名单、请求超时
- **软删除** — 用户、Provider、API Key 使用软删除（`deleted_at` 列），30 天后自动清理
- **密码复杂度** — 最少 8 字符，必须包含大写字母、小写字母和数字
- **会话 IP 绑定** — 签名密钥在登录时绑定客户端 IP，来自不同 IP 的请求将被拒绝
- **ClickHouse 查询参数化** — 所有日志查询使用参数绑定，防止注入攻击
- **LIKE 通配符转义** — 用户搜索输入自动转义，防止日志查询中的模式注入
- **Distroless 容器** — 生产环境最小攻击面 (2MB 运行镜像，无 shell)

### 运维与配置
- **动态配置** — 大部分配置存储在数据库（`system_settings` 表），可通过 Web UI（管理 > 设置，7 个分类标签页）配置
- **首次运行向导** — 引导式 `/setup` 向导，创建超级管理员账户、配置站点信息，可选添加首个 Provider 和 API Key
- **配置指南** — Web 控制台内置 `/gateway/guide` 页面，提供 Claude Code、Cursor、Continue、Cline、OpenAI SDK、Anthropic SDK 和 cURL 的一键复制配置说明；自动检测网关 URL
- **多实例同步** — 配置变更通过 Redis Pub/Sub 在多个实例间同步
- **数据保留策略** — 可配置使用记录和审计日志的保留期限，每日自动清理
- **提交前 CI 对齐** — `make precommit` 执行与 CI 完全相同的检查（cargo check、test、clippy、fmt、pnpm build）

### 可观测性
- **Prometheus 指标** — Gateway 端口 (3000) 的 `GET /metrics` 端点，暴露 `gateway_requests_total`、`gateway_request_duration_seconds`、`gateway_tokens_total`、`gateway_rate_limited_total`、`circuit_breaker_state` 等指标
- **增强健康检查** — `/health/live`（存活探针）、`/health/ready`（就绪探针，检测 PostgreSQL 和 Redis）、`/api/health`（详细延迟和连接池统计）
- **ClickHouse 审计日志** — SQL 查询所有 API 调用和工具调用记录，审计日志存储在 ClickHouse 中，提供高性能列式分析
- **审计日志转发** — 多通道投递：UDP/TCP Syslog (RFC 5424)、Kafka、HTTP Webhook — 将审计事件路由至任意 SIEM、数据湖或告警管道
- **使用量分析** — 按用户、团队、模型、时间段的 Token 消耗统计
- **费用分析** — 月度累计支出、预算使用率、按模型费用明细
- **健康仪表盘** — PostgreSQL、Redis、ClickHouse 及所有 MCP Server 的实时状态
- **统一日志查询** — 在单一页面中查询所有日志类型（平台、审计、网关、MCP、访问、应用），支持结构化搜索语法
- **HTTP 访问日志** — Gateway 和 Console 端口的每个请求记录到 ClickHouse，包含方法、路径、状态码、延迟和客户端 IP
- **应用追踪日志** — Rust tracing span 捕获并存储到 ClickHouse，用于运行时调试

## 技术栈

| 层级 | 技术 |
|------|------|
| 后端 | Rust, Axum 0.8, SQLx 0.8, fred 10 (Redis), OpenTelemetry |
| 前端 | React 19, TypeScript 6, Vite 8, shadcn/ui, Tailwind CSS 4 |
| 数据库 | PostgreSQL 18 |
| 缓存与限流 | Redis 8 |
| 审计日志存储 | ClickHouse（列式 OLAP 数据库） |
| 单点登录 | Zitadel (或任何 OIDC 提供商) |
| 容器 | Distroless (2MB 运行镜像), Helm Chart (K8s) |

## 快速开始

```bash
# 1. 启动基础设施
make infra

# 2. 启动后端 (gateway :3000 + console :3001)
cp .env.example .env
make dev-backend

# 3. 启动前端开发服务器
cd web && pnpm install && pnpm dev

# 4. 在 http://localhost:5173/setup 完成设置向导
```

生产部署请参阅 **[部署指南](docs/zh-CN/deployment-guide.md)**。

## 文档

| 文档 | 说明 |
|------|------|
| **[架构设计](docs/zh-CN/architecture.md)** | 系统设计、双端口模型、数据流图 |
| **[部署指南](docs/zh-CN/deployment-guide.md)** | Docker Compose、Kubernetes Helm、SSL、生产加固 |
| **[API 参考](docs/zh-CN/api-reference.md)** | Gateway 和 Console 的完整端点文档 |
| **[安全](docs/zh-CN/security.md)** | 认证模型、加密、RBAC、威胁模型、加固清单 |
| **[配置](docs/zh-CN/configuration.md)** | 所有环境变量及其作用 |

## 端口架构

| 端口 | 服务器 | 暴露范围 | 用途 |
|------|--------|----------|------|
| `3000` | Gateway | **公网** — 暴露给 AI 客户端 | `/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1/models`, `/mcp`, `/metrics`, `/health/*` |
| `3001` | Console | **内网** — 限制在 VPN/防火墙后 | `/api/*` 管理端点, Web UI |

> 生产环境中，**仅端口 3000** 应可从公网访问。端口 3001 应限制在管理网络内。

## 项目结构

```
ThinkWatch/
├── crates/
│   ├── server/          # 双端口 Axum 服务器 (gateway + console)
│   ├── gateway/         # AI API 代理：路由、流式、限流、费用追踪
│   ├── mcp-gateway/     # MCP 代理：JSON-RPC、工具聚合、访问控制
│   ├── auth/            # JWT、OIDC、API Key、密码哈希、RBAC
│   └── common/          # 配置、数据库、模型、加密、校验、审计日志
├── migrations/          # 12 个 PostgreSQL 迁移文件
├── web/                 # React 前端 — 约 20 个页面组件
├── deploy/
│   ├── docker/          # Dockerfile.server (distroless), Dockerfile.web (nginx)
│   ├── docker-compose.yml       # 生产部署
│   ├── docker-compose.dev.yml   # 开发环境 (PG + Redis + ClickHouse + Zitadel)
│   └── helm/think-watch/      # Kubernetes Helm Chart
└── docs/                # 详细文档
```

## 贡献

欢迎贡献。提交大型变更前请先开 Issue 讨论。

## 授权协议

ThinkWatch 采用 [Business Source License 1.1](LICENSE) 进行源码可见分发。
非生产用途可免费使用。生产用途在每个 UTC 自然月内，同时不超过
`10,000,000` Billable Tokens 且不超过 `10,000` MCP Tool Calls 时可
免费使用；任一指标超出阈值后，需购买按使用量梯度计费的商业授权。

具体的生产阈值、Billable Tokens 与 MCP Tool Calls 定义、梯度方案
以及后续切换到 `GPL-2.0-or-later` 的规则，见
[LICENSING.zh-CN.md](LICENSING.zh-CN.md)。

## Star History

<a href="https://www.star-history.com/#ThinkWatch/ThinkWatch&Date">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=ThinkWatch/ThinkWatch&type=Date&theme=dark" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=ThinkWatch/ThinkWatch&type=Date" />
   <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=ThinkWatch/ThinkWatch&type=Date" />
 </picture>
</a>
