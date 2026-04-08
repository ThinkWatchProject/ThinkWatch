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
