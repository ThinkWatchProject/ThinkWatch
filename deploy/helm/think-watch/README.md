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
