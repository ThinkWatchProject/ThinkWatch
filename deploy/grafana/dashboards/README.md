# ThinkWatch — Grafana dashboards

Starter dashboards for the metrics emitted on the `/metrics` endpoint
(console port `3001`, bearer-protected via `METRICS_BEARER_TOKEN`).

The bundled Helm chart's `ServiceMonitor` (enable with
`metrics.serviceMonitor.enabled=true`) feeds these dashboards if you
run `kube-prometheus-stack` or any operator that watches
`monitoring.coreos.com/v1` CRDs.

## Files

- `think-watch-overview.json` — single-panel-per-row starter board.
  Import via Grafana → Dashboards → New → Import → paste JSON.
  Use it as a base; rename / clone before customising so a `helm
  upgrade` doesn't drop your edits.

## Key metric families

The server emits ~50 metrics across these groups. The starter
dashboard panels are limited to the ones an SRE will actually alert
on; the full set is enumerated below so operators can build their
own boards without grepping the source.

### Audit pipeline — alertable

| Metric | What it means |
|---|---|
| `audit_log_dropped_total` | The async audit worker rejected a row because the in-memory channel was full. SUSTAINED non-zero = an incident; one-shot bursts on traffic spikes are tolerable. |
| `audit_log_sampled_out_total` | Row dropped by `audit.sample_rate < 1`. Expected when sampling is on. |
| `audit_ch_dropped_total{reason}` | CH flush gave up on a batch (`reason=no_client` / `retention_cap`). Alert if `retention_cap` > 0 for more than ~1 min — sustained CH outage. |
| `audit_ch_flush_failed_total{table}` | Per-table flush error counter. Pairs with `audit_ch_dropped_total`. |
| `audit_body_retention_above_bucket_lifecycle_total` | `audit.body_retention_days` > S3 bucket lifecycle. Audit rows older than the bucket horizon will 404 on body fetch. Configuration drift; surface in a dashboard, not pager. |

### Gateway — request hot path

| Metric | Notes |
|---|---|
| `gateway_cache_total{result}` | `result=hit` / `miss`. Cache hit rate = `hit / (hit + miss)`. |
| `gateway_rate_limited_total` | Per-key rate-limit denials at the proxy. |
| `gateway_quota_overflow_total` | Token quota exceeded. |
| `gateway_budget_fail_open_total` | Budget check couldn't reach Redis and fell open. Should be 0 in steady state. |
| `gateway_rate_limiter_fail_open_total` / `..._fail_closed_total` | Mirror, for the per-key rate limiter. |
| `gateway_sse_buffer_overflow_total` | An SSE event exceeded 8 MiB without a delimiter — stream torn down. Non-zero = a misbehaving upstream provider; investigate. |
| `gateway_stream_chunk_serialize_failed_total` | A chunk failed to JSON-encode. Should be 0; non-zero suggests a provider returned a malformed payload our types can't represent. |
| `gateway_stream_usage_estimated_total` | Provider didn't surface a `usage` block; we counted tokens locally. Higher = degraded billing accuracy on that provider. |
| `gateway_cache_invalidations_total{trigger}` | Operator-initiated cache wipes (provider edit, model rotation). |

### MCP gateway

| Metric | Notes |
|---|---|
| `mcp_sse_buffer_overflow_total` | MCP pump tore down a stream past the 8 MiB cap. |
| `mcp_pool_sse_events_truncated_total` | A long-running tool emitted > 1024 progress notifications and we truncated the audit body. |
| `mcp_upstream_session_rejected_total` | Upstream sent a malformed `Mcp-Session-Id` (too long / control chars). |
| `lifecycle_breaker_short_circuit_total` | tools/call rejected because the per-server breaker was Open. |
| `lifecycle_access_denied_total` | Allowlist gate rejected a tool. |
| `lifecycle_budget_exceeded_total` | Per-user MCP budget tripped. |
| `lifecycle_budget_fail_open_total` | Budget check couldn't reach Redis. |

### Storage / IO

| Metric | Notes |
|---|---|
| `blob_store_put_total` / `blob_store_put_bytes_total{field}` | Body offload write volume. |
| `blob_store_get_total` | Body re-read for the audit viewer. |
| `blob_store_smoke_failed_total` | Bucket smoke test failed at boot or during periodic ping. Non-zero immediately after startup = misconfiguration; later spikes = transient S3 outage. |

### Auth / sessions

| Metric | Notes |
|---|---|
| `auth_refresh_replay_total` | Stolen refresh token detected (used twice). Spike = active attack. |
| `auth_refresh_redis_error_total` | Refresh blacklist Redis error. |
| `auth_jwt_blacklist_redis_error_total` | JWT revocation Redis error. |
| `auth_pw_epoch_redis_error_total` | pw_epoch write failure (password-change invalidation incomplete). |
| `session_invalidate_failures_total` | Final-attempt failure on `invalidate_refresh_tokens`. |
| `session_invalidate_retried_total` | Retry was needed but eventually succeeded. |

### OAuth (MCP credential flow)

| Metric | Notes |
|---|---|
| `oauth_storage_retried_total{label}` | Storage retry was needed on the OAuth callback path. |
| `oauth_storage_failed_total{label}` | Storage permanently failed — user must re-authorise. |

### Settings / hot reload

| Metric | Notes |
|---|---|
| `hot_reload_http_client_failed_total` | Hot reload of the shared HTTP client failed; the previous client is retained. |
| `webhook_outbox_delete_failed_total` | Outbox row was dispatched but the post-dispatch DELETE failed — the receiver will see a duplicate on the next drain. |

### Background workers

| Metric | Notes |
|---|---|
| `supervised_task_panics_total{task}` | A `supervise_restart`-wrapped task panicked and is being restarted with exponential backoff. Non-zero = code bug to investigate. |

## Suggested alerts

Minimal alert set for a 0.5 production deployment:

```yaml
groups:
  - name: think-watch-critical
    rules:
      - alert: ThinkWatchAuditDroppingRows
        expr: rate(audit_log_dropped_total[5m]) > 0
        for: 5m
        annotations:
          summary: "Audit worker dropping events for 5+ min — incident, not a blip"

      - alert: ThinkWatchClickHouseRetentionCap
        expr: rate(audit_ch_dropped_total{reason="retention_cap"}[5m]) > 0
        for: 2m
        annotations:
          summary: "ClickHouse flush has been failing long enough to evict rows"

      - alert: ThinkWatchRefreshTokenReplay
        expr: increase(auth_refresh_replay_total[5m]) > 0
        annotations:
          summary: "Refresh-token replay detected — likely credential theft"

      - alert: ThinkWatchSupervisedTaskPanicking
        expr: increase(supervised_task_panics_total[15m]) > 0
        annotations:
          summary: "Background task {{ $labels.task }} panicked"

      - alert: ThinkWatchBlobStoreUnreachable
        expr: rate(blob_store_smoke_failed_total[5m]) > 0
        for: 5m
        annotations:
          summary: "Body-offload S3 backend unreachable for 5+ min"
```

The starter dashboard JSON doesn't bundle these alerts — wire them
via your Prometheus rule files (or `kube-prometheus-stack` values).
