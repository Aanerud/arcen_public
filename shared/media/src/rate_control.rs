//! How fast a host should encode, chosen from what the path actually carries.
//!
//! An encoder set to one bitrate for the whole session is wrong whenever the
//! path is not what it was sized for. Measured on a WAN whose capacity moved
//! from minute to minute: the same host and configuration ranged from 29 fps
//! with frames 200 ms old to 12 fps with frames seven seconds old. Once the
//! encoder offers more than the path carries, the surplus becomes queueing,
//! and every frame waits behind it.
//!
//! The signal is the host's own: how long finished frames wait before the
//! transport takes them. A host that keeps its transport buffers shallow —
//! see `arcen_transport::quic::interactive_send_window` — feels congestion as
//! that wait, not as a sudden stall seconds later. This controller answers it
//! the way remote-desktop products do (the reference speaks of a floor, a
//! ceiling and an active rate): cut fast towards what was delivered when frames
//! queue, and probe back up slowly when they do not.
//!
//! Pure and clock-free: the caller passes each interval's measurements.

// Bitrates are far below 2^52, where `f64` is exact, and are clamped back into
// the policy's bounds after every calculation.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::Duration;

/// The rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateControlPolicy {
    /// Never encode below this, however bad the path.
    pub floor_bps: u64,
    /// Never encode above this: what the session was sized for.
    pub ceiling_bps: u64,
    /// Mean frame wait above which the path is congested.
    pub congested_wait: Duration,
    /// Mean frame wait below which the path has room.
    pub clear_wait: Duration,
    /// On congestion, the new rate is at most this share of what was
    /// delivered in the interval.
    pub delivered_share: f64,
    /// On congestion, the new rate is at most this share of the old one.
    pub decrease_factor: f64,
    /// Per clear interval, the rate grows by this factor.
    pub increase_factor: f64,
    /// Clear intervals to wait after a decrease before probing up again.
    pub hold_intervals: u32,
    /// Changes smaller than this share are not worth reconfiguring for.
    pub min_change: f64,
}

impl RateControlPolicy {
    /// A policy for a session sized at `ceiling_bps`.
    #[must_use]
    pub fn for_ceiling(ceiling_bps: u64) -> Self {
        Self {
            floor_bps: (ceiling_bps / 8).max(500_000).min(ceiling_bps),
            ceiling_bps,
            congested_wait: Duration::from_millis(60),
            clear_wait: Duration::from_millis(20),
            delivered_share: 0.9,
            decrease_factor: 0.8,
            increase_factor: 1.08,
            hold_intervals: 3,
            min_change: 0.03,
        }
    }
}

/// One interval's measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSample {
    /// Video bytes the transport took in the interval.
    pub delivered_bytes: u64,
    /// How long the interval was.
    pub elapsed: Duration,
    /// Mean time finished frames waited for the transport.
    pub mean_frame_wait: Duration,
    /// Frames sent in the interval; an interval with none says nothing.
    pub frames: u64,
}

/// The current encoding rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateController {
    policy: RateControlPolicy,
    target_bps: u64,
    hold: u32,
}

impl RateController {
    /// Starts at the ceiling: a path is assumed good until it shows otherwise.
    #[must_use]
    pub const fn new(policy: RateControlPolicy) -> Self {
        Self {
            policy,
            target_bps: policy.ceiling_bps,
            hold: 0,
        }
    }

    /// The rate to encode at.
    #[must_use]
    pub const fn target_bps(&self) -> u64 {
        self.target_bps
    }

    /// Takes one interval's measurements; returns the new rate when it changed
    /// enough to reconfigure the encoder for.
    pub fn observe(&mut self, sample: RateSample) -> Option<u64> {
        if sample.frames == 0 || sample.elapsed.is_zero() {
            return None;
        }
        let previous = self.target_bps;
        if sample.mean_frame_wait >= self.policy.congested_wait {
            let delivered_bps = (sample.delivered_bytes as f64 * 8.0
                / sample.elapsed.as_secs_f64())
                * self.policy.delivered_share;
            let decreased = previous as f64 * self.policy.decrease_factor;
            self.target_bps = (delivered_bps.min(decreased) as u64)
                .clamp(self.policy.floor_bps, self.policy.ceiling_bps);
            self.hold = self.policy.hold_intervals;
        } else if sample.mean_frame_wait <= self.policy.clear_wait {
            if self.hold > 0 {
                self.hold -= 1;
            } else {
                self.target_bps = ((previous as f64 * self.policy.increase_factor) as u64)
                    .clamp(self.policy.floor_bps, self.policy.ceiling_bps);
            }
        }
        let change = (self.target_bps as f64 - previous as f64).abs() / previous.max(1) as f64;
        (change >= self.policy.min_change || (self.target_bps != previous && self.at_bound()))
            .then_some(self.target_bps)
    }

    fn at_bound(&self) -> bool {
        self.target_bps == self.policy.floor_bps || self.target_bps == self.policy.ceiling_bps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CEILING: u64 = 8_000_000;

    fn sample(delivered_mbps: f64, wait_ms: u64) -> RateSample {
        RateSample {
            delivered_bytes: (delivered_mbps * 1_000_000.0 / 8.0) as u64,
            elapsed: Duration::from_secs(1),
            mean_frame_wait: Duration::from_millis(wait_ms),
            frames: 30,
        }
    }

    #[test]
    fn a_clear_path_stays_at_the_ceiling() {
        let mut rate = RateController::new(RateControlPolicy::for_ceiling(CEILING));
        for _ in 0..20 {
            assert_eq!(rate.observe(sample(8.0, 5)), None);
        }
        assert_eq!(rate.target_bps(), CEILING);
    }

    #[test]
    fn congestion_cuts_to_what_was_delivered() {
        let mut rate = RateController::new(RateControlPolicy::for_ceiling(CEILING));
        let cut = rate.observe(sample(4.0, 200)).expect("cut");
        assert_eq!(cut, 3_600_000, "90% of the 4 Mbit/s the path delivered");
        let deeper = rate.observe(sample(3.0, 150)).expect("cut again");
        assert!(deeper < cut);
    }

    #[test]
    fn it_probes_back_up_slowly_after_holding() {
        let mut rate = RateController::new(RateControlPolicy::for_ceiling(CEILING));
        let cut = rate.observe(sample(4.0, 200)).expect("cut");
        for _ in 0..3 {
            assert_eq!(rate.observe(sample(3.6, 5)), None, "holding after a cut");
        }
        let up = rate.observe(sample(3.6, 5)).expect("probe");
        assert_eq!(up, (cut as f64 * 1.08) as u64);
        for _ in 0..40 {
            let _ = rate.observe(sample(8.0, 5));
        }
        assert_eq!(rate.target_bps(), CEILING, "and returns to the ceiling");
    }

    #[test]
    fn it_never_leaves_its_bounds() {
        let policy = RateControlPolicy::for_ceiling(CEILING);
        let mut rate = RateController::new(policy);
        for _ in 0..50 {
            let _ = rate.observe(sample(0.01, 900));
        }
        assert_eq!(rate.target_bps(), policy.floor_bps);
        assert_eq!(policy.floor_bps, 1_000_000);
    }

    #[test]
    fn an_idle_interval_says_nothing() {
        let mut rate = RateController::new(RateControlPolicy::for_ceiling(CEILING));
        let idle = RateSample {
            frames: 0,
            ..sample(0.0, 500)
        };
        assert_eq!(rate.observe(idle), None);
        assert_eq!(rate.target_bps(), CEILING);
    }

    #[test]
    fn a_wait_between_the_thresholds_holds_the_rate() {
        let mut rate = RateController::new(RateControlPolicy::for_ceiling(CEILING));
        assert_eq!(rate.observe(sample(8.0, 40)), None);
        assert_eq!(rate.target_bps(), CEILING);
    }
}
