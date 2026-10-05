# ThinkWatch Helm Chart

One-command install. Ships bundled PostgreSQL, Redis, and ClickHouse
StatefulSets so you don't need external database infrastructure to
get started. For production, swap any of them out for a managed
service by flipping `bundled: false` and providing `externalUrl`.

## Quick start

```bash
# From the repo root
make helm-deploy
```

That's equivalent to:

```bash
helm upgrade --install thinkwatch deploy/helm/think-watch \
  --namespace thinkwatch --create-namespace
```

First install auto-generates `JWT_SECRET`, `ENCRYPTION_KEY`, and
database passwords via `randAlphaNum`, stores them in the
`<release>-secrets` Secret (annotated `helm.sh/resource-policy: keep`),
and re-reads them on upgrade so they survive across releases.

Smoke test once pods are ready:

```bash
helm test thinkwatch -n thinkwatch
```

## Common overrides

```bash
# Pin an image tag (defaults to Chart.appVersion)
make helm-deploy HELM_VALUES=deploy/helm/think-watch/values-production.yaml.example

# Or one-off flags
helm upgrade --install thinkwatch deploy/helm/think-watch \
  --namespace thinkwatch --create-namespace \
  --set image.server.tag=v0.2.0 \
  --set ingress.enabled=true \
  --set ingress.gateway.host=api.example.com
```

## Using external databases

Set `bundled: false` per service and provide a URL. The chart still
auto-generates `JWT_SECRET` / `ENCRYPTION_KEY`.

```yaml
# my-values.yaml
postgres:
  bundled: false
  externalUrl: postgres://user:pass@pg.rds.amazonaws.com:5432/think_watch?sslmode=require
redis:
  bundled: false
  externalUrl: redis://:pass@redis.cache.amazonaws.com:6379
clickhouse:
  bundled: false
  externalUrl: http://clickhouse.internal:8123
  user: thinkwatch
  database: think_watch
```

```bash
helm upgrade --install thinkwatch deploy/helm/think-watch \
  -n thinkwatch --create-namespace -f my-values.yaml
```

When `bundled=false` and `externalUrl` is empty the chart fails at
install-time with an explicit message — no silent broken Secret.

### PostgreSQL behind a connection pooler

Each server instance sets up the schema when it starts, holding a
Postgres session-level advisory lock so that instances starting together
take turns. The lock belongs to one Postgres session, so `externalUrl`
must reach Postgres directly or through a pooler in session mode, never
one in transaction mode (PgBouncer `pool_mode = transaction`, or the
transaction-mode port of a managed pooler such as Supabase's): there,
the lock can stay held on a server connection the pooler keeps after the
instance is done with it, and every later start waits for it for good.
The server runs its schema setup on the connections of `DATABASE_URL`;
there is no separate URL for it.

### Several replicas starting together

Instances set up the schema one at a time, so with several replicas
starting together each one waits for those before it, and with ClickHouse
unreachable each one also retries it for up to about a minute. The server
answers its startup probe only after that. The probe gives a pod
`5 + 3 × 40 = 125` seconds by default (`startupProbe.initialDelaySeconds`,
`periodSeconds`, `failureThreshold`); raise `startupProbe.failureThreshold`
when pods are restarted before they finish starting.

### Redis Cluster

The external Redis can be a Redis Cluster. Give its URL the
`redis-cluster://` scheme and name one node or more; the server finds the
rest of the cluster from them:

```yaml
redis:
  bundled: false
  externalUrl: redis-cluster://:pass@redis-0.redis:6379?node=redis-1.redis:6379&node=redis-2.redis:6379
```

- Every node must be reachable from the server pods at the address it
  announces to the cluster (`cluster-announce-ip` / `-port`): the server
  follows the cluster's redirects to it. With `networkPolicy.enabled`,
  ports the URL doesn't name go in `networkPolicy.extraEgress` (see
  [Network policy](#network-policy)).
- A cluster has only database 0, so the URL names no `/<db>`.
- Nothing else is needed. Every script the server runs keeps its keys in
  one hash slot — the counters of one user's request share the tag
  `{user:<id>}`, a route's health keys `{<route_id>}` — and pattern
  deletes (cache invalidation) scan every primary.

Rate limits, budgets, route health, caches and the config change
notices between instances are tested against a three-primary Redis 8
cluster (`crates/test-support/tests/redis_cluster.rs`).

### Redis over TLS

Managed Redis services usually require TLS: ElastiCache with in-transit
encryption, Upstash, Azure Cache for Redis, Redis Cloud. Give the URL
the `rediss://` scheme — `rediss-cluster://` for a cluster:

```yaml
redis:
  bundled: false
  externalUrl: rediss://:pass@master.my-cache.abc123.use1.cache.amazonaws.com:6379
```

- The server checks the Redis certificate against the public CAs, the
  same roots it trusts for upstream HTTPS, and against the host name in
  the URL. Use the endpoint name the service gives, not an IP address,
  unless the certificate names that address.
- In a cluster, each node is reached at the address it announces, and
  its certificate must name that address — the host name, or the IP
  address when the node announces one. A cluster whose nodes announce
  IP addresses that their certificates do not name cannot be used over
  TLS.
- The port is whatever the service uses for TLS (Azure Cache for Redis:
  `6380`). Write it in the URL: a `rediss://` URL without one means
  `6379`, as `redis://` does.

A self-hosted Redis whose certificate a private CA signed needs that
CA. Put its PEM certificate in a Secret and name it; the server then
trusts only the certificates in it for Redis:

```bash
kubectl -n thinkwatch create secret generic redis-ca --from-file=ca.crt=./ca.crt
```

```yaml
redis:
  bundled: false
  externalUrl: rediss://:pass@redis.internal:6379
  caSecret:
    name: redis-ca
    key: ca.crt
```

The chart mounts the key at `/etc/thinkwatch/redis-ca/` and sets
`REDIS_CA_CERT` to it. The server reads it at start, so restart the
server pods after changing the Secret. Outside the chart, set
`REDIS_CA_CERT` to the PEM file's path yourself. Client certificates
(mutual TLS) are not supported: give such a Redis `tls-auth-clients no`
and authenticate with the password.

## Network policy

`networkPolicy.enabled` limits what the server pods may reach: DNS, port
`443` (upstreams, the OIDC provider), and PostgreSQL, Redis and
ClickHouse on the ports the server connects to them on:

- A bundled database: its service port (`5432`, `6379`, `8123`).
- An external one: every port its `externalUrl` names, so a database on
  another port needs no setting of its own. That is the port of each host
  in the URL, of each `node=` of a Redis Cluster or Sentinel URL, and a
  Postgres `?port=`.
- A URL without a port: the client's default for the scheme, which is
  `5432` for `postgres://`, `6379` for `redis://` and `rediss://` (TLS
  does not change it), `26379` for a Sentinel and `6379` for the primary it
  points to, and `80` for ClickHouse's `http://`.

The server talks to ClickHouse over plain HTTP only, so ClickHouse's
native port (`9000`) is not allowed. HTTPS to ClickHouse is not supported:
`clickhouse.externalUrl` must be an `http://` URL.

What no URL names goes in `networkPolicy.extraEgress`, rules added to the
server's egress as written: Redis Cluster nodes that announce ports the
URL doesn't list, a Sentinel's primary on a port other than `6379`, an
upstream, MCP server or S3 endpoint on a port other than `443` (RustFS and
MinIO listen on `9000`).

```yaml
networkPolicy:
  enabled: true
  extraEgress:
    - ports:
        - port: 7000
          endPort: 7005
          protocol: TCP
```

## Rotating secrets

`<release>-secrets` is kept on `helm uninstall`. To rotate passwords:

```bash
kubectl -n thinkwatch delete secret thinkwatch-secrets
helm upgrade thinkwatch deploy/helm/think-watch -n thinkwatch
```

Doing this **also invalidates bundled PostgreSQL data** because the
password env var changes while the PVC keeps the old role. If you
rotate DB secrets, plan to delete the PVCs too (or swap to an
external DB).

## Rendered resources

| Component    | Kind           | Condition                       |
| ------------ | -------------- | ------------------------------- |
| app secret   | Secret         | always                          |
| app config   | ConfigMap      | always                          |
| server       | Deployment     | always                          |
| web          | Deployment     | always                          |
| services     | Service × 3    | always                          |
| ingress      | Ingress × 2    | `ingress.enabled`               |
| hpa          | HPA            | `autoscaling.enabled`           |
| pdb          | PDB            | `podDisruptionBudget.enabled`   |
| netpol       | NetworkPolicy  | `networkPolicy.enabled`         |
| postgres     | StatefulSet+Svc| `postgres.bundled`              |
| redis        | StatefulSet+Svc| `redis.bundled`                 |
| clickhouse   | StatefulSet+Svc+CM×3 | `clickhouse.bundled`      |
| test hook    | Pod            | `helm test` only                |

## Makefile helpers

| Target              | Purpose                                     |
| ------------------- | ------------------------------------------- |
| `make helm-deploy`  | install/upgrade in current kube context     |
| `make helm-deploy-down` | `helm uninstall`                        |
| `make helm-template`| render manifests for review                 |
| `make helm-lint`    | `helm lint` the chart                       |

Override release name / namespace / values:

```bash
make helm-deploy HELM_RELEASE=tw HELM_NAMESPACE=prod HELM_VALUES=my-values.yaml
```
