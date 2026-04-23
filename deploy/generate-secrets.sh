#!/usr/bin/env bash
# Generate ThinkWatch production secrets.
#
# Safe to run from any CWD. Writes:
#   <project-root>/.env.production
#   <project-root>/deploy/clickhouse/users.d/default-user.xml
#
# Idempotent: refuses to overwrite an existing .env.production unless FORCE=1.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
ENV_FILE="$PROJECT_ROOT/.env.production"
CH_USERS_DIR="$SCRIPT_DIR/clickhouse/users.d"

if [ -f "$ENV_FILE" ] && [ "${FORCE:-0}" != "1" ]; then
  echo "✗ $ENV_FILE already exists. Re-run with FORCE=1 to overwrite." >&2
  exit 1
fi

if ! command -v openssl >/dev/null 2>&1; then
  echo "✗ openssl is required but not found in PATH." >&2
  exit 1
fi

umask 077

cat > "$ENV_FILE" <<EOF
# ThinkWatch Production Secrets
# Generated on $(date -u +%Y-%m-%dT%H:%M:%SZ)

JWT_SECRET=$(openssl rand -hex 32)
ENCRYPTION_KEY=$(openssl rand -hex 32)
DB_PASSWORD=$(openssl rand -base64 24 | tr -d '=/+')
REDIS_PASSWORD=$(openssl rand -base64 24 | tr -d '=/+')
CLICKHOUSE_PASSWORD=$(openssl rand -base64 24 | tr -d '=/+')
# Bearer token for /metrics scraping. Configure your Prometheus
# scrape job with the same value via authorization.credentials
# (or bearer_token).
METRICS_BEARER_TOKEN=$(openssl rand -hex 32)

DATABASE_URL=postgres://thinkwatch:\${DB_PASSWORD}@postgres:5432/think_watch?sslmode=disable
REDIS_URL=redis://:\${REDIS_PASSWORD}@redis:6379
SERVER_HOST=0.0.0.0
GATEWAY_PORT=3000
CONSOLE_PORT=3001
CORS_ORIGINS=https://console.yourdomain.com
RUST_LOG=info,think_watch=info

CLICKHOUSE_URL=http://clickhouse:8123
CLICKHOUSE_DB=think_watch
CLICKHOUSE_USER=thinkwatch

# SSO/OIDC is configured via the Web console (Admin > Settings > SSO),
# not via env vars. Leave the form blank in the wizard if you don't
# need SSO; you can enable it later without restarting.
EOF

# Generate ClickHouse user XML so the container's users.d/ mount
# creates the thinkwatch user at first boot.
CH_PASS=$(awk -F= '/^CLICKHOUSE_PASSWORD=/{print $2; exit}' "$ENV_FILE")
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

echo "✓ Wrote $ENV_FILE"
echo "✓ Wrote $CH_USERS_DIR/default-user.xml"
echo "  Review CORS_ORIGINS and any optional settings before deployment."
