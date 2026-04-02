use chrono::Utc;
use serde::Serialize;
use std::net::UdpSocket;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Audit log entry sent to Quickwit and optionally syslog.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub id: String,
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub action: String,
    pub resource: Option<String>,
    pub detail: Option<serde_json::Value>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: String,
}

impl AuditEntry {
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            user_id: None,
            api_key_id: None,
            action: action.into(),
            resource: None,
            detail: None,
            ip_address: None,
            user_agent: None,
            created_at: Utc::now().to_rfc3339(),
        }
    }

    pub fn user_id(mut self, id: Uuid) -> Self {
        self.user_id = Some(id.to_string());
        self
    }

    pub fn api_key_id(mut self, id: Uuid) -> Self {
        self.api_key_id = Some(id.to_string());
        self
    }

    pub fn resource(mut self, r: impl Into<String>) -> Self {
        self.resource = Some(r.into());
        self
    }

    pub fn detail(mut self, d: serde_json::Value) -> Self {
        self.detail = Some(d);
        self
    }

    pub fn ip_address(mut self, ip: impl Into<String>) -> Self {
        self.ip_address = Some(ip.into());
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_entry_builder_pattern() {
        let user_id = Uuid::new_v4();
        let api_key_id = Uuid::new_v4();
        let detail = serde_json::json!({"model": "claude-3"});

        let entry = AuditEntry::new("api.request")
            .user_id(user_id)
            .api_key_id(api_key_id)
            .resource("/v1/chat/completions")
            .detail(detail.clone())
            .ip_address("10.0.0.1")
            .user_agent("curl/8.0");

        assert_eq!(entry.action, "api.request");
        assert_eq!(entry.user_id.as_deref(), Some(user_id.to_string()).as_deref());
        assert_eq!(entry.api_key_id.as_deref(), Some(api_key_id.to_string()).as_deref());
        assert_eq!(entry.resource.as_deref(), Some("/v1/chat/completions"));
        assert_eq!(entry.detail, Some(detail));
        assert_eq!(entry.ip_address.as_deref(), Some("10.0.0.1"));
        assert_eq!(entry.user_agent.as_deref(), Some("curl/8.0"));
        // id and created_at should be populated automatically
        assert!(!entry.id.is_empty());
        assert!(!entry.created_at.is_empty());
    }
}

#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// Quickwit ingest endpoint, e.g. "http://localhost:7280"
    pub quickwit_url: Option<String>,
    /// Quickwit index ID
    pub quickwit_index: String,
    /// Syslog server address, e.g. "127.0.0.1:514"
    pub syslog_addr: Option<String>,
    /// Syslog facility (default: local0)
    pub syslog_facility: u8,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            quickwit_url: None,
            quickwit_index: "audit_logs".into(),
            syslog_addr: None,
            syslog_facility: 16, // local0
        }
    }
}

/// Async audit log dispatcher. Receives entries via a bounded channel, writes to Quickwit + syslog.
#[derive(Clone)]
pub struct AuditLogger {
    tx: mpsc::Sender<AuditEntry>,
}

impl AuditLogger {
    pub fn new(config: AuditConfig) -> Self {
        let (tx, rx) = mpsc::channel(10_000);

        tokio::spawn(audit_worker(config, rx));

        Self { tx }
    }

    pub fn log(&self, entry: AuditEntry) {
        if let Err(e) = self.tx.try_send(entry) {
            tracing::warn!("Audit log channel send failed (buffer full or closed): {e}");
        }
    }
}

async fn audit_worker(config: AuditConfig, mut rx: mpsc::Receiver<AuditEntry>) {
    let http_client = reqwest::Client::new();
    let syslog_socket = config.syslog_addr.as_ref().and_then(|_| {
        match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("Failed to bind syslog UDP socket: {e}");
                None
            }
        }
    });

    // Buffer for batching Quickwit ingests
    let mut batch: Vec<AuditEntry> = Vec::with_capacity(64);
    let mut flush_interval = tokio::time::interval(std::time::Duration::from_secs(2));

    loop {
        tokio::select! {
            Some(entry) = rx.recv() => {
                // Send to syslog immediately (low-latency requirement)
                if let (Some(addr), Some(sock)) = (&config.syslog_addr, &syslog_socket) {
                    send_syslog(sock, addr, &entry, config.syslog_facility);
                }
                batch.push(entry);
                // Flush if batch is large enough
                if batch.len() >= 50 {
                    flush_to_quickwit(&http_client, &config, &mut batch).await;
                }
            }
            _ = flush_interval.tick() => {
                if !batch.is_empty() {
                    flush_to_quickwit(&http_client, &config, &mut batch).await;
                }
            }
            else => break,
        }
    }
}

async fn flush_to_quickwit(
    client: &reqwest::Client,
    config: &AuditConfig,
    batch: &mut Vec<AuditEntry>,
) {
    let Some(ref url) = config.quickwit_url else {
        batch.clear();
        return;
    };

    // NDJSON format
    let ndjson: String = batch
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect::<Vec<_>>()
        .join("\n");

    let ingest_url = format!("{}/api/v1/{}/ingest", url, config.quickwit_index);

    match client
        .post(&ingest_url)
        .header("Content-Type", "application/x-ndjson")
        .body(ndjson)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!("Flushed {} audit entries to Quickwit", batch.len());
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("Quickwit ingest returned {status}: {body}");
        }
        Err(e) => {
            tracing::warn!("Quickwit ingest failed: {e}");
        }
    }

    batch.clear();
}

fn send_syslog(socket: &UdpSocket, addr: &str, entry: &AuditEntry, facility: u8) {
    // RFC 5424 syslog message
    let severity = 6u8; // informational
    let priority = facility * 8 + severity;
    let hostname = "agent-bastion";
    let app_name = "audit";
    let msg_id = &entry.action;

    let structured_data = format!(
        "[audit@0 user_id=\"{}\" action=\"{}\" resource=\"{}\" ip=\"{}\"]",
        entry.user_id.as_deref().unwrap_or("-"),
        entry.action,
        entry.resource.as_deref().unwrap_or("-"),
        entry.ip_address.as_deref().unwrap_or("-"),
    );

    let message = format!(
        "<{priority}>1 {timestamp} {hostname} {app_name} - {msg_id} {structured_data} {action} on {resource}",
        priority = priority,
        timestamp = &entry.created_at,
        hostname = hostname,
        app_name = app_name,
        msg_id = msg_id,
        structured_data = structured_data,
        action = entry.action,
        resource = entry.resource.as_deref().unwrap_or("-"),
    );

    if let Err(e) = socket.send_to(message.as_bytes(), addr) {
        tracing::warn!("Syslog send failed: {e}");
    }
}

/// Initialize the Quickwit index (create if not exists). Call once at startup.
pub async fn ensure_quickwit_index(quickwit_url: &str, index_id: &str) {
    let client = reqwest::Client::new();

    // Check if index exists
    let check_url = format!("{}/api/v1/indexes/{}", quickwit_url, index_id);
    match client.get(&check_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!("Quickwit index '{index_id}' already exists");
            return;
        }
        _ => {}
    }

    // Create index from embedded config
    let index_config = include_str!("../../../deploy/quickwit/audit_logs_index.yaml");
    let create_url = format!("{}/api/v1/indexes", quickwit_url);

    match client
        .post(&create_url)
        .header("Content-Type", "application/yaml")
        .body(index_config.to_string())
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!("Created Quickwit index '{index_id}'");
        }
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("Failed to create Quickwit index: {body}");
        }
        Err(e) => {
            tracing::warn!("Failed to connect to Quickwit: {e}");
        }
    }
}
