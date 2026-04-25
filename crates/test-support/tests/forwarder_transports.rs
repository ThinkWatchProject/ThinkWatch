//! Forwarder transport contracts: UDP syslog, TCP syslog, Kafka REST.
//!
//! Webhook delivery is well-covered (`webhook_outbox.rs`,
//! `webhook_signature.rs`, `background_tasks.rs`). The other three
//! transports the audit pipeline supports went untested, so a silent
//! regression in `send_udp_syslog` / `send_tcp_syslog` /
//! `send_kafka` would lose audit data without anyone noticing until
//! a SOC complains weeks later.
//!
//! Recipe per transport:
//!   - stand up a minimal listener (tokio UdpSocket, TcpListener,
//!     wiremock for Kafka REST proxy) on a random port
//!   - insert a `log_forwarders` row pointing at that port and reload
//!     the forwarder registry
//!   - emit one `AuditEntry` and assert what landed on the wire
//!
//! What's pinned: the RFC 5424 syslog framing (priority byte + audit@0
//! structured data block + action verb), the Kafka REST envelope
//! (`{records: [{value: <entry>}]}`), the Kafka content-type, and the
//! TCP newline terminator (UDP doesn't need one — datagram boundaries
//! are the framing).

use think_watch_common::audit::AuditEntry;
use think_watch_test_support::prelude::*;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, UdpSocket};
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn install_forwarder(app: &TestApp, forwarder_type: &str, config: serde_json::Value) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO log_forwarders (name, forwarder_type, config, log_types, enabled)
           VALUES ($1, $2, $3, ARRAY['audit']::text[], true) RETURNING id"#,
    )
    .bind(unique_name(forwarder_type))
    .bind(forwarder_type)
    .bind(config)
    .fetch_one(&app.db)
    .await
    .unwrap();
    app.state.audit.reload_forwarders().await;
    id
}

// ---------------------------------------------------------------------------
// UDP syslog
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn udp_syslog_forwarder_emits_rfc5424_framed_message() {
    let app = TestApp::spawn().await;

    // Bind on 0 → kernel assigns a free port; ask back for the actual addr.
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();

    install_forwarder(
        &app,
        "udp_syslog",
        json!({"address": bound.to_string(), "facility": 17_u32}),
    )
    .await;

    let action = "test.udp_syslog";
    app.state.audit.log(
        AuditEntry::new(action)
            .resource("forwarder_test")
            .ip_address("10.0.0.1"),
    );

    // Single recv with a generous timeout — the audit pipeline buffers
    // briefly before dispatching.
    let mut buf = vec![0u8; 4096];
    let (len, _src) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        listener.recv_from(&mut buf),
    )
    .await
    .expect("UDP listener never received a syslog datagram")
    .unwrap();
    let payload = std::str::from_utf8(&buf[..len]).expect("syslog message is UTF-8");

    // RFC 5424 framing:
    //   <PRI>1 <TIMESTAMP> <HOSTNAME> <APPNAME> <PROCID> <MSGID> <SD> <MSG>
    // priority = facility * 8 + severity = 17*8 + 6 = 142.
    assert!(
        payload.starts_with("<142>1 "),
        "wrong PRI for facility=17: {payload}"
    );
    assert!(
        payload.contains("think-watch audit"),
        "appname/msgid header missing: {payload}"
    );
    // Structured data block: [audit@0 ... action="<action>" ...]
    assert!(
        payload.contains(&format!("action=\"{action}\"")),
        "structured-data missing action: {payload}"
    );
    assert!(
        payload.contains("ip=\"10.0.0.1\""),
        "structured-data missing ip: {payload}"
    );
    // UDP must NOT have a trailing newline — datagrams self-frame.
    assert!(
        !payload.ends_with('\n'),
        "UDP syslog should not append \\n (TCP only): {payload:?}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn udp_syslog_default_facility_is_local0() {
    // facility omitted from config → default 16 (local0). PRI = 16*8+6 = 134.
    let app = TestApp::spawn().await;
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();

    install_forwarder(&app, "udp_syslog", json!({"address": bound.to_string()})).await;
    app.state
        .audit
        .log(AuditEntry::new("test.default_facility").resource("forwarder_test"));

    let mut buf = vec![0u8; 4096];
    let (len, _) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        listener.recv_from(&mut buf),
    )
    .await
    .unwrap()
    .unwrap();
    let payload = std::str::from_utf8(&buf[..len]).unwrap();
    assert!(
        payload.starts_with("<134>1 "),
        "default facility=local0 expects PRI=134, got: {payload}"
    );
}

// ---------------------------------------------------------------------------
// TCP syslog
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn tcp_syslog_forwarder_writes_newline_terminated_message() {
    let app = TestApp::spawn().await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();

    install_forwarder(
        &app,
        "tcp_syslog",
        json!({"address": bound.to_string(), "facility": 16_u32}),
    )
    .await;

    // Spawn the listener in parallel — accept one connection, slurp
    // bytes until the audit logger drops it (or until we have a
    // newline-terminated frame).
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        // Read up to 4 KiB; the audit logger keeps the connection
        // open after a successful write, so a fixed-size read with a
        // short timeout is the cleanest way to capture exactly one
        // delivery.
        let mut chunk = [0u8; 1024];
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                socket.read(&mut chunk),
            )
            .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.contains(&b'\n') {
                        break;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break, // timeout — assume nothing more coming
            }
        }
        buf
    });

    let action = "test.tcp_syslog";
    app.state.audit.log(
        AuditEntry::new(action)
            .resource("forwarder_test")
            .ip_address("10.0.0.2"),
    );

    let buf = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("TCP listener task hung")
        .expect("TCP listener task panicked");
    let payload = std::str::from_utf8(&buf).expect("TCP syslog frame is UTF-8");
    assert!(payload.starts_with("<134>1 "), "wrong PRI: {payload}");
    assert!(
        payload.contains(&format!("action=\"{action}\"")),
        "structured-data missing action: {payload}"
    );
    // TCP REQUIRES a newline terminator — RFC 6587 octet-counting is
    // not implemented here, so the receiver relies on \n framing.
    assert!(
        payload.ends_with('\n'),
        "TCP syslog must terminate with \\n for receivers to frame the message: {payload:?}"
    );
}

// ---------------------------------------------------------------------------
// Kafka REST proxy
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn kafka_forwarder_posts_records_envelope_to_topic_url() {
    let app = TestApp::spawn().await;
    let server = MockServer::start().await;

    let topic = "audit-test-topic";
    Mock::given(method("POST"))
        .and(path(format!("/topics/{topic}")))
        .and(header("content-type", "application/vnd.kafka.json.v2+json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"offsets": [{"partition": 0, "offset": 0}]})),
        )
        .mount(&server)
        .await;

    install_forwarder(
        &app,
        "kafka",
        json!({
            "broker_url": server.uri(),
            "topic": topic,
        }),
    )
    .await;

    let action = "test.kafka_envelope";
    app.state.audit.log(
        AuditEntry::new(action)
            .resource("forwarder_test")
            .user_email("ops@example.com"),
    );

    // Poll the wiremock journal for the delivery.
    let mut received: Vec<wiremock::Request> = Vec::new();
    for _ in 0..50 {
        received = server.received_requests().await.unwrap_or_default();
        if !received.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        !received.is_empty(),
        "Kafka REST proxy never received the audit entry"
    );

    // Pin the Confluent-compatible envelope shape:
    //   { "records": [ { "value": <AuditEntry> } ] }
    let body: serde_json::Value =
        serde_json::from_slice(&received[0].body).expect("Kafka body is JSON");
    let records = body["records"]
        .as_array()
        .expect("records array required by Kafka REST proxy spec");
    assert_eq!(records.len(), 1, "exactly one record per audit entry");
    assert_eq!(records[0]["value"]["action"], action);
    assert_eq!(records[0]["value"]["user_email"], "ops@example.com");
    assert_eq!(records[0]["value"]["resource"], "forwarder_test");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn kafka_forwarder_failure_lands_in_outbox_for_retry() {
    // Same DLQ contract as webhook: a 5xx response from the REST proxy
    // must enqueue a `webhook_outbox` row so the drain worker can
    // retry. Without this, a transient broker outage silently drops
    // audit data.
    let app = TestApp::spawn().await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let forwarder_id = install_forwarder(
        &app,
        "kafka",
        json!({"broker_url": server.uri(), "topic": "any"}),
    )
    .await;

    app.state
        .audit
        .log(AuditEntry::new("test.kafka_dlq").resource("forwarder_test"));

    for _ in 0..50 {
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM webhook_outbox WHERE forwarder_id = $1")
                .bind(forwarder_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        if n > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("Kafka 5xx delivery never landed in webhook_outbox — DLQ wiring broken");
}
