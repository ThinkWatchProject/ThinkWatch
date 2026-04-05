use crate::audit::AuditConfig;

/// Create a `clickhouse::Client` from our config. Returns `None` if ClickHouse
/// is not configured (no URL).
pub fn create_client(config: &AuditConfig) -> Option<clickhouse::Client> {
    let url = config.clickhouse_url.as_deref()?;

    let mut client = clickhouse::Client::default()
        .with_url(url)
        .with_database(&config.clickhouse_db)
        .with_product_info("agent-bastion", env!("CARGO_PKG_VERSION"));

    if let Some(ref user) = config.clickhouse_user {
        client = client.with_user(user);
    }
    if let Some(ref password) = config.clickhouse_password {
        client = client.with_password(password);
    }

    Some(client)
}
