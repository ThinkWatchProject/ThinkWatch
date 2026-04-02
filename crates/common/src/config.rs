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
                .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/agent_bastion".into()),
            redis_url: std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://localhost:6379".into()),
            jwt_secret: std::env::var("JWT_SECRET")
                .expect("JWT_SECRET environment variable is required"),
            encryption_key: std::env::var("ENCRYPTION_KEY")
                .expect("ENCRYPTION_KEY environment variable is required"),
            server_host: std::env::var("SERVER_HOST")
                .unwrap_or_else(|_| "0.0.0.0".into()),
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
            quickwit_index: std::env::var("QUICKWIT_INDEX")
                .unwrap_or_else(|_| "audit_logs".into()),

            // OIDC
            oidc_issuer_url: std::env::var("OIDC_ISSUER_URL").ok(),
            oidc_client_id: std::env::var("OIDC_CLIENT_ID").ok(),
            oidc_client_secret: std::env::var("OIDC_CLIENT_SECRET").ok(),
            oidc_redirect_url: std::env::var("OIDC_REDIRECT_URL").ok(),
        })
    }

    pub fn validate(&self) {
        if self.encryption_key.len() != 64 {
            panic!("ENCRYPTION_KEY must be exactly 64 hex characters (32 bytes)");
        }
    }

    pub fn gateway_addr(&self) -> String {
        format!("{}:{}", self.server_host, self.gateway_port)
    }

    pub fn console_addr(&self) -> String {
        format!("{}:{}", self.server_host, self.console_port)
    }

    pub fn audit_config(&self) -> crate::audit::AuditConfig {
        crate::audit::AuditConfig {
            quickwit_url: self.quickwit_url.clone(),
            quickwit_index: self.quickwit_index.clone(),
        }
    }

    pub fn oidc_enabled(&self) -> bool {
        self.oidc_issuer_url.is_some()
            && self.oidc_client_id.is_some()
            && self.oidc_client_secret.is_some()
    }
}
