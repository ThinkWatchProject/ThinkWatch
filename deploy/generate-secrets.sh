#!/usr/bin/env bash
set -euo pipefail

echo "# AgentBastion Production Secrets" > .env.production
echo "# Generated on $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> .env.production
echo "" >> .env.production

echo "JWT_SECRET=$(openssl rand -hex 32)" >> .env.production
echo "ENCRYPTION_KEY=$(openssl rand -hex 32)" >> .env.production
echo "DB_PASSWORD=$(openssl rand -base64 24 | tr -d '=/+')" >> .env.production
echo "REDIS_PASSWORD=$(openssl rand -base64 24 | tr -d '=/+')" >> .env.production
echo "CLICKHOUSE_PASSWORD=$(openssl rand -base64 24 | tr -d '=/+')" >> .env.production
echo "" >> .env.production

echo "DATABASE_URL=postgres://bastion:\${DB_PASSWORD}@postgres:5432/agent_bastion?sslmode=require" >> .env.production
echo "REDIS_URL=redis://:\${REDIS_PASSWORD}@redis:6379" >> .env.production
echo "SERVER_HOST=0.0.0.0" >> .env.production
echo "GATEWAY_PORT=3000" >> .env.production
echo "CONSOLE_PORT=3001" >> .env.production
echo "CORS_ORIGINS=https://console.yourdomain.com" >> .env.production
echo "RUST_LOG=info,agent_bastion=info" >> .env.production
echo "" >> .env.production
echo "# Configure these manually:" >> .env.production
echo "# CLICKHOUSE_URL=http://clickhouse:8123" >> .env.production
echo "# CLICKHOUSE_DB=agent_bastion" >> .env.production
echo "# CLICKHOUSE_USER=bastion" >> .env.production
echo "# OIDC_ISSUER_URL=" >> .env.production
echo "# OIDC_CLIENT_ID=" >> .env.production
echo "# OIDC_CLIENT_SECRET=" >> .env.production
echo "# OIDC_REDIRECT_URL=" >> .env.production

echo ""
echo "Generated .env.production with random secrets."
echo "Review and configure remaining settings before deployment."
