-- Log forwarders: web-configurable audit log forwarding destinations
CREATE TABLE IF NOT EXISTS log_forwarders (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        VARCHAR(255) NOT NULL,
    forwarder_type VARCHAR(50) NOT NULL,  -- udp_syslog, tcp_syslog, kafka, webhook
    config      JSONB NOT NULL DEFAULT '{}',
    enabled     BOOLEAN NOT NULL DEFAULT true,
    sent_count  BIGINT NOT NULL DEFAULT 0,
    error_count BIGINT NOT NULL DEFAULT 0,
    last_sent_at TIMESTAMPTZ,
    last_error  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_log_forwarders_enabled ON log_forwarders (enabled);
