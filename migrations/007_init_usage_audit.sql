CREATE TABLE usage_records (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    api_key_id      UUID REFERENCES api_keys(id),
    user_id         UUID REFERENCES users(id),
    team_id         UUID REFERENCES teams(id),
    provider_id     UUID REFERENCES providers(id),
    model_id        VARCHAR(255) NOT NULL,
    request_type    VARCHAR(50) NOT NULL,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    total_tokens    INTEGER NOT NULL DEFAULT 0,
    cost_usd        DECIMAL(12, 8) NOT NULL DEFAULT 0,
    latency_ms      INTEGER,
    status_code     INTEGER,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_usage_records_created_at ON usage_records(created_at);
CREATE INDEX idx_usage_records_user_id ON usage_records(user_id, created_at);
CREATE INDEX idx_usage_records_team_id ON usage_records(team_id, created_at);
CREATE INDEX idx_usage_records_api_key_id ON usage_records(api_key_id, created_at);

-- Audit logs are stored in Quickwit only (see deploy/quickwit/audit_logs_index.yaml)

CREATE TABLE budget_alerts (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_type   VARCHAR(50) NOT NULL,
    target_id     UUID NOT NULL,
    threshold     DECIMAL(5, 2) NOT NULL,
    current_spend DECIMAL(12, 4),
    budget_limit  DECIMAL(12, 4),
    notified_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
