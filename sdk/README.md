# SDK generation

ThinkWatch ships a single OpenAPI 3.1 spec at
`/api/openapi.json` (utoipa-derived from every `#[utoipa::path]`
on the server side). That spec is the source for any generated
SDK — Python, Go, TypeScript, Java — through the standard
[openapi-generator](https://openapi-generator.tech/) toolchain.

## One-shot SDK build

Start a dev server (`make dev` from the repo root), then from
this directory:

```bash
# 1) Snapshot the live spec into ./openapi.json
cd web && pnpm gen:sdk:dump-spec && cd ..

# 2) Pick a target generator, drop output into ./<lang>/
docker run --rm -v "$PWD:/local" openapitools/openapi-generator-cli generate \
  -i /local/openapi.json \
  -g python -o /local/python \
  --additional-properties=packageName=thinkwatch_sdk

docker run --rm -v "$PWD:/local" openapitools/openapi-generator-cli generate \
  -i /local/openapi.json \
  -g go -o /local/go \
  --additional-properties=packageName=thinkwatch
```

## What the spec covers

Every public route the server exposes — admin (`/api/admin/*`),
gateway (`/v1/*`), MCP (`/mcp/*`), auth (`/api/auth/*`).
Internal/intra-process functions (handlers without `#[utoipa::path]`)
are deliberately omitted from the spec so the SDK stays a stable
API surface, not an implementation snapshot.

## Why generate, not hand-write

A hand-written SDK drifts the moment a backend field renames; the
generator re-runs in seconds against a fresh spec dump and the diff
matches the actual server behaviour. Frontend already consumes the
same spec via `pnpm gen:api`; SDK consumers get the same guarantee.

## Versioning

The OpenAPI document carries `info.version`, populated from the
workspace Cargo.toml at build time. Bumping the workspace version
invalidates downstream SDK caches automatically — no manual
SDK version pin to maintain.
