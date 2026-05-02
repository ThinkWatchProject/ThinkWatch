-- Seed an OAuth-shaped MCP store template so the install dialog's
-- OAuth credentials section has a live trigger path.
--
-- The existing 'linear' entry uses static-token (PAT) auth. This new
-- 'linear-oauth' slug is a parallel template that targets the same
-- public MCP endpoint (mcp.linear.app/sse) but routes auth through
-- Linear's OAuth flow. Both can coexist; admins pick whichever shape
-- matches their org policy.
--
-- Why no userinfo_endpoint: Linear doesn't expose an OIDC-style
-- userinfo. User identity is reachable via a GraphQL `me` query but
-- the resolver in `crates/server/src/handlers/mcp_oauth.rs` only
-- speaks plain HTTP GETs against the userinfo URL, so leaving it
-- NULL makes the resolver fall back to JWT decode (also unavailable
-- for Linear's opaque tokens) and ultimately store NULL upstream
-- subject — that's acceptable for the test path.
--
-- Idempotent on re-runs via ON CONFLICT (slug) DO NOTHING — no
-- upsert of existing rows.

INSERT INTO mcp_store_templates
    (slug, name, description, category, tags, endpoint_template,
     oauth_issuer, oauth_authorization_endpoint, oauth_token_endpoint,
     oauth_userinfo_endpoint, oauth_default_scopes,
     allow_static_token, static_token_help_url, auth_instructions,
     deploy_type, featured)
VALUES (
    'linear-oauth',
    'Linear (OAuth)',
    'Project management — issues, projects, and cycles. OAuth flow for org SSO.',
    'developer',
    '{"project","agile","oauth"}',
    'https://mcp.linear.app/sse',
    'https://linear.app',
    'https://linear.app/oauth/authorize',
    'https://api.linear.app/oauth/token',
    NULL,
    '{"read"}',
    FALSE,
    NULL,
    'Register an OAuth application at https://linear.app/settings/api/applications, copy the client ID and secret, and paste them into the install dialog. Linear''s OAuth uses opaque tokens so per-user display names will fall back to email from the access cookie.',
    'hosted',
    false
)
ON CONFLICT (slug) DO NOTHING;
