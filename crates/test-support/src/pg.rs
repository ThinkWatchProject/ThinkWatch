//! Per-test Postgres database isolation. Every `TestApp` gets a
//! freshly-created database via `CREATE DATABASE`, runs the workspace
//! migrations into it, and drops the database when the owning value
//! goes out of scope.

use anyhow::Context;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Executor, PgConnection, PgPool};
use std::time::Duration;
use uuid::Uuid;

/// Owns a per-test database. The pool is closed and the database is
/// dropped when this value is dropped.
pub struct IsolatedDatabase {
    base_url: String,
    name: String,
    url: String,
    pool: PgPool,
}

impl IsolatedDatabase {
    /// Create a fresh database off `base_url` and run the workspace
    /// migrations. `base_url` is the connection URL up to (but not
    /// including) the database name — e.g.
    /// `postgres://user:pwd@localhost:5432`.
    pub async fn create(base_url: &str) -> anyhow::Result<Self> {
        let admin_url = format!("{}/postgres", base_url.trim_end_matches('/'));
        let name = format!("test_tw_{}", Uuid::new_v4().simple());

        // Connect to the admin DB, CREATE DATABASE, disconnect.
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .with_context(|| format!("connect to admin DB at {admin_url}"))?;
        admin
            .execute(format!("CREATE DATABASE \"{name}\"").as_str())
            .await
            .context("CREATE DATABASE for test")?;
        let _ = admin.close().await;

        let url = format!("{}/{}", base_url.trim_end_matches('/'), name);
        let pool = PgPoolOptions::new()
            .max_connections(20)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&url)
            .await
            .context("connect to per-test DB")?;

        // Use the workspace migrator from common so the schema stays
        // in lockstep with production.
        think_watch_common::db::run_migrations(&pool)
            .await
            .context("run migrations into per-test DB")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            name,
            url,
            pool,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl Drop for IsolatedDatabase {
    fn drop(&mut self) {
        // Best-effort cleanup. We can't `await` here, so spawn a
        // detached task on the current runtime when one exists. If
        // the runtime is already shutting down (e.g. test panic) the
        // database lingers — `psql` cleanup script is the fallback.
        let admin_url = format!("{}/postgres", self.base_url);
        let name = std::mem::take(&mut self.name);
        let pool = self.pool.clone();
        let handle = tokio::runtime::Handle::try_current().ok();
        if let Some(h) = handle {
            h.spawn(async move {
                pool.close().await;
                if let Ok(mut conn) = PgConnection::connect(&admin_url).await {
                    let _ = conn
                        .execute(
                            format!(
                                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                                 WHERE datname = '{name}' AND pid <> pg_backend_pid()"
                            )
                            .as_str(),
                        )
                        .await;
                    let _ = conn
                        .execute(format!("DROP DATABASE IF EXISTS \"{name}\"").as_str())
                        .await;
                    let _ = conn.close().await;
                }
            });
        }
    }
}
