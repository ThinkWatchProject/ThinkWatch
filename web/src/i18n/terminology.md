# i18n terminology

Canonical bilingual mapping for product nouns. **Always copy from
this table** — picking a synonym in a new key drifts the surface and
forces a future cleanup.

| English          | 中文        | Notes |
| ---------------- | ----------- | ----- |
| Gateway          | 网关        | The proxy itself. Capital G when it's a section title. |
| AI Gateway       | AI 网关     | The chat-completion / messages proxy surface. |
| MCP Gateway      | MCP 网关    | The Model Context Protocol tool-call proxy. |
| Console          | 控制台      | The admin web UI. Don't translate as 后台. |
| API key          | API 密钥    | Lowercase `key` in English; `密钥` not `钥匙`. |
| Provider         | 供应商      | The upstream LLM company (OpenAI, Anthropic, …). |
| Model            | 模型        | The named model (gpt-4o, claude-sonnet-4-6, …). |
| Team             | 团队        | IAM-group-style permission container; **not** a tenant. |
| Role             | 角色        | RBAC role. Avoid 权限组 in zh — that's how teams might be misread. |
| Permission       | 权限        | Single capability key (`api_keys:create`). |
| Tool             | 工具        | An MCP-discoverable callable. |
| MCP Server       | MCP 服务    | A registered upstream MCP endpoint. |
| Trace ID         | 追踪 ID     | Per-request correlation id. Don't translate as 跟踪号. |
| Audit log        | 审计日志    | The compliance-grade event stream. |
| Usage            | 用量        | Token / request volume metrics. |
| Cost             | 成本        | Cost reporting (USD by default). |
| Budget           | 预算        | Natural-period limits in `budget_caps`. |
| Rate limit       | 限流        | Sliding-window limits in `rate_limit_rules`. |
| Webhook outbox   | Webhook 出箱 | Durable retry queue. |
| Forwarder        | 转发器      | Audit log forwarder (syslog / Kafka / webhook). |

## Style

- Capitalise product nouns in English (Gateway, MCP, Team) when
  they are section titles or first sentence; lowercase when used as
  a common noun ("the gateway proxies …").
- Chinese keys never use full-width parens around English acronyms
  (e.g. write `MCP 服务`, not `MCP（服务）`).
- Don't pluralise "MCP" or "API" — they're acronyms, not nouns.
- Time strings always render UTC; if a UI element shows local time
  it must say so.

## Adding a new term

1. Add the row above with both languages.
2. Add the i18n key under the relevant `*.json` section in lockstep
   (the `pnpm check:i18n` script enforces parity).
3. If the term is ambiguous (e.g. "user" can mean account vs.
   end-user), include a Notes column entry.
