# Automatic process resources

The server sizes its execution and buffering policy at startup. No configuration
change is required. CPU availability includes the process affinity and container
CPU quota. Memory uses the smaller of physical RAM and the cgroup v1/v2 memory
limit, including ancestor limits.

## Execution

The I/O runtime keeps at most eight workers. Large or concurrent JSON decoding,
protocol conversion, redaction and content inspection use elastic blocking
execution. CPU admission is shared with password hashing: at most one job per
available CPU, further limited to one job per 128 MiB of memory allowance.
An 80 CPU server with sufficient memory can execute 80 jobs concurrently.
Extra execution threads retire after ten idle seconds.

Argon2 also has its own memory admission: one concurrent hash per 32 MiB within
one eighth of the memory allowance, with a minimum of one. Cancelling an HTTP
request does not release a hash's admission until that hash actually finishes.
The existing `TOKIO_WORKER_THREADS` override remains supported.

The server uses mimalloc with transparent huge pages disabled. On Linux it also
applies a process-local THP policy before creating runtime threads. This needs
no host sysctl or environment changes.

## Buffered bodies and streams

Requests, upstream AI/MCP responses, Redis cache reads and retained streaming
timelines share one byte budget: one quarter of the memory allowance, between
8 MiB and 8 GiB. Reservations allow 32 times the wire size for decoded JSON,
conversion, inspection and capture copies. A single response or stream timeline
is limited to the smaller of 64 MiB and one thirty-second of that budget.
The existing gateway 10 MiB and console 1 MiB request limits still apply.
Error bodies and credential endpoint responses have a separate per-read 64 KiB
limit and use the same shared budget. MCP tool discovery, provider and registry
catalogs, OAuth discovery, audit forwarder errors and S3 reads also use bounded
reads, so administrative/background requests cannot bypass admission on ingress.

Admission is incremental, including chunked responses. Readers reject growth
before appending rather than waiting while holding partial bodies. Reservations
follow shared buffers through transformations, slow clients, cancellation and
detached stream audit work. Cache reads reserve before fetching and use bounded
Redis reads even if a key changes between the size check and fetch.

Memory pressure produces HTTP 429 with `Retry-After: 1` before streaming starts.
Already-started streams terminate with an error event (MCP code `-32005`).
Local pressure does not count as an upstream failure for circuit breakers.
Oversized requests return 413; oversized buffered upstream responses fail rather
than allocating without a bound. Cache reads under pressure become misses.

## Audit backlog

Queued audit entries and retained ClickHouse retry batches share a separate byte
budget: one sixteenth of the memory allowance, between 2 MiB and 128 MiB. The
estimate includes owned strings, JSON allocations and room for temporary flush
copies. The existing 100,000 entry queue limit is a secondary bound.
When full, the logger drops new entries and emits a throttled error.

Monitor `audit_log_buffered_bytes`, `audit_log_memory_dropped_total`,
`audit_log_dropped_total` and `gateway_memory_busy_total` alongside the existing
queue-depth metrics. A drop requires operational attention, especially when
ClickHouse is unavailable for an extended period.

These budgets estimate application allocations; they are not a hard RSS cap.
Connection state, database pools, configuration, stacks and allocator overhead
also consume memory. Deployment memory and CPU limits remain authoritative.
