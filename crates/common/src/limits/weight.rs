// ============================================================================
// Weighted-token converter
//
// Maps a request's token counts to a single integer "weighted token"
// cost using each model's weights from the `models` table.
//
//   weighted = round(input       × input_weight
//                  + cache_read  × cache_read_weight
//                  + cache_write × cache_write_weight   (or _1h_)
//                  + output      × output_weight)
//
// Input read from or written to the upstream's prompt cache is priced
// apart from plain input: a cache read costs a fraction of it, a write a
// premium. Unset cache weights follow the input weight — see
// `Weights::resolve` for the ratios.
//
// Used by the gateway hot path to feed `sliding::check_and_record`
// (tokens metric) and `budget::add_weighted_tokens`. The weights
// are loaded from PG into a process-local cache the first time we
// see a given model_id; subsequent calls hit the cache directly.
//
// Cache invalidation: by TTL only (5 minutes). Weights change rarely
// (admin tunes them in the model management page) so a brief
// staleness window is acceptable. We do NOT subscribe to the limits
// pubsub for this — the channel is for rule / cap changes, not
// model rows. If we need faster propagation later, the caller can
// call `WeightCache::invalidate_all` from the model PATCH handler.
//
// Cache shape: `RwLock<HashMap<String, (Weights, expires_at)>>`.
// 5-min TTL on entries. Bounded to 1024 distinct model_ids — beyond
// that we evict the oldest by expires_at. At the platform's scale
// this won't fire in practice, but the bound stops a misbehaving
// caller from leaking memory by passing junk model strings.
// ============================================================================

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use sqlx::PgPool;
use tokio::sync::RwLock;

const CACHE_TTL: Duration = Duration::from_secs(300);
const MAX_ENTRIES: usize = 1024;

/// Cache read, as a share of the input weight, when the model sets none.
/// Anthropic bills a cache read at 0.1× input, as do OpenAI's newest
/// models; older OpenAI models discount less (0.5×, 0.25×), and a model
/// served there should set its own.
pub const CACHE_READ_RATIO: f64 = 0.1;
/// Cache write (5-minute), as a share of the input weight, when unset.
/// Anthropic's 1.25×. OpenAI does not bill writes, and reports none.
pub const CACHE_WRITE_RATIO: f64 = 1.25;
/// Cache write with a 1-hour lifetime, as a share of the input weight,
/// when unset. Anthropic's 2×.
pub const CACHE_WRITE_1H_RATIO: f64 = 2.0;

/// A model's weights, each against the platform baseline price for its
/// direction: the three cache weights, like the input weight, against
/// the input price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cache_write_1h: f64,
}

impl Weights {
    /// The weights in force, from a `models` row: an unset cache weight
    /// is the input weight times its ratio above.
    pub fn resolve(
        input: f64,
        output: f64,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
        cache_write_1h: Option<f64>,
    ) -> Self {
        Self {
            input,
            output,
            cache_read: cache_read.unwrap_or(input * CACHE_READ_RATIO),
            cache_write: cache_write.unwrap_or(input * CACHE_WRITE_RATIO),
            cache_write_1h: cache_write_1h.unwrap_or(input * CACHE_WRITE_1H_RATIO),
        }
    }
}

impl Default for Weights {
    fn default() -> Self {
        Self::resolve(1.0, 1.0, None, None, None)
    }
}

/// One request's tokens, split the way they are priced. `input` is the
/// input that was neither read from nor written to the prompt cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// The cache writes had a 1-hour lifetime.
    pub cache_write_1h: bool,
    pub output: i64,
}

#[derive(Clone)]
struct CacheEntry {
    weights: Weights,
    expires: Instant,
}

#[derive(Clone)]
pub struct WeightCache {
    inner: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

impl Default for WeightCache {
    fn default() -> Self {
        Self::new()
    }
}

impl WeightCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Look up weights for a model id. Cache hit → return; miss →
    /// query the `models` table and populate. Falls back to (1.0, 1.0)
    /// if the model row doesn't exist (unknown model = treat as
    /// baseline) so a misconfigured request never crashes the proxy.
    pub async fn get(&self, pool: &PgPool, model_id: &str) -> Weights {
        // Fast path: read lock + freshness check.
        {
            let cache = self.inner.read().await;
            if let Some(e) = cache.get(model_id)
                && e.expires > Instant::now()
            {
                return e.weights;
            }
        }

        // Slow path: query PG, then upsert into the cache.
        type Row = (
            Decimal,
            Decimal,
            Option<Decimal>,
            Option<Decimal>,
            Option<Decimal>,
        );
        let weights = match sqlx::query_as::<_, Row>(
            "SELECT input_weight, output_weight, \
                    cache_read_weight, cache_write_weight, cache_write_1h_weight \
               FROM models WHERE model_id = $1",
        )
        .bind(model_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some((i, o, cr, cw, cw1h))) => {
                let f = |d: Decimal| d.to_f64().unwrap_or(1.0);
                Weights::resolve(f(i), f(o), cr.map(f), cw.map(f), cw1h.map(f))
            }
            Ok(None) => Weights::default(),
            Err(e) => {
                tracing::warn!("weight lookup failed for {model_id}: {e}; using 1.0");
                Weights::default()
            }
        };

        let mut cache = self.inner.write().await;
        // Bound the cache. If we're at the cap, drop the entry whose
        // expires_at is furthest in the past (= the closest to expiring).
        // Linear scan, OK at 1024 entries.
        if cache.len() >= MAX_ENTRIES
            && let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, e)| e.expires)
                .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest_key);
        }
        cache.insert(
            model_id.to_string(),
            CacheEntry {
                weights,
                expires: Instant::now() + CACHE_TTL,
            },
        );
        weights
    }

    /// Drop all cached entries. Called by the model PATCH handler so a
    /// weight change takes effect immediately on the local process.
    /// (Other processes pick it up via the 5-minute TTL — close enough
    /// for an admin tweak.)
    pub async fn invalidate_all(&self) {
        self.inner.write().await.clear();
    }
}

/// Compute the weighted token cost for one request.
///
/// Pulled out into a free function so call sites that already have
/// the `Weights` (e.g. tests) don't have to thread a cache + pool
/// through. The hot path looks like:
///
///   let mult = state.weight_cache.get(&state.db, &request.model).await;
///   let weighted = weighted_tokens(&counts, mult);
pub fn weighted_tokens(t: &TokenCounts, w: Weights) -> i64 {
    let n = |x: i64| x.max(0) as f64;
    let write = if t.cache_write_1h {
        w.cache_write_1h
    } else {
        w.cache_write
    };
    // f64 then round back. i64 is plenty for any token count (max ~9.2e18).
    let sum = n(t.input) * w.input
        + n(t.cache_read) * w.cache_read
        + n(t.cache_write) * write
        + n(t.output) * w.output;
    sum.round().max(0.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(input: i64, output: i64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            ..Default::default()
        }
    }

    fn weights(input: f64, output: f64) -> Weights {
        Weights::resolve(input, output, None, None, None)
    }

    #[test]
    fn weighted_tokens_default_is_raw_sum() {
        assert_eq!(weighted_tokens(&plain(100, 50), Weights::default()), 150);
    }

    #[test]
    fn weighted_tokens_scales_each_direction() {
        // 100 input + 50 output × 3 = 100 + 150 = 250
        assert_eq!(weighted_tokens(&plain(100, 50), weights(1.0, 3.0)), 250);
    }

    #[test]
    fn weighted_tokens_clamps_negatives() {
        assert_eq!(weighted_tokens(&plain(-1, -1), Weights::default()), 0);
    }

    #[test]
    fn weighted_tokens_rounds() {
        // 3 × 1.5 = 4.5 ; 1 × 0.5 = 0.5 ; sum = 5.0
        assert_eq!(weighted_tokens(&plain(3, 1), weights(1.5, 0.5)), 5);
    }

    #[test]
    fn cache_weights_follow_the_input_weight_when_unset() {
        let w = weights(2.0, 1.0);
        assert_eq!(w.cache_read, 0.2);
        assert_eq!(w.cache_write, 2.5);
        assert_eq!(w.cache_write_1h, 4.0);
    }

    #[test]
    fn cache_reads_and_writes_are_weighted_apart_from_plain_input() {
        let t = TokenCounts {
            input: 100,
            cache_read: 1000,
            cache_write: 400,
            cache_write_1h: false,
            output: 10,
        };
        // 100 + 1000 × 0.1 + 400 × 1.25 + 10 = 100 + 100 + 500 + 10
        assert_eq!(weighted_tokens(&t, Weights::default()), 710);
        let t = TokenCounts {
            cache_write_1h: true,
            ..t
        };
        // 100 + 100 + 400 × 2 + 10
        assert_eq!(weighted_tokens(&t, Weights::default()), 1010);
    }

    #[test]
    fn a_set_cache_weight_is_used_as_is() {
        let w = Weights::resolve(1.0, 1.0, Some(0.5), Some(1.0), None);
        let t = TokenCounts {
            cache_read: 100,
            cache_write: 100,
            ..Default::default()
        };
        assert_eq!(weighted_tokens(&t, w), 150);
    }
}
