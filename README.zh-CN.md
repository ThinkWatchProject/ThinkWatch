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
- **多格式 API 代理** — 在同一端口原生支持 OpenAI Chat Completions (`/v1/chat/completions`)、Anthropic Messages (`/v1/messages`)、OpenAI Responses (`/v1/responses`，HTTP 或 WebSocket) 和 Gemini (`/v1beta/models/{model}:generateContent`) API；可直接替换 Cursor、Continue、Cline、Claude Code、Codex 以及 OpenAI/Anthropic/Gemini SDK
- **多 Provider 路由** — OpenAI、Anthropic、Google Gemini、Azure OpenAI、AWS Bedrock 或任何 OpenAI 兼容端点
- **自动格式转换** — Anthropic Messages API、Google Gemini、Azure OpenAI、AWS Bedrock Converse API 等，统一在同一接口之后
- **Provider 自动加载** — 启动时从数据库加载所有活跃 Provider 并注册到模型路由器；默认模型前缀（`gpt-`/`o1-`/`o3-`/`o4-` 对应 OpenAI，`claude-` 对应 Anthropic，`gemini-` 对应 Google）自动路由；Azure 和 Bedrock 需要显式注册模型
- **SSE 流式透传** — 零开销转发，实时 Token 计数
- **虚拟 API Key** — 按团队、项目、开发者签发 `tw-` 前缀的作用域密钥，一键吊销
- **API Key 生命周期管理** — 自动轮换并支持宽限期、按 Key 的闲置超时、到期预警、后台策略执行
- **滑动窗口限流** — 基于 Redis 的 RPM/TPM 限制，按 Key 或按用户
- **断路器** — 三态（Closed/Open/HalfOpen）断路器，可配置故障阈值和恢复周期
- **指数退避重试** — 可配置的重试策略，带抖动，适用于网络错误和上游限流
- **实时费用追踪** — 按模型计费，预算告警，团队费用归因

### MCP 网关

ThinkWatch 的 MCP 网关建立在一个大多数 MCP 代理都跳过的设计选择之上：**上游服务器看到的是真实的最终用户，而不是一个共享的 Service Account。** 其他所有能力都从这一点延伸而出。详见下方 [MCP 网关：与竞品对比](#mcp-网关与竞品对比)。

- **每用户的上游身份** — 每次 MCP 请求都使用调用用户自己的 OAuth Token 或 PAT 访问 GitHub / Notion / Linear / Slack / Atlassian / 飞书 / GitLab / Cloudflare / Google / Discord 等上游。Token 在 `mcp_user_credentials` 表中以 AES-256-GCM 加密存储。绝大多数"MCP 网关"在服务器配置上钉一个共享的管理员 Token——上游审计日志里所有操作看起来都是同一个 Service Account 干的。ThinkWatch 把真实身份端到端地传递下去。
- **同一用户多账号** — 同一个 GitHub MCP 服务器上同时绑定工作账号和个人账号，打标签，标记默认账号。同一个物理用户可以为同一个服务器持有多条凭据记录。
- **API Key → 账号覆盖** — 给同一个用户的不同 `tw-` Key 钉到同一服务器的不同上游账号上：Cursor Key 走个人 GitHub，CI Key 走 service-bot。一个用户、多个 Agent、多重身份，无需重发凭据。
- **一键 OAuth 接入** — 粘贴 MCP URL，点击「一键发现」。探测器走完整 RFC 9728 → RFC 8414 → RFC 7591 链路：通过 JSON-RPC `initialize` 触发 `WWW-Authenticate`，跟随 `resource_metadata` 提示，按路径感知规则取 AS 元数据，若上游通告 DCR 则自动注册客户端。如果上游不支持 DCR，UI 给出三个具体步骤（复制回调 URL → 在上游注册应用 → 粘贴 Client ID 回来），不暴露任何协议术语。
- **公共客户端支持** — 检测 `token_endpoint_auth_methods_supported: ["none"]` 并将 `is_public_client` 端到端传递。对于飞书等不使用 Client Secret 的 Issuer，UI 自动隐藏 Secret 输入框。
- **静态 Token 保险箱** — 对只支持 PAT / API Key 的上游（GitHub PAT、Notion 集成 Token），同样的每用户表面、同样的加密存储、同样的「我的连接」交互。静态 Token 在粘贴时即时验证，输错立刻知道。
- **每用户工具目录** — 当上游按 Scope/Role 过滤工具可见性时（Atlassian、企业 IDP），用户认证后的 `tools/list` 缓存到 `mcp_user_tools`，**只会返回给该用户**。系统级 `mcp_tools` 只存匿名可发现的工具。无跨用户泄漏；需授权服务器不再"工具数 0"等管理员手动修复。
- **三级上游身份解析** — 「我的连接」页面展示真实的上游身份（`@octocat`、`alice@acme.com`、Slack `Bob`）。解析器依次尝试 JWT 解码（免费）→ Userinfo 端点（按优先级抽取：`preferred_username` → `sub` → `accountId` → `login` → `email`）→ `.well-known` 自动发现。GitHub、Notion、Slack、Atlassian、Cloudflare、GitLab、Discord、Google 已预置。
- **MCP 应用商店，23+ 精选模板** — GitHub、Notion、Linear、Slack、Atlassian、Cloudflare、GitLab、Discord、Google、飞书等，预置了正确的 OAuth Scope、Userinfo 端点、PAT 帮助 URL。一键安装。每日从 Registry 自动刷新目录。
- **通用 MCP 客户端友好** — 对尚未授权的用户，网关仍然返回工具目录，并在每条工具上打 `_meta: { requires_user_auth: true, server_id, server_name, authorize_url }` 标记。对未授权服务器的 `tools/call` 返回 JSON-RPC 错误码 `-32050` 并附带授权 URL，Cursor / Claude Desktop / 任何符合规范的 MCP 客户端都能直接弹出授权引导，而不是看到一个空目录。
- **工具级 RBAC** — 服务器侧按角色授权工具，API Key 侧通过 `allowed_mcp_tools` 白名单进一步收紧（白名单受签发角色的授权范围约束）。一个锁死的服务 Key 可以恰好持有两个工具，多一个都不行。
- **`mcp:connect` 权限** — 守卫「我的连接」页和授权/吊销流程。默认授予 admin / team_manager / developer。
- **缓存按 `(user, account_label)` 隔离** — MCP 响应缓存绝不会把 Alice 的鉴权响应给 Bob。直连模式（无每用户凭据）服务器仍走全局缓存。
- **无竞争的 Token 刷新** — OAuth 刷新持有按 `(server, user, label)` 加锁的 `pg_advisory_xact_lock`，并发工具调用不会触发重复刷新。终态刷新失败时立即清除凭据行，下次调用干净地暴露 `NeedsUserCredentials`。
- **健康探测稳健** — 匿名探测返回 401/403 在需鉴权的 MCP 上是*预期行为*；服务器被标记为 `auth_required`（黄色），而不是 `disconnected`（红色）。/mcp/servers 列表对该状态显示"—"工具数并附悬停说明。
- **分步注册向导** — 鉴权模式感知的编辑表单、「我的连接」页的逐凭据 Test Connection 按钮、管理员防呆护栏（粘贴时验证静态 Token、不向默认账号静默回退）。
- **SSRF 加固** — 发现和 OAuth 探测的 URL 都通过注入式 URL 校验器，私有 IP 段、链路本地、元数据服务主机一律拒绝。
- **命名空间隔离** — `github__create_issue`、`postgres__query` —— 工具名跨上游永不冲突。
- **连接池与健康监控** — 自动重连，周期性后台探测，每个服务器的健康状态在仪表盘上呈现。
- **完整审计轨迹** — 每次工具调用在 ClickHouse 中记录用户、账号标签、参数、响应、延迟和错误，与 AI 网关日志并列。
- **限流与预算覆盖 MCP** — 计量 AI Token 的同一个引擎也计量 MCP 工具调用；每用户、每 API Key、每服务器作为 Subject 同时叠加生效。
- **一把 Key 双 Surface** — 同一把 `tw-` 虚拟 Key 通过 `surfaces` 白名单（`ai_gateway`、`mcp_gateway` 或两者）同时在 `/v1/chat/completions` 和 `/mcp` 上工作。

### MCP 网关：与竞品对比

目前市面上多数"MCP 网关"是薄反向代理：每个上游一个共享管理员 Token，没有最终用户身份概念，所谓"鉴权"就是"该用户是否带了网关的 Bearer Token"。这套模型在玩具场景能跑，一旦真实组织把它接到 GitHub / Atlassian / Linear / Slack 就崩盘——所有工具调用都显示为同一个 Service Account，Scope 无法因人而异，"是谁改了这个 Linear 工单"没有诚实的答案。

ThinkWatch 是为后一种场景设计的。

| 能力 | 一般 MCP 代理 | ThinkWatch |
|---|---|---|
| **上游看到真实用户** | ❌ 共享管理员 Token / 环境变量 | ✅ 每用户 OAuth Token + PAT 保险箱，AES-256-GCM 加密静态存储 |
| **同一用户多账号** | ❌ 一份配置 = 一个身份 | ✅ 工作 + 个人账号，可打标签，可设默认 |
| **API Key → 账号绑定** | ❌ Key 不透明 | ✅ Cursor → 个人，cron → service-bot，同一用户内 |
| **OAuth 接入** | ❌ 手改 JSON / 环境变量 | ✅ 粘贴 URL 一键 DCR（RFC 9728 → 8414 → 7591），公共客户端支持 |
| **每用户工具可见性** | ❌ 假定目录均匀（缓存即权限提升风险） | ✅ 独立 `mcp_user_tools`，系统目录只存匿名可发现项 |
| **通用 MCP 客户端 UX（Cursor/Claude Desktop）** | ❌ 未授权 = 空目录 | ✅ 目录 + `_meta.requires_user_auth` 标记 + `-32050` 携带 `authorize_url` |
| **工具级 RBAC** | ❌ 一刀切 | ✅ 角色侧授权 + Key 侧 `allowed_mcp_tools` 白名单（受角色约束） |
| **内置目录** | ❌ 全靠手撸 | ✅ 23+ 模板预置（GitHub / Notion / Linear / Slack / Atlassian / Cloudflare / GitLab / Discord / Google / 飞书 …） |
| **审计 / 限流 / 预算** | ❌ 仅 LLM 或缺失 | ✅ 同一引擎同时计量 AI Token 和 MCP 工具调用 |
| **响应缓存安全** | ❌ 共享缓存跨用户泄漏 | ✅ OAuth/PAT 服务器按 `(user, account_label)` 隔离 |
| **OAuth 刷新竞争** | ❌ 并发下重复刷新 | ✅ `pg_advisory_xact_lock` 按 `(server, user, label)` 加锁 |
| **健康判定** | ❌ 401/403 即"不健康"（误报） | ✅ `auth_required` 是一等的黄色状态 |
| **SSRF 防护** | ❌ 裸 fetch | ✅ 注入式 URL 校验器，私有/链路本地/元数据 IP 拒绝 |
| **一把 Key 双 Surface** | ❌ AI 与 MCP 分开两套 | ✅ 单 `tw-` Key，按 Key 配置 `surfaces` 白名单 |

如果你只需要"把几个公开 MCP 服务器开放给小团队"，简单代理够用。一旦你需要"谁、代表谁、用什么 Scope、记到哪个成本中心"——ThinkWatch 就是为此而设计的。

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
- **会话 IP 绑定** — 管理员会话与登录 IP 绑定，被窃取的令牌无法在其他网络中重放
- **Distroless 容器** — 生产环境最小攻击面 (2MB 运行镜像，无 shell)

### 运维与配置
- **动态配置** — 大部分配置存储在数据库（`system_settings` 表），可通过 Web UI（管理 > 设置，7 个分类标签页）配置
- **首次运行向导** — 引导式 `/setup` 向导，创建超级管理员账户、配置站点信息，可选添加首个 Provider 和 API Key
- **配置指南** — Web 控制台内置 `/gateway/guide` 页面，提供 Claude Code、Cursor、Continue、Cline、OpenAI SDK、Anthropic SDK 和 cURL 的一键复制配置说明；自动检测网关 URL
- **多实例同步** — 配置变更通过 Redis Pub/Sub 在多个实例间同步
- **数据保留策略** — 可配置使用记录和审计日志的保留期限，每日自动清理

### 可观测性
- **Prometheus 指标** — Gateway 端口 (3000) 的 `GET /metrics` 端点，暴露 `gateway_requests_total`、`gateway_request_duration_seconds`、`gateway_tokens_total`、`gateway_rate_limited_total`、`circuit_breaker_state` 等指标
- **增强健康检查** — `/health/live`（存活探针）、`/health/ready`（就绪探针，检测 PostgreSQL 和 Redis）、`/api/health`（详细延迟和连接池统计）
- **ClickHouse 审计日志** — SQL 查询所有 API 调用和工具调用记录，审计日志存储在 ClickHouse 中，提供高性能列式分析
- **审计日志转发** — 多通道投递：UDP/TCP Syslog (RFC 5424)、Kafka、HTTP Webhook — 将审计事件路由至任意 SIEM、数据湖或告警管道
- **使用量分析** — 按用户、团队、模型、时间段的 Token 消耗统计
- **费用分析** — 月度累计支出、预算使用率、按模型费用明细
- **健康仪表盘** — PostgreSQL、Redis、ClickHouse 及所有 MCP Server 的实时状态
- **统一日志查询** — 在单一页面中跨审计、网关、MCP、访问、平台等多类日志搜索，支持结构化查询语法

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

# 2. 生成 dev 密钥并启动后端 (gateway :3000 + console :3001)
make dev-secrets       # 从 .env.example 派生 .env，并填入随机密钥
make dev-backend

# 3. 启动前端开发服务器
cd web && pnpm install && pnpm dev

# 4. 在 http://localhost:5173/setup 完成设置向导
```

生产部署请参阅 **[部署指南](https://thinkwat.ch/zh-CN/docs/deployment-guide)**。

## 文档

完整文档：**[thinkwat.ch/zh-CN/docs](https://thinkwat.ch/zh-CN/docs)**

| 文档 | 说明 |
|------|------|
| **[架构设计](https://thinkwat.ch/zh-CN/docs/architecture)** | 系统设计、双端口模型、数据流图 |
| **[部署指南](https://thinkwat.ch/zh-CN/docs/deployment-guide)** | Docker Compose、Kubernetes Helm、SSL、生产加固 |
| **[配置说明](https://thinkwat.ch/zh-CN/docs/configuration)** | 所有环境变量及其作用 |
| **[API 参考](https://thinkwat.ch/zh-CN/docs/api-reference)** | Gateway 和 Console 的完整端点文档 |
| **[安全模型](https://thinkwat.ch/zh-CN/docs/security)** | 认证模型、加密、RBAC、威胁模型、加固清单 |
| **[密钥轮换](https://thinkwat.ch/docs/secret-rotation)** | 在生产环境中轮换 Provider 密钥、JWT secret 和管理员凭据（仅英文）|

## 端口架构

| 端口 | 服务器 | 暴露范围 | 用途 |
|------|--------|----------|------|
| `3000` | Gateway | **公网** — 暴露给 AI 客户端 | `/v1/chat/completions`, `/v1/messages`, `/v1/responses` (HTTP and WebSocket), `/v1beta/models/{model}:generateContent`, `/v1/models`, `/mcp`, `/metrics`, `/health/*` |
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
├── db/                  # 声明式 PostgreSQL Schema (schema.sql + seeds.sql)
├── web/                 # React 前端 — 约 20 个页面组件
├── deploy/
│   ├── docker/          # Dockerfile.server (distroless), Dockerfile.web (nginx)
│   ├── docker-compose.yml       # 生产部署
│   ├── docker-compose.dev.yml   # 开发环境 (PG + Redis + ClickHouse + Zitadel)
│   └── helm/think-watch/      # Kubernetes Helm Chart
└── ...
```

> 文档：**[thinkwat.ch/zh-CN/docs](https://thinkwat.ch/zh-CN/docs)**

## 相关项目

| 仓库 | 说明 |
|---|---|
| [ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite) | 面向个人开发者的桌面应用，在 macOS、Windows 和 Linux 上运行本地 AI API 网关，记录 Claude Code、Codex 等客户端每个请求的费用、所用的上游以及被脱敏的内容，也可以连接部署在服务器上的 ThinkWatch Core。MIT 协议。 |
| [ThinkWatch Core](https://github.com/ThinkWatchProject/ThinkWatch-Core) | Rust crate 与 `twcore` 网关二进制：按规则路由、故障转移、费用核算、脱敏与工具调用审查。ThinkWatch Lite 基于它构建，`twcore` 也可以作为独立网关部署在 Linux 服务器上。ThinkWatch 企业版只依赖其中三个 crate：`tw-dialect`（接口格式转换与用量解析）、`tw-guard`（脱敏、工具调用审查及其他防护）和 `tw-breaker`（熔断状态机）。MIT 协议。 |

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
