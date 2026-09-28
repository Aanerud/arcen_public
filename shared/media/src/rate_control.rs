//! Live video bitrate control shared by every Pier.
//!
//! The encoder starts at the conservative link cap that has protected the WAN
//! lab path. It may then probe toward the uncapped shape budget while transport
//! truth says the path is clear, and it cuts quickly when RTT queueing, loss or
//! congestion events show that bytes are building up inside QUIC rather than in
//! the host's frame queue.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::Duration;

use crate::video::MotionPriority;
pub use arcen_telemetry::PathSignal;

/// The rule every host applies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateControlPolicy {
    /// Never encode below this, however bad the path.
    pub floor_bps: u64,
    /// The first target, normally today's shared link cap.
    pub start_bps: u64,
    /// Never encode above this: the uncapped session shape budget.
    pub ceiling_bps: u64,
    /// Queueing above the RTT baseline that means the path is filling.
    pub congested_queue_delay: Duration,
    /// Queueing below this is treated as a clear path when loss is absent.
    pub clear_queue_delay: Duration,
    /// Frame wait remains a secondary local signal.
    pub congested_frame_wait: Duration,
    /// Local frame wait below this is clear when transport is also clear.
    pub clear_frame_wait: Duration,
    /// On congestion, target at most this share of delivered throughput.
    pub delivered_share: f64,
    /// On congestion, target at most this share of the previous target.
    pub decrease_factor: f64,
    /// Never reduce below this share of the previous target in one interval.
    pub max_single_decrease_factor: f64,
    /// Intervals after a cut where lagging non-severe signals are ignored.
    pub cut_cooldown_intervals: u32,
    /// Additive probe as a share of the ceiling each clear interval.
    pub additive_increase: f64,
    /// Multiplicative probe each clear interval.
    pub increase_factor: f64,
    /// Clear intervals to wait after a first decrease before probing up again.
    pub hold_intervals: u32,
    /// Maximum clear intervals held after repeated cuts near the same knee.
    pub max_hold_intervals: u32,
    /// Repeated cuts within this share of the last cut target are treated as
    /// the same bottleneck.
    pub knee_repeat_window: f64,
    /// Probing within this share of the remembered knee uses the slow probe.
    pub knee_slow_probe_window: f64,
    /// Slow-probe multiplicative growth near a remembered knee.
    pub knee_increase_factor: f64,
    /// A remembered knee expires after this much clear time.
    pub knee_expiry: Duration,
    /// Clear intervals that reset repeated-knee backoff.
    pub knee_clear_intervals: u32,
    /// Loss rate that is treated as congestion by itself.
    pub congested_loss_rate: f64,
    /// Loss rate that makes congestion events meaningful.
    pub event_loss_rate: f64,
    /// Queue-delay samples required before a normal queue cut.
    pub queue_delay_samples: u32,
    /// Queue delay at this multiple of the congestion threshold cuts immediately.
    pub severe_queue_delay_factor: u32,
    /// Changes smaller than this share are not worth reconfiguring for.
    pub min_change: f64,
}

impl RateControlPolicy {
    /// A compatibility policy that starts at the ceiling.
    #[must_use]
    pub fn for_ceiling(ceiling_bps: u64) -> Self {
        Self::for_bounds(ceiling_bps, ceiling_bps)
    }

    /// A policy for a session whose safe starting cap and uncapped ceiling are
    /// known.
    #[must_use]
    pub fn for_bounds(start_bps: u64, ceiling_bps: u64) -> Self {
        Self::for_bounds_and_priority(start_bps, ceiling_bps, MotionPriority::Motion)
    }

    /// A policy for a session, preserving either motion cadence or per-frame detail first.
    #[must_use]
    pub fn for_bounds_and_priority(
        start_bps: u64,
        ceiling_bps: u64,
        _priority: MotionPriority,
    ) -> Self {
        let ceiling_bps = ceiling_bps.max(1);
        let start_bps = start_bps.clamp(1, ceiling_bps);
        let floor_bps = (start_bps / 4).max(500_000).min(start_bps);
        Self {
            floor_bps,
            start_bps,
            ceiling_bps,
            congested_queue_delay: Duration::from_millis(15),
            clear_queue_delay: Duration::from_millis(5),
            congested_frame_wait: Duration::from_millis(51),
            clear_frame_wait: Duration::from_millis(50),
            delivered_share: 0.85,
            decrease_factor: 0.72,
            max_single_decrease_factor: 0.5,
            cut_cooldown_intervals: 3,
            additive_increase: 0.025,
            increase_factor: 1.08,
            hold_intervals: 2,
            max_hold_intervals: 24,
            knee_repeat_window: 0.25,
            knee_slow_probe_window: 0.15,
            knee_increase_factor: 1.02,
            knee_expiry: Duration::from_secs(30),
            knee_clear_intervals: 10,
            congested_loss_rate: 0.02,
            event_loss_rate: 0.005,
            queue_delay_samples: 2,
            severe_queue_delay_factor: 3,
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
    /// Mean time finished frames waited for the host-side writer.
    pub mean_frame_wait: Duration,
    /// Frames sent in the interval; an interval with none still carries path
    /// signal, but delivered throughput is ignored.
    pub frames: u64,
    /// Pipelines contributing to `delivered_bytes` and `frames`.
    ///
    /// Hosts apply the resulting target to each pipeline. Multi-monitor hosts
    /// may sample an aggregate mux queue, so the controller normalizes
    /// delivered throughput to one pipeline before using it as a capacity hint.
    pub pipeline_count: u32,
    /// Transport path truth for this interval.
    pub path: Option<PathSignal>,
}

/// Why the controller changed the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateChangeReason {
    /// RTT grew above the baseline, indicating queueing.
    QueueDelay,
    /// Loss or congestion events occurred on the path.
    LossOrCongestion,
    /// Finished frames waited locally too long.
    FrameWait,
    /// The path was clear long enough to probe upward.
    ProbeIncrease,
    /// The target was clamped to a configured bound.
    Bound,
}

impl RateChangeReason {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::QueueDelay => "queue_delay",
            Self::LossOrCongestion => "loss_or_congestion",
            Self::FrameWait => "frame_wait",
            Self::ProbeIncrease => "probe_increase",
            Self::Bound => "bound",
        }
    }
}

/// A target change worth applying to the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateChange {
    pub target_bps: u64,
    pub previous_bps: u64,
    pub reason: RateChangeReason,
}

/// The current encoding rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateController {
    policy: RateControlPolicy,
    target_bps: u64,
    hold: u32,
    queue_delay_streak: u32,
    last_queue_delay: Option<Duration>,
    frame_wait_streak: u32,
    last_frame_wait: Option<Duration>,
    loss_streak: u32,
    cut_cooldown_remaining: u32,
    cooldown_first_cut_target_bps: Option<u64>,
    cooldown_severe_cut_taken: bool,
    knee_bps: Option<u64>,
    knee_age: Duration,
    knee_repeats: u32,
    clear_intervals: u32,
}

impl RateController {
    /// Starts at the safe shared cap, not at the unconstrained shape budget.
    #[must_use]
    pub const fn new(policy: RateControlPolicy) -> Self {
        Self {
            policy,
            target_bps: policy.start_bps,
            hold: 0,
            queue_delay_streak: 0,
            last_queue_delay: None,
            frame_wait_streak: 0,
            last_frame_wait: None,
            loss_streak: 0,
            cut_cooldown_remaining: 0,
            cooldown_first_cut_target_bps: None,
            cooldown_severe_cut_taken: false,
            knee_bps: None,
            knee_age: Duration::ZERO,
            knee_repeats: 0,
            clear_intervals: 0,
        }
    }

    /// The rate to encode at.
    #[must_use]
    pub const fn target_bps(&self) -> u64 {
        self.target_bps
    }

    /// Takes one interval's measurements; returns a change when the encoder
    /// should be reconfigured.
    pub fn observe(&mut self, sample: RateSample) -> Option<RateChange> {
        if sample.elapsed.is_zero() {
            return None;
        }
        let in_cut_cooldown = self.cut_cooldown_remaining > 0;
        let previous = self.target_bps;
        let mut reason = None;
        if let Some(event) = self.congestion_event(sample) {
            if let Some(target) = self.cooldown_cut_target(sample, previous, event) {
                self.remember_knee(previous);
                self.target_bps = target;
                self.hold = self.knee_hold_intervals();
                self.clear_intervals = 0;
                self.knee_age = Duration::ZERO;
                if !in_cut_cooldown {
                    self.start_cut_cooldown(target);
                }
                reason = Some(event.reason);
            }
        } else if self.path_is_clear(sample) {
            self.knee_age = self.knee_age.saturating_add(sample.elapsed);
            if self.hold > 0 && previous != self.policy.floor_bps {
                self.hold -= 1;
            } else {
                self.clear_intervals = self.clear_intervals.saturating_add(1);
                if previous == self.policy.floor_bps && self.path_resets_knee(sample) {
                    self.knee_bps = None;
                    self.knee_repeats = 0;
                    self.hold = 0;
                }
                if self.path_resets_knee(sample)
                    && (self.knee_age >= self.policy.knee_expiry
                        || self.clear_intervals >= self.policy.knee_clear_intervals)
                {
                    self.knee_bps = None;
                    self.knee_repeats = 0;
                }
                if previous < self.policy.ceiling_bps {
                    self.target_bps = self.increased_target(previous);
                    reason = Some(RateChangeReason::ProbeIncrease);
                }
            }
        }
        if in_cut_cooldown {
            self.cut_cooldown_remaining = self.cut_cooldown_remaining.saturating_sub(1);
            if self.cut_cooldown_remaining == 0 {
                self.cooldown_first_cut_target_bps = None;
                self.cooldown_severe_cut_taken = false;
            }
        }
        self.target_bps = self
            .target_bps
            .clamp(self.policy.floor_bps, self.policy.ceiling_bps);
        if self.target_bps != previous && self.at_bound() {
            reason = reason.or(Some(RateChangeReason::Bound));
        }
        let change = (self.target_bps as f64 - previous as f64).abs() / previous.max(1) as f64;
        (change >= self.policy.min_change || (self.target_bps != previous && self.at_bound()))
            .then_some(RateChange {
                target_bps: self.target_bps,
                previous_bps: previous,
                reason: reason.unwrap_or(RateChangeReason::Bound),
            })
    }

    fn start_cut_cooldown(&mut self, target: u64) {
        self.cut_cooldown_remaining = self.policy.cut_cooldown_intervals;
        self.cooldown_first_cut_target_bps = Some(target);
        self.cooldown_severe_cut_taken = false;
    }

    fn cooldown_cut_target(
        &mut self,
        sample: RateSample,
        previous: u64,
        event: CongestionEvent,
    ) -> Option<u64> {
        if self.cut_cooldown_remaining == 0 {
            return Some(self.decreased_target(sample, previous, event.reason));
        }
        if !event.severe || self.cooldown_severe_cut_taken {
            return None;
        }
        self.cooldown_severe_cut_taken = true;
        let target = self.decreased_target(sample, previous, event.reason);
        let cooldown_cap = self
            .cooldown_first_cut_target_bps
            .unwrap_or(previous)
            .saturating_mul((self.policy.decrease_factor * 1_000.0).round() as u64)
            / 1_000;
        Some(target.min(cooldown_cap))
    }

    fn remember_knee(&mut self, cut_target: u64) {
        let repeated = self
            .knee_bps
            .is_some_and(|knee| within_share(cut_target, knee, self.policy.knee_repeat_window))
            && self.knee_age <= self.policy.knee_expiry;
        if repeated {
            self.knee_repeats = self.knee_repeats.saturating_add(1);
            if let Some(knee) = self.knee_bps.as_mut() {
                *knee = ((*knee).saturating_add(cut_target)) / 2;
            }
        } else {
            self.knee_bps = Some(cut_target);
            self.knee_repeats = 0;
        }
    }

    fn knee_hold_intervals(&self) -> u32 {
        let shift = self.knee_repeats.min(8);
        let factor = 1_u32.checked_shl(shift).unwrap_or(u32::MAX);
        self.policy
            .hold_intervals
            .saturating_mul(factor)
            .min(self.policy.max_hold_intervals)
    }

    fn increased_target(&self, previous: u64) -> u64 {
        if self.near_remembered_knee(previous) {
            return (previous as f64 * self.policy.knee_increase_factor).round() as u64;
        }
        let additive =
            (self.policy.ceiling_bps as f64 * self.policy.additive_increase).max(250_000.0);
        let multiplicative = previous as f64 * self.policy.increase_factor;
        multiplicative.max(previous as f64 + additive).round() as u64
    }

    fn near_remembered_knee(&self, target: u64) -> bool {
        self.knee_repeats > 0 && self.near_any_remembered_knee(target)
    }

    fn near_any_remembered_knee(&self, target: u64) -> bool {
        self.knee_bps.is_some_and(|knee| {
            self.knee_age <= self.policy.knee_expiry
                && within_share(target, knee, self.policy.knee_slow_probe_window)
        })
    }

    fn congestion_event(&mut self, sample: RateSample) -> Option<CongestionEvent> {
        if let Some(path) = sample.path {
            let loss_rate = path.loss_rate();
            let queue_delay = path.queue_delay();
            // Growth, not jitter: a millisecond of RTT noise beside a lost
            // packet is not a queue building.
            let rising_queue = self.last_queue_delay.is_some_and(|previous| {
                queue_delay >= previous.saturating_add(self.policy.clear_queue_delay)
            });
            self.last_queue_delay = Some(queue_delay);
            let queue_congested = queue_delay > self.policy.congested_queue_delay;
            if queue_congested {
                self.queue_delay_streak = self.queue_delay_streak.saturating_add(1);
            } else if queue_delay <= self.policy.clear_queue_delay {
                self.queue_delay_streak = 0;
            }
            let severe_queue_delay = queue_delay
                >= self
                    .policy
                    .congested_queue_delay
                    .saturating_mul(self.policy.severe_queue_delay_factor);
            let severe_loss = loss_rate >= self.policy.congested_loss_rate * 2.0;
            if loss_rate >= self.policy.congested_loss_rate {
                return Some(CongestionEvent {
                    reason: RateChangeReason::LossOrCongestion,
                    severe: severe_loss,
                });
            }
            if queue_congested
                && (severe_queue_delay
                    || self.queue_delay_streak >= self.policy.queue_delay_samples)
            {
                return Some(CongestionEvent {
                    reason: RateChangeReason::QueueDelay,
                    severe: severe_queue_delay,
                });
            }
            // Real links carry a random loss floor (a few tenths of a percent
            // on WiFi and VPN paths) that does not move with the send rate,
            // and QUIC reacts to every such loss with a congestion event. A
            // single lossy interval is therefore not congestion; loss has to
            // persist, or come with a queue that is actually growing.
            let lossy =
                path.congestion_events_delta != 0 && loss_rate >= self.policy.event_loss_rate;
            self.loss_streak = if lossy {
                self.loss_streak.saturating_add(1)
            } else {
                0
            };
            if path.congestion_events_delta != 0
                && (self.loss_streak >= self.policy.queue_delay_samples
                    || (loss_rate >= self.policy.event_loss_rate && rising_queue))
            {
                return Some(CongestionEvent {
                    reason: RateChangeReason::LossOrCongestion,
                    severe: severe_loss,
                });
            }
        } else {
            self.queue_delay_streak = 0;
            self.last_queue_delay = None;
            self.loss_streak = 0;
        }
        if sample.frames != 0 {
            let frame_wait_falling = self
                .last_frame_wait
                .is_some_and(|previous| sample.mean_frame_wait < previous);
            self.last_frame_wait = Some(sample.mean_frame_wait);
            if sample.mean_frame_wait >= self.policy.congested_frame_wait && !frame_wait_falling {
                self.frame_wait_streak = self.frame_wait_streak.saturating_add(1);
                let severe_wait = self
                    .policy
                    .congested_frame_wait
                    .saturating_mul(self.policy.severe_queue_delay_factor);
                if sample.mean_frame_wait >= severe_wait
                    || self.frame_wait_streak >= self.policy.queue_delay_samples
                {
                    return Some(CongestionEvent {
                        reason: RateChangeReason::FrameWait,
                        severe: sample.mean_frame_wait >= severe_wait,
                    });
                }
            } else if sample.mean_frame_wait <= self.policy.clear_frame_wait || frame_wait_falling {
                self.frame_wait_streak = 0;
            }
        }
        None
    }

    fn path_is_clear(&self, sample: RateSample) -> bool {
        let transport_clear = sample.path.is_none_or(|path| {
            path.loss_rate() < self.policy.congested_loss_rate
                && path.queue_delay() <= self.policy.congested_queue_delay
        });
        let frames_clear =
            sample.frames == 0 || sample.mean_frame_wait <= self.policy.clear_frame_wait;
        transport_clear && frames_clear
    }

    fn path_resets_knee(&self, sample: RateSample) -> bool {
        sample.path.is_none_or(|path| {
            path.loss_rate() < self.policy.event_loss_rate
                && path.queue_delay() <= self.policy.clear_queue_delay
        })
    }

    fn decreased_target(&self, sample: RateSample, previous: u64, cause: RateChangeReason) -> u64 {
        let mut target = previous as f64 * self.policy.decrease_factor;
        // Local writer wait means the send window is already backpressuring the
        // host, so bytes popped by the writer understate the path capacity.
        if self.delivered_capacity_is_reliable(sample, cause) {
            let delivered_bps = delivered_bps(sample);
            if delivered_bps.is_finite() && delivered_bps > 0.0 {
                target = target.min(delivered_bps * self.policy.delivered_share);
            }
        }
        let max_cut = previous as f64 * self.policy.max_single_decrease_factor;
        target = target.max(max_cut);
        target.round() as u64
    }

    fn delivered_capacity_is_reliable(&self, sample: RateSample, cause: RateChangeReason) -> bool {
        if sample.frames == 0 || cause == RateChangeReason::FrameWait {
            return false;
        }
        match (cause, sample.path) {
            (RateChangeReason::QueueDelay, Some(path)) => {
                path.queue_delay()
                    >= self
                        .policy
                        .congested_queue_delay
                        .saturating_mul(self.policy.severe_queue_delay_factor)
                    || self.queue_delay_streak >= self.policy.queue_delay_samples
            }
            (RateChangeReason::LossOrCongestion, Some(path)) => {
                path.loss_rate() >= self.policy.congested_loss_rate
            }
            _ => false,
        }
    }

    fn at_bound(&self) -> bool {
        self.target_bps == self.policy.floor_bps || self.target_bps == self.policy.ceiling_bps
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CongestionEvent {
    reason: RateChangeReason,
    severe: bool,
}

/// The frame rate a Detail session runs at for a bitrate target: frames are
/// shed in step with bits so each frame keeps its detail. Never above the
/// session's own rate, never below 8 fps (or the session rate, if lower).
#[must_use]
pub fn detail_framerate(max_fps: u32, start_bps: u64, target_bps: u64) -> u32 {
    let max_fps = max_fps.max(1);
    if start_bps == 0 {
        return max_fps;
    }
    let fps = (f64::from(max_fps) * target_bps as f64 / start_bps as f64).round();
    let fps = if fps.is_finite() && fps > 0.0 {
        fps.min(f64::from(u32::MAX)) as u32
    } else {
        0
    };
    fps.clamp(8.min(max_fps), max_fps)
}

/// What each encoder pipeline has actually accepted, so a controller target
/// reaches every monitor of a session even when one pipeline's control
/// mailbox was momentarily full. The latest target wins; a missed step is
/// re-sent on the next tick rather than lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineRateSync {
    applied_bps: Vec<Option<u64>>,
    applied_fps: Vec<Option<u32>>,
}

/// The result of one [`PipelineRateSync::sync`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineSyncOutcome {
    /// Pipelines that took a new bitrate in this pass.
    pub bitrate_sent: usize,
    /// Pipelines that took a new frame rate in this pass.
    pub framerate_sent: usize,
    /// Pipelines still behind the wanted state after this pass.
    pub pending: usize,
}

impl PipelineRateSync {
    /// Starts with every pipeline at the rates it was launched with.
    #[must_use]
    pub fn new(pipelines: usize, initial_bitrate: u64, initial_framerate: u32) -> Self {
        Self {
            applied_bps: vec![Some(initial_bitrate); pipelines],
            applied_fps: vec![Some(initial_framerate); pipelines],
        }
    }

    /// Sends every pipeline whatever it still lacks of the wanted state.
    /// `send_bitrate`/`send_framerate` return whether the pipeline accepted the value.
    pub fn sync(
        &mut self,
        bitrate: u64,
        framerate: Option<u32>,
        mut send_bitrate: impl FnMut(usize, u64) -> bool,
        mut send_framerate: impl FnMut(usize, u32) -> bool,
    ) -> PipelineSyncOutcome {
        let mut outcome = PipelineSyncOutcome {
            bitrate_sent: 0,
            framerate_sent: 0,
            pending: 0,
        };
        for index in 0..self.applied_bps.len() {
            let mut behind = false;
            // Frame rate first: a capenc sizes its VBV from its current frame
            // rate when a bitrate arrives.
            if let Some(fps) = framerate {
                if self.applied_fps[index] != Some(fps) {
                    if send_framerate(index, fps) {
                        self.applied_fps[index] = Some(fps);
                        outcome.framerate_sent += 1;
                    } else {
                        behind = true;
                    }
                }
            }
            if self.applied_bps[index] != Some(bitrate) {
                if send_bitrate(index, bitrate) {
                    self.applied_bps[index] = Some(bitrate);
                    outcome.bitrate_sent += 1;
                } else {
                    behind = true;
                }
            }
            if behind {
                outcome.pending += 1;
            }
        }
        outcome
    }
}

fn delivered_bps(sample: RateSample) -> f64 {
    let pipelines = u64::from(sample.pipeline_count.max(1));
    sample.delivered_bytes as f64 * 8.0 / sample.elapsed.as_secs_f64() / pipelines as f64
}

fn within_share(value: u64, reference: u64, share: f64) -> bool {
    if reference == 0 {
        return value == 0;
    }
    ((value as f64 - reference as f64).abs() / reference as f64) <= share
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_framerate_keeps_bits_per_frame_within_bounds() {
        assert_eq!(detail_framerate(30, 4_000_000, 4_000_000), 30);
        assert_eq!(detail_framerate(30, 4_000_000, 2_000_000), 15);
        assert_eq!(detail_framerate(30, 4_000_000, 100), 8);
        assert_eq!(detail_framerate(30, 4_000_000, 40_000_000), 30);
        assert_eq!(detail_framerate(5, 4_000_000, 100), 5);
        assert_eq!(detail_framerate(0, 0, 1), 1);
        assert_eq!(detail_framerate(60, 1, u64::MAX), 60);
    }

    #[test]
    fn a_missed_pipeline_step_is_resent_until_it_lands() {
        let mut sync = PipelineRateSync::new(2, 4_000_000, 30);
        let mut full = true;
        let mut sent = Vec::new();
        let outcome = sync.sync(
            2_000_000,
            Some(15),
            |index, bps| {
                if index == 1 && full {
                    return false;
                }
                sent.push((index, bps));
                true
            },
            |_, _| true,
        );
        assert_eq!(outcome.pending, 1);
        assert_eq!(sent, vec![(0, 2_000_000)]);
        full = false;
        sent.clear();
        let outcome = sync.sync(
            2_000_000,
            Some(15),
            |index, bps| {
                assert!(!full);
                sent.push((index, bps));
                true
            },
            |_, _| panic!("frame rate already applied"),
        );
        assert_eq!(outcome.pending, 0);
        assert_eq!(sent, vec![(1, 2_000_000)]);
        let outcome = sync.sync(
            2_000_000,
            Some(15),
            |_, _| panic!("in sync"),
            |_, _| panic!(),
        );
        assert_eq!(
            outcome,
            PipelineSyncOutcome {
                bitrate_sent: 0,
                framerate_sent: 0,
                pending: 0
            }
        );
    }

    const START: u64 = 4_665_600;
    const CEILING: u64 = 9_331_200;

    fn path(rtt_ms: u64, baseline_ms: u64, congestion: u64, lost: u64) -> PathSignal {
        path_with_packets(rtt_ms, baseline_ms, congestion, lost, 100)
    }

    fn path_with_packets(
        rtt_ms: u64,
        baseline_ms: u64,
        congestion: u64,
        lost: u64,
        sent: u64,
    ) -> PathSignal {
        PathSignal {
            rtt_micros: rtt_ms * 1_000,
            baseline_rtt_micros: baseline_ms * 1_000,
            congestion_window_bytes: 1_000_000,
            bytes_in_flight: None,
            congestion_events_delta: congestion,
            lost_packets_delta: lost,
            lost_bytes_delta: lost * 1200,
            sent_packets_delta: sent,
        }
    }

    fn sample(delivered_mbps: f64, wait_ms: u64, path: PathSignal) -> RateSample {
        RateSample {
            delivered_bytes: (delivered_mbps * 1_000_000.0 / 8.0) as u64,
            elapsed: Duration::from_secs(1),
            mean_frame_wait: Duration::from_millis(wait_ms),
            frames: 30,
            pipeline_count: 1,
            path: Some(path),
        }
    }

    /// Live Deck-at-home trace (VPN to the lab, 2026-09-27): a path that
    /// carries 10 Mbps of paced UDP at 0.065% loss still shows QUIC a few
    /// tenths of a percent of loss, each with a congestion event and RTT
    /// jitter of a millisecond or two. That must not walk the encoder down to
    /// its floor.
    #[test]
    fn random_loss_floor_with_jitter_does_not_walk_to_the_floor() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        let floor = rate.policy.floor_bps;
        // (lost packets per thousand, queue delay ms) per second, as logged.
        let trace = [
            (3, 1),
            (9, 0),
            (3, 2),
            (2, 1),
            (13, 1),
            (3, 0),
            (2, 0),
            (3, 0),
            (0, 0),
            (12, 0),
            (3, 1),
            (0, 0),
            (8, 0),
            (6, 0),
            (0, 11),
            (0, 0),
            (6, 0),
            (3, 0),
            (3, 0),
            (0, 0),
            (4, 6),
            (0, 5),
            (3, 1),
            (0, 0),
            (2, 2),
            (0, 0),
        ];
        let mut lowest = rate.target_bps();
        for (lost, qd) in trace {
            let target = rate.target_bps() as f64 / 1_000_000.0;
            let path = path_with_packets(33 + qd, 33, u64::from(lost != 0), lost, 1_000);
            rate.observe(sample(target, 2, path));
            lowest = lowest.min(rate.target_bps());
        }
        assert!(
            lowest >= START / 2,
            "a random loss floor cut the encoder to {lowest} bps (floor {floor})"
        );
        assert!(rate.target_bps() >= START, "ended at {}", rate.target_bps());
    }

    #[test]
    fn sustained_loss_is_still_congestion() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        let lossy = path_with_packets(33, 33, 1, 10, 1_000);
        let target = rate.target_bps() as f64 / 1_000_000.0;
        let first = rate.observe(sample(target, 2, lossy));
        assert!(first.is_none_or(|change| change.reason != RateChangeReason::LossOrCongestion));
        let before = rate.target_bps();
        let change = rate
            .observe(sample(target, 2, lossy))
            .expect("two lossy intervals cut");
        assert_eq!(change.reason, RateChangeReason::LossOrCongestion);
        assert!(change.target_bps < before);
    }

    #[test]
    fn loss_with_a_growing_queue_cuts_at_once() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        let target = rate.target_bps() as f64 / 1_000_000.0;
        rate.observe(sample(target, 2, path_with_packets(34, 33, 0, 0, 1_000)));
        let change = rate
            .observe(sample(target, 2, path_with_packets(44, 33, 1, 10, 1_000)))
            .expect("loss while the queue grows is congestion");
        assert_eq!(change.reason, RateChangeReason::LossOrCongestion);
    }

    #[test]
    fn lan_trace_climbs_to_the_ceiling_within_12_seconds() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        let mut reached = None;
        for second in 1..=12 {
            let _ = rate.observe(sample(1000.0, 1, path(1, 1, 0, 0)));
            if rate.target_bps() == CEILING {
                reached = Some(second);
                break;
            }
        }
        assert_eq!(reached, Some(10));
    }

    #[test]
    fn vpn_trace_settles_below_the_six_mbit_knee_without_wild_oscillation() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            22_900_000,
            MotionPriority::Motion,
        ));
        let mut min_after_warmup = u64::MAX;
        let mut max_after_warmup = 0;
        for second in 0..45 {
            let over = rate.target_bps() > 6_000_000;
            let rtt = if over { 78 } else { 48 };
            let delivered = if over {
                6.0
            } else {
                rate.target_bps() as f64 / 1_000_000.0
            };
            let _ = rate.observe(sample(
                delivered,
                if over { 10 } else { 2 },
                path(rtt, 45, 0, 0),
            ));
            if second >= 20 {
                min_after_warmup = min_after_warmup.min(rate.target_bps());
                max_after_warmup = max_after_warmup.max(rate.target_bps());
            }
        }
        assert!(max_after_warmup <= 6_300_000, "max {max_after_warmup}");
        assert!(min_after_warmup >= 4_000_000, "min {min_after_warmup}");
    }

    #[test]
    fn five_g_dip_backs_off_within_two_seconds_and_recovers() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        for _ in 0..10 {
            let _ = rate.observe(sample(100.0, 1, path(25, 25, 0, 0)));
        }
        assert_eq!(rate.target_bps(), CEILING);
        let before = rate.target_bps();
        let _ = rate.observe(sample(3.0, 2, path(70, 25, 0, 0)));
        let after_one = rate.target_bps();
        let _ = rate.observe(sample(3.0, 2, path(85, 25, 1, 1)));
        let after_two = rate.target_bps();
        assert!(after_one < before);
        assert!(after_two <= 3_000_000, "after_two {after_two}");
        for _ in 0..22 {
            let _ = rate.observe(sample(100.0, 1, path(25, 25, 0, 0)));
        }
        assert_eq!(rate.target_bps(), CEILING);
    }

    #[test]
    fn random_wifi_loss_below_threshold_still_reaches_near_ceiling() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        for _ in 0..12 {
            let change = rate.observe(sample(1000.0, 1, path_with_packets(2, 2, 1, 3, 1_000)));
            assert!(
                !matches!(
                    change.map(|change| change.reason),
                    Some(RateChangeReason::LossOrCongestion)
                ),
                "0.3% random loss must not be treated as congestion"
            );
        }
        assert!(
            rate.target_bps() >= CEILING * 90 / 100,
            "target {} did not reach 90% of ceiling {}",
            rate.target_bps(),
            CEILING
        );
    }

    #[test]
    fn queue_jitter_needs_persistence_before_cutting() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        let trace = [30, 42, 31, 41, 29, 40, 30, 42, 31, 39, 30, 41, 29, 40, 30];
        let mut cuts = 0;
        for rtt in trace {
            if let Some(change) = rate.observe(sample(1000.0, 1, path(rtt, 30, 0, 0)))
                && matches!(change.reason, RateChangeReason::QueueDelay)
            {
                cuts += 1;
            }
        }
        assert!(cuts <= 1, "jitter caused {cuts} queue-delay cuts");
        assert!(
            rate.target_bps() >= CEILING * 80 / 100,
            "target {} did not reach 80% of ceiling {}",
            rate.target_bps(),
            CEILING
        );
    }

    #[test]
    fn baseline_step_recovers_after_window_expiry_and_climb() {
        use arcen_telemetry::{PathSignalCounters, PathSignalState};

        let mut path_state = PathSignalState::new(Duration::from_secs(10));
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        for second in 0..10 {
            let signal = path_state.observe(
                Duration::from_secs(second),
                PathSignalCounters {
                    rtt: Duration::from_millis(30),
                    congestion_window_bytes: 1_000_000,
                    bytes_in_flight: None,
                    congestion_events: 0,
                    lost_packets: 0,
                    lost_bytes: 0,
                    sent_packets: second * 100,
                },
            );
            let _ = rate.observe(sample(1000.0, 1, signal));
        }
        assert_eq!(rate.target_bps(), CEILING);
        let mut recovered = None;
        for step in 0..55 {
            let second = 10 + step;
            let signal = path_state.observe(
                Duration::from_secs(second),
                PathSignalCounters {
                    rtt: Duration::from_millis(60),
                    congestion_window_bytes: 1_000_000,
                    bytes_in_flight: None,
                    congestion_events: 0,
                    lost_packets: 0,
                    lost_bytes: 0,
                    sent_packets: second * 100,
                },
            );
            let _ = rate.observe(sample(1000.0, 1, signal));
            if step > 10 && rate.target_bps() == CEILING {
                recovered = Some(step);
                break;
            }
        }
        assert!(
            recovered.is_some_and(|seconds| seconds <= 50),
            "did not recover within baseline window plus climb: {recovered:?}"
        );
    }

    #[test]
    fn three_mbit_bottleneck_learns_the_knee_without_sawtooth() {
        let capacity_bps = 3_000_000_u64;
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        let mut cuts_after_convergence = 0_u32;
        let mut queue_sum_ms = 0_u64;
        let mut target_sum = 0_u64;
        let mut samples = 0_u64;
        for second in 0..90 {
            let target = rate.target_bps();
            let excess = target.saturating_sub(capacity_bps) as f64 / capacity_bps as f64;
            let queue_ms = (excess * 100.0).round().max(0.0) as u64;
            let delivered_mbps = target.min(capacity_bps) as f64 / 1_000_000.0;
            let change = rate.observe(sample(delivered_mbps, 1, path(42 + queue_ms, 42, 0, 0)));
            if second >= 30 {
                if matches!(
                    change.map(|change| change.reason),
                    Some(RateChangeReason::QueueDelay)
                ) {
                    cuts_after_convergence += 1;
                }
                queue_sum_ms = queue_sum_ms.saturating_add(queue_ms);
                target_sum = target_sum.saturating_add(rate.target_bps());
                samples += 1;
            }
        }
        let mean_queue_ms = queue_sum_ms / samples;
        let mean_target = target_sum / samples;
        assert!(
            cuts_after_convergence <= 2,
            "queue cuts after convergence: {cuts_after_convergence}"
        );
        assert!(mean_queue_ms <= 15, "mean queue delay {mean_queue_ms} ms");
        assert!(
            (capacity_bps * 75 / 100..=capacity_bps).contains(&mean_target),
            "mean target {mean_target} not within 75-100% of {capacity_bps}"
        );
    }

    #[test]
    fn detail_priority_can_reduce_bitrate_below_start_for_frame_shedding() {
        let capacity_bps = 3_000_000_u64;
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Detail,
        ));
        for _ in 0..60 {
            let target = rate.target_bps();
            let excess = target.saturating_sub(capacity_bps) as f64 / capacity_bps as f64;
            let queue_ms = (excess * 100.0).round().max(0.0) as u64;
            let delivered_mbps = target.min(capacity_bps) as f64 / 1_000_000.0;
            let _ = rate.observe(sample(delivered_mbps, 1, path(42 + queue_ms, 42, 0, 0)));
        }
        assert!(rate.target_bps() < START);
        assert!(rate.target_bps() >= capacity_bps * 75 / 100);
    }

    #[test]
    fn sender_side_bottleneck_uses_local_backlog_without_rtt_growth() {
        let capacity_bps = 3_000_000_u64;
        let motion_fps = 60_u64;
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        let mut wait_sum_ms = 0_u64;
        let mut target_sum = 0_u64;
        let mut frame_sum = 0_u64;
        let mut samples = 0_u64;
        for second in 0..90 {
            let target = rate.target_bps();
            let excess = target.saturating_sub(capacity_bps) as f64 / capacity_bps as f64;
            let wait_ms = (excess * 1000.0).round().max(0.0) as u64;
            let delivered_mbps = target.min(capacity_bps) as f64 / 1_000_000.0;
            let _ = rate.observe(RateSample {
                delivered_bytes: (delivered_mbps * 1_000_000.0 / 8.0) as u64,
                elapsed: Duration::from_secs(1),
                mean_frame_wait: Duration::from_millis(wait_ms),
                frames: motion_fps,
                pipeline_count: 1,
                path: Some(path(42, 42, 0, 0)),
            });
            if second >= 30 {
                wait_sum_ms = wait_sum_ms.saturating_add(wait_ms);
                target_sum = target_sum.saturating_add(rate.target_bps());
                frame_sum = frame_sum.saturating_add(motion_fps);
                samples += 1;
            }
        }
        let mean_wait_ms = wait_sum_ms / samples;
        let mean_target = target_sum / samples;
        let mean_fps = frame_sum / samples;
        assert!(mean_fps >= 55, "motion fps {mean_fps}");
        assert!(mean_wait_ms <= 50, "mean wait {mean_wait_ms} ms");
        assert!(
            (2_400_000..=2_800_000).contains(&mean_target),
            "mean target {mean_target}"
        );
    }

    #[test]
    fn detail_sender_bottleneck_lowers_fps_to_preserve_bits_per_frame() {
        let capacity_bps = 3_000_000_u64;
        let max_fps = 60_u32;
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Detail,
        ));
        let mut wait_sum_ms = 0_u64;
        let mut target_sum = 0_u64;
        let mut samples = 0_u64;
        for second in 0..90 {
            let target = rate.target_bps();
            let excess = target.saturating_sub(capacity_bps) as f64 / capacity_bps as f64;
            let wait_ms = (excess * 1000.0).round().max(0.0) as u64;
            let delivered_mbps = target.min(capacity_bps) as f64 / 1_000_000.0;
            let _ = rate.observe(RateSample {
                delivered_bytes: (delivered_mbps * 1_000_000.0 / 8.0) as u64,
                elapsed: Duration::from_secs(1),
                mean_frame_wait: Duration::from_millis(wait_ms),
                frames: 60,
                pipeline_count: 1,
                path: Some(path(42, 42, 0, 0)),
            });
            if second >= 30 {
                wait_sum_ms = wait_sum_ms.saturating_add(wait_ms);
                target_sum = target_sum.saturating_add(rate.target_bps());
                samples += 1;
            }
        }
        let mean_wait_ms = wait_sum_ms / samples;
        let mean_target = target_sum / samples;
        assert!(mean_wait_ms <= 50, "mean wait {mean_wait_ms} ms");
        assert!(
            (2_400_000..=2_800_000).contains(&mean_target),
            "mean target {mean_target}"
        );
        let fps = (f64::from(max_fps) * rate.target_bps() as f64 / START as f64).round() as u32;
        assert!(rate.target_bps() < START);
        assert!(
            fps < max_fps,
            "detail fps {fps} did not fall below {max_fps}"
        );
        let start_bits_per_frame = START / u64::from(max_fps);
        let detail_bits_per_frame = rate.target_bps() / u64::from(fps.max(1));
        assert!(
            detail_bits_per_frame >= start_bits_per_frame * 9 / 10,
            "bits/frame fell too far: {detail_bits_per_frame} vs {start_bits_per_frame}"
        );
    }

    #[test]
    fn static_multi_monitor_startup_trace_does_not_floor_on_app_limited_bytes() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Detail,
        ));

        let trace = [
            (1.2, 194, 194, 0, 0, 1_000),
            (1.2, 39, 39, 0, 0, 1_000),
            (1.2, 52, 40, 0, 0, 1_000),
            (1.2, 34, 34, 0, 0, 1_000),
            (1.2, 33, 33, 0, 3, 1_000),
            (1.2, 33, 33, 1, 5, 1_000),
        ];
        let mut floor_seen = false;
        for (delivered_mbps, rtt, baseline, congestion, lost, sent) in trace {
            let _ = rate.observe(sample(
                delivered_mbps,
                1,
                path_with_packets(rtt, baseline, congestion, lost, sent),
            ));
            floor_seen |= rate.target_bps() == rate.policy.floor_bps;
        }

        assert!(
            !floor_seen,
            "app-limited startup trace reached the floor at {}",
            rate.target_bps()
        );
        assert!(
            rate.target_bps() >= START / 2,
            "gentle loss/event cut was too deep: {}",
            rate.target_bps()
        );

        let mut recovered = None;
        for second in 1..=10 {
            let _ = rate.observe(sample(1.2, 1, path_with_packets(45, 33, 0, 0, 1_000)));
            if rate.target_bps() >= START {
                recovered = Some(second);
                break;
            }
        }
        assert!(
            recovered.is_some(),
            "dead-band path did not probe back to start: {}",
            rate.target_bps()
        );
    }

    #[test]
    fn one_interval_cannot_cut_more_than_half() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        for _ in 0..10 {
            let _ = rate.observe(sample(100.0, 1, path(25, 25, 0, 0)));
        }
        assert_eq!(rate.target_bps(), CEILING);

        let change = rate
            .observe(sample(0.5, 1, path(100, 25, 0, 0)))
            .expect("severe queue cuts");
        assert!(matches!(change.reason, RateChangeReason::QueueDelay));
        assert!(
            change.target_bps >= change.previous_bps / 2,
            "{} -> {} cut too far",
            change.previous_bps,
            change.target_bps
        );
    }

    #[test]
    fn aggregate_multi_pipeline_delivery_is_normalized_before_capacity_cut() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Motion,
        ));
        let change = rate
            .observe(RateSample {
                delivered_bytes: 6_000_000 / 8,
                elapsed: Duration::from_secs(1),
                mean_frame_wait: Duration::from_millis(1),
                frames: 60,
                pipeline_count: 2,
                path: Some(path(100, 40, 0, 0)),
            })
            .expect("severe queue cuts");
        assert!(
            (2_300_000..=2_800_000).contains(&change.target_bps),
            "target {} was not normalized to per-pipeline capacity",
            change.target_bps
        );
    }

    #[test]
    fn mild_dead_band_allows_slow_probe_after_a_cut() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Detail,
        ));
        let _ = rate.observe(sample(3.0, 1, path(90, 40, 0, 0)));
        let after_cut = rate.target_bps();

        let mut recovered = None;
        for second in 1..=10 {
            let _ = rate.observe(sample(1.2, 1, path_with_packets(45, 33, 0, 5, 1_000)));
            if rate.target_bps() > after_cut {
                recovered = Some(second);
                break;
            }
        }
        assert!(
            recovered.is_some(),
            "loss/queue dead band did not allow probing from {after_cut}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::items_after_statements)]
    fn vpn_detail_trace_uses_one_cut_per_congestion_epoch() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Detail,
        ));
        rate.target_bps = 8_489_451;

        #[derive(Clone, Copy)]
        struct TraceSecond {
            delivered_mbps: f64,
            qd_ms: u64,
            loss_per_mille: u64,
            congestion: u64,
            wait_ms: u64,
        }

        let trace = [
            TraceSecond {
                delivered_mbps: 8.5,
                qd_ms: 1,
                loss_per_mille: 5,
                congestion: 1,
                wait_ms: 1,
            },
            TraceSecond {
                delivered_mbps: 9.2,
                qd_ms: 3,
                loss_per_mille: 6,
                congestion: 1,
                wait_ms: 1,
            },
            TraceSecond {
                delivered_mbps: 6.6,
                qd_ms: 237,
                loss_per_mille: 0,
                congestion: 0,
                wait_ms: 1,
            },
            TraceSecond {
                delivered_mbps: 4.8,
                qd_ms: 1,
                loss_per_mille: 6,
                congestion: 1,
                wait_ms: 120,
            },
            TraceSecond {
                delivered_mbps: 4.8,
                qd_ms: 1,
                loss_per_mille: 7,
                congestion: 1,
                wait_ms: 80,
            },
            TraceSecond {
                delivered_mbps: 4.8,
                qd_ms: 19,
                loss_per_mille: 13,
                congestion: 1,
                wait_ms: 70,
            },
            TraceSecond {
                delivered_mbps: 3.4,
                qd_ms: 23,
                loss_per_mille: 2,
                congestion: 0,
                wait_ms: 90,
            },
            TraceSecond {
                delivered_mbps: 3.4,
                qd_ms: 1,
                loss_per_mille: 0,
                congestion: 0,
                wait_ms: 160,
            },
            TraceSecond {
                delivered_mbps: 3.4,
                qd_ms: 0,
                loss_per_mille: 0,
                congestion: 0,
                wait_ms: 100,
            },
            TraceSecond {
                delivered_mbps: 3.4,
                qd_ms: 0,
                loss_per_mille: 0,
                congestion: 0,
                wait_ms: 60,
            },
        ];

        let mut below_min_fps_intervals = 0_u32;
        let mut lowest = rate.target_bps();
        for sample_second in trace {
            let signal = path_with_packets(
                33 + sample_second.qd_ms,
                33,
                sample_second.congestion,
                sample_second.loss_per_mille,
                1_000,
            );
            let _ = rate.observe(RateSample {
                delivered_bytes: (sample_second.delivered_mbps * 1_000_000.0 / 8.0) as u64,
                elapsed: Duration::from_secs(1),
                mean_frame_wait: Duration::from_millis(sample_second.wait_ms),
                frames: 30,
                pipeline_count: 1,
                path: Some(signal),
            });
            lowest = lowest.min(rate.target_bps());
            if detail_framerate(30, START, rate.target_bps()) < 15 {
                below_min_fps_intervals += 1;
            }
        }

        assert!(
            lowest >= 2_400_000,
            "congestion epoch compounded down to {lowest}"
        );
        assert!(
            below_min_fps_intervals <= 2,
            "Detail stayed below 15 fps for {below_min_fps_intervals} intervals"
        );
    }

    #[test]
    fn falling_frame_wait_after_a_cut_is_backlog_drain_not_new_congestion() {
        let mut rate = RateController::new(RateControlPolicy::for_bounds_and_priority(
            START,
            CEILING,
            MotionPriority::Detail,
        ));
        let _ = rate.observe(sample(4.0, 1, path(90, 40, 0, 0)));
        let after_cut = rate.target_bps();

        for wait_ms in [140, 120, 90, 70] {
            let _ = rate.observe(RateSample {
                delivered_bytes: 3_000_000 / 8,
                elapsed: Duration::from_secs(1),
                mean_frame_wait: Duration::from_millis(wait_ms),
                frames: 30,
                pipeline_count: 1,
                path: Some(path(41, 40, 0, 0)),
            });
        }

        assert!(
            rate.target_bps() >= after_cut,
            "falling backlog wait caused another cut: {after_cut} -> {}",
            rate.target_bps()
        );
    }

    #[test]
    fn it_never_leaves_its_bounds() {
        let policy =
            RateControlPolicy::for_bounds_and_priority(START, CEILING, MotionPriority::Motion);
        let mut rate = RateController::new(policy);
        for _ in 0..50 {
            let _ = rate.observe(sample(0.01, 900, path(200, 20, 1, 1)));
        }
        assert_eq!(rate.target_bps(), policy.floor_bps);
        for _ in 0..50 {
            let _ = rate.observe(sample(1000.0, 1, path(1, 1, 0, 0)));
        }
        assert_eq!(rate.target_bps(), policy.ceiling_bps);
    }
}
