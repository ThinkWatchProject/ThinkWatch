# RBAC 完善三阶段计划

## 背景

审计发现自定义角色当前没有真正接入鉴权回路：

- `require_permission` 中间件标注 `#[allow(dead_code)]`，从未挂载到任何路由。
- 所有 admin 路由用的是 `require_admin`，硬编码只检查 `r == "super_admin" || r == "admin"`。
- JWT `roles` claim 在 login 时过滤了 `is_system = TRUE`，自定义角色名字根本没进 token。
- 结果：自定义角色数据库里的 `permissions` / `allowed_models` / `allowed_mcp_servers` / `policy_document` 全是死数据。

本计划分三个阶段修复。每个阶段独立编译、独立测试、独立回滚。

---

## 阶段 1 — 让自定义角色真正生效

**目标**：把权限模型从"硬编码 SystemRole 5 个角色"切换到"基于 `claims.permissions` 的运行时检查"，并清理用户管理 API。

### 约定

- **多角色合并语义：并集**（在 rbac.rs 头注释明确声明）。
- **破坏性改动可接受**：产品未发布，旧 token 全部失效、用户需要重新登录。
- **完全移除 `require_admin`**：路由层只负责认证，授权由 handler 内联检查。

### 步骤

| # | 文件 | 改动 |
|---|---|---|
| 1.1 | `crates/auth/src/jwt.rs` | `Claims` 加 `permissions: Vec<String>` 字段；所有 `create_*_token*` 方法加一个 `permissions` 参数 |
| 1.2 | `crates/auth/src/rbac.rs` | 头注释写清并集语义；新增 `compute_user_permissions(pool, user_id) -> Vec<String>` helper，SQL 取用户所有角色的 permissions 字段并集 |
| 1.3 | `crates/server/src/handlers/auth.rs` | `login`：去掉 `is_system = TRUE` 过滤，取所有角色名；调 `compute_user_permissions` 装入 claims；`register` / `me` / `refresh` 同理 |
| 1.4 | `crates/server/src/handlers/sso.rs` | SSO 登录流程同上 |
| 1.5 | `crates/server/src/handlers/setup.rs` | 首次初始化 admin 用户后创建 token 同上 |
| 1.6 | `crates/server/src/middleware/auth_guard.rs` | `AuthUser` 加方法 `require_permission(&self, perm: &str) -> Result<(), AppError>`（检查 `claims.permissions.contains(perm)`，失败返回 `Forbidden`） |
| 1.7 | `crates/server/src/middleware/require_role.rs` | **删除整个文件**或只保留最薄的 wrapper |
| 1.8 | `crates/server/src/main.rs` | 启动后做 catalog 校验：查 `rbac_roles.permissions`，每条权限必须在 `PERMISSION_CATALOG`，否则 fail-fast |
| 1.9 | `crates/common/src/dto/mod.rs` | 新增 `RoleAssignmentRequest { role_id, scope }` 和 `RoleAssignment { id, name, is_system, scope }`；`UserResponse.roles` 改为 `role_assignments: Vec<RoleAssignment>` |
| 1.10 | `crates/server/src/handlers/admin.rs` | `list_users` 返回新 DTO；`create_user` / `update_user` 接受 `role_assignments: Vec<RoleAssignmentRequest>`，写入逻辑改成"全删全插"一笔事务 |
| 1.11 | 所有 admin handler 文件 | handler 顶部加 `auth_user.require_permission("resource:action")?` 内联检查 |
| 1.12 | `crates/server/src/app.rs` | 删除 `require_admin` layer，admin_routes 只保留 `verify_signature` + `require_auth` |
| 1.13 | `crates/auth/src/rbac.rs` | 删除 `SystemRole::has_permission`（硬编码的权限判断过时了），保留 `SystemRole::parse` 因 setup 流程还在用 |
| 1.14 | `web/src/routes/admin/users.tsx` | 单一多选选择器：系统 + 自定义角色放在一起，每条带 scope 选项；移除旧的"系统角色单选 + 自定义角色编辑器"双区结构 |
| 1.15 | `web/src/i18n/{en,zh}.json` | 删除孤儿 key `roles.resourceConstraints` |

### 路由到权限映射

| 路由 | 权限 |
|---|---|
| `GET /api/admin/providers` | `providers:read` |
| `POST /api/admin/providers` | `providers:create` |
| `POST /api/admin/providers/test` | `providers:create` |
| `GET /api/admin/providers/{id}` | `providers:read` |
| `PATCH /api/admin/providers/{id}` | `providers:update` |
| `DELETE /api/admin/providers/{id}` | `providers:delete` |
| `GET /api/mcp/servers` | `mcp_servers:read` |
| `POST /api/mcp/servers` | `mcp_servers:create` |
| `GET /api/mcp/servers/{id}` | `mcp_servers:read` |
| `PATCH /api/mcp/servers/{id}` | `mcp_servers:update` |
| `DELETE /api/mcp/servers/{id}` | `mcp_servers:delete` |
| `POST /api/mcp/servers/{id}/discover` | `mcp_servers:update` |
| `GET /api/admin/users` | `users:read` |
| `POST /api/admin/users` | `users:create` |
| `PATCH /api/admin/users/{id}` | `users:update` |
| `DELETE /api/admin/users/{id}` | `users:delete` |
| `POST /api/admin/users/{id}/force-logout` | `sessions:revoke` |
| `POST /api/admin/users/{id}/reset-password` | `users:update` |
| `GET /api/admin/settings/*` | `settings:read` |
| `PATCH /api/admin/settings/*` | `settings:write` |
| `PATCH /api/admin/settings/oidc` | `system:configure_oidc` |
| `GET /api/admin/log-forwarders` | `log_forwarders:read` |
| `POST/PATCH/DELETE /api/admin/log-forwarders/...` | `log_forwarders:write` |
| `GET /api/admin/platform-logs` | `logs:read_all` |
| `GET /api/admin/access-logs` | `logs:read_all` |
| `GET /api/admin/app-logs` | `logs:read_all` |
| `GET /api/admin/roles` | `roles:read` |
| `POST /api/admin/roles` | `roles:create` |
| `PATCH /api/admin/roles/{id}` | `roles:update` |
| `DELETE /api/admin/roles/{id}` | `roles:delete` |
| `GET /api/admin/roles/{id}/members` | `roles:read` |
| `GET /api/admin/permissions` | `roles:read` |

### 验证

```
make precommit
```

### 风险

- JWT 体积：admin 用户 ~47 条权限，多 ~1.5KB。可接受。
- 启动校验失败 = 无法启动：这就是目的。
- 用户需要重新登录。

---

## 阶段 2 — UX 改进

不改变数据模型，只改进体验。每项独立可做。

| # | 内容 | 涉及文件 |
|---|---|---|
| 2.1 | 用户详情：**有效权限预览**面板。给定 user，展示合并后的 permissions / allowed_models / allowed_mcp_servers | 后端新接口 `GET /api/admin/users/{id}/effective-permissions`；前端 `users.tsx` 详情抽屉 |
| 2.2 | 角色列表：policy 模式角色显示估算权限数 + hover 列前几条 Statement | `roles.tsx` |
| 2.3 | 删除角色对话框：**逐成员**选择迁移目标（支持拆分到多个角色） | 后端 `delete_role` 接受 `{ reassign: [{user_id, role_id}] }`；前端 `roles.tsx` 删除对话框 |
| 2.4 | 危险权限保存前**二次确认**对话框 | `roles.tsx` |
| 2.5 | 角色搜索支持权限模式（`*:delete`） | `roles.tsx` |
| 2.6 | 角色详情页：**管理成员**（添加 / 移除） | 后端新接口 `POST /api/admin/roles/{id}/members` + `DELETE /api/admin/roles/{id}/members/{user_id}`；前端 `roles.tsx` |
| 2.7 | 简单模式新增预置模板（只读用户、网关使用者、运营管理员等） | `roles.tsx` |

---

## 阶段 3 — 重活儿

每项都是独立 PR / 独立设计。

| # | 内容 | 涉及 |
|---|---|---|
| 3.1 | 系统角色差量覆盖：允许 admin 在系统角色基础上添加 / 屏蔽个别权限 | 新 migration `rbac_roles` 加 `permission_overrides JSONB` |
| 3.2 | 角色审计历史标签页 | 新接口 `GET /api/admin/roles/{id}/history`（读 audit_logs） |
| 3.3 | CodeMirror policy 编辑器（语法高亮 + JSON Schema 验证） | 新依赖 `@codemirror/lang-json`；`roles.tsx` policy 模式 |
| 3.4 | 资源约束扩展：rate limit / 时间窗口 / IP 段 | schema 改动 + 前端 UI + 网关执行层 — **部分完成**：AI 网关现在会把用户角色的 `allowed_models` 与 API key 的 allow-list 取交集执行；rate limit / 时间窗口 / IP 段、以及 MCP 网关的 `allowed_mcp_servers` 强制执行延后到有真实需求再做（MCP 网关需要先把 sqlx pool 接进去，目前是无 DB 的 actor 设计） |
| 3.5 | 角色导入 / 导出 JSON | 后端两个新接口 + 前端按钮 |

---

## 执行顺序

1. **本次 session**：完整做完阶段 1，按逻辑拆成 2-4 个 commit。
2. **后续 session**：阶段 2（按优先级选做）。
3. **再后续**：阶段 3（每项单独 PR）。

做完每个阶段都要跑 `make precommit` 全绿才能提交。

---

# 限速与配额改造计划（第二轮）

## 背景

当前限速 / 预算系统是凑出来的，且很多地方根本没生效：

| 组件 | 现状 | 问题 |
|---|---|---|
| AI 网关 sliding window | `crates/gateway/src/rate_limiter.rs` 写死 60 秒窗口（RPM/TPM） | 不能设 5h、不能设周、不能设月；多窗口不可叠加 |
| `api_keys.rate_limit_rpm` | 仅 RPM/TPM 两列 | 只有一个固定窗口 |
| `api_keys.monthly_budget` / `teams.monthly_budget` | 只触发 budget alert，**不阻断请求** | "总限额"是软约束 |
| 用户级限速 | **完全不存在** | 用户名下所有 key 加起来无限调用 |
| Provider 级限速 | **完全不存在** | 一个 provider 被一个 key 打挂会牵连所有 key |
| MCP 网关限速 | `McpRateLimiter::new(60)` 硬编码每用户每分钟 60 次 | 不可配置、不可关闭 |
| MCP 服务器级限速 | **完全不存在** | 同上 |

用户的诉求：
1. **AI 网关 / API key**：可配的窗口滑动限速（1m / 5m / 1h / **5h** / 1d / **1w**）+ **月度总额度**（按金额或请求数），每条规则独立开关。
2. **MCP 网关 / API key**：可配的短窗口滑动限速（1m / 5m / 1h）。
3. **Provider**：总限速（所有 key 累加）。
4. **MCP 服务器**：总限速（所有调用累加）。
5. 全部规则灵活可配：能独立加 / 删 / 启 / 停，不限制条数。

## 设计决策

### D1. 用一张通用规则表

不再给每个 subject 加固定列，改成一张通用的 `rate_limit_rules` 表：

```sql
CREATE TABLE rate_limit_rules (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subject_kind VARCHAR(20) NOT NULL,
        -- 'user' | 'api_key' | 'provider' | 'mcp_server'
    subject_id   UUID NOT NULL,
    surface      VARCHAR(20) NOT NULL,
        -- 'ai_gateway' | 'mcp_gateway'
    metric       VARCHAR(20) NOT NULL,
        -- 'requests' | 'tokens'
    window_secs  INTEGER NOT NULL,        -- 60 / 300 / 3600 / 18000 / 86400 / 604800
    max_count    BIGINT  NOT NULL,
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(subject_kind, subject_id, surface, metric, window_secs)
);
CREATE INDEX idx_rlr_subject ON rate_limit_rules(subject_kind, subject_id);
```

**为什么是表而不是 JSONB**：表的好处是约束 / 唯一索引 / 单条目开关都很自然，UI 也能直接 CRUD。一个 user 同时设 5h + week + month 就是 3 行。月度配额走单独的"虚拟超长窗口"还是单独的预算表？见 D2。

### D2. 月度总额度走独立的 `budget_caps` 表

月度配额和滑动窗口不是一回事：滑动窗口是"过去 N 秒里发生了多少"，月度总额度是"自然月内累计消耗了多少 token"。混到一起会很难解释。

```sql
CREATE TABLE budget_caps (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subject_kind VARCHAR(20) NOT NULL,    -- 'user' | 'api_key' | 'team' | 'provider'
    subject_id   UUID NOT NULL,
    period       VARCHAR(20) NOT NULL,    -- 'daily' | 'weekly' | 'monthly'
    -- 单位永远是「加权 tokens」，详见 D2a。不再支持 USD / requests 维度
    -- — USD 由前端按当前用量推导显示。
    limit_tokens BIGINT  NOT NULL,
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(subject_kind, subject_id, period)
);
```

`period` 是**自然周期**（按系统时区切到日 / 周 / 月开始），不是滑动窗口。计数器 key 形如 `budget:<subject>:<period>:<bucket>`，bucket 是 `2026-04`、`2026-W15`、`2026-04-08`。

**破坏性改动**：删除 `api_keys.monthly_budget`、`teams.monthly_budget` 列；现有数据迁移到 `budget_caps`（按当时的模型平均价折算成加权 token）。也删除 `api_keys.rate_limit_rpm` / `rate_limit_tpm` 列。

### D2a. 模型倍率：加权 token

不同模型的 token 成本天差地别（gpt-4o ≠ gpt-3.5-turbo）。如果直接按 raw token 计配额，预算会被贵的模型瞬间打穿。引入**加权 token**：

```sql
ALTER TABLE models
    ADD COLUMN input_multiplier  DECIMAL(8, 4) NOT NULL DEFAULT 1.0,
    ADD COLUMN output_multiplier DECIMAL(8, 4) NOT NULL DEFAULT 1.0;
```

- 单位：相对一个虚拟"基准模型"（比如 gpt-3.5-turbo = 1.0）。
- 每个请求结束后计算：
  ```
  weighted = input_tokens  × input_multiplier
           + output_tokens × output_multiplier
  ```
- 这个 `weighted` 是滑动窗口 token-metric 规则和所有 budget_caps 的累加单位。
- raw token 数继续记录在 `usage_records.input_tokens` / `output_tokens`，分析图表用 raw 数据。

**与 `models.input_price` / `output_price` 的关系**：完全独立。`*_price` 是 USD/token 真实成本，给计费 / 报表用；`*_multiplier` 是配额折算系数，给限速用。两者都允许独立配置。默认情况下 multiplier 全部 1.0，相当于按 raw token 计费。

**USD 换算**：UI 在限速 / 配额面板上不光显示 "用了 X / Y weighted tokens"，还显示 "≈ $Z"。Z 是按当前周期内 `usage_records` 真实 cost_usd 累加得出的，不是从 weighted tokens 反推 — 因为 multiplier 是相对值，没有全局 USD 汇率。这条聚合查询走 PG，结果按 5 分钟缓存。

### D3. 滑动窗口实现：分桶近似

纯滑动 + 1 周窗口需要在 Redis ZSET 里存约 50k 个成员，一周内每个请求都留一条记录。打挂 Redis 内存。

改成**分桶近似**：把窗口切成 N 个固定 bucket，每个 bucket 一个 INCR 计数器。例：

| 窗口 | bucket 大小 | bucket 数 |
|---|---|---|
| 60s | 1s | 60 |
| 5m | 5s | 60 |
| 1h | 60s | 60 |
| 5h | 5m | 60 |
| 1d | 24m | 60 |
| 1w | 168m | 60 |

每个窗口固定 60 桶，精度 ~1.6%，足够生产用。Redis 单机内存占用：每条规则 60 个 key。

实现单一 Lua 脚本 `LUA_BUCKETED_CHECK`，参数化窗口与桶大小，原子地读 + 比较 + 写当前 bucket，并对超出窗口的旧 bucket 设过期时间。

### D4. 计算缓存

每次请求都查表会成为热路径瓶颈。规则在 Redis 里以 `rules:<subject>` 开缓存 + pubsub 失效（参考已有的 `dynamic_config` 模式）。规则写入时发 `config_changed` 事件，本地缓存清掉。

冷启动 / cache miss 走 DB；命中走内存。

### D5. 失败模式

Redis 不可用时怎么办？两种选择：
- **fail-closed**（拒绝所有请求）：安全，但 Redis 抖一下整个网关就 500。
- **fail-open**（放过）：可用，但 Redis 故障期内限速形同虚设。

默认 **fail-open**，加 metric `gateway_rate_limiter_fail_open_total`，再加一个 system setting `security.rate_limit_fail_closed`（默认 false）让操作员能切换。

### D6. 多 subject 同时检查

一个 AI 网关请求同时受这些约束影响：

| Subject | 何时检查 |
|---|---|
| `api_key` | 每个 API key 自己的限速 |
| `user`(api_key.user_id) | 用户级累加（所有 key 求和） |
| `provider`(resolved from request) | 提供商级累加 |

任一超限 → 拒绝。所有都通过 → 全部 INCR。**不允许部分 INCR**：用 Lua 脚本一次原子完成（脚本接受 N 组 (key, limit) 元组，全部通过才 INCR，否则全部不动）。

MCP 网关同理：`api_key`(当前没接 MCP 的 api key 概念，可省略) + `user` + `mcp_server`。

### D7. 预算扣减时机

加权 token 数只能在上游响应回来之后才知道（输入 token 也要等 provider 计数回包，因为 tokenizer 在 provider 侧）。两种做法：

1. **预扣 + 事后修正**：请求开始时按 input token 估算扣一笔，响应回来用真实值修正。
2. **事后扣 + 软超额**：响应回来扣，临界用户能透支一次。

选 **2**，理由：
- token 估算（特别是 output）不准；预扣会把用户卡死在保守估计下。
- 软超额一次用户能接受，alert 系统已经在跑。
- 预扣还要处理失败回滚的并发问题。

文档明确："总额度是软上限，可能透支不超过一个请求消耗的加权 token"。

注意 sliding-window 限速（按 requests 维度的）跟 budget_caps 是分开的：sliding window 在请求**开始时**就 INCR + check（防 DDoS），budget_caps 在响应回来后 add_spend（按真实加权 token 累加）。这两套并行运行，各自独立。

## 阶段拆分

### 阶段 A — Schema + 通用 limits 模块（必须先做）

| # | 内容 |
|---|---|
| A.1 | Migration：新增 `rate_limit_rules`、`budget_caps`；给 `models` 加 `input_multiplier` / `output_multiplier` 列；删除 `api_keys.monthly_budget` / `teams.monthly_budget` / `api_keys.rate_limit_rpm` / `api_keys.rate_limit_tpm` |
| A.2 | `crates/common/src/limits.rs`：`RateLimitRule` / `BudgetCap` 结构、CRUD helper、Redis 缓存 + pubsub 失效 |
| A.3 | `crates/common/src/limits/sliding.rs`：通用分桶 Lua 脚本 + `check_and_record(redis, rules: &[ResolvedRule], cost_units, now)` 单次原子调用（cost_units 为本次请求消耗的单位数：requests metric 永远 1，tokens metric 是加权 token） |
| A.4 | `crates/common/src/limits/budget.rs`：`current_period_key`、`add_weighted_tokens`、`check_cap` 三个函数；按系统 TZ 计算自然周期 |
| A.5 | `crates/common/src/limits/weight.rs`：`weighted_tokens(model_id, input, output) -> i64` helper，从 models 表查 multiplier；带本地 LRU 缓存避免热路径查 PG |
| A.6 | 启动时自检：扫一遍 `rate_limit_rules.window_secs` 必须在 `[60, 60*60*24*7*4]` 区间；扫一遍 `models.input_multiplier` / `output_multiplier` 必须 `> 0` |

### 阶段 B — AI 网关接入

| # | 内容 |
|---|---|
| B.1 | `proxy.rs` 顶部把 api_key + user + provider 三个 subject 的所有 enabled 规则加载（缓存命中不查 DB） |
| B.2 | **请求开始时**：调 `limits::check_and_record` 对所有 `requests`-metric 规则原子 check + INCR；失败 → 429 |
| B.3 | **响应回来后**：算出 `weighted = input_tokens × in_mult + output_tokens × out_mult`，对所有 `tokens`-metric 规则调 `check_and_record`（这一步可能让 token 滑窗超限,但请求已经完成,只 INCR 不拒绝）；同时调 `budget::add_weighted_tokens` 累加到所有 enabled budget_caps |
| B.4 | 删除现有 `api_keys.rate_limit_rpm` / `monthly_budget` 引用；删 `BudgetAlertManager` 改用新 budget_caps（alerts 走 `budget_caps.limit_tokens × threshold` 自动生成） |

### 阶段 C — MCP 网关接入

| # | 内容 |
|---|---|
| C.1 | 把 sqlx pool 接进 `McpProxy`（这是 phase 3 时延后的工作） |
| C.2 | 删掉 `McpRateLimiter::new(60)` 硬编码，换成 `limits::check_and_record` |
| C.3 | Subject 检查：user + mcp_server |

### 阶段 D — 后端 CRUD 接口

| # | 内容 |
|---|---|
| D.1 | `GET/POST/DELETE /api/admin/limits/{subject_kind}/{subject_id}/rules` 管理 rate_limit_rules |
| D.2 | `GET/POST/DELETE /api/admin/limits/{subject_kind}/{subject_id}/budgets` 管理 budget_caps |
| D.3 | `GET /api/admin/limits/{subject_kind}/{subject_id}/usage` 当前用量（每个规则的当前 count + 每个预算的当前 spend） |
| D.4 | 权限：复用 `users:update` / `providers:update` / `mcp_servers:update`；新增 `api_keys:update` 已存在 |

### 阶段 E — 前端

| # | 内容 |
|---|---|
| E.1 | `<RateLimitEditor subjectKind subjectId surface />` 通用组件：列表 + 添加行（窗口下拉 [1m/5m/1h/5h/1d/1w] + metric 下拉 [requests/tokens] + 阈值输入 + 启用开关） + 当前用量徽章 |
| E.2 | `<BudgetCapEditor subjectKind subjectId />` 同上结构，周期下拉 [daily/weekly/monthly] + 加权 token 阈值；旁边显示「本周期已用 X tokens (≈ $Y)」，USD 走单独的 `/api/admin/limits/.../usage` 接口 |
| E.3 | 模型管理页加 `input_multiplier` / `output_multiplier` 字段（默认 1.0，admin 可改） |
| E.4 | 嵌入位置：<br>- 用户编辑对话框 → 新 tab "限速 / 配额"<br>- API key 创建 / 编辑表单 → 同名 tab<br>- Provider 编辑表单 → 同名 section<br>- MCP 服务器编辑表单 → 同名 section |
| E.5 | 全局 system setting `security.rate_limit_fail_closed` 加到 settings 页面 |

### 阶段 F — 文档 & 校验

| # | 内容 |
|---|---|
| F.1 | 启动 catalog 自检扩展：`SYSTEM_ROLE_DEFAULTS` 给 super_admin / admin 加 `rate_limits:read` / `rate_limits:write` 权限；新接口走这俩 |
| F.2 | README 加一节"限速与配额"，画清楚 user / api_key / provider 三层叠加、"任一超限即拒绝"的语义、以及 raw token vs 加权 token vs USD 三套数字的关系 |

## 风险

1. **删 `api_keys.rate_limit_rpm` 等列是破坏性改动**。生产未发布，按约定可以做。需要数据迁移：把现有非空值复制到 `rate_limit_rules` / `budget_caps`。
2. **Lua 脚本性能**：单次 `EVALSHA` ~0.1ms，按当前规则数量没问题；规则上百条后需要分批。
3. **budget 软超额**：每个 subject 最多透支一个请求消耗的加权 token。文档要写清楚。
4. **缓存一致性**：pubsub 是 best-effort；失效消息丢了会有最多 N 秒的过期数据。可接受。
5. **multiplier 默认 1.0**：升级后既有模型全部 multiplier=1.0，等于按 raw token 计费。admin 不去配的话相当于没分级。文档要提示一遍。
6. **MCP 网关接 sqlx pool**：这是 phase 3 时延后的工作，要重新评估 `McpProxy` 的构造路径。

## 执行顺序

按字母顺序串行：A → B → C → D → E → F。

- 阶段 A、D 是后端基础设施，独立可做、独立可测。
- 阶段 B、C 真正接入数据路径，会改 hot path，需要最小化改动 + 跑 e2e。
- 阶段 E 是前端，依赖 D 的接口落地。
- 阶段 F 是收尾。

每个阶段做完跑 `make precommit` 全绿才能 commit。
