# Backup & restore runbook

A ThinkWatch deployment writes durable state to three places. Each
needs its own backup story; restore order between them matters.

| Store | Holds | RPO target (recommended) | RTO target |
|---|---|---|---|
| PostgreSQL | Users, RBAC, API keys, providers, models, MCP servers + per-user OAuth credentials, dynamic settings, webhook outbox | ≤ 15 min | < 30 min |
| ClickHouse | Audit / gateway / MCP logs, body retention window, rollups | ≤ 1 h | < 4 h |
| Object store (S3 / RustFS / MinIO) | Offloaded request/response bodies | ≤ 1 h | best-effort |

This file documents the supported approaches. The example commands
target a self-hosted setup (the bundled Helm StatefulSets), but the
PG/CH/S3 sections work identically against managed services — wire
the credentials and skip the in-cluster pieces.

---

## PostgreSQL

### What you must back up

The whole database. ThinkWatch keeps no separate "metadata-only"
shadow store; users + RBAC + audit configuration + outbox + dynamic
settings all live here.

### Strategy: daily `pg_dump` + WAL archiving

1. **Logical dump (daily)** — easy restore for routine cases.

   ```bash
   POD=$(kubectl -n thinkwatch get pod -l app.kubernetes.io/component=postgres -o jsonpath='{.items[0].metadata.name}')
   kubectl -n thinkwatch exec "$POD" -- \
     pg_dump --format=custom --no-owner --clean --if-exists \
     -U thinkwatch think_watch \
     > backups/pg-$(date -u +%Y%m%dT%H%M%SZ).dump
   ```

   The `--format=custom` output is restored with `pg_restore` — it's
   smaller than plain SQL and supports parallel restore.

2. **WAL archiving (continuous)** — for low-RPO setups, wire
   `archive_mode = on` + `archive_command` pointing at the same
   object store you use for the audit bucket. The Helm chart's
   bundled Postgres does NOT enable this by default; for a real
   production deploy, set `postgres.bundled: false` and use a
   managed PG (RDS / Cloud SQL / Crunchy) that handles WAL
   shipping natively.

### Restore

```bash
POD=$(kubectl -n thinkwatch get pod -l app.kubernetes.io/component=postgres -o jsonpath='{.items[0].metadata.name}')

# Stop the server first — restoring under live writes corrupts
# the new shape.
kubectl -n thinkwatch scale deployment/think-watch-server --replicas=0

# Drop and recreate the database, then restore.
kubectl -n thinkwatch exec -i "$POD" -- \
  psql -U thinkwatch -d postgres -c "DROP DATABASE IF EXISTS think_watch; CREATE DATABASE think_watch;"

kubectl -n thinkwatch exec -i "$POD" -- \
  pg_restore --no-owner --clean --if-exists --dbname=think_watch \
  --username=thinkwatch \
  < backups/pg-20260520T030000Z.dump

# Server boot re-applies db/schema.sql idempotently, so any forward
# schema migration since the dump is reconciled.
kubectl -n thinkwatch scale deployment/think-watch-server --replicas=1
```

### Cross-version compatibility

`db/schema.sql` is idempotent — restoring a `0.5.x` dump into a
`0.6.x` cluster works as long as no column was renamed in the
intervening release. CHANGELOG entries flag those (look for
`db/release_migrations/`); apply them with `psql` before scaling
the server back up if the CHANGELOG calls one out.

---

## ClickHouse

### What you must back up

Tables holding business-record-level data (compliance / audit
chain): `audit_logs`, `gateway_logs`, `mcp_logs`, `access_logs`.

Aggregated rollups (`*_5m`, `cost_rollup_hourly`, etc.) are
rebuildable from the base tables — back them up if you have spare
storage, but recovery without them only loses dashboard history,
not source data.

### Strategy: ClickHouse `BACKUP` to S3

ClickHouse 24+ ships a native `BACKUP` statement that snapshots
tables to S3-compatible storage. Use the same bucket your body-
offload pipeline targets (different prefix).

```sql
-- Run inside the CH instance, daily at low-traffic hour.
BACKUP DATABASE think_watch
    TO S3('https://s3.example.com/thinkwatch-backups/clickhouse/$(date +%Y-%m-%d).zip',
          '<ACCESS_KEY>', '<SECRET_KEY>');
```

Retention is administered by the bucket lifecycle rule, not
ClickHouse — set the `Days` field on the backup prefix to whatever
your compliance window requires (typically 90+).

### Restore

```sql
RESTORE DATABASE think_watch
    FROM S3('https://s3.example.com/thinkwatch-backups/clickhouse/2026-05-20.zip',
            '<ACCESS_KEY>', '<SECRET_KEY>');
```

If the restore lands on a fresh ClickHouse instance, the server's
`ensure_clickhouse_tables` boot step will see the tables already
exist (the restore created them) and skip the schema apply — so
restore order is:

1. Restore CH backup.
2. Restore PG backup.
3. Boot the server.

### `audit.body_retention_days` vs the restore window

If you restore a CH backup older than `audit.body_retention_days`,
the row-level retention TTL will start expiring bodies as soon as
the MergeTree merger sees them. To preserve them, **raise
`audit.body_retention_days` via the admin UI before the restore
lands**, then lower it back once the bodies are no longer needed.

---

## Object store (offloaded bodies)

### What's in the bucket

Anything bigger than `audit.body_max_bytes` (256 KiB default).
Audit rows in ClickHouse hold an `s3://bucket/key` URL pointing to
the actual payload; the body itself lives only in this bucket.

### Strategy: bucket-level versioning + cross-region replication

ThinkWatch does NOT take application-level backups of bucket
contents. Use the storage backend's own primitive:

- **AWS S3**: enable bucket versioning + Cross-Region Replication
  to a backup bucket. Versioning means a `DELETE` is recoverable;
  CRR means an entire bucket loss is recoverable.
- **MinIO / RustFS**: enable `mc replicate` to a peer cluster.

The `audit.body_s3_lifecycle_days` rule the server installs (60
days by default) controls EXPIRATION, not deletion of versioned
prior copies. Versioning + lifecycle rules play together correctly
— prior versions stay until the lifecycle rule says otherwise.

### Restore

Bucket-level restores depend on the storage backend. For AWS S3:

```bash
# Recover a single body by version-id (used when an admin deletes
# the wrong audit row and needs the body back).
aws s3api get-object \
  --bucket thinkwatch-audit-bodies \
  --key bodies/gateway_logs/<uuid>/request_body \
  --version-id <vid> \
  recovered-body.txt
```

For full bucket loss, restore from the replica bucket (CRR) — the
S3 URLs in ClickHouse will keep working if the replica is mounted
at the same name, or rewrite them via:

```sql
ALTER TABLE gateway_logs UPDATE
  request_body = replaceAll(request_body, 's3://OLD_BUCKET/', 's3://NEW_BUCKET/')
  WHERE request_body LIKE 's3://OLD_BUCKET/%';
-- repeat for response_body, mcp_logs.tool_arguments, etc.
```

---

## DR drill — quarterly

Recovery procedures that aren't exercised, aren't real. Schedule a
DR drill at least every quarter:

1. Spin up a parallel namespace from the latest backups.
2. Boot the chart against the restored PG + CH.
3. Run the integration test suite (`make test-it`) — every shape it
   exercises depends on a successful restore.
4. Force a fresh login flow + an MCP `tools/call` against a
   production-like provider stub.
5. Time the whole sequence — that's your real RTO.

Tear the namespace down after. Log the timings in your runbook
tracker so you notice when backups grow past the RPO window.

---

## Common mistakes

- **Restoring PG before CH.** The server's startup smoke-test against
  ClickHouse will pass (CH is up) but the very first audit-emitting
  request will fail to flush because `ensure_clickhouse_tables` didn't
  run yet. Order is: CH first, PG second, server third.
- **Forgetting `secrets.encryptionKey`.** A PG restore brings back
  encrypted-at-rest cells; if the server boots with a different
  `ENCRYPTION_KEY`, every provider header / MCP credential will
  fail to decrypt and load. Restore the Helm Secret (or
  `--set secrets.encryptionKey=$(cat backup/keys/encryption.txt)`)
  alongside the PG dump.
- **Restoring into a populated DB.** `pg_restore --clean --if-exists`
  works on a fresh DB; on a populated one it can leave dangling FK
  references if the source schema differs by even one column. Always
  `DROP DATABASE` first.
