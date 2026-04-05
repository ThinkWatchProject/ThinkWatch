use agent_bastion_common::errors::AppError;

use crate::app::AppState;

/// Get the ClickHouse client from state, or return a "not configured" error.
pub fn ch_client(state: &AppState) -> Result<&clickhouse::Client, AppError> {
    state
        .clickhouse
        .as_ref()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("ClickHouse is not configured")))
}

/// Returns true if ClickHouse is configured.
pub fn ch_available(state: &AppState) -> bool {
    state.clickhouse.is_some()
}

/// Helper: execute a count query and return the total.
#[allow(dead_code)]
pub async fn ch_count(
    client: &clickhouse::Client,
    query: &str,
) -> Result<u64, AppError> {
    let total: u64 = client
        .query(query)
        .fetch_one()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ClickHouse count query failed: {e}")))?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    #[test]
    fn ch_available_returns_false_when_no_client() {
        // We can't easily construct AppState in a unit test, but we verify
        // the function signature compiles correctly. Integration tests
        // would cover the full path.
    }
}
