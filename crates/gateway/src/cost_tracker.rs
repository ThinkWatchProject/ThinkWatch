// ============================================================================
// Cost tracker
//
// Translates a request's token counts into a USD cost for the
// gateway_logs audit trail.
//
//   cost = input_price  × (input       × input_weight
//                        + cache_read  × cache_read_weight
//                        + cache_write × cache_write_weight)
//        + output_price × output × output_weight
//
// where:
//   * `input_price` / `output_price` = `(input_price_per_token,
//     output_price_per_token)` from the `platform_pricing` singleton.
//   * the weights come from the `models` row (reused from the limits
//     `WeightCache` so we don't double-query or double-cache). Unset
//     cache weights follow the input weight; see `Weights::resolve`.
//     A 1-hour cache write uses `cache_write_1h_weight`.
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

use think_watch_common::limits::weight::{TokenCounts, WeightCache, Weights};

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
    t: &TokenCounts,
    input_per_token: Decimal,
    output_per_token: Decimal,
    w: Weights,
) -> Decimal {
    let d = |x: f64| Decimal::try_from(x).unwrap_or(Decimal::ONE);
    let n = |x: i64| Decimal::from(x.max(0));
    let write = if t.cache_write_1h {
        w.cache_write_1h
    } else {
        w.cache_write
    };
    let input_units =
        n(t.input) * d(w.input) + n(t.cache_read) * d(w.cache_read) + n(t.cache_write) * d(write);
    input_units * input_per_token + n(t.output) * d(w.output) * output_per_token
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
    pub async fn calculate_cost(&self, model: &str, tokens: &TokenCounts) -> Decimal {
        let baseline = self.baseline_value().await;
        let w = self.weight_cache.get(&self.pool, model).await;
        compute_cost(
            tokens,
            baseline.input_per_token,
            baseline.output_per_token,
            w,
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

    fn plain(input: i64, output: i64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            ..Default::default()
        }
    }

    fn w(input: f64, output: f64) -> Weights {
        Weights::resolve(input, output, None, None, None)
    }

    #[test]
    fn cost_is_sum_of_input_and_output_legs() {
        // 1000 in * 0.000002 * 1.0 + 500 out * 0.000008 * 1.0
        // = 0.002 + 0.004 = 0.006
        let cost = compute_cost(&plain(1000, 500), d("0.000002"), d("0.000008"), w(1.0, 1.0));
        assert_eq!(cost, d("0.006"));
    }

    #[test]
    fn zero_tokens_yields_zero_cost() {
        let cost = compute_cost(&plain(0, 0), d("0.000002"), d("0.000008"), w(1.0, 1.0));
        assert_eq!(cost, Decimal::ZERO);
    }

    #[test]
    fn weights_scale_each_leg_independently() {
        // Halving the input weight halves only the input leg.
        let t = plain(1000, 1000);
        let baseline_cost = compute_cost(&t, d("0.000001"), d("0.000001"), w(1.0, 1.0));
        let halved_input = compute_cost(&t, d("0.000001"), d("0.000001"), w(0.5, 1.0));
        // baseline = 0.001 + 0.001 = 0.002; halved = 0.0005 + 0.001 = 0.0015
        assert_eq!(baseline_cost, d("0.002"));
        assert_eq!(halved_input, d("0.0015"));
    }

    #[test]
    fn nan_weight_falls_back_to_one_not_panic() {
        // f64::NAN doesn't convert to Decimal; the fallback keeps the
        // request from blowing up at the cost-log boundary.
        let weights = Weights {
            input: f64::NAN,
            ..w(1.0, 1.0)
        };
        let cost = compute_cost(&plain(1000, 0), d("0.000002"), d("0.000008"), weights);
        assert_eq!(cost, d("0.002"));
    }

    #[test]
    fn fractional_weight_preserves_full_precision() {
        // 1.25 scales 100 tokens @ 0.0001 to 0.0125
        let cost = compute_cost(&plain(100, 0), d("0.0001"), d("0.0001"), w(1.25, 1.0));
        assert_eq!(cost, d("0.0125"));
    }

    #[test]
    fn cached_input_is_priced_apart_from_plain_input() {
        // Anthropic-shaped: 100 fresh, 10 000 read from cache, 2 000 written.
        let t = TokenCounts {
            input: 100,
            cache_read: 10_000,
            cache_write: 2_000,
            cache_write_1h: false,
            output: 0,
        };
        // 0.000003 × (100 + 10 000 × 0.1 + 2 000 × 1.25) = 0.000003 × 3 600
        let cost = compute_cost(&t, d("0.000003"), d("0.000015"), w(1.0, 1.0));
        assert_eq!(cost, d("0.0108"));
        // At the full input price it would have been 0.000003 × 12 100.
        assert!(cost < d("0.0363"));
    }

    #[test]
    fn a_one_hour_cache_write_costs_twice_the_input_price_by_default() {
        let t = TokenCounts {
            cache_write: 1_000,
            cache_write_1h: true,
            ..Default::default()
        };
        let cost = compute_cost(&t, d("0.000003"), d("0.000015"), w(1.0, 1.0));
        assert_eq!(cost, d("0.006"));
    }
}
