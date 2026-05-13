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

/// Pure cost math, extracted from `CostTracker` so it can be unit-tested
/// without a real `PgPool` / `WeightCache`. Lives at module level rather
/// than as an `impl` method because it has no state — keeping it free
/// also lets the tests call it without constructing a tracker.
fn compute_cost(
    input_tokens: u32,
    output_tokens: u32,
    input_per_token: Decimal,
    output_per_token: Decimal,
    w_input: f64,
    w_output: f64,
) -> Decimal {
    let w_input = Decimal::try_from(w_input).unwrap_or(Decimal::ONE);
    let w_output = Decimal::try_from(w_output).unwrap_or(Decimal::ONE);
    let input_cost = Decimal::from(input_tokens) * input_per_token * w_input;
    let output_cost = Decimal::from(output_tokens) * output_per_token * w_output;
    input_cost + output_cost
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
        compute_cost(
            input_tokens,
            output_tokens,
            baseline.input_per_token,
            baseline.output_per_token,
            w.input,
            w.output,
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn cost_is_sum_of_input_and_output_legs() {
        // 1000 in * 0.000002 * 1.0 + 500 out * 0.000008 * 1.0
        // = 0.002 + 0.004 = 0.006
        let cost = compute_cost(1000, 500, d("0.000002"), d("0.000008"), 1.0, 1.0);
        assert_eq!(cost, d("0.006"));
    }

    #[test]
    fn zero_tokens_yields_zero_cost() {
        let cost = compute_cost(0, 0, d("0.000002"), d("0.000008"), 1.0, 1.0);
        assert_eq!(cost, Decimal::ZERO);
    }

    #[test]
    fn weights_scale_each_leg_independently() {
        // Halving the input weight halves only the input leg.
        let baseline_cost = compute_cost(1000, 1000, d("0.000001"), d("0.000001"), 1.0, 1.0);
        let halved_input = compute_cost(1000, 1000, d("0.000001"), d("0.000001"), 0.5, 1.0);
        // baseline = 0.001 + 0.001 = 0.002; halved = 0.0005 + 0.001 = 0.0015
        assert_eq!(baseline_cost, d("0.002"));
        assert_eq!(halved_input, d("0.0015"));
    }

    #[test]
    fn output_more_expensive_than_input_reflects_in_cost() {
        // 1000 in @ 1e-6 + 1000 out @ 5e-6 = 0.001 + 0.005 = 0.006
        let cost = compute_cost(1000, 1000, d("0.000001"), d("0.000005"), 1.0, 1.0);
        assert_eq!(cost, d("0.006"));
    }

    #[test]
    fn nan_weight_falls_back_to_one_not_panic() {
        // f64::NAN doesn't convert to Decimal; the fallback keeps the
        // request from blowing up at the cost-log boundary.
        let cost = compute_cost(1000, 0, d("0.000002"), d("0.000008"), f64::NAN, 1.0);
        // Falls back to 1.0 → 1000 * 0.000002 * 1.0 = 0.002.
        assert_eq!(cost, d("0.002"));
    }

    #[test]
    fn negative_weight_is_representable_and_yields_negative_cost() {
        // -1.0 IS representable as Decimal, so this DOES become negative.
        // Lock that in so we notice if a future refactor changes the
        // contract (e.g. by clamping at the boundary instead).
        let cost = compute_cost(1000, 0, d("0.000002"), d("0.000008"), -1.0, 1.0);
        assert_eq!(cost, d("-0.002"));
    }

    #[test]
    fn fractional_weight_preserves_full_precision() {
        // 1.25 scales 100 tokens @ 0.0001 to 0.0125
        let cost = compute_cost(100, 0, d("0.0001"), d("0.0001"), 1.25, 1.0);
        assert_eq!(cost, d("0.0125"));
    }
}
