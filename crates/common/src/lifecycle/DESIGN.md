# common/lifecycle — request pipeline DESIGN

Status: **decisions locked**. Code is being implemented to match
this document. If the implementation diverges, update this file in
the same commit.

## Problem

`crates/gateway/src/proxy.rs::proxy_chat_completion` and
`crates/mcp-gateway/src/proxy.rs::handle_tools_call` are two
implementations of the same lifecycle:

```
identity (from middleware)
  → rate-limit + budget gate
  → access control (allowed_models / allowed_mcp_tools)
  → cache lookup (short-circuit on hit)
  → circuit-breaker check
  → credential resolution
  → upstream call (buffered or streaming)
  → circuit-breaker accounting
  → cache write
  → audit emit (gateway_logs or mcp_logs)
```

The two crates have **parallel** code for each step. Observed
costs:

- The "JSON-RPC error code → breaker failure" rule for MCP was
  copy-pasted **three** times before B1 consolidated them
  ([b6306b0](commit)).
- Rate-limit fail-closed logic had to be fixed twice — once per
  gateway.
- The streaming on-done audit pattern in `mcp-gateway::proxy` was
  written from scratch this session; the AI gateway side
  ([gateway::streaming::stream_to_sse_with_restorer](crates/gateway/src/streaming.rs))
  has the same shape with subtle differences.
- Adding a new "PII redaction stage" or "content filter stage"
  requires editing both crates in parallel.

The shared engine is already there (`common::limits::sliding`,
`common::audit::AuditLogger`). What's missing is the **glue**: a
typed, surface-agnostic representation of the lifecycle that lets
both crates compose the same stages.

## Non-goals

- We are not building a generic middleware framework. Tower / axum
  already exist. This is for the **provider-side** lifecycle that
  runs AFTER the HTTP layer accepted the request.
- We are not unifying the wire formats. OpenAI chat-completion,
  Anthropic messages, JSON-RPC stay distinct — each surface owns
  its request/response/audit-row types via the [`Surface`] trait.
- We are not introducing dynamic dispatch into the hot path. Stages
  compose at compile time via concrete state-struct transitions.

## Architecture — decisions

### 1. Stages are async functions, not trait impls

```rust
pub async fn check_limits<S: Surface>(
    state: Raw<S>,
    rules: &[RateLimitRule],
    redis: &fred::clients::Client,
    audit: &AuditLogger,
) -> Result<LimitsChecked<S>, S::Response>
```

Not `trait Stage<I, O> { async fn run(...) }`. Reasoning:

- We don't need dynamic stage composition at runtime — every
  surface knows its full pipeline at compile time.
- A `Stage` trait would force every stage to take a single `&self`
  config struct, OR thread the config through generic bounds. Both
  add type plumbing without giving us anything Stage-trait-specific.
- Pure async fns are testable in isolation with `tokio::test` — no
  mock framework needed.
- Per-stage instrumentation is `#[tracing::instrument]` on the fn.

### 2. Type-state via separate state structs, not phantom markers

```rust
pub struct Raw<S: Surface> { /* identity, body, trace_id, … */ }
pub struct LimitsChecked<S: Surface> { /* + limit_result */ }
pub struct Authorized<S: Surface>    { /* + authorization */ }
pub struct CacheChecked<S: Surface>  { /* + cache_key */ }
…
```

Not `RequestState<S, Phantom<Marker>>`. Reasoning:

- Phantom markers don't actually prevent field access at runtime.
  Separate structs do — you literally cannot read `limit_result`
  from a `Raw<S>` because the field doesn't exist.
- Adding a field that one stage produces and a later stage consumes
  is a localised edit (add field to one struct, plumb through
  transition fns). No risk of "stage X reads a field that was
  supposed to be set by stage Y but isn't".
- Move semantics: each stage consumes `Raw<S>` and returns
  `LimitsChecked<S>`. Compiler-enforced single-ownership of state.
  No "did someone already process this?" confusion.

The cost (verbose struct definitions per stage) is paid once,
visible in `state.rs`, and exactly mirrors the lifecycle diagram.

### 3. Short-circuit returns `Err(S::Response)`, not a sum type

```rust
async fn check_limits<S>(…) -> Result<LimitsChecked<S>, S::Response>
```

When a stage decides "render this response, skip the rest", it
returns `Err(response)`. The surface handler unwraps via `?` —
short-circuits and normal completion thread through the same
control flow.

Stages that short-circuit **also emit their own audit row** before
returning the response. Each stage is responsible for the audit
detail it produces (rate_limited / access_denied / breaker_open /
…). The `EmitAudit` terminal stage handles the success path only.

### 4. `Surface` trait carries the per-surface associated types

```rust
pub trait Surface {
    type Identity: Send + Sync + 'static;
    type RequestBody: Send + Sync + 'static;
    type Response: Send + Sync + 'static;
    type AuditDetail: Send + Sync + 'static;

    /// The audit "actor" each surface defines (McpActor, GatewayActor).
    /// Used by `emit_audit` to attribute the row.
    fn actor(identity: &Self::Identity) -> Box<dyn AuditActor>;

    /// Surface-specific factory for the "rate-limited" response shape.
    /// MCP returns a JSON-RPC error; the AI gateway returns a 429.
    fn rate_limited_response(reason: &str) -> Self::Response;

    /// `access_denied`, `cache_unavailable`, …. Same idea.
    fn access_denied_response(reason: &str) -> Self::Response;
    // …
}

pub struct McpSurface;
impl Surface for McpSurface {
    type Identity = McpRequestIdentity;
    type RequestBody = JsonRpcRequest;
    type Response = JsonRpcResponse;
    type AuditDetail = McpAuditDetail;
    // …
}
```

Stage code reads `S::Identity`, `S::Response`, etc. — never knows
the concrete shapes. Adding a new surface (a future MCP-over-WS, an
SSE-only proxy, …) is one trait impl + the surface-specific
`InvokeUpstream` stage.

### 5. Streaming is deferred to phase 2

The streaming path (`InvokeUpstream` returns a body that the
downstream client consumes incrementally, while record_outcome /
write_cache / emit_audit run in a detached task) needs a
working prototype before locking the trait shape. The current
`mcp-gateway::proxy::streaming::build_chunk_passthrough` handles
the equivalent ad-hoc, and that pattern is what we'll generalise.

Phase 1 implements buffered-only stages. The migration will
preserve the existing streaming code path; phase 2 generalises it.

### 6. Errors

`StageError` is a structured enum in `common::lifecycle::error`
covering the **infrastructure** failures stages can have (Redis
down, audit pipeline backed up, …). User-attributable failures
(rate limited, denied, etc.) are not errors — they're short-
circuit responses.

```rust
pub enum StageError {
    /// Cache backing store unavailable.
    CacheUnavailable(anyhow::Error),
    /// Redis-backed limiter unavailable AND fail-closed mode.
    RateLimiterUnavailable(anyhow::Error),
    /// Audit pipeline rejected the row (shouldn't happen — bounded
    /// channel).
    AuditChannelFull,
    /// Catch-all for unexpected upstream / DB failures.
    Internal(anyhow::Error),
}
```

The surface handler maps `StageError` → its wire-format error.
There is exactly one place per surface that does this conversion.

## Stage inventory

| Stage              | In               | Out                  | Surface-specific? |
|--------------------|------------------|----------------------|---|
| `check_limits`     | `Raw<S>`         | `LimitsChecked<S>`   | No |
| `check_budget`     | `LimitsChecked<S>` | `BudgetChecked<S>` | No |
| `check_access`     | `BudgetChecked<S>` | `Authorized<S>`    | No (consumes `S::Identity`'s access policy) |
| `check_cache`     | `Authorized<S>`  | `CacheChecked<S>`    | Partial (cache trait is shared, key derivation is surface) |
| `check_breaker`    | `CacheChecked<S>` | `BreakerChecked<S>` | No |
| `resolve_credential` | `BreakerChecked<S>` | `CredentialResolved<S>` | Yes (MCP has UserTokenResolver; AI gateway has provider keys) |
| `invoke_upstream`  | `CredentialResolved<S>` | `Invoked<S>` | Yes (the actual HTTP call) |
| `record_outcome`   | `Invoked<S>`     | `Invoked<S>`         | No (uses common breaker abstraction) |
| `write_cache`      | `Invoked<S>`     | `Invoked<S>`         | No |
| `emit_audit`       | `Invoked<S>`     | `Emitted<S>`         | No (uses `S::AuditDetail`) |

`Emitted<S>` is the terminal state. The surface unpacks it to its
wire-format response.

## Migration plan

Since the project is unreleased and we don't carry backward-compat
debt, the migration is a single rewrite per surface, not a dual-
path cohabitation.

**Phase 1 (this session if scope allows, else next)**
- Build `common::lifecycle::*`: Surface trait, state structs,
  StageError, AND one fully-working stage (`check_limits`).
- Unit-test the stage against a fake Surface impl.
- Code goes into `common` crate; no surface uses it yet.

**Phase 2 (next session)**
- Implement remaining stages.
- Migrate `mcp-gateway::proxy::handle_tools_call` to use the
  pipeline. The old implementation is **deleted** in the same
  commit. Integration tests catch parity bugs.

**Phase 3 (next-next session)**
- Migrate `gateway::proxy::proxy_chat_completion` and its Anthropic
  / Responses siblings. This is what was previously called "B2".

There is no feature flag, no dual-path, no opt-in. If phase 2
breaks an integration test, the migration commit gets reworked
until it passes. If phase 3 reveals a stage abstraction that
doesn't fit Anthropic's wire format, the abstraction gets revised
and phase 2 is updated to match.

## What we won't do until it earns its keep

- **Pipeline builder type** (`pipeline().then(...).then(...)`) — adds
  generic plumbing without saving lines. Each surface's pipeline is
  a `~30-line async fn` with `let s = stage_n(s, ...).await?;`
  lines. That's the canonical readable shape.
- **dyn Stage dispatch** — no use case.
- **Per-stage middleware composition** — no use case.
- **A `Pipeline` macro** — no use case.

If any of these ever has a use case, we add them then. Not now.

## Estimate (revised down from sketch)

- Phase 1: 1 session.
- Phase 2: 1-2 sessions.
- Phase 3: 2 sessions.

Total: **4-5 sessions**, not the 10 sketched in the earlier draft.
The earlier estimate budgeted for the dual-path migration; we're
skipping that.
