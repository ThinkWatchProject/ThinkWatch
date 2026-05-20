# common/lifecycle — request pipeline DESIGN

Status: **draft, for review**. No code lands until this design is acked.

## Problem

`crates/gateway/src/proxy.rs::proxy_chat_completion` and
`crates/mcp-gateway/src/proxy.rs::handle_tools_call` are two
implementations of the same lifecycle. Both do:

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

The two crates have **parallel** code for each step. Side effects of
parallelism observed in this codebase:

- The "JSON-RPC error code → breaker failure" rule for MCP was
  copy-pasted **three** times before B1 step 4c consolidated them
  ([b6306b0](commit)).
- Rate-limit fail-closed logic ([b6306b0](commit)) had to be fixed
  twice — once per gateway.
- The streaming on-done audit pattern in `mcp-gateway::proxy` was
  written from scratch in this session; the AI gateway side
  ([gateway::streaming::stream_to_sse_with_restorer](crates/gateway/src/streaming.rs))
  has the same shape with subtle differences.
- Adding a new "PII redaction stage" or "content filter stage"
  requires editing both crates in parallel.

The shared engine is already there (`common::limits::sliding`,
`common::audit::AuditLogger`, soon `common::breaker` if we extract
that). What's missing is the **glue**: a typed pipeline that
composes the stages.

## Non-goals

- We are not building a generic middleware framework. Tower / axum
  already exist. This is for the **provider-side** lifecycle that
  runs AFTER the HTTP layer accepted the request.
- We are not unifying the wire formats. OpenAI chat-completion,
  Anthropic messages, JSON-RPC stay distinct — the surface adapters
  in `gateway::surfaces::*` (still TBD as part of B2) translate.
- We are not introducing dynamic dispatch into the hot path. The
  pipeline composes statically per surface.

## Proposal: `CallStage<I, O>`

```rust
// common/lifecycle/mod.rs

/// One step in a per-request pipeline. Takes an `In`, returns an
/// `Out`. Most stages also have a "short-circuit" path —
/// e.g. cache hit ⇒ emit Buffered response, skip the rest. That's
/// the `ShortCircuit<C>` arm: the type C is what the downstream
/// audit emit needs.
#[async_trait]
pub trait CallStage {
    type In;
    type Out;
    /// Surface-specific bag of "things every stage might need":
    /// AppState handle, identity, trace_id, started_at. Each stage
    /// gets it by ref, never mutates.
    type Ctx: StageCtx;

    async fn run(
        &self,
        ctx: &Self::Ctx,
        input: Self::In,
    ) -> Result<StageOutcome<Self::Out, Self::Ctx>, StageError>;
}

pub enum StageOutcome<O, Ctx: StageCtx> {
    /// Stage produced output; pipeline continues to the next stage.
    Continue(O),
    /// Stage decided this request is done. Carries the final
    /// response shape AND the audit row that should land for this
    /// short-circuit (cache hit, rate-limit denied, etc.). The
    /// surface adapter renders the response; the pipeline still
    /// runs `EmitAudit` so observability is uniform.
    ShortCircuit { response: Ctx::Response, audit: Ctx::AuditRow },
}
```

### Stages (surface-agnostic, in `common::lifecycle::stages::*`)

| Stage                | In               | Out              | Notes |
|----------------------|------------------|------------------|-------|
| `CheckLimits`        | `RawRequest`     | `RawRequest`     | Wraps existing `sliding::check_and_record`. ShortCircuit on deny. |
| `CheckBudget`        | `RawRequest`     | `RawRequest`     | Reads materialised `SurfaceConstraints`. ShortCircuit on cap. |
| `CheckAccess`        | `RawRequest`     | `AuthorizedRequest` | Type-state: emits a refined type after ACL check. |
| `CheckCache`         | `AuthorizedRequest` | `AuthorizedRequest` | ShortCircuit on hit. Cache key is `Ctx::cache_key()`. |
| `CheckBreaker`       | `AuthorizedRequest` | `AuthorizedRequest` | ShortCircuit on Open. |
| `InvokeUpstream`     | `AuthorizedRequest` | `UpstreamResponse` | Surface-specific impl (provided by gateway / mcp-gateway). |
| `RecordOutcome`      | `UpstreamResponse` | `UpstreamResponse` | Updates breaker. |
| `WriteCache`         | `UpstreamResponse` | `UpstreamResponse` | Skips if outcome is error/cancelled. |
| `EmitAudit`          | `UpstreamResponse` | `EmittedResponse` | Final stage. Always runs (even on ShortCircuit via the audit field). |

The `Ctx::Response` and `Ctx::AuditRow` associated types let each
surface plug in its own wire shape (`ChatCompletionResponse` vs
`JsonRpcResponse`; `gateway_logs` row vs `mcp_logs` row).

### Streaming branch

Streaming is a different `StageOutcome` variant:

```rust
pub enum StageOutcome<O, Ctx> {
    Continue(O),
    ShortCircuit { .. },
    Streaming(Ctx::StreamingResponse),
}
```

`Streaming` is produced by `InvokeUpstream` only. The pipeline
detects it and switches to streaming mode: subsequent stages
(`RecordOutcome`, `WriteCache`, `EmitAudit`) get attached as an
on-done task via a oneshot — exactly the pattern already in
[mcp_gateway::proxy::streaming::build_chunk_passthrough](crates/mcp-gateway/src/proxy/streaming.rs).

### Composition

```rust
// crates/mcp-gateway/src/pipeline.rs
pub fn build_pipeline() -> Pipeline<McpCtx> {
    Pipeline::new()
        .then(CheckLimits)
        .then(CheckAccess)
        .then(CheckCache)
        .then(CheckBreaker)
        .then(ResolveCredential::default())   // surface-specific
        .then(InvokeUpstream::default())      // surface-specific
        .then(RecordOutcome)
        .then(WriteCache)
        .then(EmitAudit)
}
```

`Pipeline::new()` is a builder that statically chains stages by
type. No dyn dispatch. New stages slot in with `.then(...)`.

## Type-state vs runtime

Each stage's `In` → `Out` is checked at compile time. `CheckAccess`
returns `AuthorizedRequest`; `CheckCache` only accepts
`AuthorizedRequest`. This means you can't accidentally compose a
pipeline that skips access control — the types refuse to line up.

The cost: the type plumbing is real, and adding a stage in the
middle is a type-error cascade until you fix every downstream stage.
We accept that — it's the price for "you can't forget ACL".

## Migration plan

**Phase 1: scaffolding (no behaviour change)**
1. `common::lifecycle::{CallStage, StageOutcome, StageError, Pipeline, StageCtx}` — empty trait + builder + tests.
2. `common::lifecycle::stages::{check_limits, check_budget, emit_audit}` — wrap the existing implementations as `CallStage` impls. Use `common::limits::sliding` / `common::audit::AuditLogger` underneath; no logic moves.

**Phase 2: opt-in for one surface**
3. `mcp-gateway::pipeline::build_pipeline()` — composes the new stages plus MCP-specific `InvokeUpstream`. Behind a `feature = "lifecycle_pipeline"` flag (or a runtime config flag if we want fleet rollout).
4. `handle_tools_call` calls `pipeline.run(ctx, request)` when the flag is on; old code stays for diff-checking until 1.0.

**Phase 3: parity validation**
5. Run integration suite under both paths in CI. Diff `mcp_logs` rows on the same request. Block landing until parity is clean.

**Phase 4: drop the old path**
6. Remove the dual-path branch. `handle_tools_call` is now ~30 lines: build pipeline, run, return.

**Phase 5: gateway migration (= B2)**
7. Same pattern for `proxy_chat_completion` / `proxy_anthropic_messages` / `proxy_responses`. Each becomes a thin surface adapter on top of the shared pipeline.

## Open questions

1. **`async_trait` vs hand-rolled GAT-based trait.** `async_trait` is
   ergonomic but every stage call allocates a Box<Future>. For the
   hot path that runs 9 stages per request, that's 9 boxed futures.
   Likely fine (we already allocate for tokio task spawning), but
   worth measuring after phase 2. If it shows up in profiles, switch
   to a hand-rolled `impl Future` trait — current Rust supports
   `async fn` in traits since 1.75, just with awkward bounds.

2. **Where does the on-done streaming task live?** Today it's
   spawned inside `build_chunk_passthrough`. With a pipeline,
   `InvokeUpstream` returns `Streaming(payload)`, and the pipeline
   itself must spawn the post-stream task that runs the remaining
   stages on the accumulated buffer. This is the trickiest piece of
   the design — needs a working prototype before I commit to the
   trait shape.

3. **Per-stage metrics.** Each stage should auto-emit
   `lifecycle_stage_duration_total{stage="check_limits"}` so we can
   see where time goes. Cheap if we wrap `Pipeline::run` with the
   metric; expensive if every stage `impl` has to do it. Lean toward
   the wrapper.

4. **Errors are surface-shaped.** `gateway::GatewayError` and
   `mcp_oauth`'s `JsonRpcError` are very different. The pipeline
   `StageError` carries a generic kind + structured detail; the
   surface adapter at the very end converts to the wire shape.
   Sketch only — needs the audit-row generic to land first.

5. **What about the AI gateway's failover / retry?**
   `gateway::failover::select_route_with_failover` runs multiple
   upstream attempts in one request. The current sketch has
   `InvokeUpstream` as one stage; failover would need to live
   INSIDE that stage. That's fine but means each surface's
   `InvokeUpstream` is bigger than just "one HTTP call". Document
   this clearly.

## What this is NOT

- Not a replacement for `tower::Service` — that's HTTP-layer
  middleware. Pipeline runs AFTER HTTP routing accepts.
- Not a generic plugin framework — stages are statically composed
  per-surface in code, not loaded at runtime.
- Not "now". Phase 1 is a few hundred lines + tests. Phases 2-5 are
  multi-session work. The point of this doc is to validate the
  shape BEFORE writing the trait, not to ship in one PR.

## Estimate

- Phase 1 (scaffolding): 1 session.
- Phase 2 (MCP opt-in): 1 session.
- Phase 3 (parity validation): 1 session, plus integration test
  changes.
- Phase 4 (drop dual-path): 0.5 session.
- Phase 5 (= B2, gateway migration): 2-3 sessions, depends on
  surface adapter design (chat / Anthropic / Responses).

Total: ~6-8 sessions if everything compiles on the first try (it
won't). Realistic: ~10 sessions or ~4 weeks of focused work.

## What I want from review

- Is `StageOutcome { Continue / ShortCircuit / Streaming }` the
  right enum? Or should `Streaming` be a separate trait method?
- Is the type-state cost worth it, or should stages all take/return
  the same generic `RequestState<S>` and the compile-time check
  comes from phantom-typed `S`?
- Are there stages I'm missing that exist today inline in
  `proxy_chat_completion`? Skim `crates/gateway/src/proxy.rs` lines
  1403-1700 to spot-check.
- Open question #2 (streaming on-done) — does the sketch hold up,
  or does it need a separate `StreamingPipeline` trait?
