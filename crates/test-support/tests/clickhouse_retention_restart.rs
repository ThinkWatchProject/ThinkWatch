//! A restart never takes a ClickHouse retention below the configured one.
//!
//! The server sets up the ClickHouse tables on every start, then applies
//! the configured retentions: `data.retention_days_*` as each log table's
//! TTL, `audit.body_retention_days` as the TTL of the columns holding
//! request and response bodies. The table setup used to give the body
//! columns a 30-day TTL each time, and ClickHouse acts on a TTL as soon
//! as it is set: with a longer body retention configured, every restart
//! cleared the bodies older than 30 days, although the configured TTL was
//! back a moment later.

use think_watch_server::handlers::admin::reconcile_clickhouse_ttls;
use think_watch_server::init::ensure_clickhouse_schema;
use think_watch_test_support::prelude::*;

const BODY_COLUMNS: [(&str, &str); 4] = [
    ("gateway_logs", "request_body"),
    ("gateway_logs", "response_body"),
    ("mcp_logs", "tool_arguments"),
    ("mcp_logs", "tool_result"),
];

/// The days in a `toIntervalDay(N)` on `line`.
fn interval_days(line: &str) -> Option<u64> {
    let rest = &line[line.find("toIntervalDay(")? + "toIntervalDay(".len()..];
    rest[..rest.find(')')?].parse().ok()
}

/// Every TTL that matters here, in days: each log table's and each body
/// column's, as `SHOW CREATE TABLE` gives them.
async fn ttls(ch: &clickhouse::Client) -> Vec<(String, Option<u64>)> {
    let mut out = Vec::new();
    for table in ["gateway_logs", "mcp_logs"] {
        let create: String = ch
            .query(&format!("SHOW CREATE TABLE {table}"))
            .fetch_one()
            .await
            .unwrap();
        let table_ttl = create
            .lines()
            .find(|l| l.starts_with("TTL "))
            .and_then(interval_days);
        out.push((table.to_string(), table_ttl));
        for (_, column) in BODY_COLUMNS.iter().filter(|(t, _)| *t == table) {
            let line = create
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("`{column}` ")))
                .unwrap_or_else(|| panic!("no {table}.{column} in {create}"));
            out.push((format!("{table}.{column}"), interval_days(line)));
        }
    }
    out
}

fn configured() -> Vec<(String, Option<u64>)> {
    vec![
        ("gateway_logs".into(), Some(365)),
        ("gateway_logs.request_body".into(), Some(90)),
        ("gateway_logs.response_body".into(), Some(90)),
        ("mcp_logs".into(), Some(365)),
        ("mcp_logs.tool_arguments".into(), Some(90)),
        ("mcp_logs.tool_result".into(), Some(90)),
    ]
}

/// Wait until ClickHouse has carried out every mutation of this database,
/// the TTL it materializes after an `ALTER … TTL` among them.
async fn mutations_done(ch: &clickhouse::Client) {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let pending: u64 = ch
                .query(
                    "SELECT count() FROM system.mutations \
                     WHERE database = currentDatabase() AND NOT is_done",
                )
                .fetch_one()
                .await
                .unwrap();
            if pending == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("ClickHouse finished its mutations");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn a_restart_keeps_retentions_longer_than_the_defaults() {
    let app = TestApp::spawn_with_clickhouse().await;
    let ch = app.state.clickhouse.clone().expect("ClickHouse");

    // A deployment that keeps bodies 90 days and logs a year, running.
    for (key, days) in [
        ("audit.body_retention_days", 90),
        ("data.retention_days_gateway", 365),
        ("data.retention_days_mcp", 365),
    ] {
        fixtures::set_setting(&app.db, key, json!(days))
            .await
            .unwrap();
    }
    app.state.dynamic_config.reload().await.unwrap();
    reconcile_clickhouse_ttls(&app.state).await;
    mutations_done(&ch).await;
    assert_eq!(ttls(&ch).await, configured(), "as the deployment runs");

    // Requests from 60 days ago, their bodies within the retention, and
    // from 200 days ago, within the logs'.
    ch.query(
        "INSERT INTO gateway_logs (id, model_id, request_body, response_body, created_at) VALUES \
         ('60d', 'm', 'request', 'response', now64(3) - INTERVAL 60 DAY)",
    )
    .execute()
    .await
    .unwrap();
    ch.query(
        "INSERT INTO gateway_logs (id, model_id, created_at) VALUES \
         ('200d', 'm', now64(3) - INTERVAL 200 DAY)",
    )
    .execute()
    .await
    .unwrap();
    ch.query(
        "INSERT INTO mcp_logs (id, tool_arguments, tool_result, created_at) VALUES \
         ('60d', 'arguments', 'result', now64(3) - INTERVAL 60 DAY)",
    )
    .execute()
    .await
    .unwrap();

    // The restart, step by step as the server takes it: the tables, then
    // the configured retentions. No step may shorten one.
    ensure_clickhouse_schema(&app.db, &app.state.clickhouse)
        .await
        .unwrap();
    assert_eq!(ttls(&ch).await, configured(), "after the table setup");
    reconcile_clickhouse_ttls(&app.state).await;
    assert_eq!(ttls(&ch).await, configured(), "after the retentions");

    mutations_done(&ch).await;
    let gateway: Vec<String> = ch
        .query(
            "SELECT concat(id, ' ', ifNull(request_body, '-'), ' ', ifNull(response_body, '-')) \
             FROM gateway_logs ORDER BY id",
        )
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(
        gateway,
        ["200d - -", "60d request response"],
        "the 60-day-old bodies and the 200-day-old row are still there"
    );
    let mcp: Vec<String> = ch
        .query(
            "SELECT concat(id, ' ', ifNull(tool_arguments, '-'), ' ', ifNull(tool_result, '-')) \
             FROM mcp_logs",
        )
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(mcp, ["60d arguments result"]);
}
