<p align="center">
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" />
  <img src="https://img.shields.io/badge/React-20232A?style=for-the-badge&logo=react&logoColor=61DAFB" />
  <img src="https://img.shields.io/badge/PostgreSQL-316192?style=for-the-badge&logo=postgresql&logoColor=white" />
  <img src="https://img.shields.io/badge/Redis-DC382D?style=for-the-badge&logo=redis&logoColor=white" />
  <img src="https://img.shields.io/badge/Docker-2496ED?style=for-the-badge&logo=docker&logoColor=white" />
  <img src="https://img.shields.io/badge/Kubernetes-326CE5?style=for-the-badge&logo=kubernetes&logoColor=white" />
</p>

# AgentBastion

**[English](README.md) | [中文](README.zh-CN.md)**

**企业级 AI 堡垒机。** 在一个统一控制面上，对组织内每一次 AI API 调用和 MCP 工具调用进行安全管控、审计追踪和治理。

如同 SSH 堡垒机是所有服务器访问的唯一入口，AgentBastion 是所有 AI 访问的唯一入口。每一次模型请求、每一次工具调用、每一个 Token —— 全部经过认证、授权、限流、日志记录和费用核算。

```
                    ┌──────────────────────────────────────┐
 Claude Code ──────>│                                      │──> OpenAI
 Cursor ───────────>│    Gateway  :3000                    │──> Anthropic
 自定义 Agent ─────>│    AI API + MCP 统一代理              │──> Google Gemini
 CI/CD 流水线 ─────>│                                      │──> 私有化部署 LLM
                    └──────────────────────────────────────┘
                    ┌──────────────────────────────────────┐
 管理员浏览器 ─────>│    Console  :3001                    │
                    │    管理界面 + 管理 API                │
                    └──────────────────────────────────────┘
```

## 为什么需要 AgentBastion？

随着 AI Agent 在研发团队中的普及，企业面临日益严峻的治理挑战：

- **API Key 散落各处** — 硬编码在 `.env`、在 Slack 里共享、从不轮换
- **零可见性** — 谁在用哪个模型？消耗了多少 Token？花了多少钱？
- **无访问控制** — 每个开发者都能直接访问所有模型和 MCP 工具
- **合规缺口** — AI 辅助代码生成和数据访问没有审计轨迹
- **费用失控** — 月底的 AI 账单没人能解释或归因

AgentBastion 一次部署，全部解决。

## 核心功能

### AI API 网关
- **OpenAI 兼容代理** — 直接替换；将 `OPENAI_BASE_URL` 指向 AgentBastion 即可使用
- **多 Provider 路由** — OpenAI、Anthropic、Google Gemini、Azure 或任何 OpenAI 兼容端点
- **自动格式转换** — Anthropic Messages API、Google Gemini 等，统一在 OpenAI 兼容接口之后
- **SSE 流式透传** — 零开销转发，实时 Token 计数
- **虚拟 API Key** — 按团队、项目、开发者签发 `ab-` 前缀的作用域密钥，一键吊销
- **滑动窗口限流** — 基于 Redis 的 RPM/TPM 限制，按 Key 或按用户
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
- **安全 HTTP 头** — X-Content-Type-Options、X-Frame-Options、CORS 白名单、请求超时
- **Distroless 容器** — 生产环境最小攻击面 (2MB 运行镜像，无 shell)

### 可观测性
- **Quickwit 审计日志** — 全文搜索所有 API 调用和工具调用记录
- **Syslog 转发** — RFC 5424 兼容，无缝对接现有 SIEM 系统
- **使用量分析** — 按用户、团队、模型、时间段的 Token 消耗统计
- **费用分析** — 月度累计支出、预算使用率、按模型费用明细
- **健康仪表盘** — PostgreSQL、Redis、Quickwit 及所有 MCP Server 的实时状态

## 技术栈

| 层级 | 技术 |
|------|------|
| 后端 | Rust, Axum 0.8, SQLx 0.8, fred 10 (Redis), OpenTelemetry |
| 前端 | React 19, TypeScript 6, Vite 8, shadcn/ui, Tailwind CSS 4 |
| 数据库 | PostgreSQL 18 |
| 缓存与限流 | Redis 8 |
| 审计日志搜索 | Quickwit 0.8 |
| 对象存储 | RustFS (S3 兼容) |
| 单点登录 | Zitadel (或任何 OIDC 提供商) |
| 容器 | Distroless (2MB 运行镜像), Helm Chart (K8s) |

## 快速开始

```bash
# 1. 启动基础设施
docker compose -f deploy/docker-compose.dev.yml up -d

# 2. 启动后端 (gateway :3000 + console :3001)
cp .env.example .env
cargo run -p agent-bastion-server

# 3. 启动前端开发服务器
cd web && pnpm install && pnpm dev

# 4. 打开 http://localhost:5173
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
| `3000` | Gateway | **公网** — 暴露给 AI 客户端 | `/v1/chat/completions`, `/v1/models`, `/mcp` |
| `3001` | Console | **内网** — 限制在 VPN/防火墙后 | `/api/*` 管理端点, Web UI |

> 生产环境中，**仅端口 3000** 应可从公网访问。端口 3001 应限制在管理网络内。

## 项目结构

```
AgentBastion/
├── crates/
│   ├── server/          # 双端口 Axum 服务器 (gateway + console)
│   ├── gateway/         # AI API 代理：路由、流式、限流、费用追踪
│   ├── mcp-gateway/     # MCP 代理：JSON-RPC、工具聚合、访问控制
│   ├── auth/            # JWT、OIDC、API Key、密码哈希、RBAC
│   └── common/          # 配置、数据库、模型、加密、审计日志
├── migrations/          # 7 个 PostgreSQL 迁移文件
├── web/                 # React 前端 — 15 个页面组件
├── deploy/
│   ├── docker/          # Dockerfile.server (distroless), Dockerfile.web (nginx)
│   ├── docker-compose.yml       # 生产部署
│   ├── docker-compose.dev.yml   # 开发环境 (PG + Redis + RustFS + Quickwit + Zitadel)
│   └── helm/agent-bastion/      # Kubernetes Helm Chart
└── docs/                # 详细文档
```

## 贡献

欢迎贡献。提交大型变更前请先开 Issue 讨论。

## 授权协议

AgentBastion 采用 [Business Source License 1.1](LICENSE) 进行源码可见分发。
非生产用途可免费使用。生产用途在每个 UTC 自然月内不超过
`10,000,000` Billable Tokens 时可免费使用；超过该阈值后，需购买按
使用量计费的商业授权。

具体的生产阈值、Billable Tokens 定义以及后续切换到
`GPL-2.0-or-later` 的规则，见 [LICENSING.md](LICENSING.md)。
