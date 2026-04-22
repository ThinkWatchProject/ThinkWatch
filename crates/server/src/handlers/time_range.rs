//! Dashboard / analytics time-range helper.
//!
//! The overview endpoints all share the same shape:
//! "give me totals + N buckets over the last X". This module centralises
//! the window math (how many buckets, how wide, where they start) so each
//! handler only has to pick a range and plug the outputs into its SQL.

use chrono::{DateTime, Duration, Timelike, Utc};
use serde::Deserialize;

/// Shared query-string extractor for `/api/dashboard/stats`,
/// `/api/analytics/usage/stats`, `/api/analytics/costs/stats`.
///
/// Accepts `?range=24h|7d|30d` and an optional `?compare=true` flag.
/// When `compare` is set the handler computes totals over the
/// immediately-preceding window of the same length and returns them
/// alongside the current values so the dashboard can render a delta.
/// Anything else silently falls back to the 24h default so an old
/// client with a stale param doesn't 400.
#[derive(Debug, Default, Deserialize)]
pub struct RangeQuery {
    pub range: Option<String>,
    #[serde(default)]
    pub compare: Option<bool>,
    /// Optional analytics team filter — surfaces consumed by the
    /// analytics handlers so the page-level "filter by team" dropdown
    /// actually reaches the SQL. Other RangeQuery consumers (the
    /// dashboard) silently ignore an unrecognized field.
    #[serde(default)]
    pub team_id: Option<uuid::Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeRange {
    /// 24 hourly buckets.
    Day,
    /// 7 daily buckets.
    Week,
    /// 30 daily buckets.
    Month,
}

impl TimeRange {
    pub fn parse(s: Option<&str>) -> Self {
        match s.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "7d" | "week" => Self::Week,
            "30d" | "month" => Self::Month,
            _ => Self::Day,
        }
    }

    pub fn bucket_count(self) -> usize {
        match self {
            Self::Day => 24,
            Self::Week => 7,
            Self::Month => 30,
        }
    }

    /// Width of one bucket — used only for tests/display; SQL uses trunc_unit.
    #[allow(dead_code)]
    pub fn bucket_duration(self) -> Duration {
        match self {
            Self::Day => Duration::hours(1),
            Self::Week | Self::Month => Duration::days(1),
        }
    }

    /// First bucket's start timestamp (oldest, inclusive). Anything older
    /// than this is excluded from the window.
    pub fn window_start(self, now: DateTime<Utc>) -> DateTime<Utc> {
        self.bucket_starts(now).first().copied().unwrap_or(now)
    }

    /// Previous-period window — the same-length window immediately
    /// before the current one. Returns `(start, end)` half-open
    /// `[start, end)` so SQL becomes `created_at >= start AND
    /// created_at < end`.
    ///
    /// Used by the dashboard "compare to previous" toggle so
    /// `(current - previous) / previous` gives the operator a
    /// percentage delta per card.
    pub fn prev_window(self, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        let cur_start = self.window_start(now);
        let n = self.bucket_count() as i64;
        let prev_start = match self {
            Self::Day => cur_start - Duration::hours(n),
            Self::Week | Self::Month => cur_start - Duration::days(n),
        };
        (prev_start, cur_start)
    }

    /// Bucket start timestamps (oldest → newest), aligned to the trunc unit.
    /// The i-th element is the inclusive lower bound of bucket i.
    pub fn bucket_starts(self, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        let n = self.bucket_count();
        match self {
            Self::Day => {
                // Align to the current hour, then step back.
                let anchor = now
                    .date_naive()
                    .and_hms_opt(now.hour(), 0, 0)
                    .expect("valid hms")
                    .and_utc();
                (0..n)
                    .map(|i| anchor - Duration::hours((n - 1 - i) as i64))
                    .collect()
            }
            Self::Week | Self::Month => {
                // Align to the current day, step back in days.
                let anchor = now
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .expect("valid hms")
                    .and_utc();
                (0..n)
                    .map(|i| anchor - Duration::days((n - 1 - i) as i64))
                    .collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_to_day() {
        assert_eq!(TimeRange::parse(None), TimeRange::Day);
        assert_eq!(TimeRange::parse(Some("")), TimeRange::Day);
        assert_eq!(TimeRange::parse(Some("garbage")), TimeRange::Day);
        assert_eq!(TimeRange::parse(Some("24h")), TimeRange::Day);
    }

    #[test]
    fn parse_recognises_week_and_month() {
        assert_eq!(TimeRange::parse(Some("7d")), TimeRange::Week);
        assert_eq!(TimeRange::parse(Some("WEEK")), TimeRange::Week);
        assert_eq!(TimeRange::parse(Some("30d")), TimeRange::Month);
        assert_eq!(TimeRange::parse(Some(" Month ")), TimeRange::Month);
    }

    #[test]
    fn bucket_count_and_duration_match_range() {
        assert_eq!(TimeRange::Day.bucket_count(), 24);
        assert_eq!(TimeRange::Week.bucket_count(), 7);
        assert_eq!(TimeRange::Month.bucket_count(), 30);
        assert_eq!(TimeRange::Day.bucket_duration(), Duration::hours(1));
        assert_eq!(TimeRange::Week.bucket_duration(), Duration::days(1));
        assert_eq!(TimeRange::Month.bucket_duration(), Duration::days(1));
    }

    #[test]
    fn bucket_starts_are_monotonic_and_correctly_sized() {
        let now = Utc::now();
        for r in [TimeRange::Day, TimeRange::Week, TimeRange::Month] {
            let starts = r.bucket_starts(now);
            assert_eq!(starts.len(), r.bucket_count());
            for pair in starts.windows(2) {
                assert!(pair[0] < pair[1], "buckets must strictly increase");
            }
            // Last bucket starts at or before `now` within one bucket-width.
            let last = *starts.last().unwrap();
            assert!(last <= now);
            assert!(now - last < r.bucket_duration() + Duration::seconds(1));
        }
    }

    #[test]
    fn window_start_matches_first_bucket() {
        let now = Utc::now();
        for r in [TimeRange::Day, TimeRange::Week, TimeRange::Month] {
            assert_eq!(r.window_start(now), r.bucket_starts(now)[0]);
        }
    }

    #[test]
    fn prev_window_is_same_length_and_immediately_before() {
        let now = Utc::now();
        for r in [TimeRange::Day, TimeRange::Week, TimeRange::Month] {
            let cur_start = r.window_start(now);
            let (prev_start, prev_end) = r.prev_window(now);
            // Previous window's end == current window's start (no gap,
            // no overlap).
            assert_eq!(prev_end, cur_start, "{r:?}: gap between prev/cur windows");
            // Previous window length == bucket_count × bucket_duration.
            // The "current window" stretches from window_start to a
            // few minutes past `now` (because alignment rounds down),
            // so we compare against the canonical window length, not
            // `now - cur_start`.
            let canonical_len = r.bucket_duration() * (r.bucket_count() as i32);
            let prev_len = prev_end - prev_start;
            assert_eq!(
                prev_len, canonical_len,
                "{r:?}: prev window length {prev_len:?} != canonical {canonical_len:?}"
            );
        }
    }
}
