-- Allow teams as rate-limit subjects
ALTER TABLE rate_limit_rules DROP CONSTRAINT IF EXISTS rate_limit_rules_subject_kind_check;
ALTER TABLE rate_limit_rules ADD CONSTRAINT rate_limit_rules_subject_kind_check
  CHECK (subject_kind IN ('user', 'api_key', 'provider', 'mcp_server', 'team'));
