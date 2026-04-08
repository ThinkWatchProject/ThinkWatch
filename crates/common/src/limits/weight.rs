// ============================================================================
// Weighted-token converter
//
// Maps `(model_id, raw_input_tokens, raw_output_tokens)` to a single
// integer "weighted token" cost using each model's per-direction
// multiplier from the `models` table.
//
//   weighted = round(input × input_multiplier + output × output_multiplier)
//
// Used by the gateway hot path to feed `sliding::check_and_record`
// (tokens metric) and `budget::add_weighted_tokens`. The multipliers
// are loaded from PG into a process-local cache the first time we
// see a given model_id; subsequent calls hit the cache directly.
//
// Cache invalidation: by TTL only (5 minutes). Multipliers change
// rarely (admin tunes them in the model management page) so a brief
// staleness window is acceptable. We do NOT subscribe to the limits
// pubsub for this — the channel is for rule / cap changes, not
// model rows. If we need faster propagation later, the caller can
// call `WeightCache::invalidate_all` from the model PATCH handler.
//
// Cache shape: `RwLock<HashMap<String, (Multipliers, expires_at)>>`.
// 5-min TTL on entries. Bounded to 1024 distinct model_ids — beyond
// that we evict the oldest by expires_at. At the platform's scale
// this won't fire in practice, but the bound stops a misbehaving
// caller from leaking memory by passing junk model strings.
// ============================================================================

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::sync::RwLock;

const CACHE_TTL: Duration = Duration::from_secs(300);
const MAX_ENTRIES: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub struct Multipliers {
    pub input: f64,
    pub output: f64,
}

impl Default for Multipliers {
    fn default() -> Self {
        Self {
            input: 1.0,
            output: 1.0,
        }
    }
}

#[derive(Clone)]
struct CacheEntry {
    mult: Multipliers,
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

    /// Look up multipliers for a model id. Cache hit → return; miss
    /// → query the `models` table and populate. Falls back to (1.0, 1.0)
    /// if the model row doesn't exist (unknown model = treat as
    /// baseline) so a misconfigured request never crashes the proxy.
    pub async fn get(&self, pool: &PgPool, model_id: &str) -> Multipliers {
        // Fast path: read lock + freshness check.
        {
            let cache = self.inner.read().await;
            if let Some(e) = cache.get(model_id)
                && e.expires > Instant::now()
            {
                return e.mult;
            }
        }

        // Slow path: query PG, then upsert into the cache.
        let mult = match sqlx::query_as::<_, (rust_decimal::Decimal, rust_decimal::Decimal)>(
            "SELECT input_multiplier, output_multiplier FROM models WHERE model_id = $1",
        )
        .bind(model_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some((i, o))) => {
                use rust_decimal::prelude::ToPrimitive;
                Multipliers {
                    input: i.to_f64().unwrap_or(1.0),
                    output: o.to_f64().unwrap_or(1.0),
                }
            }
            Ok(None) => Multipliers::default(),
            Err(e) => {
                tracing::warn!("weight lookup failed for {model_id}: {e}; using 1.0");
                Multipliers::default()
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
                mult,
                expires: Instant::now() + CACHE_TTL,
            },
        );
        mult
    }

    /// Drop all cached entries. Called by the model PATCH handler so a
    /// multiplier change takes effect immediately on the local process.
    /// (Other processes pick it up via the 5-minute TTL — close enough
    /// for an admin tweak.)
    pub async fn invalidate_all(&self) {
        self.inner.write().await.clear();
    }
}

/// Compute the weighted token cost for one request.
///
/// Pulled out into a free function so call sites that already have
/// the `Multipliers` (e.g. tests) don't have to thread a cache + pool
/// through. The hot path looks like:
///
///   let mult = state.weight_cache.get(&state.db, &request.model).await;
///   let weighted = weighted_tokens(input, output, mult);
pub fn weighted_tokens(input_tokens: i64, output_tokens: i64, mult: Multipliers) -> i64 {
    // Cast to f64 for the multiplication then round back. i64 is
    // plenty for any token count (max ~9.2e18).
    let w = (input_tokens.max(0) as f64) * mult.input + (output_tokens.max(0) as f64) * mult.output;
    w.round().max(0.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_tokens_default_is_raw_sum() {
        let m = Multipliers::default();
        assert_eq!(weighted_tokens(100, 50, m), 150);
    }

    #[test]
    fn weighted_tokens_scales_each_direction() {
        let m = Multipliers {
            input: 1.0,
            output: 3.0,
        };
        // 100 input + 50 output × 3 = 100 + 150 = 250
        assert_eq!(weighted_tokens(100, 50, m), 250);
    }

    #[test]
    fn weighted_tokens_clamps_negatives() {
        let m = Multipliers::default();
        assert_eq!(weighted_tokens(-1, -1, m), 0);
    }

    #[test]
    fn weighted_tokens_rounds() {
        let m = Multipliers {
            input: 1.5,
            output: 0.5,
        };
        // 3 × 1.5 = 4.5 ; 1 × 0.5 = 0.5 ; sum = 5.0
        assert_eq!(weighted_tokens(3, 1, m), 5);
    }
}
