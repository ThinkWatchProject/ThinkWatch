CREATE TABLE mcp_call_logs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    server_id       UUID REFERENCES mcp_servers(id) ON DELETE SET NULL,
    tool_name       VARCHAR(255) NOT NULL,
    user_id         UUID REFERENCES users(id) ON DELETE SET NULL,
    duration_ms     INTEGER,
    status          VARCHAR(50) NOT NULL DEFAULT 'success',
    error_message   TEXT,
    request_payload JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_mcp_call_logs_created_at ON mcp_call_logs(created_at);
CREATE INDEX idx_mcp_call_logs_server_id ON mcp_call_logs(server_id, created_at);
