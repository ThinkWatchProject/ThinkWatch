#!/usr/bin/env bash
set -euo pipefail

echo "# ThinkWatch Production Secrets" > .env.production
echo "# Generated on $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> .env.production
echo "" >> .env.production

echo "JWT_SECRET=$(openssl rand -hex 32)" >> .env.production
echo "ENCRYPTION_KEY=$(openssl rand -hex 32)" >> .env.production
echo "DB_PASSWORD=$(openssl rand -base64 24 | tr -d '=/+')" >> .env.production
echo "REDIS_PASSWORD=$(openssl rand -base64 24 | tr -d '=/+')" >> .env.production
echo "CLICKHOUSE_PASSWORD=$(openssl rand -base64 24 | tr -d '=/+')" >> .env.production
# Bearer token for /metrics scraping. Configure your Prometheus
# scrape job with the same value via `authorization.credentials`
# (or `bearer_token`).
echo "METRICS_BEARER_TOKEN=$(openssl rand -hex 32)" >> .env.production
echo "" >> .env.production

echo "DATABASE_URL=postgres://thinkwatch:\${DB_PASSWORD}@postgres:5432/think_watch?sslmode=require" >> .env.production
echo "REDIS_URL=redis://:\${REDIS_PASSWORD}@redis:6379" >> .env.production
echo "SERVER_HOST=0.0.0.0" >> .env.production
echo "GATEWAY_PORT=3000" >> .env.production
echo "CONSOLE_PORT=3001" >> .env.production
echo "CORS_ORIGINS=https://console.yourdomain.com" >> .env.production
echo "RUST_LOG=info,think_watch=info" >> .env.production
echo "" >> .env.production
echo "# Configure these manually:" >> .env.production
echo "# CLICKHOUSE_URL=http://clickhouse:8123" >> .env.production
echo "# CLICKHOUSE_DB=think_watch" >> .env.production
echo "# CLICKHOUSE_USER=thinkwatch" >> .env.production
echo "" >> .env.production
echo "# SSO/OIDC is configured via the Web console (Admin > Settings > SSO)," >> .env.production
echo "# not via env vars. Leave the form blank in the wizard if you don't" >> .env.production
echo "# need SSO; you can enable it later without restarting." >> .env.production

# Generate ClickHouse user XML from environment password
CH_PASS=$(grep '^CLICKHOUSE_PASSWORD=' .env.production | cut -d= -f2)
if [ -n "$CH_PASS" ]; then
  mkdir -p clickhouse/users.d
  cat > clickhouse/users.d/default-user.xml <<CHEOF
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
  echo "Generated clickhouse/users.d/default-user.xml"
fi

echo ""
echo "Generated .env.production with random secrets."
echo "Review and configure remaining settings before deployment."
