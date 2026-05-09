//! Concurrency tests for the template-install path on
//! `POST /api/mcp/servers` (with `template_slug`). Pin the contract
//! that two known races CANNOT recur:
//!
//! 1. Same-template parallel installs — the `FOR UPDATE` on the
//!    template row plus collision resolution must hand each install
//!    a distinct `(name, namespace_prefix)` and the install_count
//!    must equal the number of installs.
//!
//! 2. Cross-template parallel installs whose default names happen
//!    to collide — without the process-wide advisory lock, the
//!    second INSERT would race the first to the same `(name)` row
//!    and trip the UNIQUE constraint. The lock makes installs
//!    serialise across all templates, not just same-slug.
//!
//! These tests drive the public POST handler in parallel via
//! independent `TestClient`s, so the SSRF guard sees a normal
//! `https://example.com/mcp` URL and the template-install branch
//! exercises the full TX path (lock → resolve → INSERT server →
//! INSERT mcp_store_installs → bump install_count).

use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;
use think_watch_test_support::prelude::*;
use uuid::Uuid;

/// Insert a fresh `mcp_store_templates` row and return its slug.
/// Bypasses `sync_registry` because that pulls from a remote URL.
async fn seed_template(db: &PgPool, slug: &str, default_name: &str) -> String {
    sqlx::query(
        r#"INSERT INTO mcp_store_templates
            (slug, name, description, category, endpoint_template, deploy_type)
           VALUES ($1, $2, 'integration test', 'developer',
                   'https://example.com/mcp', 'manual')"#,
    )
    .bind(slug)
    .bind(default_name)
    .execute(db)
    .await
    .unwrap();
    slug.to_owned()
}

async fn admin_session(app: &TestApp) -> TestClient {
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn parallel_installs_same_template_keep_count_in_sync() {
    let app = TestApp::spawn().await;
    let pool = Arc::new(app.db.clone());

    // Slug feeds into the namespace_prefix default
    // (`slug.replace('-', '_')`), which must match
    // `[a-z0-9_]{1,32}`. Keep it short.
    let slug_owned = format!("g{}", &Uuid::new_v4().simple().to_string()[..8]);
    seed_template(&app.db, &slug_owned, "GitHub IT").await;

    const N: usize = 5;
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let app_ref = &app;
        let slug = slug_owned.clone();
        // Each parallel install runs in its own admin session
        // (independent cookie jar) — same admin user, different
        // logins. That's the closest test-harness analogue to N
        // browser tabs hitting the public endpoint at once.
        let con = admin_session(app_ref).await;
        handles.push(tokio::spawn(async move {
            con.post(
                "/api/mcp/servers",
                json!({
                    "name": "GitHub IT",
                    "namespace_prefix": slug.replace('-', "_"),
                    "endpoint_url": "https://example.com/mcp",
                    "transport_type": "streamable_http",
                    "template_slug": slug,
                }),
            )
            .await
        }));
    }

    let mut servers = Vec::with_capacity(N);
    for h in handles {
        let resp = h.await.unwrap().unwrap();
        resp.assert_ok();
        let body: Value = resp.json().unwrap();
        servers.push(body);
    }

    // Names + namespace_prefixes are pairwise distinct (the
    // collision resolver hands out _2, _3, …).
    let mut names: Vec<String> = servers
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect();
    let mut prefixes: Vec<String> = servers
        .iter()
        .map(|s| s["namespace_prefix"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    prefixes.sort();
    names.dedup();
    prefixes.dedup();
    assert_eq!(names.len(), N, "all server names should be distinct");
    assert_eq!(
        prefixes.len(),
        N,
        "all namespace_prefixes should be distinct"
    );

    // install_count + install records both equal N.
    let count: i32 =
        sqlx::query_scalar("SELECT install_count FROM mcp_store_templates WHERE slug = $1")
            .bind(&slug_owned)
            .fetch_one(&*pool)
            .await
            .unwrap();
    assert_eq!(count, N as i32, "install_count drifted");

    let install_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mcp_store_installs i
           JOIN mcp_store_templates t ON t.id = i.template_id
          WHERE t.slug = $1",
    )
    .bind(&slug_owned)
    .fetch_one(&*pool)
    .await
    .unwrap();
    assert_eq!(install_rows, N as i64);
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn parallel_installs_across_templates_with_colliding_names_serialise() {
    // Two distinct templates, same admin-supplied `name`. Without
    // the process-wide advisory lock the second INSERT would race
    // the first to the same `(name)` row and trip the UNIQUE
    // constraint (5xx). With the lock, both installs land — the
    // second one resolves to "Shared #2".
    let app = TestApp::spawn().await;
    let slug_a = format!("a{}", &Uuid::new_v4().simple().to_string()[..8]);
    let slug_b = format!("b{}", &Uuid::new_v4().simple().to_string()[..8]);
    seed_template(&app.db, &slug_a, "Shared").await;
    seed_template(&app.db, &slug_b, "Shared").await;

    let con_a = admin_session(&app).await;
    let con_b = admin_session(&app).await;
    let slug_a_clone = slug_a.clone();
    let slug_b_clone = slug_b.clone();

    let h_a = tokio::spawn(async move {
        con_a
            .post(
                "/api/mcp/servers",
                json!({
                    "name": "Shared",
                    "namespace_prefix": "shared",
                    "endpoint_url": "https://example.com/mcp",
                    "transport_type": "streamable_http",
                    "template_slug": slug_a_clone,
                }),
            )
            .await
    });
    let h_b = tokio::spawn(async move {
        con_b
            .post(
                "/api/mcp/servers",
                json!({
                    "name": "Shared",
                    "namespace_prefix": "shared",
                    "endpoint_url": "https://example.com/mcp",
                    "transport_type": "streamable_http",
                    "template_slug": slug_b_clone,
                }),
            )
            .await
    });
    let (a, b) = (h_a.await.unwrap().unwrap(), h_b.await.unwrap().unwrap());
    a.assert_ok();
    b.assert_ok();
    let body_a: Value = a.json().unwrap();
    let body_b: Value = b.json().unwrap();
    assert_ne!(
        body_a["name"].as_str(),
        body_b["name"].as_str(),
        "advisory lock must hand out distinct names",
    );
    assert_ne!(
        body_a["namespace_prefix"].as_str(),
        body_b["namespace_prefix"].as_str(),
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn install_into_unknown_template_returns_404() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let resp = con
        .post(
            "/api/mcp/servers",
            json!({
                "name": "ghost",
                "namespace_prefix": "ghost",
                "endpoint_url": "https://example.com/mcp",
                "transport_type": "streamable_http",
                "template_slug": "this-slug-does-not-exist",
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status,
        404,
        "unknown template_slug should 404; got: {}",
        resp.text()
    );
}
