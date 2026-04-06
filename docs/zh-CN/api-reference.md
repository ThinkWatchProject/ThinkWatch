**[English](../en/api-reference.md) | [中文](../zh-CN/api-reference.md)**

# ThinkWatch API 参考

本文档提供 ThinkWatch 所有 HTTP 端点的完整参考。平台运行两个服务器：

| 服务器  | 默认端口 | 用途                           |
| ------- | -------- | ------------------------------ |
| Gateway | 3000     | AI 模型代理、MCP 传输          |
| Console | 3001     | 管理 UI 和管理员 API           |

---

## 认证方案

| 方案     | 请求头                          | 使用者             |
| -------- | ------------------------------- | ------------------ |
| API Key  | `Authorization: Bearer tw-...`  | Gateway 和 Console |
| JWT      | `Authorization: Bearer <token>` | Console 和 Gateway |
| 公开     | _（无）_                        | 健康检查端点       |

网关接受 API 密钥**或** JWT。控制台管理端点需要 JWT，除非另有说明。管理员端点还额外要求 `admin` 角色。

---

## Gateway 端点（端口 3000）

网关在单一端口上提供三种 API 格式，客户端可以使用任意偏好的格式：

| 端点                        | 格式                    | 典型客户端                                   |
| --------------------------- | ----------------------- | -------------------------------------------- |
| `POST /v1/chat/completions` | OpenAI Chat Completions | Cursor、Continue、Cline、OpenAI SDK          |
| `POST /v1/messages`         | Anthropic Messages API  | Claude Code、Anthropic SDK                   |
| `POST /v1/responses`        | OpenAI Responses API    | OpenAI SDK（2025 格式）                      |
| `GET /v1/models`            | OpenAI Models           | 所有客户端                                   |

三个端点使用相同的认证方式（API Key 或 JWT），经过相同的模型路由器和速率限制器，并生成相同的用量记录和审计日志。

### POST /v1/chat/completions

OpenAI 兼容的聊天补全端点。将请求代理到已配置的上游提供商。

**认证：** API Key 或 JWT

#### 请求体

```json
{
  "model": "string",
  "messages": [
    {
      "role": "system" | "user" | "assistant",
      "content": "string"
    }
  ],
  "temperature": 0.7,
  "top_p": 1.0,
  "max_tokens": 4096,
  "stream": false,
  "stop": ["string"],
  "presence_penalty": 0.0,
  "frequency_penalty": 0.0
}
```

| 字段                | 类型            | 必填     | 默认值  | 描述                             |
| ------------------- | --------------- | -------- | ------- | ---------------------------------------- |
| `model`             | string          | 是       | —       | 模型标识符（如 `gpt-4o`）       |
| `messages`          | array\<object\> | 是       | —       | 对话历史                         |
| `messages[].role`   | string          | 是       | —       | `system`、`user`、`assistant` 之一 |
| `messages[].content`| string          | 是       | —       | 消息文本                         |
| `temperature`       | number          | 否       | 0.7     | 采样温度（0.0 - 2.0）           |
| `top_p`             | number          | 否       | 1.0     | 核采样                           |
| `max_tokens`        | integer         | 否       | —       | 最大生成 Token 数                |
| `stream`            | boolean         | 否       | false   | 启用 Server-Sent Events 流式传输 |
| `stop`              | array\<string\> | 否       | —       | 停止序列                         |
| `presence_penalty`  | number          | 否       | 0.0     | 存在惩罚（-2.0 - 2.0）          |
| `frequency_penalty` | number          | 否       | 0.0     | 频率惩罚（-2.0 - 2.0）          |

#### 响应体（非流式）

```json
{
  "id": "chatcmpl-abc123",
  "object": "chat.completion",
  "created": 1711929600,
  "model": "gpt-4o",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Hello! How can I help you?"
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 12,
    "completion_tokens": 8,
    "total_tokens": 20
  }
}
```

#### 响应体（流式，`stream: true`）

响应为 `text/event-stream` SSE 事件流：

```
data: {"id":"chatcmpl-abc123","object":"chat.completion.chunk","created":1711929600,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-abc123","object":"chat.completion.chunk","created":1711929600,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"!"},"finish_reason":null}]}

data: [DONE]
```

#### 示例

```bash
# Non-streaming
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer tw-your-api-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello"}]
  }'

# Streaming
curl -X POST http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer tw-your-api-key" \
  -H "Content-Type: application/json" \
  -N \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello"}],
    "stream": true
  }'
```

#### 错误响应

| 状态码 | 响应体                                                               | 条件                             |
| ------ | -------------------------------------------------------------------- | -------------------------------- |
| 401    | `{"error": {"message": "Missing or invalid API key", "type": "authentication_error"}}` | 缺失/无效凭证             |
| 403    | `{"error": {"message": "Model not allowed", "type": "permission_error"}}` | API 密钥不允许访问此模型 |
| 404    | `{"error": {"message": "Model not found", "type": "not_found_error"}}` | 该模型未配置提供商       |
| 429    | `{"error": {"message": "Rate limit exceeded", "type": "rate_limit_error"}}` | 该密钥已达 RPM 限制     |
| 502    | `{"error": {"message": "Upstream provider error", "type": "upstream_error"}}` | 提供商返回错误           |

---

### GET /v1/models

列出通过网关可用的所有模型。

**认证：** API Key 或 JWT

#### 请求

无请求体。无查询参数。

#### 响应体

```json
{
  "object": "list",
  "data": [
    {
      "id": "gpt-4o",
      "object": "model",
      "created": 1711929600,
      "owned_by": "openai"
    },
    {
      "id": "claude-sonnet-4-20250514",
      "object": "model",
      "created": 1711929600,
      "owned_by": "anthropic"
    }
  ]
}
```

#### 示例

```bash
curl http://localhost:3000/v1/models \
  -H "Authorization: Bearer tw-your-api-key"
```

#### 错误响应

| 状态码 | 条件               |
| ------ | -------------------- |
| 401    | 无效凭证           |

---

### POST /v1/messages

Anthropic Messages API 端点。接受原生 Anthropic 格式的请求并代理到已配置的上游提供商。这允许 Claude Code 和 Anthropic SDK 直接连接，无需在客户端进行格式转换。

**认证：** API Key 或 JWT

#### 请求体

```json
{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 1024,
  "messages": [
    {
      "role": "user",
      "content": "Hello, Claude"
    }
  ],
  "stream": false,
  "system": "You are a helpful assistant.",
  "temperature": 0.7
}
```

| 字段         | 类型            | 必填     | 默认值  | 描述                                     |
| ------------ | --------------- | -------- | ------- | ---------------------------------------- |
| `model`      | string          | 是       | --      | 模型标识符（如 `claude-sonnet-4-20250514`）|
| `max_tokens` | integer         | 是       | --      | 最大生成 Token 数                        |
| `messages`   | array\<object\> | 是       | --      | 对话历史                                 |
| `stream`     | boolean         | 否       | false   | 启用 Server-Sent Events 流式传输         |
| `system`     | string          | 否       | --      | 系统提示词                               |
| `temperature`| number          | 否       | 1.0     | 采样温度（0.0 - 1.0）                   |

#### 响应体（非流式）

```json
{
  "id": "msg_abc123",
  "type": "message",
  "role": "assistant",
  "content": [
    {
      "type": "text",
      "text": "Hello! How can I help you today?"
    }
  ],
  "model": "claude-sonnet-4-20250514",
  "stop_reason": "end_turn",
  "usage": {
    "input_tokens": 12,
    "output_tokens": 10
  }
}
```

#### 响应体（流式，`stream: true`）

响应为遵循 Anthropic 流式格式的 `text/event-stream` SSE 事件流：

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_abc123","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: message_stop
data: {"type":"message_stop"}
```

#### 示例

```bash
curl -X POST http://localhost:3000/v1/messages \
  -H "Authorization: Bearer tw-your-api-key" \
  -H "Content-Type: application/json" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

#### 错误响应

| 状态码 | 条件                             |
| ------ | -------------------------------- |
| 401    | 缺失/无效凭证                   |
| 403    | 该密钥不允许访问此模型           |
| 404    | 模型未找到                       |
| 429    | 超出速率限制                     |
| 502    | 上游提供商错误                   |

---

### POST /v1/responses

OpenAI Responses API 端点（2025 格式）。接受 OpenAI Responses 格式的请求，支持简单字符串输入和结构化消息数组。

**认证：** API Key 或 JWT

#### 请求体

```json
{
  "model": "gpt-4o",
  "input": "What is the capital of France?",
  "instructions": "You are a helpful geography assistant.",
  "stream": false,
  "temperature": 0.7,
  "max_output_tokens": 4096
}
```

`input` 字段可以是简单字符串或消息对象数组：

```json
{
  "model": "gpt-4o",
  "input": [
    {"role": "user", "content": "Hello"},
    {"role": "assistant", "content": "Hi there!"},
    {"role": "user", "content": "What is 2+2?"}
  ]
}
```

| 字段               | 类型                      | 必填     | 默认值  | 描述                                     |
| ------------------ | ------------------------- | -------- | ------- | ---------------------------------------- |
| `model`            | string                    | 是       | --      | 模型标识符                               |
| `input`            | string 或 array\<object\> | 是       | --      | 提示字符串或对话消息                     |
| `instructions`     | string                    | 否       | --      | 系统级指令                               |
| `stream`           | boolean                   | 否       | false   | 启用 Server-Sent Events 流式传输         |
| `temperature`      | number                    | 否       | 0.7     | 采样温度                                 |
| `max_output_tokens`| integer                   | 否       | --      | 最大生成 Token 数                        |

#### 响应体（非流式）

```json
{
  "id": "resp_abc123",
  "object": "response",
  "created_at": 1711929600,
  "model": "gpt-4o",
  "output": [
    {
      "type": "message",
      "role": "assistant",
      "content": [
        {
          "type": "output_text",
          "text": "The capital of France is Paris."
        }
      ]
    }
  ],
  "usage": {
    "input_tokens": 15,
    "output_tokens": 8,
    "total_tokens": 23
  }
}
```

#### 示例

```bash
curl -X POST http://localhost:3000/v1/responses \
  -H "Authorization: Bearer tw-your-api-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "input": "What is the capital of France?"
  }'
```

#### 错误响应

| 状态码 | 条件                             |
| ------ | -------------------------------- |
| 401    | 缺失/无效凭证                   |
| 403    | 该密钥不允许访问此模型           |
| 404    | 模型未找到                       |
| 429    | 超出速率限制                     |
| 502    | 上游提供商错误                   |

---

### POST /mcp

MCP（Model Context Protocol）Streamable HTTP 传输端点。接受 JSON-RPC 2.0 请求并路由到已注册的 MCP 服务器。

**认证：** API Key 或 JWT

#### 请求体

标准 JSON-RPC 2.0 封装：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "tool_name",
    "arguments": {}
  }
}
```

常用方法：

| 方法                | 描述                           |
| ------------------- | ---------------------------------- |
| `initialize`        | 初始化会话，协商能力               |
| `tools/list`        | 列出可用的 MCP 工具                |
| `tools/call`        | 调用工具                           |
| `resources/list`    | 列出可用资源                       |
| `resources/read`    | 读取资源                           |
| `prompts/list`      | 列出可用提示词                     |

#### 响应头

| 请求头           | 描述                                  |
| ---------------- | --------------------------------------------- |
| `Mcp-Session-Id` | 会话标识符（在 `initialize` 时返回）  |

#### 响应体

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "content": [
      {
        "type": "text",
        "text": "Tool output here"
      }
    ]
  }
}
```

#### 示例

```bash
# Initialize session
curl -X POST http://localhost:3000/mcp \
  -H "Authorization: Bearer tw-your-api-key" \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
      "protocolVersion": "2025-03-26",
      "capabilities": {},
      "clientInfo": {"name": "my-client", "version": "1.0.0"}
    }
  }'

# Call a tool (include session ID from initialize response)
curl -X POST http://localhost:3000/mcp \
  -H "Authorization: Bearer tw-your-api-key" \
  -H "Content-Type: application/json" \
  -H "Mcp-Session-Id: session-uuid-here" \
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "tools/call",
    "params": {"name": "search_docs", "arguments": {"query": "setup guide"}}
  }'
```

#### 错误响应

| 状态码 | 条件                                   |
| ------ | ---------------------------------------- |
| 401    | 无效凭证                               |
| 400    | JSON-RPC 请求格式错误                   |
| 404    | 未找到 MCP 会话（无效会话）             |
| 502    | 上游 MCP 服务器不可达                   |

---

### DELETE /mcp

关闭 MCP 会话并释放相关资源。

**认证：** API Key 或 JWT

#### 请求头

| 请求头           | 必填     | 描述                 |
| ---------------- | -------- | -------------------- |
| `Mcp-Session-Id` | 是       | 要终止的会话         |

#### 响应

```
HTTP/1.1 204 No Content
```

#### 示例

```bash
curl -X DELETE http://localhost:3000/mcp \
  -H "Authorization: Bearer tw-your-api-key" \
  -H "Mcp-Session-Id: session-uuid-here"
```

#### 错误响应

| 状态码 | 条件                       |
| ------ | ---------------------------- |
| 401    | 无效凭证                   |
| 404    | 会话未找到或已过期         |

---

### GET /health

基本网关健康检查。

**认证：** 公开

#### 响应体

```json
{
  "status": "ok"
}
```

#### 示例

```bash
curl http://localhost:3000/health
```

---

### GET /health/live

存活探针。如果服务器进程正在运行，返回 200。

**认证：** 公开

#### 响应体

```json
{
  "status": "alive"
}
```

#### 示例

```bash
curl http://localhost:3000/health/live
```

---

### GET /health/ready

就绪探针。如果所有关键依赖（PostgreSQL、Redis）可达，返回 200。如果任何关键依赖不可用，返回 503。

**认证：** 公开

#### 响应体（健康）

```json
{
  "status": "ready"
}
```

#### 响应体（不健康）

```json
{
  "status": "not_ready",
  "reason": "redis unreachable"
}
```

#### 示例

```bash
curl http://localhost:3000/health/ready
```

| 状态码 | 条件                                  |
| ------ | --------------------------------------- |
| 200    | 所有关键依赖健康                      |
| 503    | PostgreSQL 或 Redis 不可达            |

---

### GET /metrics

Prometheus 兼容的指标端点。暴露请求延迟直方图、Token 吞吐量计数器、活跃连接数仪表和错误率计数器。

**认证：** 公开（无需认证）

**端口：** 3000（网关）

#### 响应

```
# HELP http_requests_total Total number of HTTP requests
# TYPE http_requests_total counter
http_requests_total{method="POST",endpoint="/v1/chat/completions",status="200"} 1520

# HELP http_request_duration_seconds HTTP request latency
# TYPE http_request_duration_seconds histogram
http_request_duration_seconds_bucket{endpoint="/v1/chat/completions",le="0.5"} 300
...
```

#### 示例

```bash
curl http://localhost:3000/metrics
```

> **注意：** 此端点用于 Prometheus 抓取。在生产环境中请通过网络策略限制访问。

---

## Console 端点（端口 3001）

### 初始化设置

#### GET /api/setup/status

检查系统是否已初始化（即是否已创建管理员用户）。

**认证：** 公开

#### 响应体

```json
{
  "initialized": false,
  "needs_setup": true
}
```

| 字段           | 类型    | 描述                                     |
| -------------- | ------- | ---------------------------------------- |
| `initialized`  | boolean | 如果已创建管理员用户则为 `true`          |
| `needs_setup`  | boolean | 如果应显示设置向导则为 `true`            |

#### 示例

```bash
curl http://localhost:3001/api/setup/status
```

---

#### POST /api/setup/initialize

执行系统初始化设置：创建首个管理员用户并可选配置首个 AI 提供商。此端点仅在系统尚未初始化时可用。

**认证：** 公开（仅在未初始化时）

#### 请求体

```json
{
  "admin": {
    "email": "admin@example.com",
    "display_name": "Admin",
    "password": "your-secure-password"
  },
  "provider": {
    "name": "openai-prod",
    "display_name": "OpenAI Production",
    "provider_type": "openai",
    "base_url": "https://api.openai.com/v1",
    "api_key": "sk-..."
  }
}
```

| 字段                     | 类型   | 必填     | 描述                                            |
| ------------------------ | ------ | -------- | ----------------------------------------------- |
| `admin.email`            | string | 是       | 管理员邮箱地址                                  |
| `admin.display_name`     | string | 是       | 管理员显示名称                                  |
| `admin.password`         | string | 是       | 管理员密码（最少 8 个字符）                     |
| `provider`               | object | 否       | 可选的首个提供商配置                            |
| `provider.name`          | string | 是*      | 唯一标识符（* 如设置 provider 则必填）          |
| `provider.display_name`  | string | 是*      | 人类可读的名称                                  |
| `provider.provider_type` | string | 是*      | `openai`、`anthropic`、`google`、`azure`、`bedrock`、`custom` 之一   |
| `provider.base_url`      | string | 是*      | 提供商 API 基础 URL                             |
| `provider.api_key`       | string | 是*      | 提供商 API 密钥                                 |

#### 响应体

```json
{
  "access_token": "eyJhbGciOi...",
  "refresh_token": "eyJhbGciOi...",
  "token_type": "Bearer",
  "expires_in": 900,
  "user": {
    "id": "uuid",
    "email": "admin@example.com",
    "display_name": "Admin",
    "role": "admin"
  }
}
```

#### 示例

```bash
curl -X POST http://localhost:3001/api/setup/initialize \
  -H "Content-Type: application/json" \
  -d '{
    "admin": {
      "email": "admin@example.com",
      "display_name": "Admin",
      "password": "your-secure-password"
    }
  }'
```

#### 错误响应

| 状态码 | 条件                                     |
| ------ | ---------------------------------------- |
| 400    | 系统已初始化                             |
| 422    | 验证错误（弱密码、无效邮箱）             |
| 429    | 超出速率限制（每分钟 5 次请求）          |

> **安全：** 此端点限制为每分钟 5 次请求，并对数据库执行二次检查以防止竞态条件。

---

### 认证

#### POST /api/auth/login

使用邮箱和密码进行认证以获取 JWT 令牌。

**认证：** 公开

#### 请求体

```json
{
  "email": "string",
  "password": "string"
}
```

| 字段       | 类型   | 必填     | 描述               |
| ---------- | ------ | -------- | ------------------ |
| `email`    | string | 是       | 用户邮箱地址       |
| `password` | string | 是       | 用户密码           |

#### 响应体

```json
{
  "access_token": "eyJhbGciOi...",
  "refresh_token": "eyJhbGciOi...",
  "token_type": "Bearer",
  "expires_in": 900,
  "user": {
    "id": "uuid",
    "email": "user@example.com",
    "display_name": "Jane Doe",
    "role": "user"
  }
}
```

#### 示例

```bash
curl -X POST http://localhost:3001/api/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email": "admin@example.com", "password": "secret"}'
```

#### 错误响应

| 状态码 | 条件                      |
| ------ | --------------------------- |
| 401    | 邮箱或密码无效            |
| 422    | 缺少必填字段              |
| 429    | 登录尝试次数过多          |

---

#### POST /api/auth/register

创建新用户账户。

**认证：** 公开

#### 请求体

```json
{
  "email": "string",
  "display_name": "string",
  "password": "string"
}
```

| 字段           | 类型   | 必填     | 描述                         |
| -------------- | ------ | -------- | ---------------------------- |
| `email`        | string | 是       | 唯一邮箱地址                 |
| `display_name` | string | 是       | 显示名称                     |
| `password`     | string | 是       | 密码（最少 8 个字符）        |

#### 响应体

```json
{
  "id": "uuid",
  "email": "user@example.com",
  "display_name": "Jane Doe",
  "role": "user",
  "created_at": "2026-01-15T10:30:00Z"
}
```

#### 示例

```bash
curl -X POST http://localhost:3001/api/auth/register \
  -H "Content-Type: application/json" \
  -d '{
    "email": "newuser@example.com",
    "display_name": "New User",
    "password": "strongpassword123"
  }'
```

#### 错误响应

| 状态码 | 条件                             |
| ------ | ------------------------------ |
| 409    | 邮箱已注册                     |
| 422    | 验证错误（弱密码、无效邮箱）   |

---

#### POST /api/auth/refresh

使用刷新令牌换取新的访问令牌/刷新令牌对。

**认证：** 公开（需要有效的刷新令牌）

#### 请求体

```json
{
  "refresh_token": "string"
}
```

#### 响应体

```json
{
  "access_token": "eyJhbGciOi...",
  "refresh_token": "eyJhbGciOi...",
  "token_type": "Bearer",
  "expires_in": 900
}
```

#### 示例

```bash
curl -X POST http://localhost:3001/api/auth/refresh \
  -H "Content-Type: application/json" \
  -d '{"refresh_token": "eyJhbGciOi..."}'
```

#### 错误响应

| 状态码 | 条件                             |
| ------ | ---------------------------------- |
| 401    | 刷新令牌无效或已过期            |

---

#### GET /api/auth/sso/authorize

启动 OIDC 授权码流程。将用户浏览器重定向到已配置的身份提供商。

**认证：** 公开

#### 查询参数

无。服务器生成随机的 `state` 和 `nonce`，将它们存储在 Redis 中，并构建 OIDC 授权 URL。

#### 响应

```
HTTP/1.1 302 Found
Location: https://idp.example.com/authorize?client_id=...&redirect_uri=...&state=...&nonce=...&scope=openid+email+profile&response_type=code
```

#### 示例

```bash
# Typically opened in a browser, not curl
curl -v http://localhost:3001/api/auth/sso/authorize
```

---

#### GET /api/auth/sso/callback

OIDC 回调端点。身份提供商在用户认证后重定向到此处。

**认证：** 公开

#### 查询参数

| 参数      | 类型   | 必填     | 描述                             |
| --------- | ------ | -------- | ------------------------------------ |
| `code`    | string | 是       | 提供商返回的授权码               |
| `state`   | string | 是       | CSRF 状态参数                    |

#### 响应体

```json
{
  "access_token": "eyJhbGciOi...",
  "refresh_token": "eyJhbGciOi...",
  "token_type": "Bearer",
  "expires_in": 900,
  "user": {
    "id": "uuid",
    "email": "sso-user@corp.com",
    "display_name": "SSO User",
    "role": "user"
  }
}
```

#### 错误响应

| 状态码 | 条件                                 |
| ------ | -------------------------------------- |
| 400    | 缺少 code 或 state                  |
| 401    | state 无效（CSRF 检查失败）          |
| 401    | 与身份提供商的令牌交换失败           |
| 500    | OIDC 未配置                          |

---

#### GET /api/auth/me

返回当前已认证用户的个人资料。

**认证：** JWT

#### 响应体

```json
{
  "id": "uuid",
  "email": "user@example.com",
  "display_name": "Jane Doe",
  "role": "user",
  "created_at": "2026-01-15T10:30:00Z",
  "updated_at": "2026-03-20T14:00:00Z"
}
```

#### 示例

```bash
curl http://localhost:3001/api/auth/me \
  -H "Authorization: Bearer eyJhbGciOi..."
```

#### 错误响应

| 状态码 | 条件                   |
| ------ | ---------------------- |
| 401    | JWT 缺失或无效        |

---

### API 密钥

#### GET /api/keys

列出当前认证用户的所有 API 密钥。

**认证：** JWT

#### 响应体

```json
[
  {
    "id": "uuid",
    "name": "Production Key",
    "prefix": "tw-prod",
    "allowed_models": ["gpt-4o", "claude-sonnet-4-20250514"],
    "rate_limit_rpm": 60,
    "expires_at": "2026-06-01T00:00:00Z",
    "created_at": "2026-01-15T10:30:00Z",
    "last_used_at": "2026-03-28T09:15:00Z"
  }
]
```

> **注意：** 创建后不会再返回完整密钥值。仅显示 `prefix` 用于标识。

#### 示例

```bash
curl http://localhost:3001/api/keys \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

#### POST /api/keys

创建新的 API 密钥。

**认证：** JWT

#### 请求体

```json
{
  "name": "string",
  "allowed_models": ["string"],
  "rate_limit_rpm": 60,
  "expires_in_days": 90
}
```

| 字段              | 类型            | 必填     | 默认值    | 描述                                     |
| ----------------- | --------------- | -------- | --------- | -------------------------------------------- |
| `name`            | string          | 是       | —         | 人类可读的名称                           |
| `allowed_models`  | array\<string\> | 否       | 全部      | 限制密钥可访问的特定模型                 |
| `rate_limit_rpm`  | integer         | 否       | 无限制    | 每分钟请求数限制                         |
| `expires_in_days` | integer         | 否       | 不过期    | 密钥过期前的天数                         |

#### 响应体

```json
{
  "id": "uuid",
  "name": "Production Key",
  "key": "tw-sk-a1b2c3d4e5f6...",
  "allowed_models": ["gpt-4o"],
  "rate_limit_rpm": 60,
  "expires_at": "2026-06-01T00:00:00Z",
  "created_at": "2026-03-28T10:00:00Z"
}
```

> **重要：** `key` 字段仅在创建时返回一次。请妥善保存。

#### 示例

```bash
curl -X POST http://localhost:3001/api/keys \
  -H "Authorization: Bearer eyJhbGciOi..." \
  -H "Content-Type: application/json" \
  -d '{
    "name": "CI/CD Pipeline",
    "allowed_models": ["gpt-4o"],
    "rate_limit_rpm": 120,
    "expires_in_days": 30
  }'
```

#### 错误响应

| 状态码 | 条件                   |
| ------ | ---------------------- |
| 401    | JWT 无效              |
| 422    | 验证错误              |

---

#### GET /api/keys/{id}

获取特定 API 密钥的详情。

**认证：** JWT

#### 路径参数

| 参数      | 类型 | 描述        |
| --------- | ---- | ----------- |
| `id`      | UUID | 密钥 ID     |

#### 响应体

```json
{
  "id": "uuid",
  "name": "Production Key",
  "prefix": "tw-prod",
  "allowed_models": ["gpt-4o"],
  "rate_limit_rpm": 60,
  "expires_at": "2026-06-01T00:00:00Z",
  "created_at": "2026-01-15T10:30:00Z",
  "last_used_at": "2026-03-28T09:15:00Z"
}
```

#### 错误响应

| 状态码 | 条件                              |
| ------ | ------------------------------- |
| 401    | JWT 无效                        |
| 404    | 密钥未找到或不属于当前用户      |

---

#### DELETE /api/keys/{id}

撤销 API 密钥。此操作不可逆。

**认证：** JWT

#### 路径参数

| 参数      | 类型 | 描述        |
| --------- | ---- | ----------- |
| `id`      | UUID | 密钥 ID     |

#### 响应

```
HTTP/1.1 204 No Content
```

#### 示例

```bash
curl -X DELETE http://localhost:3001/api/keys/550e8400-e29b-41d4-a716-446655440000 \
  -H "Authorization: Bearer eyJhbGciOi..."
```

#### 错误响应

| 状态码 | 条件                              |
| ------ | ---------------------------------- |
| 401    | JWT 无效                          |
| 404    | 密钥未找到或不属于当前用户        |

---

#### PATCH /api/keys/{id}

更新现有 API 密钥的设置。

**认证：** JWT

#### 路径参数

| 参数      | 类型 | 描述        |
| --------- | ---- | ----------- |
| `id`      | UUID | 密钥 ID     |

#### 请求体

```json
{
  "allowed_models": ["gpt-4o", "claude-sonnet-4-20250514"],
  "rate_limit_rpm": 120,
  "expires_in_days": 60,
  "rotation_interval_days": 90,
  "inactivity_timeout_days": 30
}
```

| 字段                      | 类型            | 必填     | 描述                                        |
| ------------------------- | --------------- | -------- | ------------------------------------------- |
| `allowed_models`          | array\<string\> | 否       | 更新允许的模型列表                          |
| `rate_limit_rpm`          | integer         | 否       | 更新每分钟请求数限制                        |
| `expires_in_days`         | integer         | 否       | 设置或更新过期时间（从现在起的天数）        |
| `rotation_interval_days`  | integer         | 否       | 设置自动轮换间隔                            |
| `inactivity_timeout_days` | integer         | 否       | 不活跃 N 天后自动禁用密钥                   |

#### 响应体

```json
{
  "id": "uuid",
  "name": "Production Key",
  "prefix": "tw-prod",
  "allowed_models": ["gpt-4o", "claude-sonnet-4-20250514"],
  "rate_limit_rpm": 120,
  "expires_at": "2026-08-01T00:00:00Z",
  "rotation_interval_days": 90,
  "inactivity_timeout_days": 30,
  "updated_at": "2026-04-01T10:00:00Z"
}
```

#### 示例

```bash
curl -X PATCH http://localhost:3001/api/keys/550e8400-e29b-41d4-a716-446655440000 \
  -H "Authorization: Bearer eyJhbGciOi..." \
  -H "Content-Type: application/json" \
  -d '{
    "rate_limit_rpm": 120,
    "inactivity_timeout_days": 30
  }'
```

#### 错误响应

| 状态码 | 条件                              |
| ------ | ---------------------------------- |
| 401    | JWT 无效                          |
| 404    | 密钥未找到或不属于当前用户        |
| 422    | 验证错误                          |

---

#### POST /api/keys/{id}/rotate

轮换 API 密钥。生成新的密钥值并返回。旧密钥进入宽限期（可配置），在此期间新旧密钥均被接受。

**认证：** JWT

#### 路径参数

| 参数      | 类型 | 描述        |
| --------- | ---- | ----------- |
| `id`      | UUID | 密钥 ID     |

#### 响应体

```json
{
  "id": "uuid",
  "name": "Production Key",
  "key": "tw-sk-newkey123456...",
  "grace_period_ends_at": "2026-04-02T10:00:00Z",
  "rotated_at": "2026-04-01T10:00:00Z"
}
```

> **重要：** 新的 `key` 值仅返回一次。旧密钥在 `grace_period_ends_at` 之前保持有效。

#### 示例

```bash
curl -X POST http://localhost:3001/api/keys/550e8400-e29b-41d4-a716-446655440000/rotate \
  -H "Authorization: Bearer eyJhbGciOi..."
```

#### 错误响应

| 状态码 | 条件                              |
| ------ | ---------------------------------- |
| 401    | JWT 无效                          |
| 404    | 密钥未找到或不属于当前用户        |

---

#### GET /api/keys/expiring

列出指定天数内即将过期的 API 密钥。

**认证：** JWT

#### 查询参数

| 参数      | 类型    | 必填     | 默认值  | 描述                             |
| --------- | ------- | -------- | ------- | -------------------------------- |
| `days`    | integer | 否       | 7       | 向前查看的天数                   |

#### 响应体

```json
[
  {
    "id": "uuid",
    "name": "Production Key",
    "prefix": "tw-prod",
    "expires_at": "2026-04-07T00:00:00Z",
    "days_remaining": 5,
    "owner_email": "user@example.com"
  }
]
```

#### 示例

```bash
curl "http://localhost:3001/api/keys/expiring?days=14" \
  -H "Authorization: Bearer eyJhbGciOi..."
```

#### 错误响应

| 状态码 | 条件        |
| ------ | ----------- |
| 401    | JWT 无效    |

---

### 管理员 -- 提供商

#### GET /api/admin/providers

列出所有已配置的 AI 提供商。

**认证：** JWT（管理员）

#### 响应体

```json
[
  {
    "id": "uuid",
    "name": "openai-prod",
    "display_name": "OpenAI Production",
    "provider_type": "openai",
    "base_url": "https://api.openai.com/v1",
    "is_active": true,
    "created_at": "2026-01-10T08:00:00Z"
  }
]
```

> **注意：** 响应中不会返回提供商的 `api_key`。

#### 示例

```bash
curl http://localhost:3001/api/admin/providers \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

#### POST /api/admin/providers

注册新的上游 AI 提供商。

**认证：** JWT（管理员）

#### 请求体

```json
{
  "name": "string",
  "display_name": "string",
  "provider_type": "string",
  "base_url": "string",
  "api_key": "string"
}
```

| 字段            | 类型   | 必填     | 描述                                             |
| --------------- | ------ | -------- | ---------------------------------------------------- |
| `name`          | string | 是       | 唯一标识符（如 `openai-prod`）                   |
| `display_name`  | string | 是       | 人类可读的名称                                   |
| `provider_type` | string | 是       | `openai`、`anthropic`、`google`、`azure`、`bedrock`、`custom` 之一    |
| `base_url`      | string | 是       | 提供商 API 基础 URL                              |
| `api_key`       | string | 是       | 提供商 API 密钥（使用 AES-256-GCM 静态加密存储）|

#### 响应体

```json
{
  "id": "uuid",
  "name": "openai-prod",
  "display_name": "OpenAI Production",
  "provider_type": "openai",
  "base_url": "https://api.openai.com/v1",
  "is_active": true,
  "created_at": "2026-03-28T10:00:00Z"
}
```

#### 示例

```bash
curl -X POST http://localhost:3001/api/admin/providers \
  -H "Authorization: Bearer eyJhbGciOi..." \
  -H "Content-Type: application/json" \
  -d '{
    "name": "anthropic-prod",
    "display_name": "Anthropic Production",
    "provider_type": "anthropic",
    "base_url": "https://api.anthropic.com",
    "api_key": "sk-ant-..."
  }'
```

#### 错误响应

| 状态码 | 条件                       |
| ------ | ---------------------------- |
| 401    | JWT 无效                   |
| 403    | 用户不是管理员             |
| 409    | 提供商名称已存在           |
| 422    | 验证错误                   |

---

#### GET /api/admin/providers/{id}

获取特定提供商的详情。

**认证：** JWT（管理员）

#### 路径参数

| 参数      | 类型 | 描述         |
| --------- | ---- | ------------ |
| `id`      | UUID | 提供商 ID    |

#### 响应体

与列表响应中的单个项目 Schema 相同。

#### 错误响应

| 状态码 | 条件               |
| ------ | ------------------ |
| 401    | JWT 无效           |
| 403    | 非管理员           |
| 404    | 提供商未找到       |

---

#### DELETE /api/admin/providers/{id}

删除提供商。引用其模型的现有密钥将无法再解析。

**认证：** JWT（管理员）

#### 响应

```
HTTP/1.1 204 No Content
```

#### 示例

```bash
curl -X DELETE http://localhost:3001/api/admin/providers/550e8400-e29b-41d4-a716-446655440000 \
  -H "Authorization: Bearer eyJhbGciOi..."
```

#### 错误响应

| 状态码 | 条件               |
| ------ | ------------------ |
| 401    | JWT 无效           |
| 403    | 非管理员           |
| 404    | 提供商未找到       |

---

### 管理员 -- 用户

#### GET /api/admin/users

列出系统中的所有用户。

**认证：** JWT（管理员）

#### 响应体

```json
[
  {
    "id": "uuid",
    "email": "admin@example.com",
    "display_name": "Admin User",
    "role": "admin",
    "created_at": "2026-01-01T00:00:00Z",
    "updated_at": "2026-03-15T12:00:00Z"
  }
]
```

#### 示例

```bash
curl http://localhost:3001/api/admin/users \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

#### POST /api/admin/users

创建新用户（管理员配置）。

**认证：** JWT（管理员）

#### 请求体

```json
{
  "email": "string",
  "display_name": "string",
  "password": "string",
  "role": "string"
}
```

| 字段           | 类型   | 必填     | 默认值   | 描述                                           |
| -------------- | ------ | -------- | -------- | ---------------------------------------------- |
| `email`        | string | 是       | —        | 唯一邮箱地址                                   |
| `display_name` | string | 是       | —        | 显示名称                                       |
| `password`     | string | 是       | —        | 初始密码                                       |
| `role`         | string | 否       | `user`   | `admin`、`operator`、`user`、`viewer`、`service` 之一 |

#### 响应体

```json
{
  "id": "uuid",
  "email": "newadmin@example.com",
  "display_name": "New Admin",
  "role": "admin",
  "created_at": "2026-03-28T10:00:00Z"
}
```

#### 示例

```bash
curl -X POST http://localhost:3001/api/admin/users \
  -H "Authorization: Bearer eyJhbGciOi..." \
  -H "Content-Type: application/json" \
  -d '{
    "email": "operator@example.com",
    "display_name": "Ops User",
    "password": "securepassword",
    "role": "operator"
  }'
```

#### 错误响应

| 状态码 | 条件                   |
| ------ | ------------------------ |
| 401    | JWT 无效               |
| 403    | 非管理员               |
| 409    | 邮箱已存在             |
| 422    | 验证错误               |

---

### 管理员 -- MCP 服务器

#### GET /api/mcp/servers

列出所有已注册的 MCP 服务器。

**认证：** JWT（管理员）

#### 响应体

```json
[
  {
    "id": "uuid",
    "name": "docs-search",
    "description": "Internal documentation search",
    "endpoint_url": "https://mcp.internal.corp/docs",
    "transport_type": "streamable_http",
    "auth_type": "bearer",
    "is_active": true,
    "tool_count": 3,
    "created_at": "2026-02-01T08:00:00Z"
  }
]
```

#### 示例

```bash
curl http://localhost:3001/api/mcp/servers \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

#### POST /api/mcp/servers

注册新的 MCP 服务器。

**认证：** JWT（管理员）

#### 请求体

```json
{
  "name": "string",
  "description": "string",
  "endpoint_url": "string",
  "transport_type": "string",
  "auth_type": "string",
  "auth_secret": "string"
}
```

| 字段             | 类型   | 必填     | 默认值             | 描述                                        |
| ---------------- | ------ | -------- | ------------------ | ----------------------------------------------- |
| `name`           | string | 是       | —                  | 唯一服务器名称                              |
| `description`    | string | 否       | —                  | 人类可读的描述                              |
| `endpoint_url`   | string | 是       | —                  | MCP 服务器 URL                              |
| `transport_type` | string | 否       | `streamable_http`  | 传输方式：`streamable_http` 或 `sse`        |
| `auth_type`      | string | 否       | `none`             | 认证方式：`none`、`bearer`、`header`、`query` |
| `auth_secret`    | string | 否       | —                  | 认证凭证（静态加密存储）                    |

#### 响应体

```json
{
  "id": "uuid",
  "name": "docs-search",
  "description": "Internal documentation search",
  "endpoint_url": "https://mcp.internal.corp/docs",
  "transport_type": "streamable_http",
  "auth_type": "bearer",
  "is_active": true,
  "created_at": "2026-03-28T10:00:00Z"
}
```

#### 示例

```bash
curl -X POST http://localhost:3001/api/mcp/servers \
  -H "Authorization: Bearer eyJhbGciOi..." \
  -H "Content-Type: application/json" \
  -d '{
    "name": "jira-tools",
    "description": "Jira issue management tools",
    "endpoint_url": "https://mcp.internal.corp/jira",
    "auth_type": "bearer",
    "auth_secret": "mcp-server-token-here"
  }'
```

#### 错误响应

| 状态码 | 条件                        |
| ------ | ----------------------------- |
| 401    | JWT 无效                    |
| 403    | 非管理员                    |
| 409    | 服务器名称已存在            |
| 422    | 验证错误                    |

---

#### GET /api/mcp/servers/{id}

获取特定 MCP 服务器的详情。

**认证：** JWT（管理员）

#### 路径参数

| 参数      | 类型 | 描述           |
| --------- | ---- | -------------- |
| `id`      | UUID | MCP 服务器 ID  |

#### 响应体

与列表响应中的单个项目 Schema 相同。

---

#### DELETE /api/mcp/servers/{id}

移除 MCP 服务器注册。活跃的会话将被终止。

**认证：** JWT（管理员）

#### 响应

```
HTTP/1.1 204 No Content
```

#### 示例

```bash
curl -X DELETE http://localhost:3001/api/mcp/servers/550e8400-e29b-41d4-a716-446655440000 \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

#### POST /api/mcp/servers/{id}/discover

触发 MCP 服务器的工具发现。连接到服务器，调用 `tools/list`，并将发现的工具存储到数据库中。

**认证：** JWT（管理员）

#### 路径参数

| 参数      | 类型 | 描述           |
| --------- | ---- | -------------- |
| `id`      | UUID | MCP 服务器 ID  |

#### 响应体

```json
{
  "server_id": "uuid",
  "tools_discovered": 5,
  "tools": [
    {
      "name": "search_issues",
      "description": "Search Jira issues by JQL",
      "input_schema": {
        "type": "object",
        "properties": {
          "jql": {"type": "string"}
        },
        "required": ["jql"]
      }
    }
  ]
}
```

#### 示例

```bash
curl -X POST http://localhost:3001/api/mcp/servers/550e8400-e29b-41d4-a716-446655440000/discover \
  -H "Authorization: Bearer eyJhbGciOi..."
```

#### 错误响应

| 状态码 | 条件                          |
| ------ | ------------------------------- |
| 401    | JWT 无效                      |
| 403    | 非管理员                      |
| 404    | 服务器未找到                  |
| 502    | 无法连接到 MCP 服务器        |

---

### MCP 工具

#### GET /api/mcp/tools

列出所有已注册服务器中发现的 MCP 工具。

**认证：** JWT

#### 响应体

```json
[
  {
    "id": "uuid",
    "server_id": "uuid",
    "server_name": "jira-tools",
    "name": "search_issues",
    "description": "Search Jira issues by JQL",
    "input_schema": {
      "type": "object",
      "properties": {
        "jql": {"type": "string"}
      },
      "required": ["jql"]
    }
  }
]
```

#### 示例

```bash
curl http://localhost:3001/api/mcp/tools \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

### 分析

#### GET /api/analytics/usage

获取一段时间内的用量数据（请求数、Token 数）。

**认证：** JWT

#### 查询参数

| 参数         | 类型   | 必填     | 默认值     | 描述                             |
| ------------ | ------ | -------- | ---------- | ------------------------------------ |
| `from`       | string | 否       | 7 天前     | 开始日期时间（ISO 8601）         |
| `to`         | string | 否       | 当前       | 结束日期时间（ISO 8601）         |
| `group_by`   | string | 否       | `day`      | 分组方式：`hour`、`day`、`week`  |
| `model`      | string | 否       | 全部       | 按模型名称筛选                   |

#### 响应体

```json
{
  "data": [
    {
      "period": "2026-03-27",
      "request_count": 1520,
      "prompt_tokens": 450000,
      "completion_tokens": 120000,
      "total_tokens": 570000
    }
  ]
}
```

#### 示例

```bash
curl "http://localhost:3001/api/analytics/usage?from=2026-03-01T00:00:00Z&group_by=day" \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

#### GET /api/analytics/usage/stats

获取聚合的用量统计数据（总计和平均值）。

**认证：** JWT

#### 响应体

```json
{
  "total_requests": 45000,
  "total_tokens": 12500000,
  "avg_latency_ms": 850,
  "models": {
    "gpt-4o": {"requests": 30000, "tokens": 9000000},
    "claude-sonnet-4-20250514": {"requests": 15000, "tokens": 3500000}
  }
}
```

#### 示例

```bash
curl http://localhost:3001/api/analytics/usage/stats \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

#### GET /api/analytics/costs

获取一段时间内的成本数据。

**认证：** JWT

#### 查询参数

| 参数         | 类型   | 必填     | 默认值     | 描述                            |
| ------------ | ------ | -------- | ---------- | ------------------------------- |
| `from`       | string | 否       | 7 天前     | 开始日期时间（ISO 8601）        |
| `to`         | string | 否       | 当前       | 结束日期时间（ISO 8601）        |
| `group_by`   | string | 否       | `day`      | 分组方式：`hour`、`day`、`week` |

#### 响应体

```json
{
  "data": [
    {
      "period": "2026-03-27",
      "total_cost_usd": 45.20,
      "by_model": {
        "gpt-4o": 32.10,
        "claude-sonnet-4-20250514": 13.10
      }
    }
  ]
}
```

#### 示例

```bash
curl "http://localhost:3001/api/analytics/costs?from=2026-03-01T00:00:00Z" \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

#### GET /api/analytics/costs/stats

获取聚合的成本统计数据。

**认证：** JWT

#### 响应体

```json
{
  "total_cost_usd": 1250.00,
  "avg_daily_cost_usd": 41.67,
  "by_model": {
    "gpt-4o": 890.00,
    "claude-sonnet-4-20250514": 360.00
  }
}
```

#### 示例

```bash
curl http://localhost:3001/api/analytics/costs/stats \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

### 审计

#### GET /api/audit/logs

搜索审计日志条目。由 ClickHouse 支持 SQL 查询和分析。

**认证：** JWT（管理员）

#### 查询参数

| 参数      | 类型    | 必填     | 默认值  | 描述                               |
| --------- | ------- | -------- | ------- | -------------------------------------- |
| `q`       | string  | 否       | —       | 全文搜索查询                       |
| `from`    | string  | 否       | —       | 开始日期时间（ISO 8601）           |
| `to`      | string  | 否       | —       | 结束日期时间（ISO 8601）           |
| `limit`   | integer | 否       | 50      | 结果数量（最大 1000）              |
| `offset`  | integer | 否       | 0       | 分页偏移量                         |

#### 响应体

```json
{
  "total": 2340,
  "offset": 0,
  "limit": 50,
  "entries": [
    {
      "id": "uuid",
      "timestamp": "2026-03-28T09:15:00Z",
      "user_id": "uuid",
      "user_email": "admin@example.com",
      "action": "provider.create",
      "resource_type": "provider",
      "resource_id": "uuid",
      "details": {"name": "openai-prod"},
      "ip_address": "10.0.1.50",
      "user_agent": "Mozilla/5.0..."
    }
  ]
}
```

#### 常用操作值

| 操作                 | 描述                         |
| -------------------- | ---------------------------- |
| `auth.login`         | 用户登录                     |
| `auth.login_failed`  | 登录尝试失败                 |
| `auth.register`      | 用户注册                     |
| `key.create`         | API 密钥已创建               |
| `key.revoke`         | API 密钥已撤销               |
| `provider.create`    | 提供商已添加                 |
| `provider.delete`    | 提供商已移除                 |
| `mcp_server.create`  | MCP 服务器已注册             |
| `mcp_server.delete`  | MCP 服务器已移除             |
| `user.create`        | 管理员创建了用户             |

#### 示例

```bash
curl "http://localhost:3001/api/audit/logs?q=provider.create&from=2026-03-01T00:00:00Z&limit=20" \
  -H "Authorization: Bearer eyJhbGciOi..."
```

#### 错误响应

| 状态码 | 条件               |
| ------ | ------------------ |
| 401    | JWT 无效           |
| 403    | 非管理员           |

---

### 访问日志

#### 查询访问日志

`GET /api/admin/access-logs`

Gateway 和 Console 端口的 HTTP 请求日志。

**查询参数：**

| 参数 | 类型 | 说明 |
|------|------|------|
| `method` | string | HTTP 方法 (GET, POST 等) |
| `path` | string | 请求路径（子串匹配） |
| `status_code` | string | HTTP 状态码 |
| `port` | string | 端口号 (3000 或 3001) |
| `user_id` | string | 用户 UUID |
| `q` | string | 路径全文搜索 |
| `from` | string | 开始时间 (ISO 8601) |
| `to` | string | 结束时间 (ISO 8601) |
| `limit` | integer | 最大返回数 (默认 50，最大 200) |
| `offset` | integer | 分页偏移量 |

### 应用日志

#### 查询应用日志

`GET /api/admin/app-logs`

应用运行时追踪日志。

**查询参数：**

| 参数 | 类型 | 说明 |
|------|------|------|
| `level` | string | 日志级别 (TRACE, DEBUG, INFO, WARN, ERROR) |
| `target` | string | 模块/目标（子串匹配） |
| `q` | string | 消息搜索（子串匹配） |
| `from` | string | 开始时间 (ISO 8601) |
| `to` | string | 结束时间 (ISO 8601) |
| `limit` | integer | 最大返回数 (默认 50，最大 200) |
| `offset` | integer | 分页偏移量 |

### 平台日志

#### 查询平台日志

`GET /api/admin/platform-logs`

平台管理操作审计记录。

**查询参数：**

| 参数 | 类型 | 说明 |
|------|------|------|
| `user_id` | string | 用户 UUID |
| `action` | string | 操作名称 |
| `resource` | string | 资源类型 |
| `resource_id` | string | 资源 UUID |
| `from` | string | 开始时间 (ISO 8601) |
| `to` | string | 结束时间 (ISO 8601) |
| `limit` | integer | 最大返回数 (默认 50，最大 200) |
| `offset` | integer | 分页偏移量 |

### 网关日志

#### 查询网关日志

`GET /api/gateway/logs`

AI API 请求日志，包含模型、Provider、Token 和费用数据。

**查询参数：**

| 参数 | 类型 | 说明 |
|------|------|------|
| `model` | string | 模型 ID |
| `provider` | string | Provider 名称 |
| `user_id` | string | 用户 UUID |
| `api_key_id` | string | API Key UUID |
| `status_code` | integer | HTTP 状态码 |
| `from` | string | 开始时间 (ISO 8601) |
| `to` | string | 结束时间 (ISO 8601) |
| `sort_by` | string | `cost_usd`、`latency_ms` 或 `created_at`（默认） |
| `limit` | integer | 最大返回数 (默认 50，最大 200) |
| `offset` | integer | 分页偏移量 |

### MCP 日志

#### 查询 MCP 日志

`GET /api/mcp/logs`

MCP 工具调用日志。

**查询参数：**

| 参数 | 类型 | 说明 |
|------|------|------|
| `user_id` | string | 用户 UUID |
| `server_id` | string | MCP 服务器 UUID |
| `tool_name` | string | 工具名称 |
| `status` | string | 调用状态 |
| `from` | string | 开始时间 (ISO 8601) |
| `to` | string | 结束时间 (ISO 8601) |
| `sort_by` | string | `duration_ms` 或 `created_at`（默认） |
| `limit` | integer | 最大返回数 (默认 50，最大 200) |
| `offset` | integer | 分页偏移量 |

---

### 管理员设置

#### GET /api/admin/settings

获取按类别分组的所有设置。

**认证：** JWT（管理员）

#### 响应体

```json
{
  "auth": {
    "jwt_access_ttl_seconds": 900,
    "jwt_refresh_ttl_seconds": 604800
  },
  "cache": {
    "cache_ttl_seconds": 300
  },
  "security": {
    "signature_drift_seconds": 300,
    "nonce_ttl_seconds": 300,
    "content_filter_patterns": [],
    "pii_patterns": []
  },
  "budget": {
    "budget_warning_threshold": 0.8,
    "budget_critical_threshold": 0.95
  },
  "keys": {
    "api_key_max_expiry_days": 365,
    "api_key_default_rate_limit_rpm": 60
  },
  "general": {
    "data_retention_days": 30,
    "site_name": "ThinkWatch"
  }
}
```

#### 示例

```bash
curl http://localhost:3001/api/admin/settings \
  -H "Authorization: Bearer eyJhbGciOi..."
```

---

#### PATCH /api/admin/settings

更新一个或多个设置。设置在持久化之前会进行验证。

**认证：** JWT（管理员）

#### 请求体

```json
{
  "settings": {
    "jwt_access_ttl_seconds": 1800,
    "site_name": "My AI Gateway",
    "data_retention_days": 60
  }
}
```

| 字段       | 类型   | 必填     | 描述                               |
| ---------- | ------ | -------- | ---------------------------------- |
| `settings` | object | 是       | 要更新的设置键值对                 |

#### 响应体

```json
{
  "updated": ["jwt_access_ttl_seconds", "site_name", "data_retention_days"],
  "settings": {
    "jwt_access_ttl_seconds": 1800,
    "site_name": "My AI Gateway",
    "data_retention_days": 60
  }
}
```

#### 示例

```bash
curl -X PATCH http://localhost:3001/api/admin/settings \
  -H "Authorization: Bearer eyJhbGciOi..." \
  -H "Content-Type: application/json" \
  -d '{
    "settings": {
      "jwt_access_ttl_seconds": 1800,
      "data_retention_days": 90
    }
  }'
```

#### 错误响应

| 状态码 | 条件                              |
| ------ | --------------------------------- |
| 401    | JWT 无效                          |
| 403    | 非管理员                          |
| 422    | 验证错误（无效的键或值）          |

---

#### GET /api/admin/settings/category/{category}

获取特定类别的设置。

**认证：** JWT（管理员）

#### 路径参数

| 参数       | 类型   | 描述                                                                           |
| ---------- | ------ | ------------------------------------------------------------------------------ |
| `category` | string | `auth`、`cache`、`security`、`budget`、`keys`、`general`、`system`、`oidc`、`audit` 之一 |

#### 响应体

仅返回所请求类别的设置（与 `GET /api/admin/settings` 中对应部分的结构相同）。

#### 示例

```bash
curl http://localhost:3001/api/admin/settings/category/auth \
  -H "Authorization: Bearer eyJhbGciOi..."
```

#### 错误响应

| 状态码 | 条件               |
| ------ | ------------------ |
| 401    | JWT 无效           |
| 403    | 非管理员           |
| 404    | 未知类别           |

---

> **注意：** 旧版端点 `GET /api/admin/settings/system`、`GET /api/admin/settings/oidc` 和 `GET /api/admin/settings/audit` 仍然可用，作为 `GET /api/admin/settings/category/{category}` 对应类别的别名。

---

### 控制台健康检查

#### GET /api/health

详细的健康检查，报告与后端服务的连通性、延迟指标和连接池状态。

**认证：** 公开

#### 响应体（健康）

```json
{
  "status": "ok",
  "pg_latency_ms": 2.5,
  "redis_latency_ms": 0.8,
  "clickhouse_latency_ms": 5.1,
  "pool_idle": 8,
  "pool_active": 2,
  "uptime_seconds": 86400,
  "services": {
    "postgres": "ok",
    "redis": "ok",
    "clickhouse": "ok"
  }
}
```

#### 响应体（降级）

如果关键依赖（PostgreSQL 或 Redis）不可达，端点返回 HTTP 503：

```json
{
  "status": "degraded",
  "pg_latency_ms": null,
  "redis_latency_ms": 0.8,
  "clickhouse_latency_ms": 5.1,
  "pool_idle": 0,
  "pool_active": 0,
  "uptime_seconds": 86400,
  "services": {
    "postgres": "error",
    "redis": "ok",
    "clickhouse": "ok"
  }
}
```

| 字段                 | 类型    | 描述                                     |
| -------------------- | ------- | ---------------------------------------- |
| `status`             | string  | `ok` 或 `degraded`                       |
| `pg_latency_ms`      | number  | PostgreSQL 延迟（不可达时为 null）       |
| `redis_latency_ms`   | number  | Redis 延迟（不可达时为 null）            |
| `clickhouse_latency_ms`| number  | ClickHouse 延迟（不可达时为 null）       |
| `pool_idle`          | integer | 空闲数据库连接数                         |
| `pool_active`        | integer | 活跃数据库连接数                         |
| `uptime_seconds`     | integer | 服务器运行时间（秒）                     |

#### 示例

```bash
curl http://localhost:3001/api/health
```

| 状态码 | 条件                                    |
| ------ | --------------------------------------- |
| 200    | 所有服务健康                            |
| 503    | 关键依赖（PG 或 Redis）不可用          |
