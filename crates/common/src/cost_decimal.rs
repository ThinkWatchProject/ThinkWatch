//! Cost-column encoding helpers for ClickHouse Decimal columns.
//!
//! ClickHouse stores `Decimal(P, S)` as a signed integer whose value
//! equals `decimal × 10^S`. The `clickhouse` Rust crate doesn't wrap
//! that integer — you write `i64` / `i128` directly and the server
//! interprets it under the column's declared scale. These helpers
//! keep the scale constant + the encode/decode code paths in one
//! place so every crate that touches `gateway_logs.cost_usd` stays
//! in sync.
//!
//! We chose `Decimal(18, 10)` for the per-request storage:
//!   * 10 fractional digits covers sub-cent token pricing without
//!     rounding (cheapest commercial model today is ~$1.5e-7 / token,
//!     ~7 fractional digits).
//!   * 18 total digits → max single-request cost ~$1e8. Individual
//!     requests are under a dollar, so the integer side is pure
//!     headroom.
//!
//! `sum(Decimal(18, 10))` in ClickHouse widens the result to
//! `Decimal(38, 10)` which exceeds `i64` but fits `i128`. Reading
//! aggregations uses `decode_i128`; reading a single row uses
//! `decode_i64`.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// Scale of `gateway_logs.cost_usd` and every derived sum / bucket
/// we read back from ClickHouse. Changing this is a migration.
pub const COST_SCALE: u32 = 10;

const COST_SCALE_FACTOR: i128 = 10_000_000_000; // 10^10

/// Scale a `Decimal` into the raw `i64` ClickHouse expects under a
/// `Decimal(18, 10)` column. Rounds to the stored scale. Clamps to the
/// `i64` range on overflow (practical ceiling ~$9.2e8 per row; the
/// caller never sees this unless the cost math is broken).
pub fn encode_i64(value: Decimal) -> i64 {
    let scaled = value * Decimal::from(COST_SCALE_FACTOR);
    scaled.round().to_i64().unwrap_or_else(|| {
        if value.is_sign_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

/// Decode a raw `i64` from a `Decimal(18, 10)` column back into a
/// `Decimal`. The `new` constructor never fails for valid inputs
/// because 10 is well under rust_decimal's 28-scale ceiling.
pub fn decode_i64(raw: i64) -> Decimal {
    Decimal::new(raw, COST_SCALE)
}

/// Same as `decode_i64` but for the widened `Decimal(38, 10)` result
/// of a ClickHouse `sum()` aggregation. rust_decimal's mantissa is
/// only 96 bits, so values above `~7.9e28` clamp to the max. A single
/// organisation summing a lifetime of token cost won't approach this
/// — the helper still saturates rather than panics so we never 500
/// on an aggregate query.
pub fn decode_i128(raw: i128) -> Decimal {
    // Fast path: fits in i64.
    if let Ok(n) = i64::try_from(raw) {
        return Decimal::new(n, COST_SCALE);
    }
    // Slow path: build a larger Decimal by string round-trip so the
    // mantissa > 2^63 case doesn't truncate.
    let sign = if raw < 0 { "-" } else { "" };
    let mag = raw.unsigned_abs();
    let divisor: u128 = COST_SCALE_FACTOR as u128;
    let int_part = mag / divisor;
    let frac_part = mag % divisor;
    let s = format!(
        "{sign}{int_part}.{frac_part:0scale$}",
        scale = COST_SCALE as usize,
    );
    s.parse().unwrap_or(Decimal::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn encode_decode_round_trip() {
        for s in [
            "0",
            "0.0000001",
            "1.2345678901",
            "100000",
            "12345.67",
            "-0.005",
        ] {
            let d = Decimal::from_str(s).unwrap();
            let raw = encode_i64(d);
            let back = decode_i64(raw);
            assert_eq!(back, d.round_dp(COST_SCALE), "round-trip failed for {s}");
        }
    }

    #[test]
    fn decode_i128_handles_wide_sum() {
        // 10^20 raw → 10^10 USD. Exceeds i64 but fits i128.
        let raw: i128 = 100_000_000_000_000_000_000;
        let d = decode_i128(raw);
        assert_eq!(d, Decimal::from_str("10000000000").unwrap());
    }

    #[test]
    fn decode_i128_negative() {
        let raw: i128 = -12_345_000_000; // -1.2345
        let d = decode_i128(raw);
        assert_eq!(d, Decimal::from_str("-1.2345").unwrap());
    }
}
