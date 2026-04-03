-- Additional indexes for hot query paths
CREATE INDEX idx_api_keys_is_active ON api_keys(is_active) WHERE is_active = true;
CREATE INDEX idx_api_keys_expires_at ON api_keys(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX idx_usage_records_model_id ON usage_records(model_id, created_at);

-- Soft-delete support for compliance
ALTER TABLE users ADD COLUMN deleted_at TIMESTAMPTZ;
ALTER TABLE api_keys ADD COLUMN deleted_at TIMESTAMPTZ;
ALTER TABLE providers ADD COLUMN deleted_at TIMESTAMPTZ;
