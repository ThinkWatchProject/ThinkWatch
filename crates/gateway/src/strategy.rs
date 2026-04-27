//! Routing strategy: how to assign weights to a candidate route group.
//!
//! Every selection (regardless of strategy) reduces to "given N
//! candidate routes, produce N weights, then weighted-random." The
//! variants only differ in *how* the weights are derived:
//!
//!   * `Weighted`      — operator-set `weight` column directly (manual)
//!   * `Latency`       — `w ∝ 1/latency_ms^k` (autotune to the fast)
//!   * `Health`        — `w ∝ success_rate^k` (favour low error rates)
//!   * `LatencyHealth` — `(1/latency^k) × success_rate^k` (combined)
//!
//! Per-route latency / success rate come from the rolling-window
//! tracker in `health.rs`. The circuit breaker is a *separate* hard
//! gate: routes in `Open` are excluded from the candidate set entirely
//! before this module runs, so strategy weights only modulate routes
//! that are already in-flight-eligible.

use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoutingStrategy {
    /// Operator-set weight column = traffic ratios. Manual mode of the wizard.
    Weighted,
    /// Auto-tune to fastest. Weight ∝ 1/EWMA_latency^k.
    Latency,
    /// Auto-tune to healthiest. Weight ∝ success_rate^k.
    Health,
    /// Combined latency × health. Default — closest to "do the right
    /// thing" with zero configuration.
    #[default]
    LatencyHealth,
}

impl RoutingStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Weighted => "weighted",
            Self::Latency => "latency",
            Self::Health => "health",
            Self::LatencyHealth => "latency_health",
        }
    }
}

impl FromStr for RoutingStrategy {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "weighted" => Ok(Self::Weighted),
            "latency" => Ok(Self::Latency),
            "health" => Ok(Self::Health),
            "latency_health" => Ok(Self::LatencyHealth),
            _ => Err(()),
        }
    }
}

/// Inputs the strategy needs about each candidate. Plain data so this
/// module stays a pure function and is trivially testable.
#[derive(Debug, Clone, Copy)]
pub struct RouteSignal {
    /// Operator-configured `weight` from `model_routes`. Used by
    /// `Weighted` directly, and as a multiplicative bias on the
    /// auto-tune variants (so an operator can still skew 2:1 toward
    /// Provider A even with auto strategies).
    pub configured_weight: u32,
    /// Recent EWMA latency in ms. `None` ⇒ "no samples yet" — the
    /// latency factor falls back to a neutral weight so a fresh route
    /// isn't starved before observations accumulate.
    pub ewma_latency_ms: Option<f64>,
    /// Recent success rate in [0, 1]. `None` ⇒ "no samples yet" — the
    /// health factor falls back to neutral (1.0).
    pub success_rate: Option<f64>,
}

/// Compute a weight per candidate. Output length matches input length.
/// Weights are non-negative; the caller passes them straight to a
/// weighted-random walk. All zeros ⇒ caller falls back to "first
/// candidate" — matches the original `pick_weighted` behaviour.
pub fn compute_weights(strategy: RoutingStrategy, signals: &[RouteSignal], k: f64) -> Vec<f64> {
    if signals.is_empty() {
        return Vec::new();
    }

    match strategy {
        RoutingStrategy::Weighted => signals.iter().map(|s| s.configured_weight as f64).collect(),

        RoutingStrategy::Latency => signals
            .iter()
            .map(|s| latency_factor(s.ewma_latency_ms, k) * s.configured_weight as f64)
            .collect(),

        RoutingStrategy::Health => signals
            .iter()
            .map(|s| health_factor(s.success_rate, k) * s.configured_weight as f64)
            .collect(),

        RoutingStrategy::LatencyHealth => signals
            .iter()
            .map(|s| {
                latency_factor(s.ewma_latency_ms, k)
                    * health_factor(s.success_rate, k)
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

/// `success_rate^k`. Unmeasured ⇒ 1.0 (neutral, favours new routes
/// while they accumulate samples instead of starving them on the
/// pessimistic assumption that no data = bad).
fn health_factor(success_rate: Option<f64>, k: f64) -> f64 {
    let r = success_rate.unwrap_or(1.0).clamp(0.0, 1.0);
    r.powf(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(w: u32, lat: Option<f64>, sr: Option<f64>) -> RouteSignal {
        RouteSignal {
            configured_weight: w,
            ewma_latency_ms: lat,
            success_rate: sr,
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
        let w = compute_weights(
            RoutingStrategy::Latency,
            &[sig(100, Some(100.0), None), sig(100, Some(200.0), None)],
            2.0,
        );
        assert!(approx_eq(w[0] / w[1], 4.0));
    }

    #[test]
    fn latency_unknown_falls_back_to_neutral() {
        let w = compute_weights(
            RoutingStrategy::Latency,
            &[sig(100, None, None), sig(100, Some(100.0), None)],
            2.0,
        );
        assert!(w[0] > w[1]);
    }

    #[test]
    fn health_strategy_favors_healthy() {
        // 100% vs 50% success at k=2 → 1.0 vs 0.25 = 4× preference.
        let w = compute_weights(
            RoutingStrategy::Health,
            &[sig(100, None, Some(1.0)), sig(100, None, Some(0.5))],
            2.0,
        );
        assert!(approx_eq(w[0] / w[1], 4.0));
    }

    #[test]
    fn health_unknown_falls_back_to_neutral() {
        // No samples shouldn't starve the route; falls back to 1.0.
        let w = compute_weights(
            RoutingStrategy::Health,
            &[sig(100, None, None), sig(100, None, Some(0.9))],
            2.0,
        );
        assert!(w[0] > w[1]);
    }

    #[test]
    fn latency_health_combines_both() {
        // Faster (50 vs 100 → 4× at k=2) and healthier (1.0 vs 0.5
        // → 4× at k=2). Combined → 16× preference.
        let w = compute_weights(
            RoutingStrategy::LatencyHealth,
            &[
                sig(100, Some(50.0), Some(1.0)),
                sig(100, Some(100.0), Some(0.5)),
            ],
            2.0,
        );
        assert!(approx_eq(w[0] / w[1], 16.0));
    }

    #[test]
    fn configured_weight_modulates_auto_strategies() {
        // Same latency, but operator gave route 0 twice the weight.
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
            RoutingStrategy::Health,
            RoutingStrategy::LatencyHealth,
        ] {
            assert_eq!(s.as_str().parse::<RoutingStrategy>().unwrap(), s);
        }
    }

    #[test]
    fn parse_strategy_unknown_errors() {
        assert!("garbage".parse::<RoutingStrategy>().is_err());
        assert!("cost".parse::<RoutingStrategy>().is_err());
        assert!("latency_cost".parse::<RoutingStrategy>().is_err());
    }
}
