// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Refinement budget (ADR 0016 R6): a measured residual, a token bucket with
//! a bounded burst, an in-flight bound, and suspension under congestion.
//!
//! Time is a monotonic `Duration` the caller supplies, so every rule is
//! deterministic under test.

use std::time::Duration;

/// Largest burst the bucket holds.
pub const MAX_BURST_BYTES: u64 = 256 * 1024;
/// Largest refinement written but not yet acknowledged.
pub const MAX_IN_FLIGHT_BYTES: u64 = 512 * 1024;
/// Quiet time after a congestion signal before refinement resumes.
pub const RESUME_AFTER: Duration = Duration::from_secs(1);
/// Loss ratio above which refinement is suspended.
pub const SUSPEND_LOSS_RATIO: f64 = 0.01;

/// What the Pier measured over the last window.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PathSample {
    /// Bits per second the path delivered (all lanes).
    pub delivered_bps: u64,
    /// Bits per second of interactive traffic (video, audio, input, cursor).
    pub interactive_bps: u64,
    /// Congestion events in the window.
    pub congestion_events: u64,
    /// Lost / sent packets in the window.
    pub loss_ratio: f64,
    /// Whether the video lane has queued, unwritten access units.
    pub video_backlog: bool,
    /// Whether Grading frame age rose during the window.
    pub frame_age_rising: bool,
}

/// Refinement rate from a measured residual: `min(max, session cap,
/// ½ × (delivered − interactive))`, never negative.
#[must_use]
pub fn residual_rate_bps(max_mbps: u32, session_cap_bps: u64, sample: &PathSample) -> u64 {
    let residual = sample.delivered_bps.saturating_sub(sample.interactive_bps) / 2;
    residual
        .min(u64::from(max_mbps) * 1_000_000)
        .min(session_cap_bps)
}

/// Token bucket: over any interval of length `W`, bytes taken are at most
/// `rate × W + burst`, and bytes in flight never exceed
/// [`MAX_IN_FLIGHT_BYTES`]. Suspension freezes it.
#[derive(Debug, Clone)]
pub struct RefineBudget {
    rate_bps: u64,
    burst: u64,
    tokens: u64,
    last: Duration,
    in_flight: u64,
    suspended_until: Option<Duration>,
}

impl RefineBudget {
    /// A bucket at `rate_bps`, starting empty.
    #[must_use]
    pub fn new(rate_bps: u64, burst: u64, now: Duration) -> Self {
        Self {
            rate_bps,
            burst: burst.min(MAX_BURST_BYTES),
            tokens: 0,
            last: now,
            in_flight: 0,
            suspended_until: None,
        }
    }

    fn refill(&mut self, now: Duration) {
        let elapsed = now.saturating_sub(self.last);
        self.last = now;
        if self.is_suspended(now) {
            return;
        }
        let earned = u128::from(self.rate_bps) * elapsed.as_nanos() / 8 / 1_000_000_000;
        self.tokens = self
            .tokens
            .saturating_add(u64::try_from(earned).unwrap_or(u64::MAX))
            .min(self.burst);
    }

    /// Updates the rate from a fresh measurement and applies the suspension
    /// rules: a congestion event, loss above 1 %, a video backlog or rising
    /// frame age suspends refinement until one quiet second has passed.
    pub fn observe(
        &mut self,
        now: Duration,
        max_mbps: u32,
        session_cap_bps: u64,
        sample: &PathSample,
    ) {
        self.refill(now);
        self.rate_bps = residual_rate_bps(max_mbps, session_cap_bps, sample);
        if sample.congestion_events > 0
            || sample.loss_ratio > SUSPEND_LOSS_RATIO
            || sample.video_backlog
            || sample.frame_age_rising
        {
            self.suspended_until = Some(now + RESUME_AFTER);
            self.tokens = 0;
        }
    }

    /// Whether refinement is suspended at `now`.
    #[must_use]
    pub fn is_suspended(&self, now: Duration) -> bool {
        self.suspended_until.is_some_and(|until| now < until)
    }

    /// Takes `bytes` for one record if the bucket and the in-flight bound
    /// allow it.
    pub fn try_take(&mut self, bytes: u64, now: Duration) -> bool {
        self.refill(now);
        if self.is_suspended(now)
            || bytes > self.tokens
            || self.in_flight.saturating_add(bytes) > MAX_IN_FLIGHT_BYTES
        {
            return false;
        }
        self.tokens -= bytes;
        self.in_flight += bytes;
        true
    }

    /// QUIC acknowledged `bytes` of refinement.
    pub fn acknowledged(&mut self, bytes: u64) {
        self.in_flight = self.in_flight.saturating_sub(bytes);
    }

    /// The refine lane was reset; its queued bytes will never be acked.
    pub fn lane_reset(&mut self) {
        self.in_flight = 0;
    }

    /// Current rate.
    #[must_use]
    pub const fn rate_bps(&self) -> u64 {
        self.rate_bps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn residual_never_eats_interactive_traffic() {
        let sample = PathSample {
            delivered_bps: 40_000_000,
            interactive_bps: 30_000_000,
            ..PathSample::default()
        };
        assert_eq!(residual_rate_bps(25, u64::MAX, &sample), 5_000_000);
        let busy = PathSample {
            delivered_bps: 10_000_000,
            interactive_bps: 12_000_000,
            ..PathSample::default()
        };
        assert_eq!(residual_rate_bps(25, u64::MAX, &busy), 0);
        let idle = PathSample {
            delivered_bps: 900_000_000,
            ..PathSample::default()
        };
        assert_eq!(residual_rate_bps(25, 20_000_000, &idle), 20_000_000);
    }

    #[test]
    fn bytes_over_any_window_stay_within_rate_times_window_plus_burst() {
        let rate = 8_000_000; // 1 MB/s
        let mut budget = RefineBudget::new(rate, 64 * 1024, Duration::ZERO);
        let mut taken = Vec::new();
        for step in 1..=3000_u64 {
            let now = MS * u32::try_from(step).unwrap();
            if budget.try_take(10_000, now) {
                taken.push((now, 10_000_u64));
                budget.acknowledged(10_000);
            }
        }
        for window_ms in [1_u32, 10, 100, 1000] {
            let window = MS * window_ms;
            for &(start, _) in &taken {
                let sum: u64 = taken
                    .iter()
                    .filter(|(at, _)| *at >= start && *at < start + window)
                    .map(|(_, bytes)| bytes)
                    .sum();
                let bound = rate / 8 * u64::from(window_ms) / 1000 + 64 * 1024;
                assert!(sum <= bound, "{sum} > {bound} over {window_ms} ms");
            }
        }
        assert!(!taken.is_empty());
    }

    #[test]
    fn congestion_suspends_and_in_flight_is_bounded() {
        let mut budget = RefineBudget::new(800_000_000, MAX_BURST_BYTES, Duration::ZERO);
        assert!(budget.try_take(200_000, MS * 10));
        assert!(budget.try_take(200_000, MS * 20));
        assert!(!budget.try_take(200_000, MS * 30), "in-flight bound");
        budget.acknowledged(400_000);
        let congested = PathSample {
            delivered_bps: 900_000_000,
            congestion_events: 1,
            ..PathSample::default()
        };
        budget.observe(MS * 40, 1000, u64::MAX, &congested);
        assert!(budget.is_suspended(MS * 500));
        assert!(!budget.try_take(1, MS * 500));
        assert!(!budget.is_suspended(MS * 1040));
        assert!(budget.try_take(1000, MS * 1100));
    }
}
