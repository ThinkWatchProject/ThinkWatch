//! Per-user upstream credential lookup.
//!
//! For every MCP `tools/call` the proxy asks the resolver to produce
//! the `Authorization` header that should be attached to the upstream
//! request. The resolver:
//!
//! 1. Picks which row of `mcp_user_credentials` applies — either the
//!    one the API key's `mcp_account_overrides` map points at, or the
//!    user's `is_default` credential for that server.
//! 2. Decrypts the stored access_token. For `static_token` rows that
//!    is the token verbatim; for `oauth_authcode` rows we additionally
//!    refresh against the upstream's token endpoint when the cached
//!    access_token is within 60s of `expires_at`.
//! 3. Surfaces a `NeedsUserCredentials` error when the user (or this
//!    specific API key) hasn't connected yet — the proxy maps it to a
//!    JSON-RPC `-32050` so the console UI can prompt for authorization.
//!
//! Token refresh is serialized per `(server, user, account_label)` via
//! a Postgres advisory lock: when two concurrent calls both hit a
//! near-expiry token only one of them performs the network refresh.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::Row;
use uuid::Uuid;

use think_watch_common::crypto;

/// Snapshot of a server's OAuth client registration. Built once at
/// server-load time and threaded into every resolver call so we don't
/// hit the DB twice per request.
#[derive(Debug, Clone)]
pub struct OAuthClientCfg {
    pub token_endpoint: String,
    pub authorization_endpoint: Option<String>,
    pub client_id: String,
    /// Already decrypted at registry-load time so the hot path avoids
    /// per-request crypto.
    pub client_secret: String,
    pub scopes: Vec<String>,
}

/// What the resolver needs to know about the calling identity.
#[derive(Debug, Clone)]
pub struct ResolverCaller {
    pub user_id: Uuid,
    /// Parsed `api_keys.mcp_account_overrides` JSON. Looked up by
    /// `server_id.to_string()`; missing key ⇒ use the user's default.
    pub mcp_account_overrides: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    /// The user (or the calling API key's chosen account label) has no
    /// credential row for this server. Carries enough context for the
    /// console UI to direct the user to the authorize / paste-token
    /// flow.
    #[error("user has no credential for MCP server {server_id}")]
    NeedsUserCredentials {
        server_id: Uuid,
        /// `Some` when the server speaks OAuth — the console can offer
        /// one-click authorize. `None` falls back to the static-token
        /// paste flow.
        authorize_url: Option<String>,
    },
    /// Refresh failed terminally (refresh_token revoked, scope removed,
    /// upstream 5xx). The stale row is purged so the next call surfaces
    /// `NeedsUserCredentials` and the user re-authorizes.
    #[error("refresh failed for MCP server {server_id}: {message}")]
    RefreshFailed { server_id: Uuid, message: String },
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("upstream HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid token-endpoint response: {0}")]
    BadTokenResponse(String),
}

/// Cached row read from `mcp_user_credentials`. Internal to the
/// resolver; never returned across module boundaries.
struct CredentialRow {
    credential_type: String,
    access_token_encrypted: Vec<u8>,
    refresh_token_encrypted: Option<Vec<u8>>,
    expires_at: Option<DateTime<Utc>>,
    account_label: String,
}

/// Resolves per-user credentials and refreshes OAuth tokens on the
/// fly. Cheap to clone; the inner state is `Arc`-shared.
#[derive(Clone)]
pub struct UserTokenResolver {
    db: PgPool,
    /// Already-parsed AES-GCM key (32 bytes). The server crate parses
    /// the hex `ENCRYPTION_KEY` once at boot and passes it in.
    crypto_key: [u8; 32],
    http: reqwest::Client,
}

impl UserTokenResolver {
    pub fn new(db: PgPool, crypto_key: [u8; 32], http: reqwest::Client) -> Self {
        Self {
            db,
            crypto_key,
            http,
        }
    }

    /// Produce the `(header_name, header_value)` tuple to attach to
    /// the upstream request, or:
    ///   * `Ok(None)` when the server has no auth requirement at all
    ///     (no OAuth config and `allow_static_token=false`),
    ///   * `Err(NeedsUserCredentials{..})` when the user must connect
    ///     before this call can proceed.
    pub async fn resolve(
        &self,
        server_id: Uuid,
        caller: &ResolverCaller,
        oauth_cfg: Option<&OAuthClientCfg>,
        allow_static_token: bool,
    ) -> Result<Option<(String, String)>, ResolverError> {
        let preferred_label = caller
            .mcp_account_overrides
            .get(server_id.to_string())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let row = self
            .fetch_row(server_id, caller.user_id, preferred_label.as_deref())
            .await?;

        let Some(row) = row else {
            // No credential found. Anonymous server ⇒ Ok(None);
            // server-needs-auth ⇒ ask the user to connect.
            if oauth_cfg.is_none() && !allow_static_token {
                return Ok(None);
            }
            return Err(ResolverError::NeedsUserCredentials {
                server_id,
                authorize_url: None,
            });
        };

        match row.credential_type.as_str() {
            "static_token" => {
                let token = self.decrypt_to_string(&row.access_token_encrypted)?;
                Ok(Some(("Authorization".into(), format!("Bearer {token}"))))
            }
            "oauth_authcode" => {
                let now = Utc::now();
                let near_expiry = row
                    .expires_at
                    .map(|exp| exp <= now + chrono::Duration::seconds(60))
                    .unwrap_or(false);

                if !near_expiry {
                    let token = self.decrypt_to_string(&row.access_token_encrypted)?;
                    return Ok(Some(("Authorization".into(), format!("Bearer {token}"))));
                }

                // Near or past expiry. Refresh under an advisory lock
                // so concurrent calls coalesce into a single network
                // round-trip.
                let cfg = oauth_cfg.ok_or_else(|| {
                    ResolverError::BadTokenResponse(
                        "oauth_authcode credential exists but server has no oauth client config"
                            .into(),
                    )
                })?;
                let token = self
                    .refresh_locked(server_id, caller.user_id, &row.account_label, &row, cfg)
                    .await?;
                Ok(Some(("Authorization".into(), format!("Bearer {token}"))))
            }
            other => Err(ResolverError::BadTokenResponse(format!(
                "unknown credential_type {other:?}"
            ))),
        }
    }

    async fn fetch_row(
        &self,
        server_id: Uuid,
        user_id: Uuid,
        preferred_label: Option<&str>,
    ) -> Result<Option<CredentialRow>, ResolverError> {
        let row_opt = if let Some(label) = preferred_label {
            sqlx::query(
                r#"SELECT credential_type, access_token_encrypted,
                          refresh_token_encrypted, expires_at, account_label
                     FROM mcp_user_credentials
                    WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
            )
            .bind(server_id)
            .bind(user_id)
            .bind(label)
            .fetch_optional(&self.db)
            .await?
        } else {
            sqlx::query(
                r#"SELECT credential_type, access_token_encrypted,
                          refresh_token_encrypted, expires_at, account_label
                     FROM mcp_user_credentials
                    WHERE mcp_server_id = $1 AND user_id = $2 AND is_default"#,
            )
            .bind(server_id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?
        };

        Ok(row_opt.map(|r| CredentialRow {
            credential_type: r.get("credential_type"),
            access_token_encrypted: r.get("access_token_encrypted"),
            refresh_token_encrypted: r.get("refresh_token_encrypted"),
            expires_at: r.get("expires_at"),
            account_label: r.get("account_label"),
        }))
    }

    /// Refresh the access token under a Postgres advisory lock keyed
    /// by `(server_id, user_id, account_label)`. The lock is held for
    /// the duration of the transaction, which means concurrent callers
    /// either all see the freshly refreshed value or all participate
    /// in the refresh-failed unwind.
    async fn refresh_locked(
        &self,
        server_id: Uuid,
        user_id: Uuid,
        account_label: &str,
        row: &CredentialRow,
        cfg: &OAuthClientCfg,
    ) -> Result<String, ResolverError> {
        let lock_key = format!("mcp_token_refresh:{server_id}:{user_id}:{account_label}");
        let mut tx = self.db.begin().await?;

        // Take the advisory lock. `hashtextextended(text, 0)` returns
        // a stable bigint so this is idempotent across processes.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&lock_key)
            .execute(&mut *tx)
            .await?;

        // Re-check the row inside the lock. Another caller may have
        // already done the refresh while we were waiting.
        let recheck = sqlx::query(
            r#"SELECT credential_type, access_token_encrypted,
                      refresh_token_encrypted, expires_at, account_label
                 FROM mcp_user_credentials
                WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
        )
        .bind(server_id)
        .bind(user_id)
        .bind(account_label)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(current) = recheck else {
            // Row deleted between fetch and lock acquisition.
            tx.rollback().await.ok();
            return Err(ResolverError::NeedsUserCredentials {
                server_id,
                authorize_url: cfg.authorization_endpoint.clone(),
            });
        };

        let now = Utc::now();
        let still_near_expiry: bool = current
            .get::<Option<DateTime<Utc>>, _>("expires_at")
            .map(|exp| exp <= now + chrono::Duration::seconds(60))
            .unwrap_or(true);
        if !still_near_expiry {
            // Lost the race — another caller already refreshed. Use
            // their value.
            let bytes: Vec<u8> = current.get("access_token_encrypted");
            let token = self.decrypt_to_string(&bytes)?;
            tx.commit().await?;
            return Ok(token);
        }

        // Decrypt the refresh_token.
        let refresh_bytes =
            row.refresh_token_encrypted
                .as_ref()
                .ok_or_else(|| ResolverError::RefreshFailed {
                    server_id,
                    message: "no refresh_token stored".into(),
                })?;
        let refresh_token = self.decrypt_to_string(refresh_bytes)?;

        // Run the refresh.
        let new = match self.oauth_refresh(cfg, &refresh_token).await {
            Ok(v) => v,
            Err(e) => {
                // Purge so the next call returns NeedsUserCredentials.
                sqlx::query(
                    "DELETE FROM mcp_user_credentials WHERE mcp_server_id = $1
                     AND user_id = $2 AND account_label = $3",
                )
                .bind(server_id)
                .bind(user_id)
                .bind(account_label)
                .execute(&mut *tx)
                .await?;
                tx.commit().await.ok();
                return Err(ResolverError::RefreshFailed {
                    server_id,
                    message: e.to_string(),
                });
            }
        };

        let new_access_encrypted = self.encrypt(new.access_token.as_bytes())?;
        let new_refresh_encrypted = match &new.refresh_token {
            Some(r) => Some(self.encrypt(r.as_bytes())?),
            // Some upstreams omit refresh_token on refresh — keep the
            // existing one.
            None => Some(refresh_bytes.clone()),
        };
        let new_expires_at = new
            .expires_in
            .map(|secs| now + chrono::Duration::seconds(secs as i64));

        sqlx::query(
            r#"UPDATE mcp_user_credentials
                  SET access_token_encrypted  = $4,
                      refresh_token_encrypted = $5,
                      expires_at              = $6,
                      updated_at              = now()
                WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
        )
        .bind(server_id)
        .bind(user_id)
        .bind(account_label)
        .bind(&new_access_encrypted)
        .bind(&new_refresh_encrypted)
        .bind(new_expires_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(new.access_token)
    }

    async fn oauth_refresh(
        &self,
        cfg: &OAuthClientCfg,
        refresh_token: &str,
    ) -> Result<TokenResponse, ResolverError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
        ];
        let scope_joined;
        if !cfg.scopes.is_empty() {
            scope_joined = cfg.scopes.join(" ");
            form.push(("scope", &scope_joined));
        }

        let body = serde_urlencoded::to_string(&form)
            .map_err(|e| ResolverError::BadTokenResponse(format!("encode form: {e}")))?;

        let resp = self
            .http
            .post(&cfg.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(ResolverError::BadTokenResponse(format!(
                "token_endpoint returned {status}: {text}"
            )));
        }
        let parsed: TokenResponse = serde_json::from_str(&text)
            .map_err(|e| ResolverError::BadTokenResponse(format!("{e}: {text}")))?;
        Ok(parsed)
    }

    fn decrypt_to_string(&self, bytes: &[u8]) -> Result<String, ResolverError> {
        let plain = crypto::decrypt(bytes, &self.crypto_key)
            .map_err(|e| ResolverError::Crypto(e.to_string()))?;
        String::from_utf8(plain).map_err(|e| ResolverError::Crypto(e.to_string()))
    }

    fn encrypt(&self, plain: &[u8]) -> Result<Vec<u8>, ResolverError> {
        crypto::encrypt(plain, &self.crypto_key).map_err(|e| ResolverError::Crypto(e.to_string()))
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    /// Seconds until expiry. RFC 6749 declares this as a number;
    /// some upstreams stringify it — `serde` deserializes the number
    /// path on its own, callers fix the string case upstream of this
    /// crate if it ever appears.
    #[serde(default)]
    expires_in: Option<u64>,
}
