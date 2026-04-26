//! Per-request routing decision log — Redis-backed, observability only.
//!
//! Each completion writes one entry into a per-model capped Redis
//! LIST: `route_decisions:{model_id}` (LPUSH then LTRIM 0 199 to keep
//! ~200 per model). No persistence to PG/CH — explicitly out of scope.
//! The admin UI tails these for live debugging of strategy / affinity
//! / failover behaviour.
//!
//! Per-model bucketing avoids one hot model dominating a global list
//! and lets the UI filter cheaply by model. `recent()` reads any
//! number of model buckets.

use fred::clients::Client;
use fred::interfaces::{KeysInterface, ListInterface};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const PER_MODEL_CAP: i64 = 200;

/// One candidate's snapshot at decision time, included in the log so
/// the UI can show "candidate considered = picked? + weight + why
/// excluded".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub route_id: Uuid,
    pub provider_name: String,
    pub upstream_model: Option<String>,
    pub weight: f64,
    pub health_state: String,
    pub ewma_latency_ms: Option<f64>,
    /// `Some(reason)` ⇒ candidate excluded from selection (circuit
    /// breaker, RPM/TPM cap, etc). Excluded candidates are still
    /// recorded so an operator sees *why* a route was skipped.
    pub excluded_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// Wall-clock millis when the decision finished.
    pub ts_ms: i64,
    pub model_id: String,
    pub strategy: String,
    pub affinity_mode: String,
    pub affinity_hit: bool,
    /// Routes considered at the highest priority group reached.
    /// Lower-priority fallbacks aren't enumerated.
    pub candidates: Vec<CandidateRecord>,
    pub picked_route_id: Option<Uuid>,
    /// 1 = first try succeeded; >1 = failover.
    pub attempts: u32,
    pub total_latency_ms: u32,
    pub success: bool,
    pub error_message: Option<String>,
    /// Anonymous user identifier (api-key prefix or user uuid) — lets
    /// you spot "is this user always landing on the same route via
    /// affinity?" without dumping PII.
    pub user_hint: Option<String>,
}

/// Push one decision onto the model's bucket. Errors are logged and
/// swallowed — observability, not request critical path.
pub async fn push(redis: &Client, record: &DecisionRecord) {
    let key = format!("route_decisions:{}", record.model_id);
    let payload = match serde_json::to_string(record) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("decision log encode failed: {e}");
            return;
        }
    };

    // LPUSH + LTRIM separately — atomic Lua isn't worth it. Worst
    // case under racing writes is the cap is briefly exceeded by
    // 1-2 entries; the next write trims it back.
    if let Err(e) = redis.lpush::<i64, _, _>(&key, payload).await {
        tracing::warn!(?e, "decision log lpush failed");
        return;
    }
    if let Err(e) = redis.ltrim::<(), _>(&key, 0, PER_MODEL_CAP - 1).await {
        tracing::warn!(?e, "decision log ltrim failed");
    }
    // 24h TTL — auto-cleanup for cold models.
    let _: Result<bool, _> = redis.expire(&key, 86400, None).await;
}

/// Read recent decisions for a model. Newest-first via `LRANGE 0 N-1`
/// (we LPUSHed, so newest is at index 0).
pub async fn recent(redis: &Client, model_id: &str, limit: i64) -> Vec<DecisionRecord> {
    let key = format!("route_decisions:{model_id}");
    let raw: Vec<String> = redis
        .lrange(&key, 0, (limit - 1).max(0))
        .await
        .unwrap_or_default();
    raw.into_iter()
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect()
}

/// Model_ids with any buffered decisions — for the UI's filter
/// dropdown. SCAN to avoid blocking Redis on a wide KEYS call.
pub async fn list_models(redis: &Client) -> Vec<String> {
    use fred::types::scan::Scanner;
    use futures::StreamExt;

    let mut scanner = redis.scan("route_decisions:*", Some(100), None);
    let mut out: Vec<String> = Vec::new();
    while let Some(page) = scanner.next().await {
        match page {
            Ok(p) => {
                if let Some(keys) = p.results() {
                    for k in keys {
                        if let Some(s) = k.as_str()
                            && let Some(rest) = s.strip_prefix("route_decisions:")
                        {
                            out.push(rest.to_string());
                        }
                    }
                }
                p.next();
            }
            Err(e) => {
                tracing::warn!("decision-log SCAN failed: {e}");
                break;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
