// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Pure `QoS` assessment and caller-clocked health hysteresis.

use std::error::Error;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

/// One bounded `QoS` observation. `None` means unavailable, never zero.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QosSample {
    /// Caller clock in milliseconds.
    pub timestamp_ms: u64,
    /// Observed frames per second.
    pub fps_actual: Option<u32>,
    /// Requested frames per second.
    pub fps_target: Option<u32>,
    /// Frames per second of changed content the source offered for delivery
    /// in this window, when the pipeline can prove it.
    ///
    /// Damage-driven sources — `ScreenCaptureKit`, Wayland `ScreenCast`, the
    /// Keel-gated Xorg and Windows loops — produce a frame only when something
    /// changed, so a still desktop legitimately sends a handful of frames a
    /// second against a target of thirty or sixty. Frame rate is judged
    /// against the smaller of this and [`Self::fps_target`]: a session that
    /// delivered every change is healthy, one that fell behind the changes is
    /// not. `None` means the pipeline cannot tell an idle source from a slow
    /// one, and frame rate is judged against the target as before.
    ///
    /// Report it only from counters that see changes the pipeline then failed
    /// to deliver (captured, superseded, skipped), never from the frames
    /// actually sent, or a backlog would excuse itself.
    pub fps_content: Option<u32>,
    /// FPS is inside a stream start/resume/recovery grace window.
    pub fps_warmup: bool,
    /// Estimated path bandwidth.
    pub bandwidth_mbps: Option<u32>,
    /// Frames submitted by the host.
    pub frames_sent: Option<u64>,
    /// Frames dropped by the host.
    pub frames_dropped: Option<u64>,
    /// Frames decoded by the client.
    pub frames_decoded: Option<u64>,
    /// Frames presented by the client.
    pub frames_presented: Option<u64>,
    /// Host capture time.
    pub capture_time_ms: Option<u32>,
    /// Host encode time.
    pub encode_time_ms: Option<u32>,
    /// Client decode time.
    pub decode_time_ms: Option<u32>,
    /// Client display time.
    pub display_time_ms: Option<u32>,
    /// Application round-trip time.
    pub rtt_ms: Option<u32>,
    /// Input-to-acknowledgement latency.
    pub input_latency_ms: Option<u32>,
    /// Input events in the sample window.
    pub input_events: Option<u64>,
    /// Consecutive missed health intervals.
    pub heartbeat_misses: Option<u32>,
}

/// Validated health thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "QosTargetsWire", into = "QosTargetsWire")]
pub struct QosTargets {
    fps_degraded_percent: u8,
    fps_critical_percent: u8,
    rtt_degraded_ms: u32,
    rtt_critical_ms: u32,
    drop_degraded_basis_points: u16,
    drop_critical_basis_points: u16,
    input_degraded_ms: u32,
    input_critical_ms: u32,
    heartbeat_critical_misses: u32,
}

impl QosTargets {
    /// Creates a validated target set.
    ///
    /// Drop thresholds are basis points: 50 is 0.5%, 500 is 5%.
    ///
    /// # Errors
    ///
    /// Returns an error unless degraded and critical thresholds are ordered and
    /// all values are within meaningful bounds.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fps_degraded_percent: u8,
        fps_critical_percent: u8,
        rtt_degraded_ms: u32,
        rtt_critical_ms: u32,
        drop_degraded_basis_points: u16,
        drop_critical_basis_points: u16,
        input_degraded_ms: u32,
        input_critical_ms: u32,
        heartbeat_critical_misses: u32,
    ) -> Result<Self, QosTargetError> {
        if fps_degraded_percent > 100
            || fps_critical_percent >= fps_degraded_percent
            || rtt_degraded_ms == 0
            || rtt_degraded_ms >= rtt_critical_ms
            || drop_degraded_basis_points >= drop_critical_basis_points
            || drop_critical_basis_points > 10_000
            || input_degraded_ms == 0
            || input_degraded_ms >= input_critical_ms
            || heartbeat_critical_misses == 0
        {
            return Err(QosTargetError);
        }
        Ok(Self {
            fps_degraded_percent,
            fps_critical_percent,
            rtt_degraded_ms,
            rtt_critical_ms,
            drop_degraded_basis_points,
            drop_critical_basis_points,
            input_degraded_ms,
            input_critical_ms,
            heartbeat_critical_misses,
        })
    }

    /// Percentage of target FPS below which health is degraded.
    #[must_use]
    pub const fn fps_degraded_percent(self) -> u8 {
        self.fps_degraded_percent
    }

    /// Percentage of target FPS below which health is critical.
    #[must_use]
    pub const fn fps_critical_percent(self) -> u8 {
        self.fps_critical_percent
    }

    /// RTT threshold for degraded health.
    #[must_use]
    pub const fn rtt_degraded_ms(self) -> u32 {
        self.rtt_degraded_ms
    }

    /// RTT threshold for critical health.
    #[must_use]
    pub const fn rtt_critical_ms(self) -> u32 {
        self.rtt_critical_ms
    }

    /// Drop-ratio threshold for degraded health, in basis points.
    #[must_use]
    pub const fn drop_degraded_basis_points(self) -> u16 {
        self.drop_degraded_basis_points
    }

    /// Drop-ratio threshold for critical health, in basis points.
    #[must_use]
    pub const fn drop_critical_basis_points(self) -> u16 {
        self.drop_critical_basis_points
    }

    /// Input-latency threshold for degraded health.
    #[must_use]
    pub const fn input_degraded_ms(self) -> u32 {
        self.input_degraded_ms
    }

    /// Input-latency threshold for critical health.
    #[must_use]
    pub const fn input_critical_ms(self) -> u32 {
        self.input_critical_ms
    }

    /// Missed-heartbeat threshold for critical health.
    #[must_use]
    pub const fn heartbeat_critical_misses(self) -> u32 {
        self.heartbeat_critical_misses
    }
}

impl Default for QosTargets {
    fn default() -> Self {
        Self {
            fps_degraded_percent: 90,
            fps_critical_percent: 70,
            rtt_degraded_ms: 60,
            rtt_critical_ms: 150,
            drop_degraded_basis_points: 50,
            drop_critical_basis_points: 500,
            input_degraded_ms: 50,
            input_critical_ms: 120,
            heartbeat_critical_misses: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QosTargetsWire {
    fps_degraded_percent: u8,
    fps_critical_percent: u8,
    rtt_degraded_ms: u32,
    rtt_critical_ms: u32,
    drop_degraded_basis_points: u16,
    drop_critical_basis_points: u16,
    input_degraded_ms: u32,
    input_critical_ms: u32,
    heartbeat_critical_misses: u32,
}

impl Default for QosTargetsWire {
    fn default() -> Self {
        QosTargets::default().into()
    }
}

impl TryFrom<QosTargetsWire> for QosTargets {
    type Error = QosTargetError;

    fn try_from(value: QosTargetsWire) -> Result<Self, Self::Error> {
        Self::new(
            value.fps_degraded_percent,
            value.fps_critical_percent,
            value.rtt_degraded_ms,
            value.rtt_critical_ms,
            value.drop_degraded_basis_points,
            value.drop_critical_basis_points,
            value.input_degraded_ms,
            value.input_critical_ms,
            value.heartbeat_critical_misses,
        )
    }
}

impl From<QosTargets> for QosTargetsWire {
    fn from(value: QosTargets) -> Self {
        Self {
            fps_degraded_percent: value.fps_degraded_percent,
            fps_critical_percent: value.fps_critical_percent,
            rtt_degraded_ms: value.rtt_degraded_ms,
            rtt_critical_ms: value.rtt_critical_ms,
            drop_degraded_basis_points: value.drop_degraded_basis_points,
            drop_critical_basis_points: value.drop_critical_basis_points,
            input_degraded_ms: value.input_degraded_ms,
            input_critical_ms: value.input_critical_ms,
            heartbeat_critical_misses: value.heartbeat_critical_misses,
        }
    }
}

/// Invalid `QoS` threshold ordering or bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QosTargetError;

impl Display for QosTargetError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("QoS targets are out of range or not strictly ordered")
    }
}

impl Error for QosTargetError {}

/// Three-state operational health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    /// Working within targets.
    Ok,
    /// Working outside preferred targets.
    Degraded,
    /// Not working acceptably.
    Critical,
}

/// Metric responsible for a health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthCause {
    /// Delivered or experienced frame rate.
    Fps,
    /// Round-trip latency.
    Rtt,
    /// Frame loss.
    ///
    /// **Dual-purpose, and the two meanings need different counters.** On the
    /// host-delivery side this is real delivery loss, from
    /// `frames_sent`/`frames_dropped`. On the client-experience side
    /// [`assess_presentation_drop`] reuses it for frames that decoded fine and
    /// were then superseded before presentation, from
    /// `frames_decoded`/`frames_presented`. A reporter that resolves the value
    /// from the wrong side prints zero host loss for a real presentation
    /// problem, which reads as a network fault that does not exist.
    Loss,
    /// Input latency.
    InputLatency,
    /// Missed health intervals.
    Heartbeat,
}

/// State and cause for one side of a session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSide {
    /// `None` means no applicable metrics were available.
    pub state: Option<HealthState>,
    /// Dominant cause when metrics were available.
    pub dominant_cause: Option<HealthCause>,
}

/// Host delivery, client experience, and worst overall health.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthAssessment {
    /// Capture/encode/delivery health.
    pub host_delivery: HealthSide,
    /// Decode/present/network/input health.
    pub client_experience: HealthSide,
    /// Worst available state across both sides.
    pub overall: Option<HealthState>,
}

/// Assesses one sample without allocating or consulting a clock.
#[must_use]
pub fn assess_health(sample: &QosSample, targets: &QosTargets) -> HealthAssessment {
    let host_present = sample.frames_sent.is_some()
        || sample.frames_dropped.is_some()
        || sample.capture_time_ms.is_some()
        || sample.encode_time_ms.is_some();
    let client_present = sample.frames_decoded.is_some()
        || sample.frames_presented.is_some()
        || sample.decode_time_ms.is_some()
        || sample.display_time_ms.is_some()
        || sample.rtt_ms.is_some()
        || sample.input_latency_ms.is_some()
        || sample.heartbeat_misses.is_some();

    let mut host = SideAccumulator::default();
    let mut client = SideAccumulator::default();
    if host_present {
        assess_fps(sample, targets, &mut host);
        assess_drop(
            sample.frames_sent,
            sample.frames_dropped,
            targets,
            &mut host,
        );
    }
    if client_present {
        assess_fps(sample, targets, &mut client);
        assess_presentation_drop(sample, targets, &mut client);
        assess_latency(sample, targets, &mut client);
    }

    let host_delivery = host.finish();
    let client_experience = client.finish();
    let overall = match (host_delivery.state, client_experience.state) {
        (Some(host_state), Some(client_state)) => Some(host_state.max(client_state)),
        (state @ Some(_), None) | (None, state @ Some(_)) => state,
        (None, None) => None,
    };
    HealthAssessment {
        host_delivery,
        client_experience,
        overall,
    }
}

#[derive(Debug, Default)]
struct SideAccumulator {
    state: Option<HealthState>,
    cause: Option<HealthCause>,
}

impl SideAccumulator {
    fn observe(&mut self, state: HealthState, cause: HealthCause) {
        if self.state.is_none_or(|current| state > current) {
            self.state = Some(state);
            self.cause = Some(cause);
        }
    }

    fn finish(self) -> HealthSide {
        HealthSide {
            state: self.state,
            dominant_cause: self.cause,
        }
    }
}

/// Frames per second a window had to deliver to be complete: the target,
/// lowered to the content rate when the source proved it was idle for part
/// of the window. `None` when frame rate cannot be judged.
///
/// A window that delivered nothing at all is judged against the target
/// whatever the content: every Pier sends a keepalive at least once a second
/// on a still desktop (Keel's idle cadence), so silence is a stall, never
/// idleness. Silence is judged from the window's frame count where the
/// sample has one, not from `fps_actual`: one keepalive in a two-second
/// window is an integer rate of zero, and is not silence.
#[must_use]
pub fn fps_expected(sample: &QosSample) -> Option<u32> {
    let target = sample.fps_target.filter(|target| *target != 0)?;
    let delivered = sample.frames_sent.or(sample.frames_presented);
    let silent = delivered.map_or(sample.fps_actual == Some(0), |frames| frames == 0);
    if silent {
        return Some(target);
    }
    Some(
        sample
            .fps_content
            .map_or(target, |content| content.min(target)),
    )
}

/// The frame rate below which `state` applies to this sample, for reporting
/// the threshold a health transition crossed. `None` when frame rate cannot
/// be judged.
#[must_use]
pub fn fps_threshold(sample: &QosSample, targets: &QosTargets, state: HealthState) -> Option<u32> {
    let percent = if state == HealthState::Critical {
        targets.fps_critical_percent
    } else {
        targets.fps_degraded_percent
    };
    fps_expected(sample).map(|expected| expected.saturating_mul(u32::from(percent)) / 100)
}

/// Whole frames per second a window may fall short of [`fps_expected`] by
/// before it counts at all.
///
/// Rates are integer frames per window, and content and sent frames are not
/// counted at the same instant: at two changed frames a second, a frame
/// captured just before a window boundary and sent just after it reads as one
/// sent against two offered. One frame a second is that quantisation, not a
/// shortfall.
const FPS_QUANTISATION_SLACK: u32 = 1;

fn assess_fps(sample: &QosSample, targets: &QosTargets, side: &mut SideAccumulator) {
    if sample.fps_warmup {
        return;
    }
    let Some(actual) = sample.fps_actual else {
        return;
    };
    let Some(expected) = fps_expected(sample) else {
        return;
    };
    if actual.saturating_add(FPS_QUANTISATION_SLACK) >= expected {
        side.observe(HealthState::Ok, HealthCause::Fps);
        return;
    }
    let percent = u64::from(actual).saturating_mul(100) / u64::from(expected);
    let state = if percent < u64::from(targets.fps_critical_percent) {
        HealthState::Critical
    } else if percent < u64::from(targets.fps_degraded_percent) {
        HealthState::Degraded
    } else {
        HealthState::Ok
    };
    side.observe(state, HealthCause::Fps);
}

fn assess_drop(
    frames: Option<u64>,
    dropped: Option<u64>,
    targets: &QosTargets,
    side: &mut SideAccumulator,
) {
    let (Some(frames), Some(dropped)) = (frames, dropped) else {
        return;
    };
    let total = frames.saturating_add(dropped);
    if total == 0 {
        return;
    }
    let basis_points = dropped.saturating_mul(10_000) / total;
    let state = if basis_points > u64::from(targets.drop_critical_basis_points) {
        HealthState::Critical
    } else if basis_points >= u64::from(targets.drop_degraded_basis_points) {
        HealthState::Degraded
    } else {
        HealthState::Ok
    };
    side.observe(state, HealthCause::Loss);
}

/// Decoded frames a window may leave unpresented before supersession counts.
///
/// A frame decoded just before the window closed is presented just after it.
/// On a busy stream that one frame is noise; on a still desktop sending three
/// frames a window it read as a third of the stream lost, and raised a
/// critical presentation alarm on sessions that presented every frame.
const PRESENTATION_IN_FLIGHT_FRAMES: u64 = 1;

/// Fewest decoded frames in a window for which a presentation shortfall is
/// judged at all: two a second over the Deck's five-second window.
///
/// Supersession means a newer frame arrived before the last one reached the
/// screen. A still desktop sends a keepalive a second, so that cannot happen;
/// what a shortfall measures there is the client's presentation accounting
/// trailing its decode count by a frame or two at the window boundary. Seen
/// live: a Deck that put every keepalive on glass reported two of five
/// unpresented, and the Pier's view of it went critical. A client that
/// presents nothing at all is still caught, by the frame-rate rule.
const PRESENTATION_MIN_DECODED: u64 = 10;

/// Decoded frames that were superseded before presentation, in basis points
/// of the frames decoded, judged only on windows of at least
/// [`PRESENTATION_MIN_DECODED`] frames and after allowing the one frame that is legitimately
/// still in flight at the window boundary.
///
/// **This is not transport loss**, despite feeding [`HealthCause::Loss`]:
/// every frame counted here arrived intact and decoded, and was then
/// replaced by a newer one before the client presented it. Reporters call it
/// `unpresented`; see the note on [`HealthCause::Loss`].
#[must_use]
pub fn unpresented_basis_points(sample: &QosSample) -> Option<u32> {
    let (Some(decoded), Some(presented)) = (sample.frames_decoded, sample.frames_presented) else {
        return None;
    };
    if decoded == 0 {
        return (presented != 0).then_some(0);
    }
    if decoded < PRESENTATION_MIN_DECODED {
        return Some(0);
    }
    let missed = decoded
        .saturating_sub(presented)
        .saturating_sub(PRESENTATION_IN_FLIGHT_FRAMES);
    Some(u32::try_from(missed.saturating_mul(10_000) / decoded).unwrap_or(u32::MAX))
}

/// Client-side presentation supersession, reported as [`HealthCause::Loss`].
///
/// The denominator is `frames_decoded`, so a reporter must resolve this cause
/// from the client counters rather than from host delivery — see the note on
/// [`HealthCause::Loss`].
fn assess_presentation_drop(sample: &QosSample, targets: &QosTargets, side: &mut SideAccumulator) {
    let Some(basis_points) = unpresented_basis_points(sample) else {
        return;
    };
    let state = if basis_points > u32::from(targets.drop_critical_basis_points) {
        HealthState::Critical
    } else if basis_points >= u32::from(targets.drop_degraded_basis_points) {
        HealthState::Degraded
    } else {
        HealthState::Ok
    };
    side.observe(state, HealthCause::Loss);
}

fn assess_latency(sample: &QosSample, targets: &QosTargets, side: &mut SideAccumulator) {
    if let Some(rtt) = sample.rtt_ms {
        let state = if rtt > targets.rtt_critical_ms {
            HealthState::Critical
        } else if rtt > targets.rtt_degraded_ms {
            HealthState::Degraded
        } else {
            HealthState::Ok
        };
        side.observe(state, HealthCause::Rtt);
    }
    if let Some(input) = sample.input_latency_ms {
        let state = if input > targets.input_critical_ms {
            HealthState::Critical
        } else if input > targets.input_degraded_ms {
            HealthState::Degraded
        } else {
            HealthState::Ok
        };
        side.observe(state, HealthCause::InputLatency);
    }
    if let Some(misses) = sample.heartbeat_misses {
        let state = if misses >= targets.heartbeat_critical_misses {
            HealthState::Critical
        } else if misses > 0 {
            HealthState::Degraded
        } else {
            HealthState::Ok
        };
        side.observe(state, HealthCause::Heartbeat);
    }
}

/// One committed two-window health transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthTransition {
    /// Previous state, absent for initial establishment.
    pub previous: Option<HealthState>,
    /// Newly committed state.
    pub current: HealthState,
    /// Caller-provided transition time.
    pub timestamp_ms: u64,
}

/// Deterministic two-consecutive-window hysteresis tracker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HealthTracker {
    current: Option<HealthState>,
    pending: Option<HealthState>,
    pending_windows: u8,
    last_timestamp_ms: Option<u64>,
}

impl HealthTracker {
    /// Returns the committed state.
    #[must_use]
    pub const fn current(self) -> Option<HealthState> {
        self.current
    }

    /// Applies an assessment using the caller's monotonic clock.
    ///
    /// Missing health resets a pending transition and never means `Ok`.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller clock moves backwards.
    pub fn update(
        &mut self,
        timestamp_ms: u64,
        assessment: &HealthAssessment,
    ) -> Result<Option<HealthTransition>, HealthTrackerError> {
        if self
            .last_timestamp_ms
            .is_some_and(|previous| timestamp_ms < previous)
        {
            return Err(HealthTrackerError::ClockMovedBackwards);
        }
        self.last_timestamp_ms = Some(timestamp_ms);
        let Some(candidate) = assessment.overall else {
            self.pending = None;
            self.pending_windows = 0;
            return Ok(None);
        };
        if self.current == Some(candidate) {
            self.pending = None;
            self.pending_windows = 0;
            return Ok(None);
        }
        if self.pending == Some(candidate) {
            self.pending_windows = self.pending_windows.saturating_add(1);
        } else {
            self.pending = Some(candidate);
            self.pending_windows = 1;
        }
        if self.pending_windows < 2 {
            return Ok(None);
        }
        let transition = HealthTransition {
            previous: self.current,
            current: candidate,
            timestamp_ms,
        };
        self.current = Some(candidate);
        self.pending = None;
        self.pending_windows = 0;
        Ok(Some(transition))
    }
}

/// Hysteresis tracker input error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthTrackerError {
    /// Caller-provided monotonic clock decreased.
    ClockMovedBackwards,
}

impl Display for HealthTrackerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("health tracker clock moved backwards")
    }
}

impl Error for HealthTrackerError {}

/// A host's cumulative delivery counters at one health tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostCounterTotals {
    /// Frames sent since the session started.
    pub frames_sent: u64,
    /// Frames dropped since the session started.
    pub frames_dropped: u64,
    /// Media bytes sent since the session started.
    pub bytes_sent: u64,
    /// Input events applied since the session started.
    pub input_events: u64,
    /// Frames carrying changed content the capture source offered since the
    /// session started, for a damage-driven source that can prove it. The
    /// window's rate becomes [`QosSample::fps_content`], so a still desktop
    /// is judged against what changed rather than the target alone.
    pub frames_offered: Option<u64>,
}

/// One health window: what changed since the previous tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostWindow {
    /// The window's host-side sample: frame and input counts are deltas,
    /// `fps_actual` is the frames sent per second over the window,
    /// `fps_target` the contract and `fps_content` the offered rate (see
    /// [`HostCounterTotals::frames_offered`]); [`fps_expected`] combines them.
    pub sample: QosSample,
    /// Media bytes sent over the window, in megabits per second.
    pub bandwidth_mbps: f64,
    /// The window's length in milliseconds.
    pub elapsed_ms: u64,
}

/// Collects a capture loop's per-second content proofs over one health
/// window, for [`QosSample::fps_content`].
///
/// Capture loops report about once a second; health windows are longer. Only
/// the latest second would let a busy second the pipeline fell behind in,
/// followed by an idle one, read as healthy. So every second in the window
/// is recorded, and the window proves a content rate only when **every**
/// second did (each showed spare capacity: the source, not the pipeline, set
/// its pace); the rate is then the mean of their content counts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ContentWindow {
    seconds: u32,
    unproven: u32,
    frames: u64,
}

impl ContentWindow {
    /// Records one stats second: `Some(frames)` when it proved the source set
    /// the pace and that many changed frames were offered, `None` otherwise.
    pub fn record(&mut self, content: Option<u32>) {
        self.seconds = self.seconds.saturating_add(1);
        match content {
            Some(frames) => self.frames = self.frames.saturating_add(u64::from(frames)),
            None => self.unproven = self.unproven.saturating_add(1),
        }
    }

    /// The window's proven content rate, and starts the next window. `None`
    /// when any second was unproven, or when no second was reported at all.
    pub fn take_fps(&mut self) -> Option<u32> {
        let window = std::mem::take(self);
        (window.seconds > 0 && window.unproven == 0)
            .then(|| u32::try_from(window.frames / u64::from(window.seconds)).unwrap_or(u32::MAX))
    }
}

/// Turns a host's cumulative counters into per-window samples, so every host
/// reports the window its `sample_window_secs` declares rather than session
/// totals.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostWindowTracker {
    previous: HostCounterTotals,
    previous_ms: Option<u64>,
    first_ms: Option<u64>,
}

/// How long after a host's first health window its frame rate is not judged:
/// a stream ramps up after start, resume and recovery.
pub const HOST_FPS_WARMUP_MS: u64 = 10_000;

impl HostWindowTracker {
    /// Observes `totals` at the caller's monotonic `timestamp_ms`. The first
    /// window is taken to be `nominal_window_ms` long; a clock that did not
    /// advance counts as one millisecond.
    #[must_use]
    pub fn observe(
        &mut self,
        timestamp_ms: u64,
        fps_target: u32,
        totals: HostCounterTotals,
        nominal_window_ms: u64,
    ) -> HostWindow {
        let elapsed_ms = self
            .previous_ms
            .map_or(nominal_window_ms.max(1), |previous| {
                timestamp_ms.saturating_sub(previous).max(1)
            });
        let previous = self.previous;
        let first_ms = *self.first_ms.get_or_insert(timestamp_ms);
        let sent = totals.frames_sent.saturating_sub(previous.frames_sent);
        let bytes = totals.bytes_sent.saturating_sub(previous.bytes_sent);
        let offered_fps = totals.frames_offered.map(|offered| {
            let delta = offered.saturating_sub(previous.frames_offered.unwrap_or(0));
            u32::try_from(delta.saturating_mul(1_000) / elapsed_ms).unwrap_or(u32::MAX)
        });
        self.previous = totals;
        self.previous_ms = Some(timestamp_ms);
        // Bounded to u32 so the conversion is exact: 4 GB in one window is
        // far beyond any link this measures.
        let bandwidth_mbps = f64::from(u32::try_from(bytes).unwrap_or(u32::MAX)) * 8.0
            / (f64::from(u32::try_from(elapsed_ms).unwrap_or(u32::MAX)) * 1_000.0);
        HostWindow {
            sample: QosSample {
                timestamp_ms,
                fps_actual: u32::try_from(sent.saturating_mul(1_000) / elapsed_ms).ok(),
                fps_target: Some(fps_target),
                fps_content: offered_fps,
                fps_warmup: timestamp_ms.saturating_sub(first_ms) < HOST_FPS_WARMUP_MS,
                frames_sent: Some(sent),
                frames_dropped: Some(
                    totals
                        .frames_dropped
                        .saturating_sub(previous.frames_dropped),
                ),
                input_events: Some(totals.input_events.saturating_sub(previous.input_events)),
                ..QosSample::default()
            },
            bandwidth_mbps,
            elapsed_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_window_is_proven_only_when_every_second_was() {
        let mut window = ContentWindow::default();
        assert_eq!(window.take_fps(), None, "no stats line, no proof");
        // Two still seconds.
        window.record(Some(1));
        window.record(Some(3));
        assert_eq!(window.take_fps(), Some(2));
        // A busy second the pipeline fell behind in, then an idle one: the
        // idle second must not excuse the busy one.
        window.record(None);
        window.record(Some(0));
        assert_eq!(window.take_fps(), None);
        let sample = QosSample {
            fps_actual: Some(15),
            fps_target: Some(60),
            fps_content: window.take_fps(),
            frames_sent: Some(30),
            frames_dropped: Some(0),
            ..QosSample::default()
        };
        assert_eq!(
            assess_health(&sample, &QosTargets::default())
                .host_delivery
                .state,
            Some(HealthState::Critical)
        );
        // Taking starts the next window.
        window.record(Some(4));
        assert_eq!(window.take_fps(), Some(4));
    }

    #[test]
    fn host_windows_report_deltas_over_the_elapsed_window() {
        let mut tracker = HostWindowTracker::default();
        let first = tracker.observe(
            10_000,
            60,
            HostCounterTotals {
                frames_sent: 120,
                frames_dropped: 2,
                bytes_sent: 1_000_000,
                input_events: 10,
                frames_offered: None,
            },
            2_000,
        );
        assert_eq!(first.elapsed_ms, 2_000);
        assert!(first.sample.fps_warmup);
        assert_eq!(first.sample.fps_actual, Some(60));
        assert_eq!(first.sample.frames_sent, Some(120));
        assert!((first.bandwidth_mbps - 4.0).abs() < 1e-9);
        let second = tracker.observe(
            14_000,
            60,
            HostCounterTotals {
                frames_sent: 240,
                frames_dropped: 5,
                bytes_sent: 1_500_000,
                input_events: 10,
                frames_offered: None,
            },
            2_000,
        );
        assert_eq!(second.elapsed_ms, 4_000);
        assert_eq!(second.sample.fps_actual, Some(30));
        assert_eq!(second.sample.frames_sent, Some(120));
        assert_eq!(second.sample.frames_dropped, Some(3));
        assert_eq!(second.sample.input_events, Some(0));
        assert!((second.bandwidth_mbps - 1.0).abs() < 1e-9);
        // Past the warm-up a stalled host reads as zero frames: critical.
        let stalled = tracker.observe(
            22_000,
            60,
            HostCounterTotals {
                frames_sent: 240,
                frames_dropped: 5,
                bytes_sent: 1_500_000,
                input_events: 10,
                frames_offered: None,
            },
            2_000,
        );
        assert!(!stalled.sample.fps_warmup);
        assert_eq!(stalled.sample.fps_actual, Some(0));
        assert_eq!(
            assess_health(&stalled.sample, &QosTargets::default())
                .host_delivery
                .state,
            Some(HealthState::Critical)
        );
    }

    #[test]
    fn a_still_damage_driven_desktop_is_not_a_delivery_fault() {
        let mut tracker = HostWindowTracker::default();
        let totals = |sent: u64, offered: u64| HostCounterTotals {
            frames_sent: sent,
            frames_offered: Some(offered),
            ..HostCounterTotals::default()
        };
        let _ = tracker.observe(0, 60, totals(10, 10), 2_000);
        // Past the warm-up: two frames offered, two sent.
        let still = tracker.observe(20_000, 60, totals(12, 12), 2_000);
        assert_eq!(still.sample.fps_target, Some(60));
        assert_eq!(still.sample.fps_content, Some(0));
        assert_eq!(fps_expected(&still.sample), Some(0));
        assert_eq!(
            assess_health(&still.sample, &QosTargets::default())
                .host_delivery
                .state,
            Some(HealthState::Ok)
        );
        // The desktop offered 60 fps and the host sent 10: a real shortfall.
        let behind = tracker.observe(21_000, 60, totals(22, 72), 2_000);
        assert_eq!(fps_expected(&behind.sample), Some(60));
        assert_eq!(
            assess_health(&behind.sample, &QosTargets::default())
                .host_delivery
                .state,
            Some(HealthState::Critical)
        );
    }

    fn client_sample(rtt_ms: u32) -> QosSample {
        QosSample {
            frames_decoded: Some(100),
            frames_presented: Some(100),
            rtt_ms: Some(rtt_ms),
            ..QosSample::default()
        }
    }

    #[test]
    fn missing_metrics_are_unavailable_not_healthy() {
        assert_eq!(
            assess_health(&QosSample::default(), &QosTargets::default()),
            HealthAssessment::default()
        );
    }

    #[test]
    fn host_client_and_overall_use_worst_available_state() {
        let sample = QosSample {
            fps_actual: Some(59),
            fps_target: Some(60),
            frames_sent: Some(1_000),
            frames_dropped: Some(0),
            frames_decoded: Some(100),
            frames_presented: Some(100),
            rtt_ms: Some(151),
            ..QosSample::default()
        };
        let assessment = assess_health(&sample, &QosTargets::default());
        assert_eq!(assessment.host_delivery.state, Some(HealthState::Ok));
        assert_eq!(
            assessment.client_experience.state,
            Some(HealthState::Critical)
        );
        assert_eq!(assessment.overall, Some(HealthState::Critical));
    }

    #[test]
    fn thresholds_cover_fps_loss_latency_and_heartbeats() {
        let targets = QosTargets::default();
        let cases = [
            (client_sample(60), HealthState::Ok),
            (client_sample(61), HealthState::Degraded),
            (client_sample(151), HealthState::Critical),
            (
                QosSample {
                    frames_sent: Some(950),
                    frames_dropped: Some(50),
                    ..QosSample::default()
                },
                HealthState::Degraded,
            ),
            (
                QosSample {
                    frames_decoded: Some(1),
                    frames_presented: Some(1),
                    heartbeat_misses: Some(3),
                    ..QosSample::default()
                },
                HealthState::Critical,
            ),
        ];
        for (sample, expected) in cases {
            assert_eq!(assess_health(&sample, &targets).overall, Some(expected));
        }
    }

    #[test]
    fn fps_warmup_suppresses_startup_ramp_degradation() {
        let sample = QosSample {
            fps_actual: Some(2),
            fps_target: Some(30),
            fps_warmup: true,
            frames_sent: Some(4),
            frames_dropped: Some(0),
            ..QosSample::default()
        };
        let assessment = assess_health(&sample, &QosTargets::default());
        assert_eq!(assessment.host_delivery.state, Some(HealthState::Ok));
        assert_eq!(
            assessment.host_delivery.dominant_cause,
            Some(HealthCause::Loss)
        );
    }

    #[test]
    fn real_fps_drop_after_warmup_still_goes_critical() {
        let sample = QosSample {
            fps_actual: Some(2),
            fps_target: Some(30),
            frames_sent: Some(4),
            frames_dropped: Some(0),
            ..QosSample::default()
        };
        let assessment = assess_health(&sample, &QosTargets::default());
        assert_eq!(
            assessment.host_delivery.dominant_cause,
            Some(HealthCause::Fps)
        );
        assert_eq!(assessment.host_delivery.state, Some(HealthState::Critical));
    }

    fn host_sample(actual: u32, target: u32, content: Option<u32>) -> QosSample {
        QosSample {
            fps_actual: Some(actual),
            fps_target: Some(target),
            fps_content: content,
            frames_sent: Some(u64::from(actual) * 2),
            frames_dropped: Some(0),
            ..QosSample::default()
        }
    }

    fn host_state(sample: &QosSample) -> (Option<HealthState>, Option<HealthCause>) {
        let side = assess_health(sample, &QosTargets::default()).host_delivery;
        (side.state, side.dominant_cause)
    }

    #[test]
    fn an_idle_desktop_that_sent_every_change_is_healthy() {
        // The reported session: a still macOS desktop, 8 fps of keepalives
        // and cursor frames against a target of 30, nothing dropped.
        assert_eq!(
            host_state(&host_sample(8, 30, Some(8))),
            (Some(HealthState::Ok), Some(HealthCause::Fps))
        );
        // Nothing changed at all: only the once-a-second keepalive went out.
        assert_eq!(
            host_state(&host_sample(1, 30, Some(0))).0,
            Some(HealthState::Ok)
        );
        // Not even a keepalive: a stall, however still the desktop.
        assert_eq!(
            host_state(&host_sample(0, 30, Some(0))),
            (Some(HealthState::Critical), Some(HealthCause::Fps))
        );
        // One keepalive in a two-second window floors to a rate of zero; it
        // is still a keepalive, not silence (seen live on an Xorg Pier).
        let one_keepalive = QosSample {
            frames_sent: Some(1),
            ..host_sample(0, 30, Some(0))
        };
        assert_eq!(host_state(&one_keepalive).0, Some(HealthState::Ok));
        // Without content evidence the same rate is still a shortfall.
        assert_eq!(
            host_state(&host_sample(8, 30, None)),
            (Some(HealthState::Critical), Some(HealthCause::Fps))
        );
    }

    #[test]
    fn one_changed_frame_a_second_is_healthy_across_window_edges() {
        assert_eq!(
            host_state(&host_sample(1, 60, Some(1))).0,
            Some(HealthState::Ok)
        );
        // One frame was captured before the window closed and sent after it.
        assert_eq!(
            host_state(&host_sample(1, 60, Some(2))).0,
            Some(HealthState::Ok)
        );
        // Keepalives on top of the changes are never a shortfall.
        assert_eq!(
            host_state(&host_sample(3, 60, Some(1))).0,
            Some(HealthState::Ok)
        );
    }

    #[test]
    fn content_changing_faster_than_it_is_sent_is_still_flagged() {
        // Sixty changes a second, fifteen sent: an encoder or network backlog.
        assert_eq!(
            host_state(&host_sample(15, 60, Some(60))),
            (Some(HealthState::Critical), Some(HealthCause::Fps))
        );
        // The content rate never raises the bar above the target.
        assert_eq!(
            host_state(&host_sample(15, 30, Some(60))),
            (Some(HealthState::Critical), Some(HealthCause::Fps))
        );
        assert_eq!(
            host_state(&host_sample(50, 60, Some(60))),
            (Some(HealthState::Degraded), Some(HealthCause::Fps))
        );
        // A partly busy desktop is judged against what changed.
        assert_eq!(
            host_state(&host_sample(10, 60, Some(20))),
            (Some(HealthState::Critical), Some(HealthCause::Fps))
        );
        let sample = host_sample(10, 60, Some(20));
        let targets = QosTargets::default();
        assert_eq!(fps_expected(&sample), Some(20));
        assert_eq!(
            fps_threshold(&sample, &targets, HealthState::Critical),
            Some(14)
        );
        assert_eq!(
            fps_threshold(&sample, &targets, HealthState::Degraded),
            Some(18)
        );
    }

    #[test]
    fn drops_on_an_idle_desktop_are_still_flagged() {
        let sample = QosSample {
            frames_sent: Some(9),
            frames_dropped: Some(1),
            ..host_sample(5, 30, Some(5))
        };
        assert_eq!(
            host_state(&sample),
            (Some(HealthState::Critical), Some(HealthCause::Loss))
        );
        let sample = QosSample {
            frames_sent: Some(1_990),
            frames_dropped: Some(10),
            ..host_sample(1, 60, Some(0))
        };
        assert_eq!(
            host_state(&sample),
            (Some(HealthState::Degraded), Some(HealthCause::Loss))
        );
    }

    #[test]
    fn the_client_judges_presentation_against_what_it_received() {
        let client = |presented: u32, received: u32| QosSample {
            fps_actual: Some(presented),
            fps_target: Some(60),
            fps_content: Some(received),
            frames_decoded: Some(u64::from(received) * 5),
            frames_presented: Some(u64::from(presented) * 5),
            ..QosSample::default()
        };
        let state = |sample: &QosSample| {
            assess_health(sample, &QosTargets::default())
                .client_experience
                .state
        };
        assert_eq!(state(&client(3, 3)), Some(HealthState::Ok));
        assert_eq!(state(&client(30, 60)), Some(HealthState::Critical));
    }

    #[test]
    fn presentation_accounting_lag_is_not_unpresented() {
        let sample = |decoded: u64, presented: u64| QosSample {
            frames_decoded: Some(decoded),
            frames_presented: Some(presented),
            ..QosSample::default()
        };
        let state = |sample: &QosSample| {
            assess_health(sample, &QosTargets::default())
                .client_experience
                .state
        };
        // Sixty frames, fifty-nine on screen and one about to be.
        assert_eq!(unpresented_basis_points(&sample(60, 59)), Some(0));
        assert_eq!(state(&sample(60, 59)), Some(HealthState::Ok));
        // Ten of forty superseded is a real presentation problem.
        assert_eq!(unpresented_basis_points(&sample(40, 30)), Some(2_250));
        assert_eq!(state(&sample(40, 30)), Some(HealthState::Critical));
        // A still desktop's five keepalives, three accounted on screen by the
        // window's end (the live case): accounting lag, not supersession.
        assert_eq!(unpresented_basis_points(&sample(5, 3)), Some(0));
        assert_eq!(state(&sample(5, 3)), Some(HealthState::Ok));
        assert_eq!(unpresented_basis_points(&sample(0, 0)), None);
        assert_eq!(unpresented_basis_points(&sample(0, 2)), Some(0));
    }

    #[test]
    fn tracker_requires_two_consecutive_windows_and_does_not_flap() {
        let targets = QosTargets::default();
        let mut tracker = HealthTracker::default();
        let ok = assess_health(&client_sample(20), &targets);
        let degraded = assess_health(&client_sample(100), &targets);
        assert_eq!(tracker.update(1, &ok), Ok(None));
        assert_eq!(
            tracker.update(2, &ok),
            Ok(Some(HealthTransition {
                previous: None,
                current: HealthState::Ok,
                timestamp_ms: 2,
            }))
        );
        assert_eq!(tracker.update(3, &degraded), Ok(None));
        assert_eq!(tracker.update(4, &ok), Ok(None));
        assert_eq!(tracker.update(5, &degraded), Ok(None));
        assert_eq!(
            tracker.update(6, &degraded),
            Ok(Some(HealthTransition {
                previous: Some(HealthState::Ok),
                current: HealthState::Degraded,
                timestamp_ms: 6,
            }))
        );
    }

    #[test]
    fn missing_data_resets_pending_and_clock_must_be_monotonic() {
        let targets = QosTargets::default();
        let mut tracker = HealthTracker::default();
        let critical = assess_health(&client_sample(200), &targets);
        assert_eq!(tracker.update(10, &critical), Ok(None));
        assert_eq!(tracker.update(11, &HealthAssessment::default()), Ok(None));
        assert_eq!(tracker.update(12, &critical), Ok(None));
        assert_eq!(
            tracker.update(11, &critical),
            Err(HealthTrackerError::ClockMovedBackwards)
        );
    }

    #[test]
    fn qos_targets_deserialize_with_defaults_and_validate() {
        let targets: QosTargets =
            serde_json::from_str(r#"{"rtt_degraded_ms":80,"rtt_critical_ms":200}"#)
                .expect("valid partial targets");
        assert_eq!(targets.rtt_degraded_ms(), 80);
        assert_eq!(targets.fps_degraded_percent(), 90);
        assert!(
            serde_json::from_str::<QosTargets>(r#"{"rtt_degraded_ms":200,"rtt_critical_ms":100}"#)
                .is_err()
        );
    }
}
