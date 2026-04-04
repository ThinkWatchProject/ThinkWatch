use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub database_url: String,
    pub redis_url: String,
    pub jwt_secret: String,
    pub encryption_key: String,
    pub server_host: String,

    /// Gateway port: AI API (/v1/*) + MCP (/mcp) + health check
    pub gateway_port: u16,
    /// Console port: Web UI (/api/*) + management API
    pub console_port: u16,

    pub cors_origins: Vec<String>,

    // Quickwit (audit log search engine)
    pub quickwit_url: Option<String>,
    pub quickwit_index: String,
    pub quickwit_bearer_token: Option<String>,

    // OIDC / SSO (e.g. Zitadel)
    pub oidc_issuer_url: Option<String>,
    pub oidc_client_id: Option<String>,
    pub oidc_client_secret: Option<String>,
    pub oidc_redirect_url: Option<String>,
}

impl AppConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        dotenvy::dotenv().ok();

        Ok(Self {
            database_url: std::env::var("DATABASE_URL")
                .expect("DATABASE_URL environment variable is required"),
            redis_url: std::env::var("REDIS_URL")
                .expect("REDIS_URL environment variable is required"),
            jwt_secret: std::env::var("JWT_SECRET")
                .expect("JWT_SECRET environment variable is required"),
            encryption_key: std::env::var("ENCRYPTION_KEY")
                .expect("ENCRYPTION_KEY environment variable is required"),
            server_host: std::env::var("SERVER_HOST").unwrap_or_else(|_| "0.0.0.0".into()),
            gateway_port: std::env::var("GATEWAY_PORT")
                .unwrap_or_else(|_| "3000".into())
                .parse()?,
            console_port: std::env::var("CONSOLE_PORT")
                .unwrap_or_else(|_| "3001".into())
                .parse()?,
            cors_origins: std::env::var("CORS_ORIGINS")
                .unwrap_or_else(|_| "http://localhost:5173".into())
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),

            // Quickwit
            quickwit_url: std::env::var("QUICKWIT_URL").ok(),
            quickwit_index: std::env::var("QUICKWIT_INDEX").unwrap_or_else(|_| "audit_logs".into()),
            quickwit_bearer_token: std::env::var("QUICKWIT_BEARER_TOKEN").ok(),

            // OIDC
            oidc_issuer_url: std::env::var("OIDC_ISSUER_URL").ok(),
            oidc_client_id: std::env::var("OIDC_CLIENT_ID").ok(),
            oidc_client_secret: std::env::var("OIDC_CLIENT_SECRET").ok(),
            oidc_redirect_url: std::env::var("OIDC_REDIRECT_URL").ok(),
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.encryption_key.len() != 64 {
            return Err("ENCRYPTION_KEY must be exactly 64 hex characters (32 bytes)".into());
        }
        if hex::decode(&self.encryption_key).is_err() {
            return Err("ENCRYPTION_KEY must be valid hex characters".into());
        }

        // JWT secret entropy check
        if self.jwt_secret.len() < 32 {
            return Err("JWT_SECRET must be at least 32 characters (256 bits)".into());
        }
        if self
            .jwt_secret
            .chars()
            .collect::<std::collections::HashSet<_>>()
            .len()
            <= 1
        {
            return Err("JWT_SECRET must not consist of a single repeated character".into());
        }

        // Redis must have a password
        if !self.redis_url.contains("://default:") && !self.redis_url.contains("://redis:") {
            // Accept password in userinfo or query param
            let has_password = self.redis_url.contains('@')
                && !self.redis_url.starts_with("redis://@")
                && !self.redis_url.starts_with("redis://localhost")
                && !self.redis_url.starts_with("redis://127.0.0.1");
            if !has_password {
                tracing::warn!(
                    "REDIS_URL does not appear to contain a password — ensure Redis requires authentication in production"
                );
            }
        }

        // Quickwit auth warning
        if self.quickwit_url.is_some() && self.quickwit_bearer_token.is_none() {
            tracing::warn!(
                "QUICKWIT_URL is set without QUICKWIT_BEARER_TOKEN — Quickwit has no authentication. Ensure it is only accessible on a private network"
            );
        }

        Ok(())
    }

    pub fn gateway_addr(&self) -> String {
        format!("{}:{}", self.server_host, self.gateway_port)
    }

    pub fn console_addr(&self) -> String {
        format!("{}:{}", self.server_host, self.console_port)
    }

    #[cfg(test)]
    pub(crate) fn test_config(jwt_secret: &str, encryption_key: &str) -> Self {
        Self {
            database_url: "postgres://test".into(),
            redis_url: "redis://test".into(),
            jwt_secret: jwt_secret.into(),
            encryption_key: encryption_key.into(),
            server_host: "0.0.0.0".into(),
            gateway_port: 3000,
            console_port: 3001,
            cors_origins: vec!["http://localhost".into()],
            quickwit_url: None,
            quickwit_index: "test".into(),
            quickwit_bearer_token: None,
            oidc_issuer_url: None,
            oidc_client_id: None,
            oidc_client_secret: None,
            oidc_redirect_url: None,
        }
    }

    pub fn audit_config(&self) -> crate::audit::AuditConfig {
        crate::audit::AuditConfig {
            quickwit_url: self.quickwit_url.clone(),
            quickwit_index: self.quickwit_index.clone(),
            quickwit_bearer_token: self.quickwit_bearer_token.clone(),
        }
    }

    pub fn oidc_enabled(&self) -> bool {
        self.oidc_issuer_url.is_some()
            && self.oidc_client_id.is_some()
            && self.oidc_client_secret.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(jwt_secret: &str, encryption_key: &str) -> AppConfig {
        AppConfig::test_config(jwt_secret, encryption_key)
    }

    #[test]
    fn validate_rejects_short_encryption_key() {
        // 32 hex chars instead of the required 64
        let cfg = make_config(
            "a]b2c3d4e5f6g7h8i9j0k1l2m3n4o5p6",
            "aabbccdd11223344aabbccdd11223344",
        );
        let result = cfg.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("64 hex characters"));
    }

    #[test]
    fn validate_rejects_short_jwt_secret() {
        // 16-char JWT secret, needs 32
        let cfg = make_config(
            "short_jwt_secret",
            "aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344",
        );
        let result = cfg.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("at least 32 characters"));
    }

    #[test]
    fn validate_rejects_single_char_jwt_secret() {
        // 34 chars but all 'a' — single repeated character
        let cfg = make_config(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344",
        );
        let result = cfg.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("single repeated character"));
    }

    #[test]
    fn validate_accepts_valid_config() {
        let cfg = make_config(
            "a_valid_jwt_secret_with_enough_entropy!",
            "aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344",
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_non_hex_encryption_key() {
        // 64 chars but contains non-hex characters (g, z, etc.)
        let cfg = make_config(
            "a_valid_jwt_secret_with_enough_entropy!",
            "zzzzzzzz11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344",
        );
        let result = cfg.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("valid hex"));
    }
}
