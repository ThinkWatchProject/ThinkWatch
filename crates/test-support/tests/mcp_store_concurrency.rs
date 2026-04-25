//! Concurrency tests for the MCP store install path. Pin the
//! contract that two known races CANNOT recur:
//!
//! 1. Same-template parallel installs — the `FOR UPDATE` on the
//!    template row plus collision resolution must hand each install
//!    a distinct `(name, namespace_prefix)` and the install_count
//!    must equal the number of installs.
//!
//! 2. Cross-template parallel installs whose default names happen
//!    to collide — this used to trip the UNIQUE constraint on
//!    `mcp_servers.name` because `FOR UPDATE` only serialised
//!    same-template installs. The advisory lock added to
//!    `install_template_into_db` makes it serialise across all
//!    installs.

use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use think_watch_server::handlers::mcp_store::install_template_into_db;
use think_watch_test_support::prelude::*;
use uuid::Uuid;

/// Insert a fresh `mcp_store_templates` row and return its id.
/// Bypasses `sync_registry` because that pulls from a remote URL.
async fn seed_template(db: &PgPool, slug: &str, default_name: &str) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO mcp_store_templates
            (slug, name, description, category, endpoint_template, deploy_type)
           VALUES ($1, $2, 'integration test', 'developer',
                   'https://example.com/mcp', 'manual')
           RETURNING id"#,
    )
    .bind(slug)
    .bind(default_name)
    .fetch_one(db)
    .await
    .unwrap();
    id
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn parallel_installs_same_template_keep_count_in_sync() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    // Slug feeds into the namespace_prefix default
    // (`slug.replace('-', '_')`), which must match
    // `[a-z0-9_]{1,32}`. Keep it short.
    let slug = format!("g{}", &Uuid::new_v4().simple().to_string()[..8]);
    let template_id = seed_template(&app.db, &slug, "GitHub IT").await;

    const N: usize = 5;
    let pool = Arc::new(app.db.clone());
    let installer = admin.user.id;

    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            install_template_into_db(
                &pool,
                template_id,
                "github-it",
                "https://example.com/mcp",
                "streamable_http",
                None,
                json!({}),
                None,
                None,
                installer,
            )
            .await
        }));
    }

    let mut servers = Vec::with_capacity(N);
    for h in handles {
        let server = h.await.unwrap().expect("install must succeed");
        servers.push(server);
    }

    // Names + namespace_prefixes are pairwise distinct (the
    // collision resolver hands out _2, _3, …).
    let mut names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    let mut prefixes: Vec<&str> = servers
        .iter()
        .map(|s| s.namespace_prefix.as_str())
        .collect();
    names.sort();
    prefixes.sort();
    names.dedup();
    prefixes.dedup();
    assert_eq!(
        names.len(),
        N,
        "all server names should be distinct: {servers:?}"
    );
    assert_eq!(
        prefixes.len(),
        N,
        "all namespace_prefixes should be distinct"
    );

    // install_count + install records both equal N.
    let count: i32 =
        sqlx::query_scalar("SELECT install_count FROM mcp_store_templates WHERE id = $1")
            .bind(template_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(count, N as i32, "install_count drifted");

    let install_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mcp_store_installs WHERE template_id = $1")
            .bind(template_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(install_rows, N as i64);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn parallel_installs_across_templates_with_colliding_names_serialise() {
    // Two distinct templates, same default `name`. Without the
    // process-wide advisory lock the second INSERT would race the
    // first to the same `(name)` row and trip the UNIQUE constraint
    // (5xx). With the lock, both installs land — the second one
    // resolves to "Shared #2".
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let slug_a = format!("a{}", &Uuid::new_v4().simple().to_string()[..8]);
    let slug_b = format!("b{}", &Uuid::new_v4().simple().to_string()[..8]);
    let t_a = seed_template(&app.db, &slug_a, "Shared").await;
    let t_b = seed_template(&app.db, &slug_b, "Shared").await;
    let installer = admin.user.id;
    let pool = Arc::new(app.db.clone());

    let pool_a = pool.clone();
    let pool_b = pool.clone();
    let h_a = tokio::spawn(async move {
        install_template_into_db(
            &pool_a,
            t_a,
            "shared-a",
            "https://example.com/mcp",
            "streamable_http",
            None,
            json!({}),
            None,
            None,
            installer,
        )
        .await
    });
    let h_b = tokio::spawn(async move {
        install_template_into_db(
            &pool_b,
            t_b,
            "shared-b",
            "https://example.com/mcp",
            "streamable_http",
            None,
            json!({}),
            None,
            None,
            installer,
        )
        .await
    });
    let (a, b) = (h_a.await.unwrap(), h_b.await.unwrap());

    let server_a = a.expect("first install must succeed");
    let server_b = b.expect("second install must succeed (advisory lock serialises)");
    assert_ne!(
        server_a.name, server_b.name,
        "advisory lock must hand out distinct names: {} vs {}",
        server_a.name, server_b.name
    );
    assert_ne!(server_a.namespace_prefix, server_b.namespace_prefix);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn install_into_deleted_template_returns_404() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let stale_id = Uuid::new_v4(); // never inserted

    let res = install_template_into_db(
        &app.db,
        stale_id,
        "nonexistent",
        "https://example.com/mcp",
        "streamable_http",
        None,
        json!({}),
        None,
        None,
        admin.user.id,
    )
    .await;
    assert!(res.is_err(), "missing template must yield an error");
}
