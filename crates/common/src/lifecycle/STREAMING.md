# common/lifecycle — streaming DESIGN (phase 2 deferred work)

Status: **decisions locked**. Code is being implemented to match
this document. If the implementation diverges, update this file in
the same commit. Companion to [DESIGN.md](./DESIGN.md) §5.

## What "streaming" means in this codebase

Two surfaces, two wire shapes, one shared post-call shape:

| Surface | Upstream produces | Wire to client | Post-call work runs |
|---|---|---|---|
| AI gateway (`proxy.rs::proxy_chat_completion` L1686–L1863) | `Stream<Item = Result<ChatCompletionChunk, GatewayError>>` from provider | OpenAI SSE (chunk JSON + `[DONE]`) | After `[DONE]` / first error / consumer drop |
| MCP (`proxy/streaming.rs::build_chunk_passthrough`) | reqwest `bytes_stream()` framed as SSE events | Re-framed SSE (one downstream event per upstream event) | After natural EOF / transport error / consumer drop |

Both already converge on the same shape inside an `on_done`
callback that runs in a detached `tokio::spawn`:

```
detached task:
  outcome = oneshot::Receiver::await
    .unwrap_or(ClientCancelled)              // sender dropped ⇒ client left
  → record_outcome  (breaker accounting)
  → write_cache     (only on Natural + non-error)
  → record_usage    (limits/budget post-debit — AI gateway today)
  → emit_audit      (single emit site, outcome-derived status)
```

The shared piece is **the post-call lifecycle tail running in a
detached task driven by a oneshot**. The bytes-on-the-wire pump
stays surface-specific.

## Architecture — decisions

### S1. Streaming is an `Invocation` variant from `invoke_upstream`

`invoke_upstream` (surface-specific per DESIGN.md §Stage inventory)
returns an `Invocation<S>`:

```rust
pub enum Invocation<S: Surface> {
    /// Buffered upstream — the response is in hand and post-invoke
    /// stages run in the foreground.
    Buffered(Invoked<S>),
    /// Streaming upstream — `response` is the already-wrapped SSE
    /// body handed back to axum synchronously; `tail` resolves to
    /// the materialised `Invoked<S>` when the stream terminates.
    Streaming {
        /// `S::StreamResponse` — distinct from `S::Response`
        /// because the streaming wire shape (`StreamingPayload`
        /// for MCP, axum `Sse<>::into_response()` for the AI
        /// gateway) rarely matches the inner buffered shape that
        /// the cache and audit pipeline operate on.
        response: S::StreamResponse,
        tail: Pin<Box<dyn Future<Output = Invoked<S>> + Send>>,
    },
}
```

The surface handler matches on `Invocation`:

- `Buffered(invoked)` → run post-invoke stages in the foreground.
- `Streaming { response, tail }` → return `response` to axum
  immediately, **and** `tokio::spawn(async { run_post_invoke(tail.await).await; })`
  on the same lifecycle plumbing.

Reasoning: the surface MUST get the SSE body back synchronously so
the first chunk hits the wire before any post-call latency. The
`Invocation` enum is the single fork point; everything downstream
of it sees `Invoked<S>` regardless of mode.

### S2. `Invoked<S>` carries a `CapturedView<S>` that unifies the modes

```rust
pub struct Invoked<S: Surface> {
    pub identity: S::Identity,
    pub trace_id: String,
    pub started_at: Instant,
    pub client_ip: Option<String>,
    pub limit_check: LimitCheckRecord,
    pub access_candidate: String,
    pub view: CapturedView<S>,
}

pub enum CapturedView<S: Surface> {
    Buffered(S::Response),
    Streaming {
        outcome: StreamOutcome,
        captured: S::StreamCaptured,
    },
}
```

`record_outcome`, `write_cache`, `emit_audit` each match on `view`
internally — same stage signatures, single source of truth for
each step. **Stages are not double-implemented for streaming.**

`S::StreamCaptured` is a new associated type on `Surface`:

| Surface | `StreamCaptured` |
|---|---|
| AI gateway | `{ chunks: Vec<ChatCompletionChunk>, usage: Option<Usage> }` |
| MCP | `{ events: Vec<serde_json::Value> }` |

Each surface picks the shape its `assemble_*` / cache write /
audit body capture needs.

### S3. `StreamOutcome` is a shared enum in `common::lifecycle::streaming`

```rust
pub enum StreamOutcome {
    Natural,
    UpstreamError {
        error_type: String,
        message: String,
        status_code: i64,
    },
    ClientCancelled,
}

impl StreamOutcome {
    /// `(logged_status, optional_detail_blob)` consumed by
    /// `emit_audit` to derive the audit row's status.
    pub fn logged_status_and_detail(&self) -> (i64, Option<serde_json::Value>);
}
```

Lifted verbatim from `gateway::streaming::StreamOutcome`. The MCP
side's `crate::proxy::streaming::StreamOutcome` is **deleted** in
the same commit that migrates MCP — there is no compatibility
shim, no re-export. The migration commit fixes every call site in
one edit.

The carried `status_code` field is new on MCP. Mapping:
- `Natural` → 200
- `UpstreamError { status_code }` → that status (preserves the
  upstream's actual HTTP code; no 502 blanket)
- `ClientCancelled` → 499

### S4. The detached task runs the same post-invoke stages

```rust
async fn run_post_invoke<S: Surface>(
    invoked: Invoked<S>,
    deps: &PipelineDeps,
) -> Result<Emitted<S>, S::Response> {
    let invoked = record_outcome::<S>(invoked, deps).await?;
    let invoked = write_cache::<S>(invoked, deps).await?;
    let emitted = emit_audit::<S>(invoked, deps).await?;
    Ok(emitted)
}
```

Buffered handler calls it in the foreground and uses `emitted.response`.
Streaming handler spawns it; the `Result` is discarded because the
SSE response is already on the wire — short-circuits inside the
post-invoke stages can only affect the audit row, not the client.

This is the single audit-emit site for streaming. There is **no**
synchronous tail that also emits — eliminating the class of bugs
where two emit paths produced two rows for one request.

### S5. Surfaces own the pump; pipeline owns the tail

The pump (bytes/chunks → SSE body + capture buffer + oneshot
signal) lives in surface code. Each surface writes one function
with signature:

```rust
// AI gateway
fn build_chat_pump(
    upstream: Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>,
    restorer: Option<PiiStreamRestorer>,
) -> (S::Response, Pin<Box<dyn Future<Output = (StreamOutcome, S::StreamCaptured)> + Send>>);

// MCP
fn build_mcp_pump(
    upstream: reqwest::Response,
    request_id: Option<Value>,
) -> (S::Response, Pin<Box<dyn Future<Output = (StreamOutcome, S::StreamCaptured)> + Send>>);
```

The pump owns the `oneshot::Sender`, the chunk-retention cap
(`MAX_CACHED_CHUNKS = 2048` on the AI side), PII restoration, the
SSE re-framing — all surface-specific. The pipeline never sees
any of it.

The pump's tail future is what `invoke_upstream` wraps into
`Invocation::Streaming.tail` after attaching the
`Identity`/`trace_id`/`limit_check`/etc. context that turns
`(outcome, captured)` into a full `Invoked<S>`.

### S6. `record_usage` is NOT in this phase

`post_flight_account` (limits/budget post-debit) exists on the AI
gateway today and not on MCP. Lifting it into a shared
`record_usage` stage requires a `S::Usage` abstraction with a
surface-specific `record` impl — that's its own design and it's
not blocking the streaming refactor.

Phase 2 (this doc) lands streaming-unification with
`record_outcome`/`write_cache`/`emit_audit`. The AI gateway's
`post_flight_account` continues to be called inside `emit_audit`
(today's site) for the streaming path. A follow-up phase lifts it.

### S7. No `dyn StreamPump` trait, no `Pipeline` builder

The pump is one free function per surface. The pipeline is the
`Invocation` enum + the `Invoked<S>` state + a shared
`run_post_invoke` async fn. Adding a `Pump` trait would force a
trait object indirection in the hot path for zero deduplication —
each surface has exactly one pump, called from exactly one stage.

### S8. Cache-hit "re-emit as SSE" is unaffected

The AI gateway's cache-hit path (`proxy.rs` L1643–L1663) wraps a
buffered response in a single-chunk SSE stream. That path
short-circuits before `invoke_upstream` via `check_cache`'s
`Err(S::Response)` return (DESIGN.md §3). The wrapping happens in
the surface's response factory; the pipeline doesn't see it.

## Stage inventory diff vs DESIGN.md

DESIGN.md's `record_outcome` / `write_cache` / `emit_audit` rows
in the Stage inventory table apply unchanged. The new wiring is:

| Item | Where it lives |
|---|---|
| `Invocation<S>` enum | `common::lifecycle::state` |
| `CapturedView<S>` enum | `common::lifecycle::state` |
| `StreamOutcome` enum | `common::lifecycle::streaming` (new module) |
| `S::StreamCaptured` assoc type | `common::lifecycle::surface` |
| Pump fns | Surface crates (`gateway`, `mcp-gateway`) |
| `run_post_invoke` | `common::lifecycle::stages` |

## Migration plan

Single rewrite per surface — no flags, no dual-path.

**Step 1 — common scaffolding**
- Add `StreamOutcome` (lifted from `gateway::streaming`),
  `Invocation<S>`, `CapturedView<S>`, `S::StreamCaptured` to the
  `Surface` trait.
- Implement `run_post_invoke::<S>` against `Invoked<S>` with the
  match-on-`view` shape inside each stage.
- Unit-test against the `test_surface` fake.

**Step 2 — migrate MCP**
- Replace `build_chunk_passthrough` with `build_mcp_pump`. Returns
  `(S::Response, BoxFuture<(StreamOutcome, McpStreamCaptured)>)`.
- Delete `crate::proxy::streaming::StreamOutcome` (its uses inside
  this crate become `common::lifecycle::streaming::StreamOutcome`).
- Delete `emit_tools_call_audit`'s on-done call site in favour of
  the shared `emit_audit` stage.
- Integration tests in `crates/test-support/tests/mcp_*` catch
  parity. The audit row schema is unchanged on the wire; only the
  emit site moves.

**Step 3 — migrate AI gateway**
- Replace the `on_done` closure inside `proxy_chat_completion` L1748–L1861
  with the `Invocation::Streaming` return from `invoke_upstream`.
- `stream_to_sse_with_restorer` becomes the body of `build_chat_pump`.
  Its `on_done`-via-oneshot mechanic is exactly what the new
  signature needs; the call site shrinks to `(response, tail)`.
- `post_flight_account` stays where it is for now (S6).
- Integration tests in `gateway_proxy.rs` and `gateway_logs_*` catch
  parity.

If step 2 reveals an abstraction mismatch, revise the common
scaffolding and re-run step 2 before touching step 3.

## What we won't do until it earns its keep

- **Dynamic pump dispatch (`dyn StreamPump`)** — no use case (S7).
- **Resumable streams** — client drop ends the request (S5).
- **A pipeline-level `record_usage` stage** — its own design,
  doesn't block this work (S6).
- **Unifying the cache-hit SSE wrapper with streaming** — the
  cache hit never enters `invoke_upstream` (S8).

## Estimate

- Step 1 (common scaffolding + tests): 0.5 session.
- Step 2 (MCP migration): 1 session.
- Step 3 (AI gateway migration): 1 session.

Total: **2.5 sessions**, on top of the 4–5 already budgeted in
DESIGN.md.
