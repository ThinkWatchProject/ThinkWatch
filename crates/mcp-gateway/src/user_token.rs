//! Upstream credential resolution for MCP requests.
//!
//! For every MCP request the proxy asks the resolver to produce the
//! HTTP header that should be attached to the upstream call. Two
//! storage backends share the same lifecycle code:
//!
//!   * **Per-user** (`mcp_user_credentials`) — the historical mode.
//!     Each user authorizes / pastes their own token; the API key's
//!     `mcp_account_overrides` map decides which of their accounts
//!     this specific call should use.
//!
//!   * **Admin-shared** (`mcp_server_shared_credentials`) — one row
//!     per server, populated by an admin in the server-edit UI. Every
//!     caller of that server reuses the same upstream credential.
//!     Per-user audit / quota attribution is unchanged: the calling
//!     user is still recorded as the actor; only the upstream bearer
//!     is shared.
//!
//! Both backends speak the same shape (`access_token_encrypted`,
//! `refresh_token_encrypted`, `expires_at`) so the OAuth-refresh code
//! is unified — the only thing that changes between modes is the SQL
//! that fetches and updates the row, dispatched through
//! [`CredLocator`].
//!
//! The header that finally hits the upstream is built from two
//! per-server fields: `auth_header_name` and `auth_value_template`.
//! The template carries a single `{{token}}` placeholder; everything
//! else is literal. This is intentionally narrower than
//! `custom_headers` — that one substitutes `{{user_id}}` /
//! `{{user_email}}` for non-secret identification, and conflating
//! the two kinds of templating in one spot has historically been a
//! source of bugs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as TokioMutex;
use uuid::Uuid;

use tw_crypto::crypto;

use crate::cache::McpResponseCache;

/// In-process per-key serialization for OAuth refresh.
///
/// The previous implementation took a `pg_advisory_xact_lock` and held
/// the surrounding transaction open across the upstream HTTP refresh
/// call. Under a slow OAuth provider every concurrent refresh held a
/// PG connection for the duration of the upstream round-trip, starving
/// the rest of the application of pool slots.
///
/// Now: per-`lock_key` `tokio::Mutex` serializes refreshes inside this
/// process; no DB connection is held during the HTTP call. The DB is
/// only touched in two short windows — recheck-and-decrypt before
/// the call, and write-back after. Cross-process races (multiple node
/// instances refreshing the same credential simultaneously) are
/// handled by recheck-on-entry plus the OAuth provider's own
/// refresh_token reuse detection: the loser sees a permanent failure
/// and the user re-authorizes. For ThinkWatch's single-node default
/// deployment this is not a concern.
type RefreshLockMap = Arc<StdMutex<HashMap<String, Arc<TokioMutex<()>>>>>;

/// Snapshot of a server's OAuth client registration. Built once at
/// server-load time and threaded into every resolver call so we don't
/// hit the DB twice per request.
#[derive(Debug, Clone)]
pub struct OAuthClientCfg {
    pub token_endpoint: String,
    pub authorization_endpoint: Option<String>,
    pub client_id: String,
    /// Already decrypted at registry-load time so the hot path avoids
    /// per-request crypto. `None` for public clients (AS advertises
    /// `token_endpoint_auth_methods_supported: ["none"]`, e.g. Feishu)
    /// where PKCE is the sole authenticator at the token endpoint.
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
}

/// Whether a server's credential is per-user or admin-shared. Drives
/// which storage backend the resolver reads/writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CredentialOwner {
    #[default]
    PerUser,
    AdminShared,
}

impl CredentialOwner {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialOwner::PerUser => "per_user",
            CredentialOwner::AdminShared => "admin_shared",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "admin_shared" => CredentialOwner::AdminShared,
            // Default to per_user for any unrecognized value — the
            // schema CHECK constraint guarantees only the two known
            // strings reach us, so this is just defensive.
            _ => CredentialOwner::PerUser,
        }
    }
}

/// Single-valued authentication shape. Mutually exclusive — there
/// is no "OAuth + PAT" combo. PAT and OAuth are different forms of
/// auth, not stages of a fallback chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthShape {
    #[default]
    Anonymous,
    OAuth,
    Static,
}

impl AuthShape {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthShape::Anonymous => "anonymous",
            AuthShape::OAuth => "oauth",
            AuthShape::Static => "static",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "oauth" => AuthShape::OAuth,
            "static" => AuthShape::Static,
            // CHECK constraint guarantees only the three known
            // strings reach us; defensive fallback to anonymous.
            _ => AuthShape::Anonymous,
        }
    }
}

/// Server-level auth configuration handed to the resolver. Built once
/// at registry-load time; immutable for the duration of a request.
#[derive(Debug, Clone)]
pub struct ServerAuthCfg {
    pub credential_owner: CredentialOwner,
    pub auth_shape: AuthShape,
    /// `Some` when `auth_shape == OAuth`. Required to refresh OAuth
    /// credentials (per-user or admin-shared). `None` for static and
    /// anonymous shapes.
    pub oauth_cfg: Option<OAuthClientCfg>,
    /// HTTP header under which the token is injected (`Authorization`,
    /// `X-API-Key`, …).
    pub auth_header_name: String,
    /// Header value template — `{{token}}` is replaced with the
    /// resolved access token. Validated at write time.
    pub auth_value_template: String,
}

impl ServerAuthCfg {
    /// Apply the template to a resolved token. Single-allocation —
    /// the template is short and called per request.
    fn build_injection(&self, token: &str) -> AuthInjection {
        AuthInjection {
            header_name: self.auth_header_name.clone(),
            header_value: self.auth_value_template.replace("{{token}}", token),
        }
    }

    /// Whether the server requires *some* credential before a request
    /// can succeed. Anonymous shape ⇒ no credential ever; OAuth /
    /// static ⇒ either a per-user row or a shared row must exist.
    fn needs_credential(&self) -> bool {
        !matches!(self.auth_shape, AuthShape::Anonymous)
    }
}

/// What the resolver needs to know about the calling identity.
#[derive(Debug, Clone)]
pub struct ResolverCaller {
    pub user_id: Uuid,
    /// Parsed `api_keys.mcp_account_overrides` JSON. Looked up by
    /// `server_id.to_string()`; missing key ⇒ use the user's default.
    /// Ignored for admin-shared servers.
    pub mcp_account_overrides: serde_json::Value,
}

/// Where the resolver should fetch the credential row from.
#[derive(Debug, Clone)]
enum CredLocator {
    /// Read from `mcp_user_credentials`, picking the row by
    /// `(server_id, user_id, preferred_label)` if `preferred_label` is
    /// set; otherwise the user's `is_default` row.
    PerUser {
        server_id: Uuid,
        user_id: Uuid,
        preferred_label: Option<String>,
    },
    /// Read from `mcp_server_shared_credentials` — single row per
    /// server.
    AdminShared { server_id: Uuid },
}

impl CredLocator {
    fn server_id(&self) -> Uuid {
        match self {
            CredLocator::PerUser { server_id, .. } => *server_id,
            CredLocator::AdminShared { server_id } => *server_id,
        }
    }

    /// Stable key for the `pg_advisory_xact_lock` that serializes
    /// concurrent OAuth refreshes. Per-user and admin_shared use
    /// disjoint key spaces — they can never collide.
    fn refresh_lock_key(&self) -> String {
        match self {
            CredLocator::PerUser {
                server_id,
                user_id,
                preferred_label: Some(label),
            } => format!("mcp_token_refresh:per_user:{server_id}:{user_id}:{label}"),
            CredLocator::PerUser {
                server_id,
                user_id,
                preferred_label: None,
            } => format!("mcp_token_refresh:per_user:{server_id}:{user_id}:__default__"),
            CredLocator::AdminShared { server_id } => {
                format!("mcp_token_refresh:admin_shared:{server_id}")
            }
        }
    }

    /// What identity to wipe from the response cache when the
    /// credential changes (refresh / permanent failure). For per-user
    /// we wipe just that user's lane; for admin_shared the whole
    /// server lane goes (every caller saw the previous bearer).
    async fn invalidate_cache_after_change(&self, cache: &McpResponseCache) {
        match self {
            CredLocator::PerUser {
                server_id, user_id, ..
            } => cache.invalidate_user_lane(server_id, user_id).await,
            CredLocator::AdminShared { server_id } => {
                cache.invalidate_server_lane(server_id).await;
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    /// The calling user (per-user mode) or the admin (admin-shared
    /// mode) hasn't connected yet. Carries enough context for the
    /// console UI to direct the right actor to the right flow.
    #[error("MCP server {server_id} has no credential available")]
    NeedsUserCredentials {
        server_id: Uuid,
        /// `Some` when the server speaks OAuth — the console can offer
        /// one-click authorize. `None` falls back to the static-token
        /// paste flow.
        authorize_url: Option<String>,
        /// Tells the console *who* needs to act: `PerUser` ⇒ the
        /// calling user must connect; `AdminShared` ⇒ an admin must
        /// configure the shared credential. Drives the message text.
        owner: CredentialOwner,
    },
    /// Refresh failed. Carries `kind` so callers can tell apart a
    /// permanent rejection (refresh_token revoked, scope removed —
    /// row deleted, configurer must re-authorize) from a transient
    /// hiccup (upstream 5xx, network blip — row preserved, next call
    /// retries).
    #[error("refresh failed for MCP server {server_id}: {message}")]
    RefreshFailed {
        server_id: Uuid,
        kind: RefreshFailureKind,
        message: String,
    },
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("upstream HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid token-endpoint response: {0}")]
    BadTokenResponse(String),
}

/// Discriminant on `ResolverError::RefreshFailed`. Drives whether the
/// caller should prompt for re-authorization or just surface a
/// transient-error retry hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailureKind {
    /// 4xx from token endpoint, encoding error, or malformed upstream
    /// response. Credential row deleted — the configurer must
    /// re-authorize.
    Permanent,
    /// 5xx / network / DNS / timeout. Credential row preserved — the
    /// next call will retry.
    Transient,
}

/// Auth header tuple finally attached to the upstream request.
#[derive(Debug, Clone)]
pub struct AuthInjection {
    pub header_name: String,
    pub header_value: String,
}

impl AuthInjection {
    pub fn as_pair(&self) -> (&str, &str) {
        (self.header_name.as_str(), self.header_value.as_str())
    }
}

/// Cached row read from either credential table. Internal to the
/// resolver; never crosses module boundaries.
struct CredentialRow {
    credential_type: String,
    access_token_encrypted: Vec<u8>,
    refresh_token_encrypted: Option<Vec<u8>>,
    expires_at: Option<DateTime<Utc>>,
    /// Only populated for per-user rows. None ⇒ admin_shared.
    account_label: Option<String>,
}

/// Resolves upstream credentials and refreshes OAuth tokens on the
/// fly. Cheap to clone; the inner state is `Arc`-shared.
#[derive(Clone)]
pub struct UserTokenResolver {
    db: PgPool,
    /// Already-parsed AES-GCM key (32 bytes). The server crate parses
    /// the hex `ENCRYPTION_KEY` once at boot and passes it in.
    crypto_key: [u8; 32],
    http: reqwest::Client,
    /// Response cache handle — used to invalidate after any credential
    /// change (refresh / permanent-failure delete) so pre-rotation
    /// cached responses can't be served against the post-rotation
    /// upstream identity.
    cache: McpResponseCache,
    /// Per-key tokio mutexes for refresh serialization. See
    /// [`RefreshLockMap`] doc.
    refresh_locks: RefreshLockMap,
}

impl UserTokenResolver {
    pub fn new(
        db: PgPool,
        crypto_key: [u8; 32],
        http: reqwest::Client,
        cache: McpResponseCache,
    ) -> Self {
        Self {
            db,
            crypto_key,
            http,
            cache,
            refresh_locks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Acquire (creating if needed) the per-key tokio mutex used to
    /// serialize OAuth refreshes for a given credential locator.
    fn refresh_mutex_for(&self, lock_key: &str) -> Arc<TokioMutex<()>> {
        let mut guard = self
            .refresh_locks
            .lock()
            .expect("refresh_locks mutex poisoned");
        Arc::clone(
            guard
                .entry(lock_key.to_owned())
                .or_insert_with(|| Arc::new(TokioMutex::new(()))),
        )
    }

    /// Drop the per-key mutex from the map if no other caller is
    /// currently holding or waiting on it. Called on the success path
    /// after a refresh so the map doesn't grow unbounded for
    /// long-lived processes that see many distinct credentials.
    fn try_evict_refresh_mutex(&self, lock_key: &str, mu: &Arc<TokioMutex<()>>) {
        // We hold one Arc, the map holds another. Anyone else awaiting
        // would also hold an Arc — so 2 means "only us + the map."
        if Arc::strong_count(mu) <= 2 {
            let mut guard = self
                .refresh_locks
                .lock()
                .expect("refresh_locks mutex poisoned");
            // Re-check under the std mutex: another caller may have
            // grabbed the Arc between our check and the lock.
            if let Some(entry) = guard.get(lock_key)
                && Arc::strong_count(entry) <= 2
            {
                guard.remove(lock_key);
            }
        }
    }

    /// Produce the auth header to attach to the upstream request, or
    /// `Ok(None)` when the server has no auth requirement at all.
    /// Returns `Err(NeedsUserCredentials)` when the server requires a
    /// credential but none is configured yet.
    pub async fn resolve(
        &self,
        server_id: Uuid,
        cfg: &ServerAuthCfg,
        caller: &ResolverCaller,
    ) -> Result<Option<AuthInjection>, ResolverError> {
        let locator = self.locator_for(server_id, cfg, caller);
        let row = self.fetch_row(&locator).await?;

        let Some(row) = row else {
            // No credential row. Anonymous server ⇒ Ok(None);
            // requires-auth ⇒ ask the appropriate actor to connect.
            if !cfg.needs_credential() {
                return Ok(None);
            }
            return Err(ResolverError::NeedsUserCredentials {
                server_id,
                authorize_url: None,
                owner: cfg.credential_owner,
            });
        };

        let token = match row.credential_type.as_str() {
            "static_token" => self.decrypt_to_string(&row.access_token_encrypted)?,
            "oauth_authcode" => {
                let now = Utc::now();
                let near_expiry = row
                    .expires_at
                    .map(|exp| exp <= now + chrono::Duration::seconds(60))
                    .unwrap_or(false);
                if !near_expiry {
                    self.decrypt_to_string(&row.access_token_encrypted)?
                } else {
                    let oauth_cfg = cfg.oauth_cfg.as_ref().ok_or_else(|| {
                        ResolverError::BadTokenResponse(
                            "oauth_authcode credential exists but server has no oauth client config"
                                .into(),
                        )
                    })?;
                    // For per-user defaults the locator carries
                    // `preferred_label: None`, but refresh writes need
                    // the exact label. Pull it off the row we just
                    // fetched and rebuild the locator so the inner
                    // refresh path always has a concrete target.
                    let refresh_locator = match (&locator, &row.account_label) {
                        (
                            CredLocator::PerUser {
                                server_id,
                                user_id,
                                preferred_label: None,
                            },
                            Some(actual_label),
                        ) => CredLocator::PerUser {
                            server_id: *server_id,
                            user_id: *user_id,
                            preferred_label: Some(actual_label.clone()),
                        },
                        _ => locator.clone(),
                    };
                    self.refresh_locked(&refresh_locator, &row, oauth_cfg)
                        .await?
                }
            }
            other => {
                return Err(ResolverError::BadTokenResponse(format!(
                    "unknown credential_type {other:?}"
                )));
            }
        };

        Ok(Some(cfg.build_injection(&token)))
    }

    fn locator_for(
        &self,
        server_id: Uuid,
        cfg: &ServerAuthCfg,
        caller: &ResolverCaller,
    ) -> CredLocator {
        match cfg.credential_owner {
            CredentialOwner::AdminShared => CredLocator::AdminShared { server_id },
            CredentialOwner::PerUser => {
                // When the API key's `mcp_account_overrides` map names
                // a label for this server, look it up *exactly* — no
                // fall-through to the user's `is_default` credential.
                // The override is an explicit "use *this* account"
                // routing decision.
                let preferred_label = caller
                    .mcp_account_overrides
                    .get(server_id.to_string())
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                CredLocator::PerUser {
                    server_id,
                    user_id: caller.user_id,
                    preferred_label,
                }
            }
        }
    }

    async fn fetch_row(
        &self,
        locator: &CredLocator,
    ) -> Result<Option<CredentialRow>, ResolverError> {
        let row_opt = match locator {
            CredLocator::PerUser {
                server_id,
                user_id,
                preferred_label: Some(label),
            } => sqlx::query(
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
            .map(|r| CredentialRow {
                credential_type: r.get("credential_type"),
                access_token_encrypted: r.get("access_token_encrypted"),
                refresh_token_encrypted: r.get("refresh_token_encrypted"),
                expires_at: r.get("expires_at"),
                account_label: Some(r.get("account_label")),
            }),
            CredLocator::PerUser {
                server_id,
                user_id,
                preferred_label: None,
            } => sqlx::query(
                r#"SELECT credential_type, access_token_encrypted,
                          refresh_token_encrypted, expires_at, account_label
                     FROM mcp_user_credentials
                    WHERE mcp_server_id = $1 AND user_id = $2 AND is_default"#,
            )
            .bind(server_id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?
            .map(|r| CredentialRow {
                credential_type: r.get("credential_type"),
                access_token_encrypted: r.get("access_token_encrypted"),
                refresh_token_encrypted: r.get("refresh_token_encrypted"),
                expires_at: r.get("expires_at"),
                account_label: Some(r.get("account_label")),
            }),
            CredLocator::AdminShared { server_id } => sqlx::query(
                r#"SELECT credential_type, access_token_encrypted,
                          refresh_token_encrypted, expires_at
                     FROM mcp_server_shared_credentials
                    WHERE mcp_server_id = $1"#,
            )
            .bind(server_id)
            .fetch_optional(&self.db)
            .await?
            .map(|r| CredentialRow {
                credential_type: r.get("credential_type"),
                access_token_encrypted: r.get("access_token_encrypted"),
                refresh_token_encrypted: r.get("refresh_token_encrypted"),
                expires_at: r.get("expires_at"),
                account_label: None,
            }),
        };
        Ok(row_opt)
    }

    /// Refresh the access token under an in-process per-key tokio
    /// mutex. The mutex is held across the upstream HTTP refresh so
    /// concurrent callers for the same credential see a single
    /// refresh, but NO Postgres connection is held during the HTTP —
    /// only short read/write windows before and after.
    ///
    /// Cross-process concurrency: refreshes from sibling processes
    /// race directly. The OAuth provider's refresh_token reuse
    /// detection is the source of truth — the loser sees a permanent
    /// failure and the user re-authorizes. Single-node deployments
    /// (the default) never hit this path.
    async fn refresh_locked(
        &self,
        locator: &CredLocator,
        row: &CredentialRow,
        cfg: &OAuthClientCfg,
    ) -> Result<String, ResolverError> {
        let lock_key = locator.refresh_lock_key();
        let mu = self.refresh_mutex_for(&lock_key);
        let _hold = mu.lock().await;

        // Phase 1 — short read window. Re-check the row inside the
        // serialization point: another caller in this process may
        // have refreshed while we were waiting on the mutex.
        let recheck = self.fetch_row(locator).await?;
        let Some(current) = recheck else {
            self.try_evict_refresh_mutex(&lock_key, &mu);
            return Err(ResolverError::NeedsUserCredentials {
                server_id: locator.server_id(),
                authorize_url: cfg.authorization_endpoint.clone(),
                owner: match locator {
                    CredLocator::PerUser { .. } => CredentialOwner::PerUser,
                    CredLocator::AdminShared { .. } => CredentialOwner::AdminShared,
                },
            });
        };

        let now = Utc::now();
        let still_near_expiry: bool = current
            .expires_at
            .map(|exp| exp <= now + chrono::Duration::seconds(60))
            .unwrap_or(true);
        if !still_near_expiry {
            // Lost the race — another caller already refreshed. Use
            // their value.
            let token = self.decrypt_to_string(&current.access_token_encrypted)?;
            self.try_evict_refresh_mutex(&lock_key, &mu);
            return Ok(token);
        }

        let refresh_bytes =
            row.refresh_token_encrypted
                .as_ref()
                .ok_or_else(|| ResolverError::RefreshFailed {
                    server_id: locator.server_id(),
                    kind: RefreshFailureKind::Permanent,
                    message: "no refresh_token stored".into(),
                })?;
        let refresh_token = self.decrypt_to_string(refresh_bytes)?;

        // Phase 2 — HTTP. NO DB connection held here.
        let new = match self.oauth_refresh(cfg, &refresh_token).await {
            Ok(v) => {
                metrics::counter!("mcp_token_refresh_total", "outcome" => "success").increment(1);
                v
            }
            Err(OAuthRefreshFailure::Transient(msg)) => {
                metrics::counter!("mcp_token_refresh_total", "outcome" => "transient_failure")
                    .increment(1);
                tracing::warn!(
                    server_id = %locator.server_id(), error = %msg,
                    "OAuth token refresh hit a transient failure; credential preserved for retry"
                );
                self.try_evict_refresh_mutex(&lock_key, &mu);
                return Err(ResolverError::RefreshFailed {
                    server_id: locator.server_id(),
                    kind: RefreshFailureKind::Transient,
                    message: msg,
                });
            }
            Err(OAuthRefreshFailure::Permanent(msg)) => {
                metrics::counter!("mcp_token_refresh_total", "outcome" => "permanent_failure")
                    .increment(1);
                tracing::warn!(
                    server_id = %locator.server_id(), error = %msg,
                    "OAuth token refresh failed permanently; credential row deleted"
                );
                let mut tx = self.db.begin().await?;
                self.delete_row(&mut tx, locator).await?;
                tx.commit().await.ok();
                locator.invalidate_cache_after_change(&self.cache).await;
                self.try_evict_refresh_mutex(&lock_key, &mu);
                return Err(ResolverError::RefreshFailed {
                    server_id: locator.server_id(),
                    kind: RefreshFailureKind::Permanent,
                    message: msg,
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
            // try_from so a u64::MAX from a malicious upstream
            // doesn't silently wrap into a negative i64.
            .and_then(|secs| i64::try_from(secs).ok())
            .map(|secs| now + chrono::Duration::seconds(secs));

        // Phase 3 — short write window.
        let mut tx = self.db.begin().await?;
        self.write_refreshed_row(
            &mut tx,
            locator,
            &new_access_encrypted,
            new_refresh_encrypted.as_deref(),
            new_expires_at,
        )
        .await?;
        tx.commit().await?;

        // Bearer just changed — pre-rotation cached responses are
        // stale relative to the new identity the upstream will see.
        locator.invalidate_cache_after_change(&self.cache).await;

        self.try_evict_refresh_mutex(&lock_key, &mu);
        Ok(new.access_token)
    }

    async fn delete_row(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        locator: &CredLocator,
    ) -> Result<(), ResolverError> {
        match locator {
            CredLocator::PerUser {
                server_id,
                user_id,
                preferred_label,
            } => {
                let label = preferred_label
                    .as_deref()
                    .ok_or_else(|| ResolverError::Crypto("missing label inside lock".into()))?;
                sqlx::query(
                    r#"DELETE FROM mcp_user_credentials
                        WHERE mcp_server_id = $1 AND user_id = $2 AND account_label = $3"#,
                )
                .bind(server_id)
                .bind(user_id)
                .bind(label)
                .execute(&mut **tx)
                .await?;
            }
            CredLocator::AdminShared { server_id } => {
                sqlx::query(
                    r#"DELETE FROM mcp_server_shared_credentials WHERE mcp_server_id = $1"#,
                )
                .bind(server_id)
                .execute(&mut **tx)
                .await?;
            }
        }
        Ok(())
    }

    async fn write_refreshed_row(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        locator: &CredLocator,
        access: &[u8],
        refresh: Option<&[u8]>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), ResolverError> {
        match locator {
            CredLocator::PerUser {
                server_id,
                user_id,
                preferred_label,
            } => {
                let label = preferred_label
                    .as_deref()
                    .ok_or_else(|| ResolverError::Crypto("missing label inside lock".into()))?;
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
                .bind(label)
                .bind(access)
                .bind(refresh)
                .bind(expires_at)
                .execute(&mut **tx)
                .await?;
            }
            CredLocator::AdminShared { server_id } => {
                sqlx::query(
                    r#"UPDATE mcp_server_shared_credentials
                          SET access_token_encrypted  = $2,
                              refresh_token_encrypted = $3,
                              expires_at              = $4,
                              updated_at              = now()
                        WHERE mcp_server_id = $1"#,
                )
                .bind(server_id)
                .bind(access)
                .bind(refresh)
                .bind(expires_at)
                .execute(&mut **tx)
                .await?;
            }
        }
        Ok(())
    }

    async fn oauth_refresh(
        &self,
        cfg: &OAuthClientCfg,
        refresh_token: &str,
    ) -> Result<TokenResponse, OAuthRefreshFailure> {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", cfg.client_id.as_str()),
        ];
        if let Some(secret) = cfg.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let scope_joined;
        if !cfg.scopes.is_empty() {
            scope_joined = cfg.scopes.join(" ");
            form.push(("scope", &scope_joined));
        }

        let body = serde_urlencoded::to_string(&form)
            .map_err(|e| OAuthRefreshFailure::Permanent(format!("encode form: {e}")))?;

        let resp = match self
            .http
            .post(&cfg.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Err(OAuthRefreshFailure::Transient(format!("network: {e}"))),
        };

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // Only treat actually-irrecoverable grant rejections as
            // Permanent — the original "any 4xx → Permanent" rule
            // would delete a live admin-shared credential on a
            // single 429 (rate limit), 408 (request timeout), or
            // 423 (locked) hiccup, forcing the entire org to
            // re-authorize. Permanent triggers credential row
            // deletion in `refresh_locked`; reserve it for cases
            // where the grant itself is dead.
            //
            // RFC 6749 §5.2 says `invalid_grant` is THE permanent
            // signal — refresh token revoked, expired, or no longer
            // valid for this client. Parse the JSON `error` field
            // before classifying. 401 we treat as Permanent too
            // (auth failed against the token endpoint itself).
            let msg = format!("token_endpoint returned {status}: {text}");
            let permanent = if status == reqwest::StatusCode::UNAUTHORIZED {
                true
            } else if status == reqwest::StatusCode::BAD_REQUEST {
                serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                    .as_deref()
                    == Some("invalid_grant")
            } else {
                false
            };
            return Err(if permanent {
                OAuthRefreshFailure::Permanent(msg)
            } else {
                OAuthRefreshFailure::Transient(msg)
            });
        }
        let parsed: TokenResponse = serde_json::from_str(&text).map_err(|e| {
            // Drop `text` from the message — a malformed-but-token-
            // bearing response (rare misbehaving servers) would
            // leak bearer values through the error path. The serde
            // position info in `{e}` is enough to triage.
            OAuthRefreshFailure::Permanent(format!("parse: {e}"))
        })?;
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

#[derive(Debug)]
enum OAuthRefreshFailure {
    Permanent(String),
    Transient(String),
}

/// Validate an `auth_value_template` at write time. Returns Err with
/// a user-facing message when the template references a placeholder
/// other than `{{token}}` — those are deliberately disallowed.
///
/// Stand-alone fn so handlers can call it before any DB write.
pub fn validate_auth_value_template(template: &str) -> Result<(), String> {
    if template.is_empty() {
        return Err("auth_value_template must not be empty".into());
    }
    // Walk the template looking for any `{{...}}` segment. The only
    // accepted token name is `token` — anything else (including
    // user_id / user_email which have meaning in custom_headers) is a
    // bug in the calling form, so we reject hard rather than silently
    // pass through.
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| "auth_value_template has unmatched `{{`".to_string())?;
        let name = after[..end].trim();
        if name != "token" {
            return Err(format!(
                "auth_value_template only supports the `{{{{token}}}}` placeholder; found `{{{{{name}}}}}`"
            ));
        }
        rest = &after[end + 2..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_only_token_placeholder_accepted() {
        validate_auth_value_template("Bearer {{token}}").unwrap();
        validate_auth_value_template("{{token}}").unwrap();
        validate_auth_value_template("token {{token}}").unwrap();
        validate_auth_value_template("plain literal").unwrap();
    }

    #[test]
    fn template_rejects_other_placeholders() {
        assert!(validate_auth_value_template("Bearer {{user_id}}").is_err());
        assert!(validate_auth_value_template("X {{user_email}} {{token}}").is_err());
        assert!(validate_auth_value_template("Bearer {{").is_err());
    }

    #[test]
    fn build_injection_substitutes_token() {
        let cfg = ServerAuthCfg {
            credential_owner: CredentialOwner::PerUser,
            auth_shape: AuthShape::Static,
            oauth_cfg: None,
            auth_header_name: "X-API-Key".into(),
            auth_value_template: "{{token}}".into(),
        };
        let inj = cfg.build_injection("ghp_abc");
        assert_eq!(inj.header_name, "X-API-Key");
        assert_eq!(inj.header_value, "ghp_abc");
    }

    #[test]
    fn build_injection_bearer_default() {
        let cfg = ServerAuthCfg {
            credential_owner: CredentialOwner::PerUser,
            auth_shape: AuthShape::Static,
            oauth_cfg: None,
            auth_header_name: "Authorization".into(),
            auth_value_template: "Bearer {{token}}".into(),
        };
        let inj = cfg.build_injection("xyz");
        assert_eq!(inj.header_value, "Bearer xyz");
    }

    #[test]
    fn build_injection_token_legacy_format() {
        let cfg = ServerAuthCfg {
            credential_owner: CredentialOwner::PerUser,
            auth_shape: AuthShape::Static,
            oauth_cfg: None,
            auth_header_name: "Authorization".into(),
            auth_value_template: "token {{token}}".into(),
        };
        let inj = cfg.build_injection("ghp_x");
        assert_eq!(inj.header_value, "token ghp_x");
    }

    #[test]
    fn admin_shared_locator_disjoint_lock_keys() {
        let server_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let shared = CredLocator::AdminShared { server_id }.refresh_lock_key();
        let per_user = CredLocator::PerUser {
            server_id,
            user_id,
            preferred_label: Some("work".into()),
        }
        .refresh_lock_key();
        assert!(shared.starts_with("mcp_token_refresh:admin_shared:"));
        assert!(per_user.starts_with("mcp_token_refresh:per_user:"));
        assert_ne!(shared, per_user);
    }
}
