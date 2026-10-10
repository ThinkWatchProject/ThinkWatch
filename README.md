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

**A self-hosted AI API and MCP gateway for organizations.** Every model request and every MCP tool call passes through one gateway, where it is authenticated against the organization's identity provider, checked against limits and budgets, inspected by security guards, priced, and written to the audit log. It plays the role for AI access that a bastion host plays for server access.

**For individual developers:** [ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite) is a free desktop app (MIT) that runs a local gateway for Claude Code, Codex and other AI clients on macOS, Windows and Linux.

**Sponsors:** [Want to appear here?](mailto:fylorn@outlook.com?subject=ThinkWatch%20Lite%20Sponsorship)

```
                    ┌──────────────────────────────────────┐
 Claude Code ──────>│                                      │──> OpenAI
 Cursor ───────────>│    Gateway  :3000                    │──> Anthropic
 Custom Agent ─────>│    AI API + MCP                      │──> Google Gemini
 CI/CD Pipeline ───>│                                      │──> Azure OpenAI / AWS Bedrock
                    └──────────────────────────────────────┘
                    ┌──────────────────────────────────────┐
 Admin Browser ────>│    Console  :3001                    │
                    │    Management UI + Admin API         │
                    └──────────────────────────────────────┘
```

## Highlights

- **MCP tool calls run as the real user.** Each user connects their own GitHub, Notion, Linear, Slack or Atlassian account through OAuth or a personal token, so the upstream's own audit log shows who acted. Tokens are encrypted at rest, tool lists are cached per user, and each tool can be granted per role and per API key.
- **Security guards on every request.** Outbound redaction replaces credentials and personal data anywhere in a request with placeholders such as `<<TW_EMAIL_1>>` before it goes upstream, and restores them in the answer, streamed ones included. Tool-call inspection checks the tool calls in each response for dangerous commands, and the content filter looks for prompt-injection phrases and hidden characters in what the caller sent, then refuses the request, deletes them or records them.
- **Identity from the organization's directory.** Sign-in works through any OIDC provider (Zitadel, Okta, Azure AD and others), with optional TOTP. Five built-in roles, from Super Admin to Viewer, and custom roles decide who may use which models, tools and admin pages.
- **One key for AI and MCP.** Users receive `tw-` virtual keys that can be scoped to the AI gateway, the MCP gateway or both. Keys are stored only as hashes and rotate with a grace period.
- **Rate limits and budgets.** Sliding windows from one minute to one week limit requests or tokens, and daily, weekly or monthly budgets cap spending. Both attach to users, API keys or roles, and rate limits apply to MCP tool calls as well as model requests.
- **Cost accounting that finance can use.** Spend is reported by model, user, provider and cost center, with CSV chargeback reports and a month-end forecast. Per-model weights make expensive models count for more against the same quota.
- **Audit trail in ClickHouse.** Every model request and tool call is recorded with user, parameters, response, latency and errors, and captured bodies can be redacted with the outbound redaction rules before storage (off by default). Events can be forwarded to a SIEM over Syslog, Kafka (through a REST proxy) or signed webhooks.
- **One endpoint for every client.** OpenAI Chat Completions, OpenAI Responses, Anthropic Messages and Gemini requests are served on one port and converted to whatever the upstream speaks. Routing spreads traffic by weight, latency or health, and a circuit breaker takes failing upstreams out of rotation.

## Quick start

```bash
# 1. Start infrastructure
make infra

# 2. Generate dev secrets + start backend (gateway :3000 + console :3001)
make dev-secrets       # writes .env from .env.example with random secrets
make dev-backend

# 3. Start frontend dev server
cd web && pnpm install && pnpm dev

# 4. Complete the setup wizard at http://localhost:5173/setup
```

The setup wizard creates the first Super Admin account, sets the site name and issues a first API key; providers are added in the console afterwards. The console then has copy-paste setup instructions for Claude Code, Cursor, Continue, Cline, the OpenAI and Anthropic SDKs, and cURL.

## Deployment

| Option | Command | Notes |
|---|---|---|
| Docker Compose | `make deploy` | Generates `.env.production` with random secrets on first run |
| Kubernetes | `make helm-deploy` | Helm chart in `deploy/helm/think-watch`; secrets are generated on install and kept on upgrade |

The gateway (port `3000`) is the only part that clients need to reach. The console (port `3001`) serves the management UI and admin API and belongs behind a VPN or firewall. See the **[Deployment Guide](https://thinkwat.ch/docs/deployment-guide)** for TLS, hardening and production settings.

## Behaviour worth knowing

**MCP identity**
- A user may connect several accounts to one server (work and personal, for example) and pin each `tw-` key to one of them.
- Adding a server takes its URL: the gateway discovers the OAuth endpoints and registers itself when the upstream supports dynamic client registration. The MCP Store ships 37 ready-made templates.
- A user who has not yet connected an account still sees the tool list; calling a tool returns JSON-RPC error `-32050` with the authorization URL, which compliant MCP clients can show.
- Responses from servers that use per-user credentials are cached per user and account, never shared.

**Security guards**
- There are three guards, each with three modes: off, observe, and one named for what it does — replace (outbound redaction), cut off (tool-call inspection) and enforce (content filter). A new installation starts all three in observe mode: hits go to the audit log and nothing is changed until a guard is switched to its third mode.
- Every rule is listed on the console's security page, built-in and custom. Built-in rules can be switched on or off, tool-call and content rules can take another action, custom rules can be added, and a sample can be tried against one rule or a whole guard first.
- Outbound redaction searches the whole request, system prompt and earlier answers included, but not base64 payloads. A match becomes `<<TW_LABEL_n>>` — `SECRET` for credentials, `ID_NUMBER`, `CARD_NUMBER`, `EMAIL` and `PHONE` for personal data, a label of its own for a custom rule — and is restored in the answer. E-mail addresses and phone numbers ship switched off.
- A content rule matches a phrase, a regular expression or code points (`U+200B`, `U+E0000–U+E007F`), and either refuses the request, deletes what it matched from the caller's messages and tool results, or records only. Hidden characters are content rules: Unicode tag characters and bidirectional controls ship on, zero-width and private-use characters off.
- A tool-call rule cuts the response at the call or records it. Besides the dangerous-command rules, two built-in rules catch a credential sent to an unknown host and a local file uploaded to an external host.
- A model's maximum output tokens, set on the Models page, caps `max_tokens` on every request to that model; it replaces the old output length guardrail.

**Limits and budgets**
- Every limit and budget is checked before the request, against what earlier requests used; tokens are counted after the response, so one request can cross a token limit or a budget before the next is refused. A refused request counts against nothing, and its `429` says in `Retry-After` when the limit frees.
- A limit set on an API key applies on top of its owner's, on a counter of its own.
- A route's RPM and TPM caps count over a sliding minute. A route at its cap is skipped for the model's next route; when every route is at its cap, the request gets `429`. Such a request has passed the caller's own limits and counts on them.
- `GET /v1/usage` on the gateway port tells a client, with its API key, what it has used today and this month and what room its limits leave.
- Redis can be a single node or a Redis Cluster.
- If Redis is unavailable, or the database fails while a request's limits are loaded, limits fail open by default. Setting `security.rate_limit_fail_closed` refuses those requests instead.
- Budget alerts fire once per period at 50%, 80%, 95% and 100%.

## Documentation

Product page: **[thinkwat.ch/thinkwatch](https://thinkwat.ch/thinkwatch)** · Full documentation: **[thinkwat.ch/docs](https://thinkwat.ch/docs)**

| Document | Description |
|---|---|
| [Architecture](https://thinkwat.ch/docs/architecture) | System design, dual-port model, data flow |
| [Deployment Guide](https://thinkwat.ch/docs/deployment-guide) | Docker Compose, Kubernetes, TLS, production hardening |
| [Configuration](https://thinkwat.ch/docs/configuration) | Environment variables and settings |
| [API Reference](https://thinkwat.ch/docs/api-reference) | Gateway and console endpoints |
| [Security](https://thinkwat.ch/docs/security) | Auth model, encryption, RBAC, threat model |
| [Secret Rotation](https://thinkwat.ch/docs/secret-rotation) | Rotating provider keys, JWT secrets and admin credentials |

## Built on ThinkWatch Core

ThinkWatch uses four crates from [ThinkWatch Core](https://github.com/ThinkWatchProject/ThinkWatch-Core) (MIT): `tw-dialect` for conversion between API formats and usage parsing, `tw-guard` for redaction, tool-call inspection and the other guards, `tw-breaker` for the circuit-breaker state machine, and `tw-bedrock` for Amazon Bedrock signing, event streams and the model catalog.

[ThinkWatch Lite](https://github.com/ThinkWatchProject/ThinkWatch-Lite) is the desktop edition for individual developers, a local gateway for Claude Code, Codex and other clients on macOS, Windows and Linux (MIT).

## Contributing

Contributions are welcome. Please open an issue to discuss before submitting a PR for major changes.

## License

ThinkWatch is source-available under the [Business Source License 1.1](LICENSE).
Non-production use is free. Production use is free up to both `10,000,000`
Billable Tokens and `10,000` MCP Tool Calls per UTC calendar month; above
either threshold, a commercial license is required and priced by usage tiers.

See [LICENSING.md](LICENSING.md) for the production-use thresholds, the
Billable Token and MCP Tool Call definitions, the tiering model, and the
changeover to `GPL-2.0-or-later`.
