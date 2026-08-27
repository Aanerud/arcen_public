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

fn assess_fps(sample: &QosSample, targets: &QosTargets, side: &mut SideAccumulator) {
    let (Some(actual), Some(target)) = (sample.fps_actual, sample.fps_target) else {
        return;
    };
    if target == 0 {
        return;
    }
    let percent = u64::from(actual).saturating_mul(100) / u64::from(target);
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

/// Client-side presentation supersession, reported as [`HealthCause::Loss`].
///
/// The denominator is `frames_decoded`, so a reporter must resolve this cause
/// from the client counters rather than from host delivery — see the note on
/// [`HealthCause::Loss`].
fn assess_presentation_drop(sample: &QosSample, targets: &QosTargets, side: &mut SideAccumulator) {
    let (Some(decoded), Some(presented)) = (sample.frames_decoded, sample.frames_presented) else {
        return;
    };
    assess_drop(
        Some(presented),
        Some(decoded.saturating_sub(presented)),
        targets,
        side,
    );
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

#[cfg(test)]
mod tests {
    use super::*;

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
