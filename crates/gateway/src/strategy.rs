//! Routing strategy: how to assign weights to a candidate route group.
//!
//! Every selection (regardless of strategy) reduces to "given N
//! candidate routes, produce N weights, then weighted-random." The
//! variants only differ in *how* the weights are derived:
//!
//!   * `Weighted`     — use the operator-set `weight` column directly
//!     (admin owns the ratios; default for "manual mode" in the UI)
//!   * `Latency`      — `w ∝ 1/latency_ms^k` (autotune to the fast)
//!   * `Cost`         — `w ∝ 1/effective_cost_per_token`
//!   * `LatencyCost`  — combined `(1/latency^k) × (1/cost)` (default
//!     for "auto mode" — most balanced behaviour out of the box)
//!
//! Per-route observed latency comes from the rolling-window EWMA in
//! `health.rs`. Per-route cost is computed once at router-load time
//! (function of `models.input/output_weight × platform_pricing`).
//!
//! Failover is not this module's concern: the proxy filters the
//! candidate set down to "enabled, circuit-closed, under cap" routes
//! before asking `compute_weights` for weights, picks one, and on
//! retryable error tries another from the remaining healthy set.
//! There used to be a `Priority` strategy that did strict ordered
//! failover via a separate `model_routes.priority` column; both went
//! away in the v2 redesign because the circuit breaker covers the
//! same use-case without the extra mental model.

use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoutingStrategy {
    /// Operator-set weight column = traffic ratios. The "manual mode"
    /// of the wizard.
    Weighted,
    /// Auto-tune to fastest. Weight ∝ 1/EWMA_latency^k.
    Latency,
    /// Cheapest first. Weight ∝ 1/cost_per_token.
    Cost,
    /// Combined latency + cost. Weight ∝ (1/latency^k) × (1/cost).
    /// Default global strategy after v2 — closest to "do the right
    /// thing" with zero configuration.
    #[default]
    LatencyCost,
}

impl RoutingStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Weighted => "weighted",
            Self::Latency => "latency",
            Self::Cost => "cost",
            Self::LatencyCost => "latency_cost",
        }
    }
}

impl FromStr for RoutingStrategy {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "weighted" => Ok(Self::Weighted),
            "latency" => Ok(Self::Latency),
            "cost" => Ok(Self::Cost),
            "latency_cost" => Ok(Self::LatencyCost),
            _ => Err(()),
        }
    }
}

/// Inputs the strategy needs about each candidate. Plain data so this
/// module stays a pure function and is trivially testable.
#[derive(Debug, Clone, Copy)]
pub struct RouteSignal {
    /// Operator-configured `weight` from `model_routes`. Used by
    /// `Weighted` directly, and as a multiplicative override on the
    /// auto-tune variants (so an operator can still bias 2:1 toward
    /// Provider A even with a `latency` strategy).
    pub configured_weight: u32,
    /// Recent EWMA latency in ms. `None` means "no samples yet" —
    /// auto-tune strategies fall back to a neutral weight.
    pub ewma_latency_ms: Option<f64>,
    /// Effective $/token, summed input + output. `None` is treated
    /// as "unknown cost" — `Cost` / `LatencyCost` skip the cost factor.
    pub cost_per_token: Option<f64>,
}

/// Compute a weight per candidate. Output length matches input length.
/// Weights are non-negative; the caller passes them straight to a
/// weighted-random walk. All zeros ⇒ caller falls back to "first
/// candidate" — matches the original `pick_weighted` behaviour.
pub fn compute_weights(
    strategy: RoutingStrategy,
    signals: &[RouteSignal],
    latency_k: f64,
) -> Vec<f64> {
    if signals.is_empty() {
        return Vec::new();
    }

    match strategy {
        RoutingStrategy::Weighted => signals.iter().map(|s| s.configured_weight as f64).collect(),

        RoutingStrategy::Latency => signals
            .iter()
            .map(|s| latency_factor(s.ewma_latency_ms, latency_k) * s.configured_weight as f64)
            .collect(),

        RoutingStrategy::Cost => signals
            .iter()
            .map(|s| cost_factor(s.cost_per_token) * s.configured_weight as f64)
            .collect(),

        RoutingStrategy::LatencyCost => signals
            .iter()
            .map(|s| {
                latency_factor(s.ewma_latency_ms, latency_k)
                    * cost_factor(s.cost_per_token)
                    * s.configured_weight as f64
            })
            .collect(),
    }
}

/// `1 / latency^k` with an unmeasured fallback of 1.0 ms — neutral
/// enough that a fresh route isn't starved while waiting for samples.
fn latency_factor(ewma_ms: Option<f64>, k: f64) -> f64 {
    let ms = ewma_ms.unwrap_or(1.0).max(1.0);
    1.0 / ms.powf(k)
}

/// `1 / cost` with an unmeasured fallback of 1.0 (neutral).
fn cost_factor(cost: Option<f64>) -> f64 {
    let c = cost.unwrap_or(1.0).max(1e-12);
    1.0 / c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(w: u32, lat: Option<f64>, cost: Option<f64>) -> RouteSignal {
        RouteSignal {
            configured_weight: w,
            ewma_latency_ms: lat,
            cost_per_token: cost,
        }
    }

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn weighted_passes_through_configured() {
        let w = compute_weights(
            RoutingStrategy::Weighted,
            &[sig(80, None, None), sig(20, None, None)],
            2.0,
        );
        assert!(approx_eq(w[0] / w[1], 4.0));
    }

    #[test]
    fn latency_strategy_favors_fast_at_k_2() {
        // Aggressive (k=2): 100ms vs 200ms → 4× preference.
        let w = compute_weights(
            RoutingStrategy::Latency,
            &[sig(100, Some(100.0), None), sig(100, Some(200.0), None)],
            2.0,
        );
        assert!(approx_eq(w[0] / w[1], 4.0));
    }

    #[test]
    fn latency_strategy_more_aggressive_at_higher_k() {
        // k=4 → 16× preference for the route at half latency.
        let w = compute_weights(
            RoutingStrategy::Latency,
            &[sig(100, Some(100.0), None), sig(100, Some(200.0), None)],
            4.0,
        );
        assert!(approx_eq(w[0] / w[1], 16.0));
    }

    #[test]
    fn latency_unknown_falls_back_to_neutral() {
        let w = compute_weights(
            RoutingStrategy::Latency,
            &[sig(100, None, None), sig(100, Some(100.0), None)],
            2.0,
        );
        // Unmeasured route gets 1/1^2 = 1 vs measured 1/100^2 = 0.0001.
        // Neutral fallback intentionally biases toward the new route
        // until samples accumulate.
        assert!(w[0] > w[1]);
    }

    #[test]
    fn cost_strategy_favors_cheap() {
        let w = compute_weights(
            RoutingStrategy::Cost,
            &[
                sig(100, None, Some(0.000001)),
                sig(100, None, Some(0.000005)),
            ],
            2.0,
        );
        assert!(approx_eq(w[0] / w[1], 5.0));
    }

    #[test]
    fn latency_cost_combines_both() {
        // Faster (50 vs 100 → 4× at k=2) and cheaper (1e-6 vs 2e-6
        // → 2×). Combined → 8× preference.
        let w = compute_weights(
            RoutingStrategy::LatencyCost,
            &[
                sig(100, Some(50.0), Some(1e-6)),
                sig(100, Some(100.0), Some(2e-6)),
            ],
            2.0,
        );
        assert!(approx_eq(w[0] / w[1], 8.0));
    }

    #[test]
    fn configured_weight_modulates_auto_strategies() {
        // Same latency, but operator gave route 0 twice the weight.
        // Latency strategy still respects that bias.
        let w = compute_weights(
            RoutingStrategy::Latency,
            &[sig(200, Some(100.0), None), sig(100, Some(100.0), None)],
            2.0,
        );
        assert!(approx_eq(w[0] / w[1], 2.0));
    }

    #[test]
    fn empty_signals_returns_empty() {
        assert!(compute_weights(RoutingStrategy::Weighted, &[], 2.0).is_empty());
    }

    #[test]
    fn parse_strategy_round_trip() {
        for s in [
            RoutingStrategy::Weighted,
            RoutingStrategy::Latency,
            RoutingStrategy::Cost,
            RoutingStrategy::LatencyCost,
        ] {
            assert_eq!(s.as_str().parse::<RoutingStrategy>().unwrap(), s);
        }
    }

    #[test]
    fn priority_value_no_longer_parses() {
        // Removed in v2 — admins who had this set get migrated to
        // latency_cost via 003_routing_v2.sql.
        assert!("priority".parse::<RoutingStrategy>().is_err());
    }

    #[test]
    fn parse_strategy_unknown_errors() {
        assert!("garbage".parse::<RoutingStrategy>().is_err());
    }
}
