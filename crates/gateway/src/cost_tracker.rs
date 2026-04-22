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

use rust_decimal::Decimal;
use sqlx::PgPool;
use tokio::sync::RwLock;

use think_watch_common::limits::weight::WeightCache;

/// Platform-wide per-token prices in USD as `Decimal` so the cost
/// math stays precision-preserving end-to-end — stored as-is in the
/// `platform_pricing` row and compared against tokens + weights
/// without a lossy `f64` round trip.
#[derive(Debug, Clone)]
struct Baseline {
    input_per_token: Decimal,
    output_per_token: Decimal,
    expires: Instant,
}

impl Baseline {
    fn fresh(input: Decimal, output: Decimal) -> Self {
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
fn fallback_input() -> Decimal {
    Decimal::new(20, 7) // 0.0000020
}
fn fallback_output() -> Decimal {
    Decimal::new(80, 7) // 0.0000080
}

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

    /// Compute USD cost for a request as `Decimal`. Async because
    /// both the baseline and the weight can fall through to the DB on
    /// a cache miss.
    ///
    /// The per-model weight stored in `WeightCache` is `f64` (used
    /// elsewhere to scale i64 token counts for rate-limiting); we
    /// lift it to `Decimal` for the money multiply. Precision loss
    /// at the weight boundary is ~15 sig-figs which is ample for the
    /// 0.1–10× range weights actually live in.
    pub async fn calculate_cost(
        &self,
        model: &str,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Decimal {
        let baseline = self.baseline_value().await;
        let w = self.weight_cache.get(&self.pool, model).await;

        let w_input = Decimal::try_from(w.input).unwrap_or(Decimal::ONE);
        let w_output = Decimal::try_from(w.output).unwrap_or(Decimal::ONE);

        let input_cost = Decimal::from(input_tokens) * baseline.input_per_token * w_input;
        let output_cost = Decimal::from(output_tokens) * baseline.output_per_token * w_output;
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
            if let Some(ref b) = *r
                && b.expires > Instant::now()
            {
                return b.clone();
            }
        }

        // Slow path: query the singleton.
        let loaded = match sqlx::query_as::<_, (Decimal, Decimal)>(
            "SELECT input_price_per_token, output_price_per_token \
             FROM platform_pricing WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
        {
            Ok(Some((i, o))) => Baseline::fresh(i, o),
            _ => {
                tracing::warn!("platform_pricing lookup failed; using built-in defaults");
                Baseline::fresh(fallback_input(), fallback_output())
            }
        };

        *self.baseline.write().await = Some(loaded.clone());
        loaded
    }
}
