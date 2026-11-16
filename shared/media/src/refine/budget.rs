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

    /// Sets the rate from `now` on (tokens earned so far are kept).
    pub fn set_rate(&mut self, rate_bps: u64, now: Duration) {
        self.refill(now);
        self.rate_bps = rate_bps;
    }

    /// Current rate.
    #[must_use]
    pub const fn rate_bps(&self) -> u64 {
        self.rate_bps
    }

    /// Holds refinement (the link budget asked it to yield): the bucket is
    /// frozen and emptied until `until`, or until [`Self::release`].
    pub fn hold_until(&mut self, now: Duration, until: Duration) {
        self.refill(now);
        self.suspended_until = Some(until.max(now));
        self.tokens = 0;
    }

    /// Ends a hold early (the link budget resumed refinement).
    pub fn release(&mut self, now: Duration) {
        self.refill(now);
        self.suspended_until = None;
        self.last = now;
    }
}

/// Where the refinement rate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineRateSource {
    /// No path sample yet: the session's video bitrate target, the only
    /// figure known before the first window.
    Floor,
    /// The measured residual of ADR 0016 R6.
    Residual,
}

impl RefineRateSource {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Floor => "floor",
            Self::Residual => "residual",
        }
    }
}

/// Why refinement is held (ADR 0016 R6 suspension rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineHold {
    /// A QUIC congestion event with loss in the window.
    Congestion,
    /// Loss above [`SUSPEND_LOSS_RATIO`].
    Loss,
    /// The video waited for the writer: motion wins.
    VideoBacklog,
    /// The video's frame age rose.
    FrameAge,
    /// Queueing rose while refinement was sending: refinement pauses for
    /// one window to learn whether it caused it.
    QueueDelayProbe,
}

impl RefineHold {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Congestion => "congestion",
            Self::Loss => "loss",
            Self::VideoBacklog => "video_backlog",
            Self::FrameAge => "frame_age",
            Self::QueueDelayProbe => "queue_delay_probe",
        }
    }
}

/// Loss that makes a QUIC congestion event count as congestion (the video
/// controller's `event_loss_rate`): a random-loss floor fires an event for
/// every lost packet and is not congestion.
pub const CONGESTION_EVENT_LOSS_RATIO: f64 = 0.005;
/// Queueing above the RTT baseline that, while refinement sends, is probed
/// (the video controller's `congested_queue_delay`).
pub const PROBE_QUEUE_DELAY: Duration = Duration::from_millis(15);
/// Queueing at or below this is a clear path.
pub const CLEAR_QUEUE_DELAY: Duration = Duration::from_millis(5);
/// Mean time a video frame waited for the writer that counts as a backlog.
pub const BACKLOG_FRAME_WAIT: Duration = Duration::from_millis(20);
/// Rise of the mean frame wait between windows that counts as rising
/// frame age (above [`FRAME_AGE_FLOOR`]).
pub const FRAME_AGE_RISE: Duration = Duration::from_millis(5);
/// Frame wait below which a rise is noise.
pub const FRAME_AGE_FLOOR: Duration = Duration::from_millis(10);
/// Refinement bytes in a window that make it "sending" (a digest or a
/// stray tile is not).
pub const REFINE_ACTIVE_BYTES: u64 = 16 * 1024;
/// The congestion window holds this many bandwidth-delay products in a
/// BBR cruise (quinn's `cwnd_gain`), so `cwnd / rtt / gain` estimates what
/// the path delivers. For a loss-based controller it under-estimates by
/// the same factor, the safe side.
pub const CWND_GAIN: u64 = 2;
/// A rate never grows by more than this factor in one window.
pub const MAX_STEP_UP: u64 = 2;
/// Queueing at this multiple of [`PROBE_QUEUE_DELAY`] is probed at once;
/// below it, only queueing that lasts two windows is (the video
/// controller's own `severe_queue_delay_factor` and `queue_delay_samples`).
pub const SEVERE_QUEUE_DELAY_FACTOR: u32 = 3;
/// Lowest rate an AIMD back-off leaves.
pub const MIN_LINK_RATE_BPS: u64 = 500_000;

/// One window of what the Pier measured (the shared sampler's input).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RefineLinkWindow {
    /// How long the window was.
    pub elapsed: Duration,
    /// The QUIC path over the window, `None` until a sample exists.
    pub path: Option<arcen_telemetry::PathSignal>,
    /// Interactive bytes the Pier wrote (video, audio, input, cursor).
    pub interactive_bytes: u64,
    /// Refinement bytes the Pier handed to the refine lane.
    pub refine_bytes: u64,
    /// The video queue held a frame the writer had not taken.
    pub video_backlog: bool,
    /// Mean time video frames waited for the writer.
    pub video_frame_wait: Duration,
}

/// What a window decided.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RefineRateDecision {
    /// The refinement rate from now on (0 while held).
    pub rate_bps: u64,
    /// Where it came from.
    pub source: RefineRateSource,
    /// Why refinement is held, if it is.
    pub hold: Option<RefineHold>,
    /// The residual's inputs.
    pub sample: PathSample,
    /// Refinement sent in this window and pauses for the next one: queueing
    /// in it may be refinement's own, so the video controller waits one
    /// window for the answer (`RateHints::refine_active`). A window whose
    /// refinement goes on (a clear path, or queueing the probe already
    /// found to be the path's own) never defers a video cut.
    pub refine_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DelayProbe {
    /// No unexplained queueing.
    Idle,
    /// Refinement paused for one window at this queue delay.
    Probing { before: Duration },
    /// The probe showed this queueing is the path's own; only a further
    /// rise while refinement sends is probed again.
    PathOwned { level: Duration },
}

/// The refinement budget of one Pier session (ADR 0016 R6): a rate from the
/// link's measured residual, never from the video's own target, with the
/// suspension rules and a one-window probe before queueing is blamed on
/// refinement. Pure: the Pier feeds one [`RefineLinkWindow`] per window.
#[derive(Debug, Clone)]
pub struct RefineLinkBudget {
    max_mbps: u32,
    session_cap_bps: u64,
    rate_bps: u64,
    limit_bps: u64,
    now: Duration,
    hold_until: Option<Duration>,
    hold_reason: Option<RefineHold>,
    last_frame_wait: Option<Duration>,
    last_queue_delay: Duration,
    probe: DelayProbe,
    holds: u64,
    backoffs: u64,
    /// A back-off was taken in this window: the limit does not grow in it.
    backed_off: bool,
}

impl RefineLinkBudget {
    /// A budget for a session whose ceiling is `max_mbps` (and
    /// `session_cap_bps` across its monitors).
    #[must_use]
    pub const fn new(max_mbps: u32, session_cap_bps: u64) -> Self {
        Self {
            max_mbps,
            session_cap_bps,
            rate_bps: 0,
            limit_bps: u64::MAX,
            now: Duration::ZERO,
            hold_until: None,
            hold_reason: None,
            last_frame_wait: None,
            last_queue_delay: Duration::ZERO,
            probe: DelayProbe::Idle,
            holds: 0,
            backoffs: 0,
            backed_off: false,
        }
    }

    fn ceiling(&self) -> u64 {
        (u64::from(self.max_mbps) * 1_000_000).min(self.session_cap_bps)
    }

    /// Holds started so far.
    #[must_use]
    pub const fn holds(&self) -> u64 {
        self.holds
    }

    /// Back-offs taken because refinement's own queueing was proven.
    #[must_use]
    pub const fn backoffs(&self) -> u64 {
        self.backoffs
    }

    /// The path's delivery estimate: what the window carried, or what the
    /// congestion window says the path delivers, whichever is larger.
    fn delivered_bps(window: &RefineLinkWindow, path: &arcen_telemetry::PathSignal) -> u64 {
        let secs = window.elapsed.as_secs_f64().max(1e-3);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        let measured =
            ((window.interactive_bytes + window.refine_bytes) as f64 * 8.0 / secs) as u64;
        let rtt = path.rtt_micros.max(1);
        let from_window = u128::from(path.congestion_window_bytes) * 8 * 1_000_000
            / u128::from(rtt)
            / u128::from(CWND_GAIN);
        measured.max(u64::try_from(from_window).unwrap_or(u64::MAX))
    }

    /// Takes one window; returns the rate refinement may use from now on.
    /// `video_target_bps` is the floor used while no path sample exists.
    pub fn observe(
        &mut self,
        window: &RefineLinkWindow,
        video_target_bps: u64,
    ) -> RefineRateDecision {
        self.now = self.now.saturating_add(window.elapsed);
        let refine_active = window.refine_bytes >= REFINE_ACTIVE_BYTES;
        let Some(path) = window.path else {
            let rate = video_target_bps.min(self.ceiling());
            self.rate_bps = rate;
            return RefineRateDecision {
                rate_bps: rate,
                source: RefineRateSource::Floor,
                hold: None,
                sample: PathSample {
                    video_backlog: window.video_backlog,
                    ..PathSample::default()
                },
                refine_active,
            };
        };
        let secs = window.elapsed.as_secs_f64().max(1e-3);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        let interactive_bps = (window.interactive_bytes as f64 * 8.0 / secs) as u64;
        let frame_wait = window.video_frame_wait;
        let frame_age_rising = frame_wait >= FRAME_AGE_FLOOR
            && self
                .last_frame_wait
                .is_some_and(|last| frame_wait >= last.saturating_add(FRAME_AGE_RISE));
        self.last_frame_wait = Some(frame_wait);
        let loss_ratio = path.loss_rate();
        let sample = PathSample {
            delivered_bps: Self::delivered_bps(window, &path),
            interactive_bps,
            congestion_events: path.congestion_events_delta,
            loss_ratio,
            video_backlog: window.video_backlog || frame_wait >= BACKLOG_FRAME_WAIT,
            frame_age_rising,
        };
        let hold = self.choose_hold(&sample, &path, refine_active);
        if let Some(reason) = hold {
            // Only refinement's own sending is backed off: a congested
            // session start (keyframes in slow start) is not its doing.
            if refine_active && matches!(reason, RefineHold::Congestion | RefineHold::Loss) {
                self.back_off();
            }
            if !self.is_held() {
                self.holds += 1;
            }
            self.hold_until = Some(self.now.saturating_add(RESUME_AFTER));
            self.hold_reason = Some(reason);
        }
        let residual = residual_rate_bps(self.max_mbps, self.session_cap_bps, &sample);
        if self.is_held() {
            return RefineRateDecision {
                rate_bps: 0,
                source: RefineRateSource::Residual,
                hold: self.hold_reason,
                sample,
                refine_active,
            };
        }
        // Refinement goes on: its sending is no reason for the video to wait.
        let refine_active = false;
        self.hold_reason = None;
        if hold.is_none() && !std::mem::take(&mut self.backed_off) && self.limit_bps != u64::MAX {
            self.limit_bps = self.limit_bps.saturating_add(self.limit_bps / 2);
            if self.limit_bps >= self.ceiling() {
                self.limit_bps = u64::MAX;
            }
        }
        // The first residual ramps up from the floor, not from nothing.
        let start = if self.rate_bps == 0 {
            video_target_bps.min(self.ceiling())
        } else {
            self.rate_bps
        }
        .max(MIN_LINK_RATE_BPS);
        self.rate_bps = residual
            .min(self.limit_bps)
            .min(start.saturating_mul(MAX_STEP_UP));
        RefineRateDecision {
            rate_bps: self.rate_bps,
            source: RefineRateSource::Residual,
            hold: None,
            sample,
            refine_active,
        }
    }

    /// The R6 suspension rule that applies to this window, if any; starts
    /// a queue-delay probe when that is the reason.
    fn choose_hold(
        &mut self,
        sample: &PathSample,
        path: &arcen_telemetry::PathSignal,
        refine_active: bool,
    ) -> Option<RefineHold> {
        let queue_delay = path.queue_delay();
        let probe_threshold = self.settle_probe(queue_delay);
        let lasted = self.last_queue_delay > probe_threshold
            || queue_delay >= probe_threshold.saturating_mul(SEVERE_QUEUE_DELAY_FACTOR);
        self.last_queue_delay = queue_delay;
        if sample.congestion_events > 0 && sample.loss_ratio >= CONGESTION_EVENT_LOSS_RATIO {
            Some(RefineHold::Congestion)
        } else if sample.loss_ratio > SUSPEND_LOSS_RATIO && path.lost_packets_delta >= 2 {
            Some(RefineHold::Loss)
        } else if sample.video_backlog {
            Some(RefineHold::VideoBacklog)
        } else if sample.frame_age_rising {
            Some(RefineHold::FrameAge)
        } else if refine_active && queue_delay > probe_threshold && lasted {
            self.probe = DelayProbe::Probing {
                before: queue_delay,
            };
            Some(RefineHold::QueueDelayProbe)
        } else {
            None
        }
    }

    /// Halves the rate and caps it there: refinement resumes from half its
    /// rate, and the limit grows again only from the next clear window.
    fn back_off(&mut self) {
        self.limit_bps = (self.rate_bps / 2).max(MIN_LINK_RATE_BPS);
        self.rate_bps = self.limit_bps;
        self.backoffs += 1;
        self.backed_off = true;
    }

    /// Takes the queue-delay probe's verdict (in the window refinement was
    /// held for it) and returns the queueing that starts the next probe.
    fn settle_probe(&mut self, queue_delay: Duration) -> Duration {
        match self.probe {
            DelayProbe::Probing { before } => {
                if queue_delay <= CLEAR_QUEUE_DELAY.max(before / 2) {
                    // Pausing refinement cleared the queue: it was refinement's.
                    self.back_off();
                    self.probe = DelayProbe::Idle;
                } else {
                    self.probe = DelayProbe::PathOwned { level: queue_delay };
                }
            }
            DelayProbe::PathOwned { level } => {
                if queue_delay <= CLEAR_QUEUE_DELAY {
                    self.probe = DelayProbe::Idle;
                } else if queue_delay < level {
                    self.probe = DelayProbe::PathOwned { level: queue_delay };
                }
            }
            DelayProbe::Idle => {}
        }
        match self.probe {
            DelayProbe::PathOwned { level } => level.saturating_add(PROBE_QUEUE_DELAY),
            _ => PROBE_QUEUE_DELAY,
        }
    }

    fn is_held(&self) -> bool {
        self.hold_until.is_some_and(|until| self.now < until)
    }
}

impl RefineRateDecision {
    /// The capenc control line that applies this decision
    /// (`REFINE-RATE <bps> [HOLD]`). `None` for the floor: capenc already
    /// follows the video target until the first link rate.
    #[must_use]
    pub const fn capenc_command(&self) -> Option<crate::capenc_control::CapencControlCommand> {
        match self.source {
            RefineRateSource::Floor => None,
            RefineRateSource::Residual => {
                Some(crate::capenc_control::CapencControlCommand::RefineRate {
                    bps: self.rate_bps,
                    hold: self.hold.is_some(),
                })
            }
        }
    }
}

/// One window's outcome from [`RefineBudgetSampler::window`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RefineBudgetTick {
    /// The decision.
    pub decision: RefineRateDecision,
    /// It differs enough from the previous one to log at info level: the
    /// source or hold changed, or the rate moved by a fifth or more.
    pub changed: bool,
}

/// The shared sampler every Pier feeds once per window (ADR 0016 R6): it
/// turns running counters into a [`RefineLinkWindow`], keeps the
/// [`RefineLinkBudget`], and counts the video controller's deferrals for
/// the telemetry.
#[derive(Debug, Clone)]
pub struct RefineBudgetSampler {
    budget: RefineLinkBudget,
    refine_bytes: Option<u64>,
    last: Option<RefineRateDecision>,
    video_deferrals: u64,
}

/// What a Pier knows at the end of a window.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RefineBudgetInputs {
    /// How long the window was.
    pub elapsed: Duration,
    /// The QUIC path over the window, `None` without one.
    pub path: Option<arcen_telemetry::PathSignal>,
    /// Interactive bytes (video, audio, input) written in the window.
    pub interactive_bytes: u64,
    /// Refinement bytes handed to the refine lane so far (a running count,
    /// `arcen_transport::RefineLaneMeter::written_bytes`).
    pub refine_bytes_total: u64,
    /// Video waited for its writer at the end of the window.
    pub video_backlog: bool,
    /// Mean time video frames waited for the writer in the window.
    pub video_frame_wait: Duration,
    /// The video's current bitrate target (the floor without a path).
    pub video_target_bps: u64,
}

impl RefineBudgetSampler {
    /// A sampler for a session whose refinement ceiling is `max_mbps`.
    #[must_use]
    pub const fn new(max_mbps: u32) -> Self {
        Self {
            budget: RefineLinkBudget::new(max_mbps, u64::MAX),
            refine_bytes: None,
            last: None,
            video_deferrals: 0,
        }
    }

    /// Takes one window.
    pub fn window(&mut self, inputs: RefineBudgetInputs) -> RefineBudgetTick {
        let refine_bytes = inputs
            .refine_bytes_total
            .saturating_sub(self.refine_bytes.unwrap_or(inputs.refine_bytes_total));
        self.refine_bytes = Some(inputs.refine_bytes_total);
        let decision = self.budget.observe(
            &RefineLinkWindow {
                elapsed: inputs.elapsed,
                path: inputs.path,
                interactive_bytes: inputs.interactive_bytes,
                refine_bytes,
                video_backlog: inputs.video_backlog,
                video_frame_wait: inputs.video_frame_wait,
            },
            inputs.video_target_bps,
        );
        let changed = self.last.is_none_or(|last| {
            last.source != decision.source
                || last.hold != decision.hold
                || decision.rate_bps.abs_diff(last.rate_bps) >= last.rate_bps / 5
        });
        if changed {
            self.last = Some(decision);
        }
        RefineBudgetTick { decision, changed }
    }

    /// The video controller left a delay signal for refinement to answer
    /// (`RateController::last_refine_deferral`).
    pub fn note_video_deferral(&mut self) {
        self.video_deferrals += 1;
    }

    /// Video-controller deferrals so far.
    #[must_use]
    pub const fn video_deferrals(&self) -> u64 {
        self.video_deferrals
    }

    /// Holds started so far.
    #[must_use]
    pub const fn holds(&self) -> u64 {
        self.budget.holds()
    }

    /// Back-offs taken so far.
    #[must_use]
    pub const fn backoffs(&self) -> u64 {
        self.budget.backoffs()
    }

    /// The telemetry fields of `tick`, the same on every Pier, for the
    /// `video.refine.budget` event (`arcen_telemetry::names::refine::BUDGET`):
    /// `rate_bps`, `source`, `hold`, `delivered_bps`, `interactive_bps`,
    /// `holds`, `backoffs`, `video_deferrals`.
    #[must_use]
    pub fn summary(&self, tick: &RefineBudgetTick) -> String {
        let decision = &tick.decision;
        format!(
            "rate_bps={} source={} hold={} delivered_bps={} interactive_bps={} holds={} backoffs={} video_deferrals={}",
            decision.rate_bps,
            decision.source.token(),
            decision.hold.map_or("none", RefineHold::token),
            decision.sample.delivered_bps,
            decision.sample.interactive_bps,
            self.holds(),
            self.backoffs(),
            self.video_deferrals,
        )
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

    use crate::rate_control::{
        RateChangeReason, RateControlPolicy, RateController, RateHints, RateSample,
    };
    use arcen_telemetry::PathSignal;

    const SECOND: Duration = Duration::from_secs(1);

    /// A bottleneck of `capacity_bps` with a drop-tail buffer of
    /// `buffer_s` seconds, observed once a second the way a Pier observes
    /// QUIC: smoothed RTT = base + queue, cwnd from the congestion
    /// controller's (possibly stale) bandwidth estimate.
    struct Link {
        capacity_bps: f64,
        base_rtt_ms: f64,
        buffer_s: f64,
        cwnd_for_bps: f64,
        queue_bits: f64,
    }

    impl Link {
        fn new(capacity_mbps: f64, cwnd_for_mbps: f64) -> Self {
            Self {
                capacity_bps: capacity_mbps * 1e6,
                base_rtt_ms: 40.0,
                buffer_s: 0.25,
                cwnd_for_bps: cwnd_for_mbps * 1e6,
                queue_bits: 0.0,
            }
        }

        /// Carries `offered_bps` for one second; returns the path signal at
        /// its end.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        fn carry(&mut self, offered_bps: f64) -> PathSignal {
            let sent_packets = (offered_bps / 8.0 / 1200.0).max(30.0) as u64;
            self.queue_bits = (self.queue_bits + offered_bps - self.capacity_bps).max(0.0);
            let limit = self.capacity_bps * self.buffer_s;
            let dropped_bits = (self.queue_bits - limit).max(0.0);
            self.queue_bits = self.queue_bits.min(limit);
            let lost = (dropped_bits / 8.0 / 1200.0) as u64;
            let queue_ms = self.queue_bits / self.capacity_bps * 1000.0;
            let rtt_ms = self.base_rtt_ms + queue_ms;
            PathSignal {
                rtt_micros: (rtt_ms * 1000.0) as u64,
                baseline_rtt_micros: (self.base_rtt_ms * 1000.0) as u64,
                congestion_window_bytes: (2.0 * self.cwnd_for_bps * self.base_rtt_ms / 1000.0 / 8.0)
                    as u64,
                bytes_in_flight: None,
                congestion_events_delta: u64::from(lost > 0),
                lost_packets_delta: lost,
                lost_bytes_delta: lost * 1200,
                sent_packets_delta: sent_packets,
            }
        }
    }

    struct Run {
        cuts: Vec<(usize, RateChangeReason)>,
        refine_bits: f64,
        holds: Vec<(usize, RefineHold)>,
        deferrals: usize,
    }

    /// Runs the video controller and the refinement budget together over
    /// `link` for `seconds`: the video wants `video_demand_mbps` (capped by
    /// its target), refinement wants as much as its rate allows while
    /// `refine_wants` is true. Each second's decisions apply to the next.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn run(
        link: &mut Link,
        seconds: usize,
        video_demand_mbps: impl Fn(usize) -> f64,
        refine_wants: impl Fn(usize) -> bool,
    ) -> Run {
        let mut video = RateController::new(RateControlPolicy::for_bounds(4_700_000, 25_000_000));
        let mut budget = RefineLinkBudget::new(25, u64::MAX);
        let mut refine_rate = 0.0_f64;
        let mut first = true;
        let mut out = Run {
            cuts: Vec::new(),
            refine_bits: 0.0,
            holds: Vec::new(),
            deferrals: 0,
        };
        for second in 0..seconds {
            let video_bps = (video_demand_mbps(second) * 1e6).min(video.target_bps() as f64);
            let refine_bps = if refine_wants(second) {
                refine_rate
            } else {
                0.0
            };
            let path = link.carry(video_bps + refine_bps);
            out.refine_bits += refine_bps;
            let window = RefineLinkWindow {
                elapsed: SECOND,
                path: (!first).then_some(path),
                interactive_bytes: (video_bps / 8.0) as u64,
                refine_bytes: (refine_bps / 8.0) as u64,
                video_backlog: false,
                video_frame_wait: Duration::from_millis(2),
            };
            first = false;
            let decision = budget.observe(&window, video.target_bps());
            if let Some(hold) = decision.hold {
                out.holds.push((second, hold));
            }
            refine_rate = decision.rate_bps as f64;
            let sample = RateSample {
                delivered_bytes: (video_bps / 8.0) as u64,
                peak_pipeline_delivered_bytes: (video_bps / 8.0) as u64,
                elapsed: SECOND,
                mean_frame_wait: Duration::from_millis(2),
                frames: 30,
                pipeline_count: 1,
                path: Some(path),
            };
            let hints = RateHints {
                refine_active: decision.refine_active,
                ..RateHints::default()
            };
            if let Some(change) = video.observe_with(sample, hints) {
                if change.target_bps < change.previous_bps {
                    out.cuts.push((second, change.reason));
                }
            }
            out.deferrals += usize::from(video.last_refine_deferral().is_some());
        }
        out
    }

    const STILL_VIDEO_MBPS: f64 = 0.1;

    #[test]
    fn refinement_on_a_fixed_capacity_link_never_cuts_the_video() {
        // The congestion window knows the link: refinement takes half the
        // residual and never builds a queue.
        let mut link = Link::new(20.0, 20.0);
        let run = run(&mut link, 30, |_| STILL_VIDEO_MBPS, |_| true);
        assert!(run.cuts.is_empty(), "video cut: {:?}", run.cuts);
        assert!(run.holds.is_empty(), "holds: {:?}", run.holds);
        // Not the video's 4.7 Mbit/s: about half of a 20 Mbit/s link.
        let mean_mbps = run.refine_bits / 30.0 / 1e6;
        assert!(mean_mbps > 8.0, "refinement averaged {mean_mbps:.1} Mbit/s");
    }

    #[test]
    fn refinement_that_overruns_a_link_yields_and_the_video_keeps_its_target() {
        // The link has 8 Mbit/s but the congestion window still remembers
        // 20: refinement overruns it and queues. It must find that out
        // itself, and the video must not be cut for refinement's queue.
        let mut link = Link::new(8.0, 20.0);
        let run = run(&mut link, 40, |_| STILL_VIDEO_MBPS, |_| true);
        assert!(run.cuts.is_empty(), "video cut: {:?}", run.cuts);
        assert!(
            run.holds
                .iter()
                .any(|(_, hold)| *hold == RefineHold::QueueDelayProbe),
            "refinement never probed: {:?}",
            run.holds
        );
        assert!(run.deferrals > 0);
        let mean_mbps = run.refine_bits / 40.0 / 1e6;
        assert!(mean_mbps > 3.0, "refinement averaged {mean_mbps:.1} Mbit/s");
    }

    #[test]
    fn real_congestion_with_refinement_idle_still_cuts_the_video() {
        // Motion on a link that cannot carry it; nothing to refine.
        let mut link = Link::new(3.0, 3.0);
        let run = run(&mut link, 6, |_| 10.0, |_| false);
        let (second, reason) = *run.cuts.first().expect("the video was never cut");
        assert!(second <= 2, "first cut at {second} s");
        assert!(matches!(
            reason,
            RateChangeReason::QueueDelay | RateChangeReason::LossOrCongestion
        ));
        assert_eq!(run.deferrals, 0);
    }

    #[test]
    fn real_congestion_during_refinement_cuts_once_refinement_has_paused() {
        // Motion and refinement on a link that carries neither: refinement
        // pauses, the queue stays, and the video is cut.
        let mut link = Link::new(3.0, 20.0);
        let run = run(&mut link, 8, |_| 6.0, |_| true);
        let (second, _) = *run.cuts.first().expect("the video was never cut");
        assert!(second <= 3, "first cut at {second} s");
    }

    #[test]
    fn motion_holds_refinement_in_the_same_window() {
        let mut budget = RefineLinkBudget::new(25, u64::MAX);
        let path = Link::new(20.0, 20.0).carry(1e6);
        let quiet = RefineLinkWindow {
            elapsed: SECOND,
            path: Some(path),
            interactive_bytes: 10_000,
            refine_bytes: 500_000,
            video_backlog: false,
            video_frame_wait: Duration::from_millis(2),
        };
        assert!(budget.observe(&quiet, 4_700_000).rate_bps > 0);
        for moving in [
            RefineLinkWindow {
                video_backlog: true,
                ..quiet
            },
            RefineLinkWindow {
                video_frame_wait: BACKLOG_FRAME_WAIT,
                ..quiet
            },
        ] {
            let mut budget = budget.clone();
            let held = budget.observe(&moving, 4_700_000);
            assert_eq!(held.hold, Some(RefineHold::VideoBacklog));
            assert_eq!(held.rate_bps, 0);
            // Held for one quiet window, then back.
            let next = budget.observe(&quiet, 4_700_000);
            assert_eq!(next.hold, None);
            assert!(next.rate_bps > 0);
        }
    }

    #[test]
    fn without_a_path_sample_the_video_target_is_the_floor() {
        let mut budget = RefineLinkBudget::new(25, u64::MAX);
        let window = RefineLinkWindow {
            elapsed: SECOND,
            ..RefineLinkWindow::default()
        };
        let decision = budget.observe(&window, 4_700_000);
        assert_eq!(decision.source, RefineRateSource::Floor);
        assert_eq!(decision.rate_bps, 4_700_000);
        assert_eq!(budget.observe(&window, 90_000_000).rate_bps, 25_000_000);
    }

    #[test]
    fn queueing_that_outlasts_the_probe_is_the_paths_and_costs_refinement_nothing() {
        // A VPN path whose RTT drifts 20 ms above its baseline with no load:
        // refinement pauses once, sees the queue stay, and goes on at its
        // rate without backing off or pausing again at the same level.
        let mut budget = RefineLinkBudget::new(25, u64::MAX);
        let mut drift = Link::new(20.0, 20.0).carry(1e6);
        let window = |path| RefineLinkWindow {
            elapsed: SECOND,
            path: Some(path),
            interactive_bytes: 10_000,
            refine_bytes: 1_000_000,
            video_backlog: false,
            video_frame_wait: Duration::from_millis(2),
        };
        for _ in 0..4 {
            let _ = budget.observe(&window(drift), 4_700_000);
        }
        let before = budget.observe(&window(drift), 4_700_000).rate_bps;
        drift.rtt_micros = drift.baseline_rtt_micros + 20_000;
        // One window of queueing is not probed (the video would not act on
        // it either); the second is.
        assert_eq!(budget.observe(&window(drift), 4_700_000).hold, None);
        assert_eq!(
            budget.observe(&window(drift), 4_700_000).hold,
            Some(RefineHold::QueueDelayProbe)
        );
        let after = budget.observe(&window(drift), 4_700_000);
        assert_eq!(after.hold, None);
        assert!(
            after.rate_bps > before / 2,
            "{} after {before}",
            after.rate_bps
        );
        assert_eq!(budget.backoffs(), 0);
        assert_eq!(budget.observe(&window(drift), 4_700_000).hold, None);
    }

    #[test]
    fn a_hold_freezes_the_bucket_until_released() {
        let mut bucket = RefineBudget::new(8_000_000, MAX_BURST_BYTES, Duration::ZERO);
        assert!(bucket.try_take(10_000, MS * 100));
        bucket.acknowledged(10_000);
        bucket.hold_until(MS * 100, MS * 3_100);
        assert!(!bucket.try_take(1, MS * 2_000), "held");
        bucket.release(MS * 2_000);
        assert!(
            !bucket.try_take(10_000, MS * 2_000),
            "a release starts empty"
        );
        assert!(bucket.try_take(10_000, MS * 2_100));
    }

    #[test]
    fn the_sampler_takes_running_counters_and_says_what_to_tell_capenc() {
        let mut sampler = RefineBudgetSampler::new(25);
        let path = Link::new(20.0, 20.0).carry(1e6);
        let inputs = |total, path| RefineBudgetInputs {
            elapsed: SECOND,
            path,
            interactive_bytes: 10_000,
            refine_bytes_total: total,
            video_backlog: false,
            video_frame_wait: Duration::from_millis(1),
            video_target_bps: 4_700_000,
        };
        let floor = sampler.window(inputs(2_000_000, None));
        assert!(floor.changed);
        assert_eq!(floor.decision.capenc_command(), None);
        let first = sampler.window(inputs(2_000_000, Some(path)));
        // The first running count is the baseline, not a burst.
        assert!(!first.decision.refine_active);
        assert!(matches!(
            first.decision.capenc_command(),
            Some(crate::capenc_control::CapencControlCommand::RefineRate { hold: false, .. })
        ));
        // Sending on a clear path never asks the video to wait.
        let second = sampler.window(inputs(3_000_000, Some(path)));
        assert!(!second.decision.refine_active);
        let mut queued = path;
        queued.rtt_micros += 50_000;
        let probe = sampler.window(inputs(4_000_000, Some(queued)));
        assert_eq!(probe.decision.hold, Some(RefineHold::QueueDelayProbe));
        assert!(probe.decision.refine_active);
    }

    #[test]
    fn congestion_while_refinement_is_idle_holds_but_does_not_back_off() {
        let mut budget = RefineLinkBudget::new(25, u64::MAX);
        let mut lossy = Link::new(20.0, 20.0).carry(1e6);
        lossy.congestion_events_delta = 1;
        lossy.lost_packets_delta = 5;
        lossy.sent_packets_delta = 300;
        let window = RefineLinkWindow {
            elapsed: SECOND,
            path: Some(lossy),
            interactive_bytes: 400_000,
            refine_bytes: 0,
            video_backlog: false,
            video_frame_wait: Duration::from_millis(2),
        };
        assert_eq!(
            budget.observe(&window, 4_700_000).hold,
            Some(RefineHold::Congestion)
        );
        assert_eq!(budget.backoffs(), 0);
        let refining = RefineLinkWindow {
            refine_bytes: 500_000,
            ..window
        };
        let _ = budget.observe(&refining, 4_700_000);
        assert_eq!(budget.backoffs(), 1);
    }

    #[test]
    fn a_back_off_halves_the_rate_refinement_resumes_at() {
        let quiet = Link::new(20.0, 20.0).carry(1e6);
        let window = |path, refine_bytes| RefineLinkWindow {
            elapsed: SECOND,
            path: Some(path),
            interactive_bytes: 10_000,
            refine_bytes,
            video_backlog: false,
            video_frame_wait: Duration::from_millis(2),
        };
        // Congestion while refinement sends.
        let mut budget = RefineLinkBudget::new(25, u64::MAX);
        for _ in 0..5 {
            let _ = budget.observe(&window(quiet, 1_000_000), 4_700_000);
        }
        let before = budget
            .observe(&window(quiet, 1_000_000), 4_700_000)
            .rate_bps;
        let mut lossy = quiet;
        lossy.congestion_events_delta = 1;
        lossy.lost_packets_delta = 6;
        lossy.sent_packets_delta = 600;
        assert_eq!(
            budget
                .observe(&window(lossy, 1_000_000), 4_700_000)
                .rate_bps,
            0
        );
        let resumed = budget.observe(&window(quiet, 0), 4_700_000).rate_bps;
        assert!(resumed <= before / 2, "{resumed} after {before}");
        // Queueing the probe proves to be refinement's.
        let mut budget = RefineLinkBudget::new(25, u64::MAX);
        for _ in 0..5 {
            let _ = budget.observe(&window(quiet, 1_000_000), 4_700_000);
        }
        let before = budget
            .observe(&window(quiet, 1_000_000), 4_700_000)
            .rate_bps;
        let mut queued = quiet;
        queued.rtt_micros = queued.baseline_rtt_micros + 60_000;
        let probe = budget.observe(&window(queued, 1_000_000), 4_700_000);
        assert_eq!(probe.hold, Some(RefineHold::QueueDelayProbe));
        let verdict = budget.observe(&window(quiet, 0), 4_700_000);
        assert_eq!(budget.backoffs(), 1);
        assert!(
            verdict.rate_bps <= before / 2,
            "{} after {before}",
            verdict.rate_bps
        );
        // And it grows again from there.
        assert!(
            budget
                .observe(&window(quiet, 1_000_000), 4_700_000)
                .rate_bps
                > verdict.rate_bps
        );
    }

    #[test]
    fn the_first_residual_ramps_up_from_the_video_target() {
        let mut budget = RefineLinkBudget::new(25, u64::MAX);
        let mut lossy = Link::new(20.0, 20.0).carry(1e6);
        lossy.congestion_events_delta = 1;
        lossy.lost_packets_delta = 6;
        lossy.sent_packets_delta = 600;
        let window = |path| RefineLinkWindow {
            elapsed: SECOND,
            path: Some(path),
            interactive_bytes: 10_000,
            refine_bytes: 0,
            video_backlog: false,
            video_frame_wait: Duration::from_millis(2),
        };
        // A session that starts in slow-start losses, refinement idle.
        assert_eq!(budget.observe(&window(lossy), 4_700_000).rate_bps, 0);
        let first = budget.observe(&window(Link::new(20.0, 20.0).carry(1e6)), 4_700_000);
        assert_eq!(first.rate_bps, 9_400_000);
    }
}
