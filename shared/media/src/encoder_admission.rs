use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use crate::{
    ActivityClass, ChromaSubsampling, DirtyRatio, MAX_MULTI_MONITOR_COUNT,
    RegionActivityDiagnostics, RegionGeneration, RegionId, RegionMediaPlan, RegionMediaRoster,
    SessionMonitorId,
};

const BASIS_POINTS: u32 = 10_000;
const BASIS_POINTS_U16: u16 = 10_000;
const MILLIFPS_SCALE: u128 = 1_000;
const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Maximum number of exact encoder-set candidates admitted in one bounded run.
///
/// Four regions can produce five useful hardware/software splits (all hardware
/// through all software). Adaptive performance then tries at most three
/// same-GPU codecs for each split: AV1, HEVC, and H.264. The bound covers that
/// deliberate cross product; it is not a claim about vendor session capacity.
pub const MAX_ENCODER_SET_CANDIDATES: usize = (MAX_MULTI_MONITOR_COUNT + 1) * 3;
/// Safety cap for measured frames per region and candidate.
pub const MAX_ENCODER_PROBE_FRAMES_PER_REGION: u16 = 600;
/// Safety cap for warm-up frames per region and candidate.
pub const MAX_ENCODER_PROBE_WARMUP_FRAMES: u16 = 64;
/// Safety cap for one representative measurement window.
pub const MAX_ENCODER_PROBE_WINDOW: Duration = Duration::from_secs(60);
/// Safety cap for one adapter-owned probe execution.
pub const MAX_ENCODER_PROBE_DURATION: Duration = Duration::from_secs(300);
/// Maximum opaque binding token size.
pub const MAX_ENCODER_BINDING_ID_BYTES: usize = 256;

/// Priority the host must preserve while considering encoder reassignment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionAdmissionPriority {
    Standard,
    FullColorRequired,
}

/// Region activity and required service rate used to build a representative
/// encoder probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionActivityProfile {
    pub session_monitor_id: SessionMonitorId,
    pub region_generation: RegionGeneration,
    pub region_id: RegionId,
    pub activity_class: ActivityClass,
    pub dirty_ratio: DirtyRatio,
    pub target_fps: u32,
    pub priority: RegionAdmissionPriority,
}

impl RegionActivityProfile {
    /// Builds a profile from one region-owned Keel diagnostic snapshot.
    #[must_use]
    pub const fn from_diagnostics(
        session_monitor_id: SessionMonitorId,
        diagnostics: RegionActivityDiagnostics,
        target_fps: u32,
        priority: RegionAdmissionPriority,
    ) -> Self {
        Self {
            session_monitor_id,
            region_generation: diagnostics.generation,
            region_id: diagnostics.region_id,
            activity_class: diagnostics.activity.class,
            dirty_ratio: diagnostics.activity.current_dirty_ratio,
            target_fps,
            priority,
        }
    }
}

/// Validated activity profile roster for one complete region generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionActivityProfiles {
    profiles: Vec<RegionActivityProfile>,
}

impl RegionActivityProfiles {
    /// Validates a bounded, same-generation profile roster.
    ///
    /// # Errors
    ///
    /// Returns the first invalid count, zero target, generation mismatch, or
    /// duplicate monitor/region identity.
    pub fn new(profiles: Vec<RegionActivityProfile>) -> Result<Self, EncoderAdmissionError> {
        if profiles.is_empty() || profiles.len() > MAX_MULTI_MONITOR_COUNT {
            return Err(EncoderAdmissionError::InvalidProfileCount(profiles.len()));
        }
        let generation = profiles[0].region_generation;
        let mut monitor_ids = BTreeSet::new();
        let mut region_ids = BTreeSet::new();
        for profile in &profiles {
            if profile.target_fps == 0 {
                return Err(EncoderAdmissionError::ZeroTargetFps(
                    profile.session_monitor_id,
                ));
            }
            if profile.region_generation != generation {
                return Err(EncoderAdmissionError::MixedActivityGeneration {
                    expected: generation,
                    received: profile.region_generation,
                });
            }
            if !monitor_ids.insert(profile.session_monitor_id) {
                return Err(EncoderAdmissionError::DuplicateActivityMonitor(
                    profile.session_monitor_id,
                ));
            }
            if !region_ids.insert(profile.region_id) {
                return Err(EncoderAdmissionError::DuplicateActivityRegion(
                    profile.region_id,
                ));
            }
        }
        Ok(Self { profiles })
    }

    #[must_use]
    pub fn profiles(&self) -> &[RegionActivityProfile] {
        &self.profiles
    }

    #[must_use]
    pub fn profile(&self, monitor_id: SessionMonitorId) -> Option<RegionActivityProfile> {
        self.profiles
            .iter()
            .copied()
            .find(|profile| profile.session_monitor_id == monitor_id)
    }
}

/// Bounded opaque identity for the exact platform encoder/output binding.
///
/// Linux adapters use their planned Xorg head/output tuple. Windows adapters
/// use the stable adapter-LUID/target tuple. The admission core never parses
/// this value; an injected platform adapter must refuse to auto-select a
/// different GPU or backend.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EncoderBindingId(String);

impl EncoderBindingId {
    /// Creates a nonempty bounded binding token.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is empty or exceeds
    /// [`MAX_ENCODER_BINDING_ID_BYTES`].
    pub fn new(value: impl Into<String>) -> Result<Self, EncoderAdmissionError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_ENCODER_BINDING_ID_BYTES {
            return Err(EncoderAdmissionError::InvalidBindingIdLength(value.len()));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for EncoderBindingId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Exact platform binding for one media-roster entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionEncoderBinding {
    pub session_monitor_id: SessionMonitorId,
    pub binding_id: EncoderBindingId,
}

/// One complete exact encoder set considered by host policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderSetCandidate {
    roster: RegionMediaRoster,
    bindings: Vec<RegionEncoderBinding>,
}

impl EncoderSetCandidate {
    /// Pairs every media plan with exactly one opaque platform binding.
    ///
    /// # Errors
    ///
    /// Returns an error when a binding is missing, duplicated, or refers to a
    /// monitor absent from the roster.
    pub fn new(
        roster: RegionMediaRoster,
        bindings: Vec<RegionEncoderBinding>,
    ) -> Result<Self, EncoderAdmissionError> {
        if bindings.len() != roster.plans().len() {
            return Err(EncoderAdmissionError::BindingCountMismatch {
                plans: roster.plans().len(),
                bindings: bindings.len(),
            });
        }
        let mut ids = BTreeSet::new();
        for binding in &bindings {
            if !ids.insert(binding.session_monitor_id) {
                return Err(EncoderAdmissionError::DuplicateBindingMonitor(
                    binding.session_monitor_id,
                ));
            }
            if roster.plan(binding.session_monitor_id).is_none() {
                return Err(EncoderAdmissionError::UnknownBindingMonitor(
                    binding.session_monitor_id,
                ));
            }
        }
        for plan in roster.plans() {
            if !ids.contains(&plan.session_monitor_id) {
                return Err(EncoderAdmissionError::MissingBinding(
                    plan.session_monitor_id,
                ));
            }
        }
        Ok(Self { roster, bindings })
    }

    #[must_use]
    pub const fn roster(&self) -> &RegionMediaRoster {
        &self.roster
    }

    #[must_use]
    pub fn binding(&self, monitor_id: SessionMonitorId) -> Option<&EncoderBindingId> {
        self.bindings
            .iter()
            .find(|binding| binding.session_monitor_id == monitor_id)
            .map(|binding| &binding.binding_id)
    }
}

/// Explicit, measured admission thresholds.
///
/// This type intentionally has no `Default`: a host must supply thresholds
/// derived from its accepted measurement policy rather than inheriting a
/// vendor/session-count claim from this shared crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderAdmissionThresholds {
    pub measurement_window: Duration,
    pub max_probe_duration: Duration,
    pub warmup_frames: u16,
    pub max_sample_frames_per_region: u16,
    pub max_p95_encode_latency: Duration,
    pub max_p95_queue_age: Duration,
    pub min_delivered_fps_basis_points: u16,
    pub min_fairness_basis_points: u16,
}

impl EncoderAdmissionThresholds {
    /// Derives admission thresholds from the host's validated `QoS` policy.
    ///
    /// The two-second window and bounded warm-up are measurement mechanics.
    /// Acceptance values come from operator-loadable `QoS` targets: degraded
    /// FPS becomes both the delivery and fairness floor, while degraded RTT
    /// bounds encoder and queue latency. No GPU vendor/session claim is used.
    #[must_use]
    pub fn from_qos_targets(targets: arcen_telemetry::QosTargets) -> Self {
        let minimum_basis_points = u16::from(targets.fps_degraded_percent()) * 100;
        let latency = Duration::from_millis(u64::from(targets.rtt_degraded_ms()));
        Self {
            measurement_window: Duration::from_secs(2),
            max_probe_duration: Duration::from_secs(15),
            warmup_frames: 8,
            max_sample_frames_per_region: MAX_ENCODER_PROBE_FRAMES_PER_REGION,
            max_p95_encode_latency: latency,
            max_p95_queue_age: latency,
            min_delivered_fps_basis_points: minimum_basis_points,
            min_fairness_basis_points: minimum_basis_points,
        }
    }

    /// Validates threshold ordering and safety bounds.
    ///
    /// # Errors
    ///
    /// Returns the first invalid duration, frame bound, or basis-point value.
    pub fn validate(self) -> Result<(), EncoderAdmissionError> {
        if self.measurement_window.is_zero() || self.measurement_window > MAX_ENCODER_PROBE_WINDOW {
            return Err(EncoderAdmissionError::InvalidMeasurementWindow(
                self.measurement_window,
            ));
        }
        if self.max_probe_duration < self.measurement_window
            || self.max_probe_duration > MAX_ENCODER_PROBE_DURATION
        {
            return Err(EncoderAdmissionError::InvalidProbeDuration(
                self.max_probe_duration,
            ));
        }
        if self.warmup_frames > MAX_ENCODER_PROBE_WARMUP_FRAMES {
            return Err(EncoderAdmissionError::TooManyWarmupFrames(
                self.warmup_frames,
            ));
        }
        if self.max_sample_frames_per_region == 0
            || self.max_sample_frames_per_region > MAX_ENCODER_PROBE_FRAMES_PER_REGION
        {
            return Err(EncoderAdmissionError::InvalidSampleFrameBound(
                self.max_sample_frames_per_region,
            ));
        }
        if self.max_p95_encode_latency.is_zero()
            || self.max_p95_encode_latency > self.max_probe_duration
        {
            return Err(EncoderAdmissionError::InvalidEncodeLatencyThreshold(
                self.max_p95_encode_latency,
            ));
        }
        if self.max_p95_queue_age > self.max_probe_duration {
            return Err(EncoderAdmissionError::InvalidQueueAgeThreshold(
                self.max_p95_queue_age,
            ));
        }
        for (name, value) in [
            (
                "min_delivered_fps_basis_points",
                self.min_delivered_fps_basis_points,
            ),
            ("min_fairness_basis_points", self.min_fairness_basis_points),
        ] {
            if value == 0 || u32::from(value) > BASIS_POINTS {
                return Err(EncoderAdmissionError::InvalidBasisPoints { name, value });
            }
        }
        Ok(())
    }
}

/// Synthetic content shape requested from an injected measurement adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepresentativeFrameKind {
    Sparse,
    FullMotion,
}

/// One measured representative frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepresentativeFrame {
    pub sequence: u16,
    pub kind: RepresentativeFrameKind,
    pub dirty_ratio: DirtyRatio,
}

/// Exact per-region request passed to the injected platform adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderProbeRequest {
    pub candidate_index: usize,
    pub plan: RegionMediaPlan,
    pub binding_id: EncoderBindingId,
    pub activity: RegionActivityProfile,
    pub measurement_window: Duration,
    pub max_probe_duration: Duration,
    /// Warm-up frames are always full-motion and excluded from measurements.
    pub warmup_frames: u16,
    pub sample_frames: Vec<RepresentativeFrame>,
}

/// Adapter-owned failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncoderProbeFailureKind {
    ContextOpen,
    Encode,
    DeadlineExceeded,
    InvalidMeasurement,
    Panicked,
}

/// Typed failure returned by an injected measurement adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderProbeFailure {
    pub kind: EncoderProbeFailureKind,
    pub detail: String,
}

impl EncoderProbeFailure {
    #[must_use]
    pub fn context_open(detail: impl Into<String>) -> Self {
        Self {
            kind: EncoderProbeFailureKind::ContextOpen,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn encode(detail: impl Into<String>) -> Self {
        Self {
            kind: EncoderProbeFailureKind::Encode,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn deadline(detail: impl Into<String>) -> Self {
        Self {
            kind: EncoderProbeFailureKind::DeadlineExceeded,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self {
            kind: EncoderProbeFailureKind::InvalidMeasurement,
            detail: detail.into(),
        }
    }
}

impl Display for EncoderProbeFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.detail)
    }
}

impl Error for EncoderProbeFailure {}

/// One raw adapter observation corresponding to one requested sample frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderProbeSample {
    pub sequence: u16,
    pub kind: RepresentativeFrameKind,
    pub queue_age: Duration,
    pub encode_latency: Duration,
    pub delivered: bool,
}

/// Raw bounded trace returned by one exact encoder context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderProbeTrace {
    pub elapsed: Duration,
    pub samples: Vec<EncoderProbeSample>,
}

/// Pure injectable boundary for platform/native encoder measurement.
///
/// The framework calls this concurrently for every entry in one candidate.
/// Implementations must open exactly `request.binding_id` with exactly
/// `request.plan`; automatic adapter/backend selection is a contract breach.
/// Implementations must also honor `request.max_probe_duration`.
pub trait EncoderMeasurementAdapter: Sync {
    /// Measures one exact planned encoder binding.
    ///
    /// # Errors
    ///
    /// Returns a typed context-open, encode, deadline, or measurement error.
    fn measure(
        &self,
        request: &EncoderProbeRequest,
    ) -> Result<EncoderProbeTrace, EncoderProbeFailure>;
}

/// Measured diagnostics for one region encoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionEncoderMeasurements {
    pub session_monitor_id: SessionMonitorId,
    pub offered_frames: u16,
    pub delivered_frames: u16,
    pub elapsed: Duration,
    pub p50_encode_latency: Duration,
    pub p95_encode_latency: Duration,
    pub p50_queue_age: Duration,
    pub p95_queue_age: Duration,
    pub delivered_millifps: u32,
    pub delivery_ratio_basis_points: u16,
}

/// Aggregate metrics for one concurrently measured exact encoder set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderSetMeasurements {
    pub p50_encode_latency: Duration,
    pub p95_encode_latency: Duration,
    pub p50_queue_age: Duration,
    pub p95_queue_age: Duration,
    pub delivered_millifps: u32,
    pub fairness_basis_points: u16,
    pub per_region: Vec<RegionEncoderMeasurements>,
}

/// Measured threshold violation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncoderThresholdViolation {
    EncodeLatency {
        session_monitor_id: SessionMonitorId,
        measured_p95: Duration,
        maximum_p95: Duration,
    },
    QueueAge {
        session_monitor_id: SessionMonitorId,
        measured_p95: Duration,
        maximum_p95: Duration,
    },
    DeliveredFps {
        session_monitor_id: SessionMonitorId,
        measured_millifps: u32,
        required_millifps: u32,
    },
    Fairness {
        measured_basis_points: u16,
        required_basis_points: u16,
    },
}

/// Per-region adapter failure retained in an atomic candidate attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionProbeFailure {
    pub session_monitor_id: SessionMonitorId,
    pub failure: EncoderProbeFailure,
}

/// Result of exercising one complete candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncoderSetAttemptOutcome {
    Passed(EncoderSetMeasurements),
    ThresholdFailed {
        measurements: EncoderSetMeasurements,
        violations: Vec<EncoderThresholdViolation>,
    },
    ProbeFailed {
        failures: Vec<RegionProbeFailure>,
    },
}

impl EncoderSetAttemptOutcome {
    #[must_use]
    pub const fn passed(&self) -> bool {
        matches!(self, Self::Passed(_))
    }
}

/// One candidate and its complete measured outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderSetAttempt {
    pub candidate_index: usize,
    pub candidate: EncoderSetCandidate,
    pub outcome: EncoderSetAttemptOutcome,
}

/// Host-authoritative aggregate admission decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncoderSetDecision {
    Accept {
        selected_candidate_index: usize,
        attempts: Vec<EncoderSetAttempt>,
    },
    Reassign {
        selected_candidate_index: usize,
        attempts: Vec<EncoderSetAttempt>,
    },
    Reject {
        attempts: Vec<EncoderSetAttempt>,
    },
}

impl EncoderSetDecision {
    #[must_use]
    pub const fn selected_candidate_index(&self) -> Option<usize> {
        match self {
            Self::Accept {
                selected_candidate_index,
                ..
            }
            | Self::Reassign {
                selected_candidate_index,
                ..
            } => Some(*selected_candidate_index),
            Self::Reject { .. } => None,
        }
    }

    #[must_use]
    pub fn attempts(&self) -> &[EncoderSetAttempt] {
        match self {
            Self::Accept { attempts, .. }
            | Self::Reassign { attempts, .. }
            | Self::Reject { attempts } => attempts,
        }
    }
}

/// Invalid aggregate admission input.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncoderAdmissionError {
    InvalidProfileCount(usize),
    ZeroTargetFps(SessionMonitorId),
    MixedActivityGeneration {
        expected: RegionGeneration,
        received: RegionGeneration,
    },
    DuplicateActivityMonitor(SessionMonitorId),
    DuplicateActivityRegion(RegionId),
    InvalidBindingIdLength(usize),
    BindingCountMismatch {
        plans: usize,
        bindings: usize,
    },
    DuplicateBindingMonitor(SessionMonitorId),
    UnknownBindingMonitor(SessionMonitorId),
    MissingBinding(SessionMonitorId),
    InvalidMeasurementWindow(Duration),
    InvalidProbeDuration(Duration),
    TooManyWarmupFrames(u16),
    InvalidSampleFrameBound(u16),
    InvalidEncodeLatencyThreshold(Duration),
    InvalidQueueAgeThreshold(Duration),
    InvalidBasisPoints {
        name: &'static str,
        value: u16,
    },
    InvalidCandidateCount(usize),
    ActivityRosterMismatch,
    CandidateRosterMismatch {
        candidate_index: usize,
        session_monitor_id: SessionMonitorId,
    },
    CandidateGeometryChanged {
        candidate_index: usize,
        session_monitor_id: SessionMonitorId,
    },
    CandidateEpochChanged {
        candidate_index: usize,
        session_monitor_id: SessionMonitorId,
    },
    CandidateFpsBelowTarget {
        candidate_index: usize,
        session_monitor_id: SessionMonitorId,
        candidate_fps: u32,
        target_fps: u32,
    },
    FullColorDowngrade {
        candidate_index: usize,
        session_monitor_id: SessionMonitorId,
    },
    WorkloadExceedsFrameBound {
        session_monitor_id: SessionMonitorId,
        required: u128,
        maximum: u16,
    },
}

impl Display for EncoderAdmissionError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfileCount(count) => {
                write!(formatter, "activity profile count {count} is outside 1..=4")
            }
            Self::ZeroTargetFps(id) => {
                write!(
                    formatter,
                    "activity profile {} has zero target fps",
                    id.get()
                )
            }
            Self::MixedActivityGeneration { expected, received } => write!(
                formatter,
                "activity generation {} does not match {}",
                received.get(),
                expected.get()
            ),
            Self::DuplicateActivityMonitor(id) => {
                write!(formatter, "duplicate activity monitor {}", id.get())
            }
            Self::DuplicateActivityRegion(id) => {
                write!(formatter, "duplicate activity region {}", id.get())
            }
            Self::InvalidBindingIdLength(length) => {
                write!(formatter, "encoder binding id length {length} is invalid")
            }
            Self::BindingCountMismatch { plans, bindings } => write!(
                formatter,
                "encoder candidate has {plans} plans but {bindings} bindings"
            ),
            Self::DuplicateBindingMonitor(id) => {
                write!(
                    formatter,
                    "duplicate encoder binding for monitor {}",
                    id.get()
                )
            }
            Self::UnknownBindingMonitor(id) => {
                write!(
                    formatter,
                    "encoder binding refers to unknown monitor {}",
                    id.get()
                )
            }
            Self::MissingBinding(id) => {
                write!(
                    formatter,
                    "encoder binding is missing for monitor {}",
                    id.get()
                )
            }
            Self::InvalidMeasurementWindow(value) => {
                write!(formatter, "invalid encoder measurement window {value:?}")
            }
            Self::InvalidProbeDuration(value) => {
                write!(formatter, "invalid encoder probe duration {value:?}")
            }
            Self::TooManyWarmupFrames(value) => {
                write!(
                    formatter,
                    "encoder warm-up frame count {value} exceeds its bound"
                )
            }
            Self::InvalidSampleFrameBound(value) => {
                write!(formatter, "encoder sample frame bound {value} is invalid")
            }
            Self::InvalidEncodeLatencyThreshold(value) => {
                write!(formatter, "invalid p95 encode latency threshold {value:?}")
            }
            Self::InvalidQueueAgeThreshold(value) => {
                write!(formatter, "invalid p95 queue-age threshold {value:?}")
            }
            Self::InvalidBasisPoints { name, value } => {
                write!(formatter, "{name}={value} is outside 1..=10000")
            }
            Self::InvalidCandidateCount(count) => write!(
                formatter,
                "encoder candidate count {count} is outside 1..={MAX_ENCODER_SET_CANDIDATES}"
            ),
            Self::ActivityRosterMismatch => {
                formatter.write_str("activity profiles do not exactly cover the media roster")
            }
            Self::CandidateRosterMismatch {
                candidate_index,
                session_monitor_id,
            } => write!(
                formatter,
                "encoder candidate {candidate_index} changed monitor roster at {}",
                session_monitor_id.get()
            ),
            Self::CandidateGeometryChanged {
                candidate_index,
                session_monitor_id,
            } => write!(
                formatter,
                "encoder candidate {candidate_index} changed geometry for monitor {}",
                session_monitor_id.get()
            ),
            Self::CandidateEpochChanged {
                candidate_index,
                session_monitor_id,
            } => write!(
                formatter,
                "encoder candidate {candidate_index} changed stream epoch for monitor {}",
                session_monitor_id.get()
            ),
            Self::CandidateFpsBelowTarget {
                candidate_index,
                session_monitor_id,
                candidate_fps,
                target_fps,
            } => write!(
                formatter,
                "encoder candidate {candidate_index} offers {candidate_fps} fps for monitor {} below its {target_fps} fps activity target",
                session_monitor_id.get()
            ),
            Self::FullColorDowngrade {
                candidate_index,
                session_monitor_id,
            } => write!(
                formatter,
                "encoder candidate {candidate_index} downgraded full-color monitor {} below 4:4:4",
                session_monitor_id.get()
            ),
            Self::WorkloadExceedsFrameBound {
                session_monitor_id,
                required,
                maximum,
            } => write!(
                formatter,
                "representative workload for monitor {} needs {required} frames, exceeding configured bound {maximum}",
                session_monitor_id.get()
            ),
        }
    }
}

impl Error for EncoderAdmissionError {}

/// Concurrently measures candidates in host order and returns the first
/// complete passing set.
///
/// Candidate zero is the host's exact primary plan. A later passing candidate
/// is returned as [`EncoderSetDecision::Reassign`]. If every candidate fails,
/// the result is one atomic [`EncoderSetDecision::Reject`]; no subset can be
/// selected.
///
/// # Errors
///
/// Returns before invoking the adapter when thresholds, profiles, bindings,
/// candidate topology, service rate, or full-color invariants are invalid.
pub fn admit_encoder_sets<A: EncoderMeasurementAdapter>(
    candidates: Vec<EncoderSetCandidate>,
    profiles: &RegionActivityProfiles,
    thresholds: EncoderAdmissionThresholds,
    adapter: &A,
) -> Result<EncoderSetDecision, EncoderAdmissionError> {
    thresholds.validate()?;
    validate_candidates(&candidates, profiles)?;

    let mut attempts = Vec::with_capacity(candidates.len());
    for (candidate_index, candidate) in candidates.into_iter().enumerate() {
        let outcome =
            measure_candidate(candidate_index, &candidate, profiles, thresholds, adapter)?;
        let passed = outcome.passed();
        attempts.push(EncoderSetAttempt {
            candidate_index,
            candidate,
            outcome,
        });
        if passed {
            return Ok(if candidate_index == 0 {
                EncoderSetDecision::Accept {
                    selected_candidate_index: candidate_index,
                    attempts,
                }
            } else {
                EncoderSetDecision::Reassign {
                    selected_candidate_index: candidate_index,
                    attempts,
                }
            });
        }
    }
    Ok(EncoderSetDecision::Reject { attempts })
}

fn validate_candidates(
    candidates: &[EncoderSetCandidate],
    profiles: &RegionActivityProfiles,
) -> Result<(), EncoderAdmissionError> {
    if candidates.is_empty() || candidates.len() > MAX_ENCODER_SET_CANDIDATES {
        return Err(EncoderAdmissionError::InvalidCandidateCount(
            candidates.len(),
        ));
    }
    let baseline = candidates[0].roster().plans();
    if baseline.len() != profiles.profiles().len()
        || baseline
            .iter()
            .any(|plan| profiles.profile(plan.session_monitor_id).is_none())
    {
        return Err(EncoderAdmissionError::ActivityRosterMismatch);
    }

    for (candidate_index, candidate) in candidates.iter().enumerate() {
        let plans = candidate.roster().plans();
        if plans.len() != baseline.len() {
            return Err(EncoderAdmissionError::ActivityRosterMismatch);
        }
        for (baseline_plan, plan) in baseline.iter().zip(plans) {
            if baseline_plan.session_monitor_id != plan.session_monitor_id {
                return Err(EncoderAdmissionError::CandidateRosterMismatch {
                    candidate_index,
                    session_monitor_id: plan.session_monitor_id,
                });
            }
            if baseline_plan.width != plan.width || baseline_plan.height != plan.height {
                return Err(EncoderAdmissionError::CandidateGeometryChanged {
                    candidate_index,
                    session_monitor_id: plan.session_monitor_id,
                });
            }
            if baseline_plan.stream_epoch != plan.stream_epoch {
                return Err(EncoderAdmissionError::CandidateEpochChanged {
                    candidate_index,
                    session_monitor_id: plan.session_monitor_id,
                });
            }
            let profile = profiles
                .profile(plan.session_monitor_id)
                .ok_or(EncoderAdmissionError::ActivityRosterMismatch)?;
            if plan.fps < profile.target_fps {
                return Err(EncoderAdmissionError::CandidateFpsBelowTarget {
                    candidate_index,
                    session_monitor_id: plan.session_monitor_id,
                    candidate_fps: plan.fps,
                    target_fps: profile.target_fps,
                });
            }
            if profile.priority == RegionAdmissionPriority::FullColorRequired
                && plan.video.chroma != ChromaSubsampling::Yuv444
            {
                return Err(EncoderAdmissionError::FullColorDowngrade {
                    candidate_index,
                    session_monitor_id: plan.session_monitor_id,
                });
            }
        }
    }
    Ok(())
}

fn measure_candidate<A: EncoderMeasurementAdapter>(
    candidate_index: usize,
    candidate: &EncoderSetCandidate,
    profiles: &RegionActivityProfiles,
    thresholds: EncoderAdmissionThresholds,
    adapter: &A,
) -> Result<EncoderSetAttemptOutcome, EncoderAdmissionError> {
    let mut requests = Vec::with_capacity(candidate.roster().plans().len());
    for plan in candidate.roster().plans() {
        let activity = profiles
            .profile(plan.session_monitor_id)
            .ok_or(EncoderAdmissionError::ActivityRosterMismatch)?;
        let binding_id = candidate
            .binding(plan.session_monitor_id)
            .ok_or(EncoderAdmissionError::MissingBinding(
                plan.session_monitor_id,
            ))?
            .clone();
        requests.push(EncoderProbeRequest {
            candidate_index,
            plan: *plan,
            binding_id,
            activity,
            measurement_window: thresholds.measurement_window,
            max_probe_duration: thresholds.max_probe_duration,
            warmup_frames: thresholds.warmup_frames,
            sample_frames: representative_frames(activity, thresholds)?,
        });
    }

    let measured = std::thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(requests.len()));
        let mut handles = Vec::with_capacity(requests.len());
        for request in requests {
            let fallback = request.clone();
            let barrier = Arc::clone(&barrier);
            handles.push((
                fallback,
                scope.spawn(move || {
                    barrier.wait();
                    let result = adapter.measure(&request);
                    (request, result)
                }),
            ));
        }
        handles
            .into_iter()
            .map(|(fallback, handle)| match handle.join() {
                Ok((request, result)) => (request, result),
                Err(_) => (
                    fallback,
                    Err(EncoderProbeFailure {
                        kind: EncoderProbeFailureKind::Panicked,
                        detail: "measurement adapter panicked".to_owned(),
                    }),
                ),
            })
            .collect::<Vec<_>>()
    });

    let mut failures = Vec::new();
    let mut builds = Vec::with_capacity(measured.len());
    for (request, result) in measured {
        match result.and_then(|trace| measurements_for_trace(&request, &trace)) {
            Ok(build) => builds.push(build),
            Err(failure) => failures.push(RegionProbeFailure {
                session_monitor_id: request.plan.session_monitor_id,
                failure,
            }),
        }
    }
    if !failures.is_empty() {
        return Ok(EncoderSetAttemptOutcome::ProbeFailed { failures });
    }

    let measurements = aggregate_measurements(builds);
    let violations = threshold_violations(&measurements, profiles, thresholds);
    if violations.is_empty() {
        Ok(EncoderSetAttemptOutcome::Passed(measurements))
    } else {
        Ok(EncoderSetAttemptOutcome::ThresholdFailed {
            measurements,
            violations,
        })
    }
}

fn representative_frames(
    profile: RegionActivityProfile,
    thresholds: EncoderAdmissionThresholds,
) -> Result<Vec<RepresentativeFrame>, EncoderAdmissionError> {
    let window_nanos = thresholds.measurement_window.as_nanos();
    let required = (u128::from(profile.target_fps) * window_nanos)
        .div_ceil(NANOS_PER_SECOND)
        .max(1);
    if required > u128::from(thresholds.max_sample_frames_per_region) {
        return Err(EncoderAdmissionError::WorkloadExceedsFrameBound {
            session_monitor_id: profile.session_monitor_id,
            required,
            maximum: thresholds.max_sample_frames_per_region,
        });
    }
    let count =
        u16::try_from(required).map_err(|_| EncoderAdmissionError::WorkloadExceedsFrameBound {
            session_monitor_id: profile.session_monitor_id,
            required,
            maximum: thresholds.max_sample_frames_per_region,
        })?;
    let (kind, dirty_ratio) = match profile.activity_class {
        ActivityClass::Idle | ActivityClass::Sparse => {
            (RepresentativeFrameKind::Sparse, profile.dirty_ratio)
        }
        ActivityClass::Scroll | ActivityClass::FullMotion => {
            (RepresentativeFrameKind::FullMotion, DirtyRatio::FULL)
        }
    };
    Ok((0..count)
        .map(|sequence| RepresentativeFrame {
            sequence,
            kind,
            dirty_ratio,
        })
        .collect())
}

fn measurements_for_trace(
    request: &EncoderProbeRequest,
    trace: &EncoderProbeTrace,
) -> Result<RegionMeasurementBuild, EncoderProbeFailure> {
    if trace.elapsed.is_zero() {
        return Err(EncoderProbeFailure::invalid(
            "measurement elapsed time is zero",
        ));
    }
    if trace.elapsed > request.max_probe_duration {
        return Err(EncoderProbeFailure::deadline(format!(
            "measurement elapsed {:?}, deadline {:?}",
            trace.elapsed, request.max_probe_duration
        )));
    }
    if trace.samples.len() != request.sample_frames.len() {
        return Err(EncoderProbeFailure::invalid(format!(
            "adapter returned {} samples for {} requested frames",
            trace.samples.len(),
            request.sample_frames.len()
        )));
    }

    let mut encode_latencies = Vec::with_capacity(trace.samples.len());
    let mut queue_ages = Vec::with_capacity(trace.samples.len());
    let mut delivered_frames = 0u16;
    for (expected, sample) in request.sample_frames.iter().zip(&trace.samples) {
        if sample.sequence != expected.sequence || sample.kind != expected.kind {
            return Err(EncoderProbeFailure::invalid(format!(
                "sample {} does not match requested sequence/kind",
                sample.sequence
            )));
        }
        let total = sample
            .queue_age
            .checked_add(sample.encode_latency)
            .ok_or_else(|| EncoderProbeFailure::invalid("sample duration overflow"))?;
        if total > trace.elapsed {
            return Err(EncoderProbeFailure::invalid(format!(
                "sample {} queue+encode time {:?} exceeds trace elapsed {:?}",
                sample.sequence, total, trace.elapsed
            )));
        }
        encode_latencies.push(sample.encode_latency);
        queue_ages.push(sample.queue_age);
        if sample.delivered {
            delivered_frames = delivered_frames.saturating_add(1);
        }
    }
    encode_latencies.sort_unstable();
    queue_ages.sort_unstable();
    let offered_frames = u16::try_from(trace.samples.len())
        .map_err(|_| EncoderProbeFailure::invalid("sample count exceeds u16"))?;
    let delivered_millifps = delivered_millifps(delivered_frames, trace.elapsed);
    let delivery_ratio_basis_points =
        ratio_basis_points(u32::from(delivered_frames), u32::from(offered_frames));
    let measurements = RegionEncoderMeasurements {
        session_monitor_id: request.plan.session_monitor_id,
        offered_frames,
        delivered_frames,
        elapsed: trace.elapsed,
        p50_encode_latency: percentile(&encode_latencies, 50),
        p95_encode_latency: percentile(&encode_latencies, 95),
        p50_queue_age: percentile(&queue_ages, 50),
        p95_queue_age: percentile(&queue_ages, 95),
        delivered_millifps,
        delivery_ratio_basis_points,
    };
    Ok(RegionMeasurementBuild {
        measurements,
        encode_latencies,
        queue_ages,
    })
}

#[derive(Debug)]
struct RegionMeasurementBuild {
    measurements: RegionEncoderMeasurements,
    encode_latencies: Vec<Duration>,
    queue_ages: Vec<Duration>,
}

fn aggregate_measurements(builds: Vec<RegionMeasurementBuild>) -> EncoderSetMeasurements {
    let mut encode_latencies = Vec::new();
    let mut queue_ages = Vec::new();
    let mut delivered_millifps = 0u32;
    let mut ratios = Vec::with_capacity(builds.len());
    let mut per_region = Vec::with_capacity(builds.len());
    for mut build in builds {
        encode_latencies.append(&mut build.encode_latencies);
        queue_ages.append(&mut build.queue_ages);
        delivered_millifps =
            delivered_millifps.saturating_add(build.measurements.delivered_millifps);
        ratios.push(build.measurements.delivery_ratio_basis_points);
        per_region.push(build.measurements);
    }
    encode_latencies.sort_unstable();
    queue_ages.sort_unstable();
    EncoderSetMeasurements {
        p50_encode_latency: percentile(&encode_latencies, 50),
        p95_encode_latency: percentile(&encode_latencies, 95),
        p50_queue_age: percentile(&queue_ages, 50),
        p95_queue_age: percentile(&queue_ages, 95),
        delivered_millifps,
        fairness_basis_points: jains_fairness_basis_points(&ratios),
        per_region,
    }
}

fn threshold_violations(
    measurements: &EncoderSetMeasurements,
    profiles: &RegionActivityProfiles,
    thresholds: EncoderAdmissionThresholds,
) -> Vec<EncoderThresholdViolation> {
    let mut violations = Vec::new();
    for region in &measurements.per_region {
        if region.p95_encode_latency > thresholds.max_p95_encode_latency {
            violations.push(EncoderThresholdViolation::EncodeLatency {
                session_monitor_id: region.session_monitor_id,
                measured_p95: region.p95_encode_latency,
                maximum_p95: thresholds.max_p95_encode_latency,
            });
        }
        if region.p95_queue_age > thresholds.max_p95_queue_age {
            violations.push(EncoderThresholdViolation::QueueAge {
                session_monitor_id: region.session_monitor_id,
                measured_p95: region.p95_queue_age,
                maximum_p95: thresholds.max_p95_queue_age,
            });
        }
        if let Some(profile) = profiles.profile(region.session_monitor_id) {
            let target_millifps = profile.target_fps.saturating_mul(1_000);
            let required_millifps =
                scale_basis_points_ceil(target_millifps, thresholds.min_delivered_fps_basis_points);
            if region.delivered_millifps < required_millifps {
                violations.push(EncoderThresholdViolation::DeliveredFps {
                    session_monitor_id: region.session_monitor_id,
                    measured_millifps: region.delivered_millifps,
                    required_millifps,
                });
            }
        }
    }
    if measurements.fairness_basis_points < thresholds.min_fairness_basis_points {
        violations.push(EncoderThresholdViolation::Fairness {
            measured_basis_points: measurements.fairness_basis_points,
            required_basis_points: thresholds.min_fairness_basis_points,
        });
    }
    violations
}

fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn delivered_millifps(delivered_frames: u16, elapsed: Duration) -> u32 {
    if delivered_frames == 0 || elapsed.is_zero() {
        return 0;
    }
    let value = u128::from(delivered_frames)
        .saturating_mul(MILLIFPS_SCALE)
        .saturating_mul(NANOS_PER_SECOND)
        / elapsed.as_nanos();
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn ratio_basis_points(numerator: u32, denominator: u32) -> u16 {
    if denominator == 0 {
        return BASIS_POINTS_U16;
    }
    let rounded = (u128::from(numerator) * u128::from(BASIS_POINTS) + u128::from(denominator) / 2)
        / u128::from(denominator);
    u16::try_from(rounded.min(u128::from(BASIS_POINTS))).unwrap_or(BASIS_POINTS_U16)
}

fn scale_basis_points_ceil(value: u32, basis_points: u16) -> u32 {
    let numerator = u128::from(value) * u128::from(basis_points);
    let scaled = numerator.div_ceil(u128::from(BASIS_POINTS));
    u32::try_from(scaled).unwrap_or(u32::MAX)
}

fn jains_fairness_basis_points(values: &[u16]) -> u16 {
    if values.is_empty() {
        return BASIS_POINTS_U16;
    }
    let sum = values.iter().map(|value| u128::from(*value)).sum::<u128>();
    let sum_squares = values
        .iter()
        .map(|value| u128::from(*value).pow(2))
        .sum::<u128>();
    if sum_squares == 0 {
        return BASIS_POINTS_U16;
    }
    let denominator = u128::try_from(values.len())
        .unwrap_or(u128::MAX)
        .saturating_mul(sum_squares);
    let numerator = sum
        .saturating_mul(sum)
        .saturating_mul(u128::from(BASIS_POINTS));
    let rounded = (numerator + denominator / 2) / denominator;
    u16::try_from(rounded.min(u128::from(BASIS_POINTS))).unwrap_or(BASIS_POINTS_U16)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::video::EncoderBackend;
    use crate::{MediaStreamEpoch, VideoCodec, VideoConfiguration};

    use super::*;

    #[derive(Clone, Copy)]
    enum Behavior {
        Healthy,
        Stall865Ms,
        DeliverQuarter,
        FailOpen,
    }

    struct FakeAdapter {
        behavior: BTreeMap<(usize, u16), Behavior>,
        calls: AtomicUsize,
    }

    impl FakeAdapter {
        fn new(behavior: BTreeMap<(usize, u16), Behavior>) -> Self {
            Self {
                behavior,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl EncoderMeasurementAdapter for FakeAdapter {
        fn measure(
            &self,
            request: &EncoderProbeRequest,
        ) -> Result<EncoderProbeTrace, EncoderProbeFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let behavior = self
                .behavior
                .get(&(
                    request.candidate_index,
                    request.plan.session_monitor_id.get(),
                ))
                .copied()
                .unwrap_or(Behavior::Healthy);
            if matches!(behavior, Behavior::FailOpen) {
                return Err(EncoderProbeFailure::context_open(
                    "synthetic context open failed",
                ));
            }
            let encode_latency = if matches!(behavior, Behavior::Stall865Ms) {
                Duration::from_millis(865)
            } else {
                Duration::from_millis(5)
            };
            let elapsed = if matches!(behavior, Behavior::Stall865Ms) {
                encode_latency
                    .checked_mul(
                        u32::try_from(request.sample_frames.len())
                            .expect("bounded sample count fits u32"),
                    )
                    .expect("bounded test elapsed")
            } else {
                request.measurement_window
            };
            let samples = request
                .sample_frames
                .iter()
                .map(|frame| EncoderProbeSample {
                    sequence: frame.sequence,
                    kind: frame.kind,
                    queue_age: Duration::from_millis(1),
                    encode_latency,
                    delivered: !matches!(behavior, Behavior::DeliverQuarter)
                        || frame.sequence % 4 == 0,
                })
                .collect();
            Ok(EncoderProbeTrace { elapsed, samples })
        }
    }

    struct ConcurrentAdapter {
        entered: Arc<Barrier>,
        active: AtomicUsize,
        maximum: AtomicUsize,
    }

    impl EncoderMeasurementAdapter for ConcurrentAdapter {
        fn measure(
            &self,
            request: &EncoderProbeRequest,
        ) -> Result<EncoderProbeTrace, EncoderProbeFailure> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            self.entered.wait();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(healthy_trace(request))
        }
    }

    struct RecordingAdapter {
        kinds: Mutex<Vec<RepresentativeFrameKind>>,
    }

    impl EncoderMeasurementAdapter for RecordingAdapter {
        fn measure(
            &self,
            request: &EncoderProbeRequest,
        ) -> Result<EncoderProbeTrace, EncoderProbeFailure> {
            self.kinds
                .lock()
                .expect("recording lock")
                .extend(request.sample_frames.iter().map(|frame| frame.kind));
            Ok(healthy_trace(request))
        }
    }

    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("monitor id")
    }

    fn profile(
        monitor_id: u16,
        class: ActivityClass,
        target_fps: u32,
        priority: RegionAdmissionPriority,
    ) -> RegionActivityProfile {
        RegionActivityProfile {
            session_monitor_id: sid(monitor_id),
            region_generation: RegionGeneration::new(1).expect("generation"),
            region_id: RegionId::new(u32::from(monitor_id)).expect("region id"),
            activity_class: class,
            dirty_ratio: if class == ActivityClass::FullMotion {
                DirtyRatio::FULL
            } else {
                DirtyRatio::ZERO
            },
            target_fps,
            priority,
        }
    }

    fn profiles(values: Vec<RegionActivityProfile>) -> RegionActivityProfiles {
        RegionActivityProfiles::new(values).expect("profiles")
    }

    fn video(chroma: ChromaSubsampling) -> VideoConfiguration {
        VideoConfiguration {
            codec: if chroma == ChromaSubsampling::Yuv444 {
                VideoCodec::H265
            } else {
                VideoCodec::H264
            },
            chroma,
            ..VideoConfiguration::legacy_h264()
        }
    }

    fn candidate(backends: &[(EncoderBackend, ChromaSubsampling, u32)]) -> EncoderSetCandidate {
        let plans = backends
            .iter()
            .enumerate()
            .map(|(index, (backend, chroma, fps))| {
                let monitor_id = sid(u16::try_from(index + 1).expect("bounded index"));
                RegionMediaPlan::new(
                    monitor_id,
                    MediaStreamEpoch::new(7).expect("epoch"),
                    *backend,
                    video(*chroma),
                    1_920,
                    1_080,
                    *fps,
                    crate::BitrateBudgetKbps::nominal_for_geometry(1_920, 1_080, *fps),
                )
                .expect("plan")
            })
            .collect::<Vec<_>>();
        let bindings = plans
            .iter()
            .map(|plan| RegionEncoderBinding {
                session_monitor_id: plan.session_monitor_id,
                binding_id: EncoderBindingId::new(format!(
                    "test:{}:{}",
                    plan.session_monitor_id.get(),
                    plan.backend.ready_token()
                ))
                .expect("binding"),
            })
            .collect();
        EncoderSetCandidate::new(RegionMediaRoster::new(plans).expect("roster"), bindings)
            .expect("candidate")
    }

    fn thresholds() -> EncoderAdmissionThresholds {
        EncoderAdmissionThresholds {
            measurement_window: Duration::from_secs(1),
            max_probe_duration: Duration::from_secs(10),
            warmup_frames: 2,
            max_sample_frames_per_region: 120,
            max_p95_encode_latency: Duration::from_millis(20),
            max_p95_queue_age: Duration::from_millis(10),
            min_delivered_fps_basis_points: 9_000,
            min_fairness_basis_points: 9_500,
        }
    }

    fn healthy_trace(request: &EncoderProbeRequest) -> EncoderProbeTrace {
        EncoderProbeTrace {
            elapsed: request.measurement_window,
            samples: request
                .sample_frames
                .iter()
                .map(|frame| EncoderProbeSample {
                    sequence: frame.sequence,
                    kind: frame.kind,
                    queue_age: Duration::from_millis(1),
                    encode_latency: Duration::from_millis(5),
                    delivered: true,
                })
                .collect(),
        }
    }

    #[test]
    fn context_that_opens_but_stalls_865_ms_is_reassigned() {
        let candidates = vec![
            candidate(&[(EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 4)]),
            candidate(&[(EncoderBackend::OpenH264, ChromaSubsampling::Yuv420, 4)]),
        ];
        let profiles = profiles(vec![profile(
            1,
            ActivityClass::FullMotion,
            4,
            RegionAdmissionPriority::Standard,
        )]);
        let adapter = FakeAdapter::new(BTreeMap::from([((0, 1), Behavior::Stall865Ms)]));

        let decision =
            admit_encoder_sets(candidates, &profiles, thresholds(), &adapter).expect("decision");
        assert!(matches!(
            decision,
            EncoderSetDecision::Reassign {
                selected_candidate_index: 1,
                ..
            }
        ));
        let first = &decision.attempts()[0].outcome;
        let EncoderSetAttemptOutcome::ThresholdFailed {
            measurements,
            violations,
        } = first
        else {
            panic!("primary candidate should fail measured thresholds");
        };
        assert_eq!(
            measurements.per_region[0].p95_encode_latency,
            Duration::from_millis(865)
        );
        assert!(
            violations.iter().any(|violation| matches!(
                violation,
                EncoderThresholdViolation::EncodeLatency { .. }
            ))
        );
    }

    #[test]
    fn one_active_region_and_idle_siblings_remain_fair() {
        let candidate = candidate(&[
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 60),
            (EncoderBackend::OpenH264, ChromaSubsampling::Yuv420, 30),
            (EncoderBackend::OpenH264, ChromaSubsampling::Yuv420, 30),
        ]);
        let profiles = profiles(vec![
            profile(
                1,
                ActivityClass::FullMotion,
                60,
                RegionAdmissionPriority::Standard,
            ),
            profile(2, ActivityClass::Idle, 1, RegionAdmissionPriority::Standard),
            profile(3, ActivityClass::Idle, 1, RegionAdmissionPriority::Standard),
        ]);
        let adapter = FakeAdapter::new(BTreeMap::new());

        let decision =
            admit_encoder_sets(vec![candidate], &profiles, thresholds(), &adapter).expect("accept");
        let EncoderSetAttemptOutcome::Passed(measurements) = &decision.attempts()[0].outcome else {
            panic!("fully delivered active/idle workload should pass");
        };
        assert_eq!(measurements.fairness_basis_points, 10_000);
        assert_eq!(measurements.per_region[0].offered_frames, 60);
        assert_eq!(measurements.per_region[1].offered_frames, 1);
        assert_eq!(measurements.per_region[2].offered_frames, 1);
    }

    #[test]
    fn all_motion_starvation_fails_fairness() {
        let candidate = candidate(&[
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 60),
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 60),
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 60),
        ]);
        let profiles = profiles(vec![
            profile(
                1,
                ActivityClass::FullMotion,
                60,
                RegionAdmissionPriority::Standard,
            ),
            profile(
                2,
                ActivityClass::FullMotion,
                60,
                RegionAdmissionPriority::Standard,
            ),
            profile(
                3,
                ActivityClass::FullMotion,
                60,
                RegionAdmissionPriority::Standard,
            ),
        ]);
        let adapter = FakeAdapter::new(BTreeMap::from([((0, 2), Behavior::DeliverQuarter)]));

        let decision =
            admit_encoder_sets(vec![candidate], &profiles, thresholds(), &adapter).expect("reject");
        assert!(matches!(decision, EncoderSetDecision::Reject { .. }));
        let EncoderSetAttemptOutcome::ThresholdFailed {
            measurements,
            violations,
        } = &decision.attempts()[0].outcome
        else {
            panic!("unfair all-motion delivery should fail thresholds");
        };
        assert!(measurements.fairness_basis_points < 9_500);
        assert!(
            violations
                .iter()
                .any(|violation| matches!(violation, EncoderThresholdViolation::Fairness { .. }))
        );
    }

    #[test]
    fn full_color_priority_cannot_be_downgraded_by_reassignment() {
        let candidates = vec![
            candidate(&[
                (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv444, 60),
                (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 60),
            ]),
            candidate(&[
                (EncoderBackend::OpenH264, ChromaSubsampling::Yuv420, 30),
                (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 60),
            ]),
        ];
        let profiles = profiles(vec![
            profile(
                1,
                ActivityClass::Sparse,
                30,
                RegionAdmissionPriority::FullColorRequired,
            ),
            profile(
                2,
                ActivityClass::Sparse,
                30,
                RegionAdmissionPriority::Standard,
            ),
        ]);
        let adapter = FakeAdapter::new(BTreeMap::new());

        let error = admit_encoder_sets(candidates, &profiles, thresholds(), &adapter)
            .expect_err("4:4:4 downgrade must fail before measurement");
        assert_eq!(
            error,
            EncoderAdmissionError::FullColorDowngrade {
                candidate_index: 1,
                session_monitor_id: sid(1),
            }
        );
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn one_failed_context_rejects_the_complete_set_atomically() {
        let candidate = candidate(&[
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 30),
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 30),
        ]);
        let profiles = profiles(vec![
            profile(
                1,
                ActivityClass::Sparse,
                30,
                RegionAdmissionPriority::Standard,
            ),
            profile(
                2,
                ActivityClass::Sparse,
                30,
                RegionAdmissionPriority::Standard,
            ),
        ]);
        let adapter = FakeAdapter::new(BTreeMap::from([((0, 2), Behavior::FailOpen)]));

        let decision =
            admit_encoder_sets(vec![candidate], &profiles, thresholds(), &adapter).expect("reject");
        assert_eq!(decision.selected_candidate_index(), None);
        assert!(matches!(decision, EncoderSetDecision::Reject { .. }));
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn every_context_in_one_candidate_is_exercised_concurrently() {
        let candidate = candidate(&[
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 30),
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 30),
        ]);
        let profiles = profiles(vec![
            profile(
                1,
                ActivityClass::Sparse,
                30,
                RegionAdmissionPriority::Standard,
            ),
            profile(
                2,
                ActivityClass::Sparse,
                30,
                RegionAdmissionPriority::Standard,
            ),
        ]);
        let adapter = ConcurrentAdapter {
            entered: Arc::new(Barrier::new(2)),
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        };

        let decision =
            admit_encoder_sets(vec![candidate], &profiles, thresholds(), &adapter).expect("accept");
        assert!(matches!(decision, EncoderSetDecision::Accept { .. }));
        assert_eq!(adapter.maximum.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn activity_class_selects_sparse_or_full_motion_frames() {
        let candidate = candidate(&[
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 2),
            (EncoderBackend::NativeNvenc, ChromaSubsampling::Yuv420, 2),
        ]);
        let profiles = profiles(vec![
            profile(1, ActivityClass::Idle, 2, RegionAdmissionPriority::Standard),
            profile(
                2,
                ActivityClass::FullMotion,
                2,
                RegionAdmissionPriority::Standard,
            ),
        ]);
        let adapter = RecordingAdapter {
            kinds: Mutex::new(Vec::new()),
        };

        let decision =
            admit_encoder_sets(vec![candidate], &profiles, thresholds(), &adapter).expect("accept");
        assert!(matches!(decision, EncoderSetDecision::Accept { .. }));
        let kinds = adapter.kinds.lock().expect("recording lock");
        assert!(kinds.contains(&RepresentativeFrameKind::Sparse));
        assert!(kinds.contains(&RepresentativeFrameKind::FullMotion));
    }
}
