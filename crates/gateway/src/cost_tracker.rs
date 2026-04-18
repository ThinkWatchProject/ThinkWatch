// ============================================================================
// Cost tracker
//
// Translates `(model_id, prompt_tokens, completion_tokens)` into a USD
// cost for the gateway_logs audit trail.
//
//   cost = platform_baseline × model_weight × tokens
//
// where:
//   * `platform_baseline` = `(input_price_per_token, output_price_per_token)`
//     from the `platform_pricing` singleton table.
//   * `model_weight` = per-model `(input_weight, output_weight)` from
//     the `models` row (reused from the limits `WeightCache` so we
//     don't double-query or double-cache).
//
// The tracker owns a read-through cache of the platform baseline with a
// 60-second TTL. Admins change it rarely, and a 60s lag on a brand-new
// baseline in cost logs is acceptable. The PATCH handler can also call
// `invalidate_baseline()` for instant propagation.
//
// Unknown models fall back to the default (1.0, 1.0) weight from the
// cache, so logging never errors out — the cost will just be
// `baseline × raw tokens`, which is the sensible default.
// ============================================================================

use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::sync::RwLock;

use think_watch_common::limits::weight::WeightCache;

/// Platform-wide per-token prices in USD. Fresh copy loaded from
/// `platform_pricing` on first call / after TTL.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    input_per_token: f64,
    output_per_token: f64,
    expires: Instant,
}

impl Baseline {
    fn fresh(input: f64, output: f64) -> Self {
        Self {
            input_per_token: input,
            output_per_token: output,
            expires: Instant::now() + BASELINE_TTL,
        }
    }
}

const BASELINE_TTL: Duration = Duration::from_secs(60);

/// Fallback used when the platform_pricing query fails at first call.
/// Matches the DB defaults so cost logs are still useful pre-config.
const FALLBACK_INPUT_PER_TOKEN: f64 = 0.0000020;
const FALLBACK_OUTPUT_PER_TOKEN: f64 = 0.0000080;

pub struct CostTracker {
    pool: PgPool,
    weight_cache: WeightCache,
    baseline: Arc<RwLock<Option<Baseline>>>,
}

impl CostTracker {
    pub fn new(pool: PgPool, weight_cache: WeightCache) -> Self {
        Self {
            pool,
            weight_cache,
            baseline: Arc::new(RwLock::new(None)),
        }
    }

    /// Compute USD cost for a request. Async because both the baseline
    /// and the weight can fall through to the DB on a cache miss.
    pub async fn calculate_cost(&self, model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
        let baseline = self.baseline_value().await;
        let w = self.weight_cache.get(&self.pool, model).await;

        let input_cost = f64::from(input_tokens) * baseline.input_per_token * w.input;
        let output_cost = f64::from(output_tokens) * baseline.output_per_token * w.output;
        input_cost + output_cost
    }

    /// Drop the cached baseline so the next call reloads from DB.
    /// Called from the `PATCH /admin/platform-pricing` handler.
    pub async fn invalidate_baseline(&self) {
        *self.baseline.write().await = None;
    }

    async fn baseline_value(&self) -> Baseline {
        // Fast path: fresh cached value.
        {
            let r = self.baseline.read().await;
            if let Some(b) = *r
                && b.expires > Instant::now()
            {
                return b;
            }
        }

        // Slow path: query the singleton.
        let loaded = match sqlx::query_as::<_, (rust_decimal::Decimal, rust_decimal::Decimal)>(
            "SELECT input_price_per_token, output_price_per_token \
             FROM platform_pricing WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
        {
            Ok(Some((i, o))) => {
                use rust_decimal::prelude::ToPrimitive;
                Baseline::fresh(
                    i.to_f64().unwrap_or(FALLBACK_INPUT_PER_TOKEN),
                    o.to_f64().unwrap_or(FALLBACK_OUTPUT_PER_TOKEN),
                )
            }
            _ => {
                tracing::warn!("platform_pricing lookup failed; using built-in defaults");
                Baseline::fresh(FALLBACK_INPUT_PER_TOKEN, FALLBACK_OUTPUT_PER_TOKEN)
            }
        };

        *self.baseline.write().await = Some(loaded);
        loaded
    }
}
