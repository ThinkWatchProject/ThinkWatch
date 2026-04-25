//! Per-test ClickHouse database isolation.
//!
//! Mirrors `pg.rs` for the analytics side: each test that opts into
//! ClickHouse gets a freshly-created database, the production
//! `ensure_clickhouse_tables` schema is loaded into it, and the
//! database is dropped on scope exit. Tests that don't need CH
//! analytics skip this entirely (`TestApp::spawn` defaults to no
//! ClickHouse client).

use anyhow::{Context, Result};
use clickhouse::Client;
use uuid::Uuid;

/// Owns a per-test ClickHouse database. Drops the database on Drop.
/// Held inside `TestApp` so its lifetime matches the test.
pub struct IsolatedClickHouseDatabase {
    url: String,
    user: Option<String>,
    password: Option<String>,
    name: String,
    client: Client,
}

impl IsolatedClickHouseDatabase {
    /// Create a fresh database, load the ThinkWatch schema, and
    /// return a wired-up client pointed at it.
    pub async fn create(url: &str, user: Option<&str>, password: Option<&str>) -> Result<Self> {
        let name = format!("test_tw_{}", Uuid::new_v4().simple());

        // Bootstrap client points at the default DB so we can
        // `CREATE DATABASE` regardless of whether `name` exists yet.
        let mut admin = Client::default().with_url(url).with_database("default");
        if let Some(u) = user {
            admin = admin.with_user(u);
        }
        if let Some(p) = password {
            admin = admin.with_password(p);
        }

        admin
            .query(&format!("CREATE DATABASE \"{name}\""))
            .execute()
            .await
            .context("CREATE DATABASE on test ClickHouse")?;

        // Test client targets the new DB.
        let mut client = Client::default().with_url(url).with_database(&name);
        if let Some(u) = user {
            client = client.with_user(u);
        }
        if let Some(p) = password {
            client = client.with_password(p);
        }

        // Load the production schema into the per-test DB. The
        // bundled init SQL contains a `CREATE DATABASE IF NOT EXISTS
        // think_watch;` line we have to skip, but every CREATE TABLE
        // statement is unqualified and therefore lands in the
        // connection's current database.
        load_schema(&client).await?;

        Ok(Self {
            url: url.to_string(),
            user: user.map(String::from),
            password: password.map(String::from),
            name,
            client,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn database(&self) -> &str {
        &self.name
    }
}

impl Drop for IsolatedClickHouseDatabase {
    fn drop(&mut self) {
        let url = self.url.clone();
        let user = self.user.clone();
        let password = self.password.clone();
        let name = std::mem::take(&mut self.name);
        let handle = tokio::runtime::Handle::try_current().ok();
        if let Some(h) = handle {
            h.spawn(async move {
                let mut admin = Client::default().with_url(&url).with_database("default");
                if let Some(u) = user {
                    admin = admin.with_user(u);
                }
                if let Some(p) = password {
                    admin = admin.with_password(p);
                }
                let _ = admin
                    .query(&format!("DROP DATABASE IF EXISTS \"{name}\""))
                    .execute()
                    .await;
            });
        }
    }
}

async fn load_schema(client: &Client) -> Result<()> {
    let init_sql = include_str!("../../../deploy/clickhouse/initdb.d/01_init.sql");

    // Strip `--` line comments and the embedded `CREATE DATABASE`
    // (handled out-of-band) before splitting on `;`. A semicolon
    // inside a comment would otherwise split a CREATE TABLE in half.
    let cleaned: String = init_sql
        .lines()
        .map(|l| l.find("--").map(|i| &l[..i]).unwrap_or(l))
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    for stmt in cleaned.split(';') {
        let trimmed = stmt.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.to_ascii_uppercase().starts_with("CREATE DATABASE") {
            // Skip the `CREATE DATABASE think_watch` line — we
            // already created our per-test DB and the client is
            // already pointed at it.
            continue;
        }
        client
            .query(trimmed)
            .execute()
            .await
            .with_context(|| format!("CH schema stmt failed: {trimmed:.120}"))?;
    }
    Ok(())
}
