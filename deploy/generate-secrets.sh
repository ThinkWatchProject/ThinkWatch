#!/usr/bin/env bash
# Generate ThinkWatch env file (dev or prod) from .env.example.
#
# Usage:
#   bash deploy/generate-secrets.sh --dev    # writes .env
#   bash deploy/generate-secrets.sh --prod   # writes .env.production
#                                            # (+ deploy/clickhouse/users.d/default-user.xml)
#
# Reads `.env.example` as the single source of truth and:
#   * drops lines tagged for the OTHER mode (`# dev:` vs `# prod:`)
#   * strips the tag from active-mode lines
#   * substitutes every `__SECRET_HEX_<N>__` token with `openssl rand -hex <N>`
#
# Idempotent: refuses to overwrite an existing output unless FORCE=1.
# Safe to run from any CWD.

set -euo pipefail

MODE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dev)   MODE="dev"; shift ;;
    --prod)  MODE="prod"; shift ;;
    -h|--help)
      sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "✗ unknown arg: $1" >&2
      echo "  usage: $0 --dev | --prod" >&2
      exit 1
      ;;
  esac
done

if [ -z "$MODE" ]; then
  echo "✗ specify --dev or --prod" >&2
  echo "  usage: $0 --dev | --prod" >&2
  exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
TEMPLATE="$PROJECT_ROOT/.env.example"

case "$MODE" in
  dev)  OUT="$PROJECT_ROOT/.env" ;;
  prod) OUT="$PROJECT_ROOT/.env.production" ;;
esac

if [ ! -f "$TEMPLATE" ]; then
  echo "✗ template not found: $TEMPLATE" >&2
  exit 1
fi

if [ -f "$OUT" ] && [ "${FORCE:-0}" != "1" ]; then
  echo "✗ $OUT already exists. Re-run with FORCE=1 to overwrite." >&2
  exit 1
fi

if ! command -v openssl >/dev/null 2>&1; then
  echo "✗ openssl is required but not found in PATH." >&2
  exit 1
fi

umask 077

OTHER="prod"
[ "$MODE" = "prod" ] && OTHER="dev"

# Step 1: mode-filter the template. Drop other-mode lines; strip the
# `# dev:` / `# prod:` prefix (plus its trailing space) from this-mode
# lines. Anything else passes through unchanged.
#
# Step 2: substitute every `__SECRET_HEX_<N>__` token with a fresh
# `openssl rand -hex <N>`. We loop in bash (not awk) so we can shell
# out per token — awk would need `getline cmd` per match which is
# fiddlier than just iterating in bash.
{
  echo "# Generated $(date -u +%Y-%m-%dT%H:%M:%SZ) by deploy/generate-secrets.sh --$MODE"
  echo "# DO NOT EDIT BY HAND — edit .env.example then re-run with FORCE=1."
  echo ""
  awk -v mode="$MODE" -v other="$OTHER" '
    {
      if ($0 ~ "^# " other ":") next
      if ($0 ~ "^# " mode ":") sub("^# " mode ": *", "")
      print
    }
  ' "$TEMPLATE"
} | while IFS= read -r line || [ -n "$line" ]; do
  while [[ "$line" =~ __SECRET_HEX_([0-9]+)__ ]]; do
    n="${BASH_REMATCH[1]}"
    secret=$(openssl rand -hex "$n")
    line="${line/__SECRET_HEX_${n}__/$secret}"
  done
  echo "$line"
done > "$OUT"

chmod 600 "$OUT"
echo "✓ Wrote $OUT"

# Prod also needs the ClickHouse user XML. The official CH image consumes
# `CLICKHOUSE_PASSWORD` natively in dev (no XML required), but the prod
# compose mounts a users.d/ override so we write the password there.
if [ "$MODE" = "prod" ]; then
  CH_USERS_DIR="$SCRIPT_DIR/clickhouse/users.d"
  CH_PASS=$(awk -F= '/^CLICKHOUSE_PASSWORD=/{print $2; exit}' "$OUT")
  mkdir -p "$CH_USERS_DIR"
  cat > "$CH_USERS_DIR/default-user.xml" <<CHEOF
<clickhouse>
  <users>
    <default remove="remove">
    </default>
    <thinkwatch>
      <profile>default</profile>
      <networks>
        <ip>::/0</ip>
      </networks>
      <password><![CDATA[${CH_PASS}]]></password>
      <quota>default</quota>
      <access_management>1</access_management>
    </thinkwatch>
  </users>
</clickhouse>
CHEOF
  echo "✓ Wrote $CH_USERS_DIR/default-user.xml"
  echo "  Review CORS_ORIGINS and any optional settings before deployment."
fi
