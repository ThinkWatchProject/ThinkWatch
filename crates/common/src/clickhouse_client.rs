use std::time::Duration;

use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::audit::AuditConfig;

/// How long opening a connection to ClickHouse may take. The `clickhouse`
/// crate's own connector has no limit, so a ClickHouse whose network
/// drops packets instead of refusing them held each attempt for the
/// operating system's TCP timeout (about two minutes on Linux) — at start,
/// while holding the ClickHouse setup lock that other instances queue
/// behind.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The crate's defaults, which a client built by hand has to restate.
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);
/// Below ClickHouse's own keep-alive timeout (3 s before 23.11, 10 s
/// since), so the client never reuses a socket the server has closed.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Create a `clickhouse::Client` from our config. Returns `None` if ClickHouse
/// is not configured (no URL).
///
/// Plain HTTP only: the crate is built without TLS, and the connector
/// refuses an `https://` URL.
pub fn create_client(config: &AuditConfig) -> Option<clickhouse::Client> {
    let url = config.clickhouse_url.as_deref()?;

    let mut connector = HttpConnector::new();
    connector.set_keepalive(Some(TCP_KEEPALIVE));
    connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
    let http = HyperClient::builder(TokioExecutor::new())
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build(connector);

    let mut client = clickhouse::Client::with_http_client(http)
        .with_url(url)
        .with_database(&config.clickhouse_db)
        .with_product_info("think-watch", env!("CARGO_PKG_VERSION"));

    if let Some(ref user) = config.clickhouse_user {
        client = client.with_user(user);
    }
    if let Some(ref password) = config.clickhouse_password {
        client = client.with_password(password);
    }

    Some(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(url: &str) -> AuditConfig {
        AuditConfig {
            clickhouse_url: Some(url.to_owned()),
            ..Default::default()
        }
    }

    /// A ClickHouse that never answers the handshake fails the query
    /// after the connect timeout, not the operating system's.
    #[tokio::test]
    async fn an_unanswered_connect_gives_up_after_the_timeout() {
        // A listener whose accept queue is full drops further SYNs
        // unanswered, as a firewall that drops packets does. Fill it.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(1).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut held = Vec::new();
        while let Ok(stream) = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        {
            held.push(stream.unwrap());
            assert!(held.len() < 64, "the accept queue never filled");
        }

        let client = create_client(&config(&format!("http://{addr}"))).unwrap();
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            CONNECT_TIMEOUT * 4,
            client.query("SELECT 1").fetch_one::<u8>(),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(
            matches!(result, Ok(Err(_))),
            "still waiting after {elapsed:?}, or answered"
        );
        // The operating system gives up far later: about two minutes on
        // Linux, some 8 s on macOS for a local address.
        assert!(
            elapsed < CONNECT_TIMEOUT + Duration::from_secs(1),
            "{elapsed:?}"
        );
        drop(held);
    }
}
