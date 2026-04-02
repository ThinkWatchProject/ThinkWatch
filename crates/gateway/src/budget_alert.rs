use fred::clients::Client;
use serde::Serialize;
use std::sync::Arc;

/// Configuration for budget alert webhooks.
#[derive(Debug, Clone)]
pub struct BudgetAlertConfig {
    /// Webhook URL to POST alerts to.
    pub webhook_url: Option<String>,
    /// Thresholds as percentages (e.g. `[0.50, 0.80, 0.95]`).
    pub thresholds: Vec<f64>,
}

impl Default for BudgetAlertConfig {
    fn default() -> Self {
        Self {
            webhook_url: None,
            thresholds: vec![0.50, 0.80, 0.95],
        }
    }
}

/// Payload sent to the webhook endpoint when a budget threshold is crossed.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetAlertPayload {
    pub alert_type: String,
    pub key: String,
    pub threshold_pct: f64,
    pub current_spend: f64,
    pub budget_limit: f64,
    pub period: String,
    pub timestamp: String,
}

/// Monitors spending against budgets and fires webhooks at configurable thresholds.
#[derive(Clone)]
pub struct BudgetAlertManager {
    redis: Client,
    http_client: reqwest::Client,
    config: Arc<BudgetAlertConfig>,
}

impl BudgetAlertManager {
    pub fn new(redis: Client, config: BudgetAlertConfig) -> Self {
        Self {
            redis,
            http_client: reqwest::Client::new(),
            config: Arc::new(config),
        }
    }

    /// Check if any budget threshold has been crossed and fire a webhook if so.
    ///
    /// Uses Redis SET NX to ensure each threshold alert is sent at most once per
    /// period (year-month). Webhook delivery is async fire-and-forget.
    pub async fn check_and_alert(&self, key: &str, current_spend: f64, budget_limit: f64) {
        if budget_limit <= 0.0 {
            return;
        }

        let webhook_url = match &self.config.webhook_url {
            Some(url) if !url.is_empty() => url.clone(),
            _ => return,
        };

        let ratio = current_spend / budget_limit;
        let period = chrono::Utc::now().format("%Y-%m").to_string();

        for &threshold in &self.config.thresholds {
            if ratio >= threshold {
                let redis_key =
                    format!("budget_alert:{key}:{period}:{:.0}", threshold * 100.0);

                // SET NX — only succeeds if the key does not already exist
                let set_result: Result<bool, _> = fred::interfaces::KeysInterface::set(
                    &self.redis,
                    &redis_key,
                    "1",
                    Some(fred::types::Expiration::EX(30 * 24 * 3600)), // 30 day TTL
                    Some(fred::types::SetOptions::NX),
                    false,
                )
                .await;

                let was_set = match set_result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("Failed to check budget alert dedup key: {e}");
                        continue;
                    }
                };

                if !was_set {
                    // Already alerted for this threshold/period
                    continue;
                }

                let alert_type = if ratio >= 1.0 {
                    "budget_exceeded"
                } else {
                    "budget_warning"
                };

                let payload = BudgetAlertPayload {
                    alert_type: alert_type.to_string(),
                    key: key.to_string(),
                    threshold_pct: threshold,
                    current_spend,
                    budget_limit,
                    period: period.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                };

                let http_client = self.http_client.clone();
                let url = webhook_url.clone();

                // Fire-and-forget: don't block the request
                tokio::spawn(async move {
                    match http_client.post(&url).json(&payload).send().await {
                        Ok(resp) => {
                            tracing::info!(
                                key = %payload.key,
                                threshold = %payload.threshold_pct,
                                status = %resp.status(),
                                "Budget alert webhook delivered"
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                key = %payload.key,
                                threshold = %payload.threshold_pct,
                                "Budget alert webhook failed: {e}"
                            );
                        }
                    }
                });
            }
        }
    }
}
