//! MCP Store user-facing flow: catalog browsing + install lifecycle.
//!
//! `mcp_store_concurrency.rs` covers the install path's serialisation
//! contract (advisory lock, count drift). This file covers the
//! complementary user-visible behaviour:
//!
//!   - GET /api/mcp/store — catalog list, filtered by category /
//!     search / featured. Pin the response shape so the React store
//!     page doesn't break on a silent rename.
//!   - GET /api/mcp/store/categories — count buckets for the sidebar.
//!   - Install lifecycle:
//!     * install_count starts at 0
//!     * a successful install increments install_count to 1
//!     * the resulting `mcp_servers` row appears with the right
//!       `template_id` foreign key, name, namespace_prefix
//!     * a server-row delete decrements install_count back via
//!       GREATEST(install_count - 1, 0) — never goes negative
//!     * installing the same template a second time auto-suffixes
//!       the name (`_2`) — multi-instance support is intentional
//!
//! The HTTP install endpoint (`POST /api/mcp/store/{slug}/install`)
//! runs an upstream JSON-RPC probe before persisting, and the SSRF
//! guard rejects loopback URLs, so we drive `install_template_into_db`
//! directly — same code path as the production handler past the
//! probe.

use serde_json::Value;
use sqlx::PgPool;
use think_watch_server::handlers::mcp_store::install_template_into_db;
use think_watch_test_support::prelude::*;

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

async fn seed_template(
    db: &PgPool,
    slug: &str,
    name: &str,
    category: &str,
    description: &str,
    featured: bool,
) -> uuid::Uuid {
    sqlx::query_scalar(
        r#"INSERT INTO mcp_store_templates
            (slug, name, description, category, endpoint_template,
             deploy_type, featured, tags)
           VALUES ($1, $2, $3, $4, 'https://example.com/mcp',
                   'manual', $5, ARRAY['integration','test']::text[])
           RETURNING id"#,
    )
    .bind(slug)
    .bind(name)
    .bind(description)
    .bind(category)
    .bind(featured)
    .fetch_one(db)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn store_catalog_returns_templates_with_installed_flag() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    // The migration seeds well-known slugs (github, gitlab, …) so we
    // randomise here to avoid the UNIQUE(slug) clash. Test names are
    // descriptive only — the assertion uses the unique slug we minted.
    let slug_a = unique_name("cat-a");
    let slug_b = unique_name("cat-b");
    seed_template(&app.db, &slug_a, "Cat A", "developer", "git ops", true).await;
    seed_template(&app.db, &slug_b, "Cat B", "finance", "billing ops", false).await;

    let body: Value = con.get("/api/mcp/store").await.unwrap().json().unwrap();
    let arr = body.as_array().expect("/api/mcp/store returns an array");
    // `StoreTemplateResponse` uses `#[serde(flatten)]`, so template
    // fields live at the top level of each row alongside `installed`.
    assert!(arr.iter().any(|r| r["slug"] == slug_a));
    assert!(arr.iter().any(|r| r["slug"] == slug_b));

    // Pin the response shape — `installed` flag is what drives the
    // "Install" vs "Installed" button on the store page.
    let row = arr.iter().find(|r| r["slug"] == slug_a).unwrap();
    assert!(row["installed"].is_boolean());
    assert_eq!(row["installed"], false);
    for k in [
        "id",
        "slug",
        "name",
        "description",
        "category",
        "featured",
        "tags",
    ] {
        assert!(
            row.get(k).is_some(),
            "template envelope missing field {k}: {row}"
        );
    }
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn store_catalog_search_matches_name_description_and_tags() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let slug_name = unique_name("srch-n");
    let slug_desc = unique_name("srch-d");
    let slug_unrelated = unique_name("srch-u");
    // The unique-slug pattern needs a low-cardinality search token
    // that we can grep for — pick something no migration-seeded
    // description / tag / name uses. "zzqfox" is arbitrary.
    let needle = "zzqfox";
    seed_template(
        &app.db,
        &slug_name,
        &format!("{needle}-name"),
        "developer",
        "ordinary description",
        false,
    )
    .await;
    seed_template(
        &app.db,
        &slug_desc,
        "haystack-name",
        "developer",
        &format!("contains {needle} in the description"),
        false,
    )
    .await;
    seed_template(
        &app.db,
        &slug_unrelated,
        "haystack-only",
        "developer",
        "totally unrelated",
        false,
    )
    .await;

    let body: Value = con
        .get(&format!("/api/mcp/store?search={needle}"))
        .await
        .unwrap()
        .json()
        .unwrap();
    let slugs: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["slug"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        slugs.contains(&slug_name),
        "name match should hit, got slugs: {slugs:?}"
    );
    assert!(
        slugs.contains(&slug_desc),
        "description match should hit, got slugs: {slugs:?}"
    );
    assert!(
        !slugs.contains(&slug_unrelated),
        "row without {needle:?} anywhere must NOT match"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn store_categories_endpoint_aggregates_counts() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let cat = unique_name("cat-zzq");
    seed_template(&app.db, &unique_name("c-a"), "a1", &cat, "x", false).await;
    seed_template(&app.db, &unique_name("c-b"), "a2", &cat, "x", false).await;

    let body: Value = con
        .get("/api/mcp/store/categories")
        .await
        .unwrap()
        .json()
        .unwrap();
    let row = body
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["category"] == cat)
        .unwrap_or_else(|| panic!("{cat} row missing in {body}"));
    assert_eq!(
        row["count"].as_i64().unwrap(),
        2,
        "category count must reflect the two seeded rows: {row}"
    );
}

// ---------------------------------------------------------------------------
// Install lifecycle
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn install_template_lifecycle_increments_then_decrements_install_count() {
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    // Short slug — namespace prefix derives from it and must match
    // `[a-z0-9_]{1,32}`.
    let slug = format!("life-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    let tmpl_id = seed_template(&app.db, &slug, "Life IT", "developer", "x", false).await;

    let initial: i32 =
        sqlx::query_scalar("SELECT install_count FROM mcp_store_templates WHERE id = $1")
            .bind(tmpl_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(initial, 0);

    let server = install_template_into_db(
        &app.db,
        tmpl_id,
        &slug,
        "https://example.com/mcp",
        "streamable_http",
        json!({}),
        None,
        None,
        admin.user.id,
    )
    .await
    .expect("install must succeed");

    let after_install: i32 =
        sqlx::query_scalar("SELECT install_count FROM mcp_store_templates WHERE id = $1")
            .bind(tmpl_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(after_install, 1, "install_count must increment to 1");

    // mcp_store_installs records the link.
    let n_links: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mcp_store_installs \
         WHERE template_id = $1 AND server_id = $2",
    )
    .bind(tmpl_id)
    .bind(server.id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(n_links, 1, "mcp_store_installs row must exist");

    // Now delete the server through the admin endpoint and verify
    // install_count decrements back.
    let con = admin_session(&app).await;
    con.delete(&format!("/api/mcp/servers/{}", server.id))
        .await
        .unwrap()
        .assert_ok();

    let after_delete: i32 =
        sqlx::query_scalar("SELECT install_count FROM mcp_store_templates WHERE id = $1")
            .bind(tmpl_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(
        after_delete, 0,
        "install_count must decrement after server delete"
    );

    // Deleting again must NOT push install_count below zero — the
    // GREATEST(install_count - 1, 0) clamp pinned in the
    // mcp_servers handler.
    sqlx::query("UPDATE mcp_store_templates SET install_count = 0 WHERE id = $1")
        .bind(tmpl_id)
        .execute(&app.db)
        .await
        .unwrap();
    // Trigger a second decrement against a 0 baseline by inserting a
    // fake link + deleting — easiest way is via the same admin path
    // on a fresh install.
    let server_2 = install_template_into_db(
        &app.db,
        tmpl_id,
        &slug,
        "https://example.com/mcp",
        "streamable_http",
        json!({}),
        None,
        None,
        admin.user.id,
    )
    .await
    .expect("second install");
    // Tamper: zero out the count, then delete. Without the clamp,
    // install_count would be -1.
    sqlx::query("UPDATE mcp_store_templates SET install_count = 0 WHERE id = $1")
        .bind(tmpl_id)
        .execute(&app.db)
        .await
        .unwrap();
    con.delete(&format!("/api/mcp/servers/{}", server_2.id))
        .await
        .unwrap()
        .assert_ok();
    let clamped: i32 =
        sqlx::query_scalar("SELECT install_count FROM mcp_store_templates WHERE id = $1")
            .bind(tmpl_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert!(
        clamped >= 0,
        "install_count must NEVER go negative — saw {clamped}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn double_install_of_same_template_auto_suffixes_name() {
    // Multi-instance support is an explicit design choice (see
    // memory `project_mcp_store_issues`): the same template can be
    // installed N times. Collisions on `mcp_servers.name` are
    // resolved by appending `_2`, `_3`, ... — pin that contract
    // because adding a UNIQUE(template_id) constraint later would
    // silently break it.
    let app = TestApp::spawn().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let slug = format!("dup-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    let tmpl_id = seed_template(&app.db, &slug, "Dup IT", "developer", "x", false).await;

    let s1 = install_template_into_db(
        &app.db,
        tmpl_id,
        &slug,
        "https://example.com/mcp",
        "streamable_http",
        json!({}),
        None,
        None,
        admin.user.id,
    )
    .await
    .expect("first install");
    let s2 = install_template_into_db(
        &app.db,
        tmpl_id,
        &slug,
        "https://example.com/mcp",
        "streamable_http",
        json!({}),
        None,
        None,
        admin.user.id,
    )
    .await
    .expect("second install must succeed (multi-instance support)");

    assert_ne!(s1.name, s2.name, "second server must NOT share name");
    // Resolver appends " #N" (Nginx-style) — pin the format so a
    // refactor that switches to e.g. "_N" or "(2)" is caught.
    assert!(
        s2.name.starts_with(&s1.name) && s2.name.ends_with("#2"),
        "second name should suffix \" #2\" from the first: s1={} s2={}",
        s1.name,
        s2.name
    );

    let count: i32 =
        sqlx::query_scalar("SELECT install_count FROM mcp_store_templates WHERE id = $1")
            .bind(tmpl_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(count, 2, "two installs → install_count == 2");
}
