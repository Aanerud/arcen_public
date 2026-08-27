//! Host-neutral per-region capture/encode service scheduling.
//!
//! [`RegionActivityScheduler`] is the product adopter of
//! [`RegionActivityGrid`](crate::RegionActivityGrid): it binds one region's
//! 16x16 changed-tile grid to Keel's [`IdleCadence`] keepalive state machine
//! and turns the measured damage of the frames a capture pipeline already
//! produces into one explicit serve/skip decision plus bounded per-region
//! diagnostics.
//!
//! The scheduler exists to avoid encode/convert/emit work on *static* regions
//! only. It never relaxes a correctness guarantee:
//!
//! - the first frame after construction, [`RegionActivityScheduler::reset`],
//!   or [`RegionActivityScheduler::rebind`] is always served as a keyframe;
//! - a host-forced keyframe ([`ForcedKeyframe`]) is always served;
//! - a region is always served at least once per
//!   [`RegionSchedulePolicy::keyframe_interval`] as a keyframe and at least
//!   once per [`RegionSchedulePolicy::max_idle_refresh`] as a refresh, so a
//!   suppressed region can never be starved;
//! - any measured damage is served immediately;
//! - input/focus activity wakes the region for
//!   [`RegionSchedulePolicy::input_wake_grace`] even when the captured pixels
//!   have not changed yet;
//! - [`RegionServiceDecision::is_mandatory`] marks the decisions a host
//!   optimizer, admission gate, or aggregate scheduler must never drop.
//!
//! Per-region state is fully independent: one region's motion cannot change
//! another region's cadence, deadlines, or telemetry. Cross-region delivery
//! *ordering* stays with the hosts' shared fair roster; this module only
//! decides whether one region has work worth serving, and by when it must be
//! served regardless.
//!
//! Encoder and chroma policy remain host authoritative. Nothing here selects a
//! backend, changes a negotiated frame rate, or bypasses encoder admission.

use core::time::Duration;
use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_keel::{
    ActivityClass, ActivityHint, BgraFrame, BlockGrid, CadenceRecommendation, DamageMap, EmitMode,
    HashKernel, IdleCadence, KernelPreference,
};

use crate::{
    RegionActivityDiagnostics, RegionActivityError, RegionActivityGrid, RegionActivityOwner,
};

/// Shortest accepted nominal capture interval (1000 fps).
pub const MIN_REGION_FRAME_INTERVAL: Duration = Duration::from_millis(1);
/// Longest accepted nominal capture interval (1 fps).
pub const MAX_REGION_FRAME_INTERVAL: Duration = Duration::from_secs(1);
/// Longest accepted bound between two services of one region.
pub const MAX_REGION_IDLE_REFRESH: Duration = Duration::from_secs(10);
/// Longest accepted bound between two keyframes of one region.
pub const MAX_REGION_KEYFRAME_INTERVAL: Duration = Duration::from_secs(60);
/// Consecutive idle classifications before a static region may back off.
pub const REGION_IDLE_BACKOFF_STREAK: u16 = 4;
/// Largest multiple of the frame interval a static region may back off by.
pub const REGION_IDLE_BACKOFF_FACTOR: u32 = 8;
/// Number of bounded counters in [`RegionScheduleTelemetry::fields`].
pub const REGION_SCHEDULE_TELEMETRY_FIELDS: usize = 15;

/// Rejection building a [`RegionSchedulePolicy`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RegionSchedulePolicyError {
    ZeroTargetFps,
    FrameInterval(Duration),
    IdleRefreshBelowFrameInterval {
        frame_interval: Duration,
        max_idle_refresh: Duration,
    },
    IdleRefreshTooLong(Duration),
    KeyframeIntervalBelowIdleRefresh {
        max_idle_refresh: Duration,
        keyframe_interval: Duration,
    },
    KeyframeIntervalTooLong(Duration),
    InputWakeGraceTooLong {
        input_wake_grace: Duration,
        max_idle_refresh: Duration,
    },
}

impl Display for RegionSchedulePolicyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroTargetFps => formatter.write_str("region target fps must be nonzero"),
            Self::FrameInterval(value) => write!(
                formatter,
                "frame interval {value:?} is outside {MIN_REGION_FRAME_INTERVAL:?}..={MAX_REGION_FRAME_INTERVAL:?}"
            ),
            Self::IdleRefreshBelowFrameInterval {
                frame_interval,
                max_idle_refresh,
            } => write!(
                formatter,
                "max idle refresh {max_idle_refresh:?} is shorter than frame interval {frame_interval:?}"
            ),
            Self::IdleRefreshTooLong(value) => write!(
                formatter,
                "max idle refresh {value:?} exceeds {MAX_REGION_IDLE_REFRESH:?}"
            ),
            Self::KeyframeIntervalBelowIdleRefresh {
                max_idle_refresh,
                keyframe_interval,
            } => write!(
                formatter,
                "keyframe interval {keyframe_interval:?} is shorter than max idle refresh {max_idle_refresh:?}"
            ),
            Self::KeyframeIntervalTooLong(value) => write!(
                formatter,
                "keyframe interval {value:?} exceeds {MAX_REGION_KEYFRAME_INTERVAL:?}"
            ),
            Self::InputWakeGraceTooLong {
                input_wake_grace,
                max_idle_refresh,
            } => write!(
                formatter,
                "input wake grace {input_wake_grace:?} exceeds max idle refresh {max_idle_refresh:?}"
            ),
        }
    }
}

impl Error for RegionSchedulePolicyError {}

/// Validated, host-supplied service bounds for one region.
///
/// Every bound is a *correctness floor*, not a target: the scheduler may only
/// ever serve a region more often than these bounds require, never less.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionSchedulePolicy {
    frame_interval: Duration,
    max_idle_refresh: Duration,
    keyframe_interval: Duration,
    input_wake_grace: Duration,
}

impl RegionSchedulePolicy {
    /// Validates one region's service bounds.
    ///
    /// # Errors
    ///
    /// Returns [`RegionSchedulePolicyError`] when the frame interval is out of
    /// range, when a longer bound is shorter than the bound it must contain,
    /// or when a bound exceeds its documented ceiling.
    pub fn new(
        frame_interval: Duration,
        max_idle_refresh: Duration,
        keyframe_interval: Duration,
        input_wake_grace: Duration,
    ) -> Result<Self, RegionSchedulePolicyError> {
        if frame_interval < MIN_REGION_FRAME_INTERVAL || frame_interval > MAX_REGION_FRAME_INTERVAL
        {
            return Err(RegionSchedulePolicyError::FrameInterval(frame_interval));
        }
        if max_idle_refresh < frame_interval {
            return Err(RegionSchedulePolicyError::IdleRefreshBelowFrameInterval {
                frame_interval,
                max_idle_refresh,
            });
        }
        if max_idle_refresh > MAX_REGION_IDLE_REFRESH {
            return Err(RegionSchedulePolicyError::IdleRefreshTooLong(
                max_idle_refresh,
            ));
        }
        if keyframe_interval < max_idle_refresh {
            return Err(
                RegionSchedulePolicyError::KeyframeIntervalBelowIdleRefresh {
                    max_idle_refresh,
                    keyframe_interval,
                },
            );
        }
        if keyframe_interval > MAX_REGION_KEYFRAME_INTERVAL {
            return Err(RegionSchedulePolicyError::KeyframeIntervalTooLong(
                keyframe_interval,
            ));
        }
        if input_wake_grace > max_idle_refresh {
            return Err(RegionSchedulePolicyError::InputWakeGraceTooLong {
                input_wake_grace,
                max_idle_refresh,
            });
        }
        Ok(Self {
            frame_interval,
            max_idle_refresh,
            keyframe_interval,
            input_wake_grace,
        })
    }

    /// Derives the frame interval from a region's negotiated target rate.
    ///
    /// # Errors
    ///
    /// Returns [`RegionSchedulePolicyError::ZeroTargetFps`] for a zero rate,
    /// otherwise the errors documented by [`Self::new`].
    pub fn from_target_fps(
        target_fps: u32,
        max_idle_refresh: Duration,
        keyframe_interval: Duration,
        input_wake_grace: Duration,
    ) -> Result<Self, RegionSchedulePolicyError> {
        if target_fps == 0 {
            return Err(RegionSchedulePolicyError::ZeroTargetFps);
        }
        let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(target_fps));
        Self::new(
            frame_interval,
            max_idle_refresh,
            keyframe_interval,
            input_wake_grace,
        )
    }

    #[must_use]
    pub const fn frame_interval(self) -> Duration {
        self.frame_interval
    }

    #[must_use]
    pub const fn max_idle_refresh(self) -> Duration {
        self.max_idle_refresh
    }

    #[must_use]
    pub const fn keyframe_interval(self) -> Duration {
        self.keyframe_interval
    }

    #[must_use]
    pub const fn input_wake_grace(self) -> Duration {
        self.input_wake_grace
    }
}

/// Why a host demanded an immediate keyframe for this region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForcedKeyframe {
    /// Pipeline start or first client attachment.
    Startup,
    /// Client or transport requested recovery (IDR).
    ClientRequest,
    /// Capture recovered after loss, modeset, or geometry change.
    Recovery,
    /// Region plan, encoder, or chroma policy was reconfigured.
    Reconfigure,
}

impl ForcedKeyframe {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::ClientRequest => "client-request",
            Self::Recovery => "recovery",
            Self::Reconfigure => "reconfigure",
        }
    }
}

/// Host-authoritative facts for one capture tick.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RegionScheduleSignals {
    /// Source knowledge that hashes cannot infer (scroll/moved rectangles).
    pub hint: ActivityHint,
    /// A keyframe the host requires regardless of measured activity.
    pub forced_keyframe: Option<ForcedKeyframe>,
    /// Input, pointer, or focus activity targeting this region.
    pub input_activity: bool,
}

impl RegionScheduleSignals {
    #[must_use]
    pub const fn keyframe(kind: ForcedKeyframe) -> Self {
        Self {
            hint: ActivityHint::None,
            forced_keyframe: Some(kind),
            input_activity: false,
        }
    }

    #[must_use]
    pub const fn input() -> Self {
        Self {
            hint: ActivityHint::None,
            forced_keyframe: None,
            input_activity: true,
        }
    }

    #[must_use]
    pub const fn with_hint(hint: ActivityHint) -> Self {
        Self {
            hint,
            forced_keyframe: None,
            input_activity: false,
        }
    }
}

/// Whether this region has work worth serving on this tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionServiceAction {
    Serve,
    Skip,
}

/// Why the scheduler reached its [`RegionServiceAction`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionServiceReason {
    /// First frame after construction, reset, or generation rebind.
    StartupBaseline,
    /// Host demanded a keyframe.
    ForcedKeyframe(ForcedKeyframe),
    /// The bounded keyframe interval elapsed.
    KeyframeDeadline,
    /// The region's own pixels changed.
    Damage,
    /// Input or focus activity within the wake grace.
    InputWake,
    /// The bounded max-idle refresh elapsed with no measured change.
    MaxIdleRefresh,
    /// Static content with no pending deadline; encode work was avoided.
    StaticSuppressed,
}

impl RegionServiceReason {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::StartupBaseline => "startup-baseline",
            Self::ForcedKeyframe(kind) => kind.name(),
            Self::KeyframeDeadline => "keyframe-deadline",
            Self::Damage => "damage",
            Self::InputWake => "input-wake",
            Self::MaxIdleRefresh => "max-idle-refresh",
            Self::StaticSuppressed => "static-suppressed",
        }
    }

    /// True when serving this reason requires a full independently decodable
    /// picture.
    #[must_use]
    pub const fn is_keyframe(self) -> bool {
        matches!(
            self,
            Self::StartupBaseline | Self::ForcedKeyframe(_) | Self::KeyframeDeadline
        )
    }

    /// True when a host, admission gate, or aggregate scheduler must not drop
    /// this service.
    #[must_use]
    pub const fn is_mandatory(self) -> bool {
        self.is_keyframe() || matches!(self, Self::MaxIdleRefresh)
    }
}

/// One region's complete scheduling decision for one capture tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionServiceDecision {
    pub action: RegionServiceAction,
    pub reason: RegionServiceReason,
    /// True when this service must carry a full independently decodable picture.
    pub keyframe: bool,
    /// The region-owned activity snapshot this decision was derived from.
    pub activity: RegionActivityDiagnostics,
    /// Keel's advisory cadence for the observed activity.
    pub cadence: CadenceRecommendation,
    /// Advisory capture interval until the next tick; never past a deadline.
    pub recommended_interval: Duration,
    /// Time left before this region must be served regardless of activity.
    pub deadline_remaining: Duration,
}

impl RegionServiceDecision {
    #[must_use]
    pub const fn serves(self) -> bool {
        matches!(self.action, RegionServiceAction::Serve)
    }

    /// True when this decision must not be dropped by a downstream optimizer.
    #[must_use]
    pub const fn is_mandatory(self) -> bool {
        self.serves() && self.reason.is_mandatory()
    }

    #[must_use]
    pub const fn class(self) -> ActivityClass {
        self.activity.activity.class
    }
}

/// Fixed-size, saturating per-region scheduling counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RegionScheduleTelemetry {
    pub ticks: u64,
    pub served: u64,
    pub skipped: u64,
    pub keyframes: u64,
    pub damage_serves: u64,
    pub input_wakes: u64,
    pub idle_refreshes: u64,
    pub idle_ticks: u64,
    pub sparse_ticks: u64,
    pub scroll_ticks: u64,
    pub full_motion_ticks: u64,
    pub service_failures: u64,
    pub current_skip_streak: u32,
    pub max_skip_streak: u32,
    pub max_service_gap_micros: u64,
}

impl RegionScheduleTelemetry {
    /// Returns every counter as a fixed-size, allocation-free field set.
    #[must_use]
    pub const fn fields(self) -> [(&'static str, u64); REGION_SCHEDULE_TELEMETRY_FIELDS] {
        [
            ("ticks", self.ticks),
            ("served", self.served),
            ("skipped", self.skipped),
            ("keyframes", self.keyframes),
            ("damage_serves", self.damage_serves),
            ("input_wakes", self.input_wakes),
            ("idle_refreshes", self.idle_refreshes),
            ("idle_ticks", self.idle_ticks),
            ("sparse_ticks", self.sparse_ticks),
            ("scroll_ticks", self.scroll_ticks),
            ("full_motion_ticks", self.full_motion_ticks),
            ("service_failures", self.service_failures),
            ("current_skip_streak", self.current_skip_streak as u64),
            ("max_skip_streak", self.max_skip_streak as u64),
            ("max_service_gap_micros", self.max_service_gap_micros),
        ]
    }

    fn record_class(&mut self, class: ActivityClass) {
        let counter = match class {
            ActivityClass::Idle => &mut self.idle_ticks,
            ActivityClass::Sparse => &mut self.sparse_ticks,
            ActivityClass::Scroll => &mut self.scroll_ticks,
            ActivityClass::FullMotion => &mut self.full_motion_ticks,
        };
        *counter = counter.saturating_add(1);
    }
}

/// Bounded per-region scheduling diagnostics for aggregate reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionScheduleSnapshot {
    pub owner: RegionActivityOwner,
    pub activity: RegionActivityDiagnostics,
    pub telemetry: RegionScheduleTelemetry,
    pub cadence: CadenceRecommendation,
    pub recommended_interval: Duration,
    pub deadline_remaining: Duration,
}

/// One region's activity grid, keepalive cadence, and bounded service bounds.
#[derive(Debug)]
pub struct RegionActivityScheduler {
    policy: RegionSchedulePolicy,
    activity: RegionActivityGrid,
    cadence: IdleCadence,
    telemetry: RegionScheduleTelemetry,
    since_service: Duration,
    since_keyframe: Duration,
    since_input: Option<Duration>,
    pending_keyframe: bool,
    committed_since_service: Duration,
    committed_since_keyframe: Duration,
    last_decision: Option<RegionServiceDecision>,
}

impl RegionActivityScheduler {
    /// Allocates one region-owned activity grid and its schedule state.
    ///
    /// # Errors
    ///
    /// Returns the geometry errors documented by
    /// [`RegionActivityGrid::new`].
    pub fn new(
        owner: RegionActivityOwner,
        width: usize,
        height: usize,
        preference: KernelPreference,
        policy: RegionSchedulePolicy,
    ) -> Result<Self, RegionActivityError> {
        Ok(Self::from_activity_grid(
            RegionActivityGrid::new(owner, width, height, preference)?,
            policy,
        ))
    }

    /// Promotes an existing region-owned grid without allocating more storage.
    #[must_use]
    pub fn from_activity_grid(activity: RegionActivityGrid, policy: RegionSchedulePolicy) -> Self {
        Self {
            policy,
            activity,
            cadence: IdleCadence::new(policy.max_idle_refresh),
            telemetry: RegionScheduleTelemetry::default(),
            since_service: Duration::ZERO,
            since_keyframe: Duration::ZERO,
            since_input: None,
            pending_keyframe: true,
            committed_since_service: Duration::ZERO,
            committed_since_keyframe: Duration::ZERO,
            last_decision: None,
        }
    }

    #[must_use]
    pub const fn owner(&self) -> RegionActivityOwner {
        self.activity.owner()
    }

    #[must_use]
    pub const fn policy(&self) -> RegionSchedulePolicy {
        self.policy
    }

    #[must_use]
    pub const fn grid(&self) -> BlockGrid {
        self.activity.grid()
    }

    #[must_use]
    pub const fn kernel(&self) -> HashKernel {
        self.activity.kernel()
    }

    /// The 16x16 changed-tile map backing the last observed frame.
    #[must_use]
    pub fn damage_map(&self) -> DamageMap<'_> {
        self.activity.damage_map()
    }

    #[must_use]
    pub const fn diagnostics(&self) -> RegionActivityDiagnostics {
        self.activity.diagnostics()
    }

    #[must_use]
    pub const fn telemetry(&self) -> RegionScheduleTelemetry {
        self.telemetry
    }

    #[must_use]
    pub const fn last_decision(&self) -> Option<RegionServiceDecision> {
        self.last_decision
    }

    /// Bounded per-region diagnostics safe to emit every reporting interval.
    #[must_use]
    pub fn snapshot(&self) -> RegionScheduleSnapshot {
        let activity = self.activity.diagnostics();
        RegionScheduleSnapshot {
            owner: self.activity.owner(),
            activity,
            telemetry: self.telemetry,
            cadence: activity.activity.cadence,
            recommended_interval: self
                .last_decision
                .map_or(self.policy.frame_interval, |decision| {
                    decision.recommended_interval
                }),
            deadline_remaining: self.deadline_remaining(),
        }
    }

    /// Time left before this region must be served regardless of activity.
    #[must_use]
    pub fn deadline_remaining(&self) -> Duration {
        self.policy
            .max_idle_refresh
            .saturating_sub(self.since_service)
            .min(
                self.policy
                    .keyframe_interval
                    .saturating_sub(self.since_keyframe),
            )
    }

    /// True when this region must be served on the next observation even if
    /// its pixels have not changed.
    #[must_use]
    pub fn service_due(&self) -> bool {
        self.pending_keyframe || self.deadline_remaining() == Duration::ZERO
    }

    /// Clears activity, cadence, and deadline state; the next service is a
    /// startup/recovery keyframe. Cumulative telemetry is retained.
    pub fn reset(&mut self) {
        self.activity.reset();
        self.reset_schedule_state();
    }

    /// Rebinds to a strictly newer region generation and forces a keyframe.
    ///
    /// # Errors
    ///
    /// Returns the errors documented by [`RegionActivityGrid::rebind`].
    pub fn rebind(&mut self, owner: RegionActivityOwner) -> Result<(), RegionActivityError> {
        self.activity.rebind(owner)?;
        self.reset_schedule_state();
        Ok(())
    }

    /// Observes one captured frame and decides whether to serve this region.
    ///
    /// `elapsed` is the monotonic time since the previous observation, so this
    /// module stays free of any clock or platform dependency.
    ///
    /// # Errors
    ///
    /// Returns the ownership and geometry errors documented by
    /// [`RegionActivityGrid::update_with_hint`]. Schedule state is unchanged
    /// when the frame is rejected.
    pub fn observe(
        &mut self,
        owner: RegionActivityOwner,
        frame: BgraFrame<'_>,
        elapsed: Duration,
        signals: RegionScheduleSignals,
    ) -> Result<RegionServiceDecision, RegionActivityError> {
        let activity = self.activity.update_with_hint(owner, frame, signals.hint)?;
        Ok(self.advance(activity, elapsed, signals))
    }

    /// Re-arms the scheduler when a served decision could not be delivered.
    ///
    /// Call immediately after a [`RegionServiceAction::Serve`] the pipeline
    /// failed to encode or emit: the region keeps its previous deadlines and
    /// is served again on the next observation, so a lost keyframe is never
    /// silently downgraded to a delta.
    pub fn note_service_failed(&mut self) {
        self.telemetry.service_failures = self.telemetry.service_failures.saturating_add(1);
        self.since_service = self.committed_since_service;
        self.since_keyframe = self.committed_since_keyframe;
        if self
            .last_decision
            .is_some_and(|decision| decision.keyframe && decision.serves())
        {
            self.pending_keyframe = true;
        }
        self.cadence.note_frame();
    }

    /// Records a service the host performed without an activity observation,
    /// such as a full-damage bypass frame that skipped the damage hash.
    pub fn note_external_service(&mut self, keyframe: bool) {
        self.commit_service(keyframe);
        self.telemetry.served = self.telemetry.served.saturating_add(1);
        if keyframe {
            self.telemetry.keyframes = self.telemetry.keyframes.saturating_add(1);
        }
    }

    fn reset_schedule_state(&mut self) {
        self.cadence = IdleCadence::new(self.policy.max_idle_refresh);
        self.since_service = Duration::ZERO;
        self.since_keyframe = Duration::ZERO;
        self.since_input = None;
        self.pending_keyframe = true;
        self.committed_since_service = Duration::ZERO;
        self.committed_since_keyframe = Duration::ZERO;
        self.last_decision = None;
    }

    fn advance(
        &mut self,
        activity: RegionActivityDiagnostics,
        elapsed: Duration,
        signals: RegionScheduleSignals,
    ) -> RegionServiceDecision {
        self.telemetry.ticks = self.telemetry.ticks.saturating_add(1);
        self.telemetry.record_class(activity.activity.class);
        self.since_service = self.since_service.saturating_add(elapsed);
        self.since_keyframe = self.since_keyframe.saturating_add(elapsed);
        self.since_input = match (signals.input_activity, self.since_input) {
            (true, _) => Some(Duration::ZERO),
            (false, Some(since)) => Some(since.saturating_add(elapsed)),
            (false, None) => None,
        };

        let input_wake = self.input_wake_active();
        let dirty = !activity.activity.summary.is_clean();
        if dirty || activity.activity.baseline_refresh || input_wake {
            self.cadence.note_frame();
        }

        let keyframe_deadline_due = self.since_keyframe >= self.policy.keyframe_interval;
        let idr_pending =
            signals.forced_keyframe.is_some() || keyframe_deadline_due || self.pending_keyframe;
        let (action, reason) = match self.cadence.decision(idr_pending, self.since_service) {
            Some(EmitMode::FirstFrame) => (
                RegionServiceAction::Serve,
                RegionServiceReason::StartupBaseline,
            ),
            Some(EmitMode::Idr) => (
                RegionServiceAction::Serve,
                match signals.forced_keyframe {
                    Some(kind) => RegionServiceReason::ForcedKeyframe(kind),
                    None if self.pending_keyframe => {
                        RegionServiceReason::ForcedKeyframe(ForcedKeyframe::Recovery)
                    }
                    None => RegionServiceReason::KeyframeDeadline,
                },
            ),
            Some(EmitMode::Activity) => (
                RegionServiceAction::Serve,
                if dirty {
                    RegionServiceReason::Damage
                } else {
                    RegionServiceReason::InputWake
                },
            ),
            Some(EmitMode::Keepalive) => (
                RegionServiceAction::Serve,
                RegionServiceReason::MaxIdleRefresh,
            ),
            None => (
                RegionServiceAction::Skip,
                RegionServiceReason::StaticSuppressed,
            ),
        };

        let keyframe = reason.is_keyframe();
        match action {
            RegionServiceAction::Serve => {
                self.telemetry.served = self.telemetry.served.saturating_add(1);
                match reason {
                    RegionServiceReason::Damage => {
                        self.telemetry.damage_serves =
                            self.telemetry.damage_serves.saturating_add(1);
                    }
                    RegionServiceReason::InputWake => {
                        self.telemetry.input_wakes = self.telemetry.input_wakes.saturating_add(1);
                    }
                    RegionServiceReason::MaxIdleRefresh => {
                        self.telemetry.idle_refreshes =
                            self.telemetry.idle_refreshes.saturating_add(1);
                    }
                    RegionServiceReason::StartupBaseline
                    | RegionServiceReason::ForcedKeyframe(_)
                    | RegionServiceReason::KeyframeDeadline
                    | RegionServiceReason::StaticSuppressed => {}
                }
                if keyframe {
                    self.telemetry.keyframes = self.telemetry.keyframes.saturating_add(1);
                }
                self.telemetry.current_skip_streak = 0;
                self.cadence.on_submitted();
                self.commit_service(keyframe);
            }
            RegionServiceAction::Skip => {
                self.telemetry.skipped = self.telemetry.skipped.saturating_add(1);
                self.telemetry.current_skip_streak =
                    self.telemetry.current_skip_streak.saturating_add(1);
                self.telemetry.max_skip_streak = self
                    .telemetry
                    .max_skip_streak
                    .max(self.telemetry.current_skip_streak);
            }
        }

        let decision = RegionServiceDecision {
            action,
            reason,
            keyframe,
            activity,
            cadence: activity.activity.cadence,
            recommended_interval: self.recommended_interval(activity, input_wake),
            deadline_remaining: self.deadline_remaining(),
        };
        self.last_decision = Some(decision);
        decision
    }

    fn commit_service(&mut self, keyframe: bool) {
        self.committed_since_service = self.since_service;
        self.committed_since_keyframe = self.since_keyframe;
        self.telemetry.max_service_gap_micros = self
            .telemetry
            .max_service_gap_micros
            .max(u64::try_from(self.since_service.as_micros()).unwrap_or(u64::MAX));
        self.since_service = Duration::ZERO;
        if keyframe {
            self.since_keyframe = Duration::ZERO;
            self.pending_keyframe = false;
        }
    }

    fn input_wake_active(&self) -> bool {
        self.since_input
            .is_some_and(|since| since <= self.policy.input_wake_grace)
    }

    fn recommended_interval(
        &self,
        activity: RegionActivityDiagnostics,
        input_wake: bool,
    ) -> Duration {
        let frame_interval = self.policy.frame_interval;
        if input_wake || self.pending_keyframe {
            return frame_interval;
        }
        if activity.activity.cadence != CadenceRecommendation::Keepalive
            || activity.activity.class != ActivityClass::Idle
            || activity.activity.class_streak < REGION_IDLE_BACKOFF_STREAK
        {
            return frame_interval;
        }
        frame_interval
            .saturating_mul(REGION_IDLE_BACKOFF_FACTOR)
            .min(self.policy.max_idle_refresh)
            .min(self.deadline_remaining())
            .max(frame_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RegionGeneration, RegionId};

    const WIDTH: usize = 64;
    const HEIGHT: usize = 64;
    const FRAME: Duration = Duration::from_millis(16);
    const IDLE_REFRESH: Duration = Duration::from_millis(160);
    const KEYFRAME: Duration = Duration::from_millis(960);

    fn owner(generation: u64, region_id: u32) -> RegionActivityOwner {
        RegionActivityOwner::new(
            RegionGeneration::new(generation).expect("generation"),
            RegionId::new(region_id).expect("region id"),
        )
    }

    fn policy() -> RegionSchedulePolicy {
        RegionSchedulePolicy::new(FRAME, IDLE_REFRESH, KEYFRAME, Duration::from_millis(64))
            .expect("policy")
    }

    fn scheduler(owner: RegionActivityOwner) -> RegionActivityScheduler {
        RegionActivityScheduler::new(owner, WIDTH, HEIGHT, KernelPreference::Xxh3, policy())
            .expect("scheduler")
    }

    fn frame(pixels: &[u8]) -> BgraFrame<'_> {
        BgraFrame::new(pixels, WIDTH, HEIGHT, WIDTH * 4).expect("frame")
    }

    /// Paints `blocks` 16x16 tiles starting at the top-left of the region.
    fn paint_tiles(pixels: &mut [u8], blocks: usize, value: u8) {
        let blocks_wide = WIDTH / 16;
        for block in 0..blocks {
            let block_x = (block % blocks_wide) * 16;
            let block_y = (block / blocks_wide) * 16;
            for row in block_y..block_y + 16 {
                let start = (row * WIDTH + block_x) * 4;
                pixels[start..start + 16 * 4].fill(value);
            }
        }
    }

    fn baseline(scheduler: &mut RegionActivityScheduler, owner: RegionActivityOwner) {
        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let decision = scheduler
            .observe(
                owner,
                frame(&pixels),
                Duration::ZERO,
                RegionScheduleSignals::default(),
            )
            .expect("baseline");
        assert!(decision.serves());
        assert_eq!(decision.reason, RegionServiceReason::StartupBaseline);
        assert!(decision.keyframe);
    }

    #[test]
    fn policy_rejects_inverted_or_out_of_range_bounds() {
        assert_eq!(
            RegionSchedulePolicy::new(
                Duration::from_micros(500),
                IDLE_REFRESH,
                KEYFRAME,
                Duration::ZERO
            ),
            Err(RegionSchedulePolicyError::FrameInterval(
                Duration::from_micros(500)
            ))
        );
        assert!(matches!(
            RegionSchedulePolicy::new(FRAME, Duration::from_millis(8), KEYFRAME, Duration::ZERO),
            Err(RegionSchedulePolicyError::IdleRefreshBelowFrameInterval { .. })
        ));
        assert!(matches!(
            RegionSchedulePolicy::new(
                FRAME,
                IDLE_REFRESH,
                Duration::from_millis(80),
                Duration::ZERO
            ),
            Err(RegionSchedulePolicyError::KeyframeIntervalBelowIdleRefresh { .. })
        ));
        assert!(matches!(
            RegionSchedulePolicy::new(FRAME, IDLE_REFRESH, KEYFRAME, Duration::from_secs(5)),
            Err(RegionSchedulePolicyError::InputWakeGraceTooLong { .. })
        ));
        assert!(matches!(
            RegionSchedulePolicy::new(
                FRAME,
                Duration::from_secs(20),
                Duration::from_secs(30),
                Duration::ZERO
            ),
            Err(RegionSchedulePolicyError::IdleRefreshTooLong(_))
        ));
        assert!(matches!(
            RegionSchedulePolicy::new(
                FRAME,
                IDLE_REFRESH,
                Duration::from_secs(120),
                Duration::ZERO
            ),
            Err(RegionSchedulePolicyError::KeyframeIntervalTooLong(_))
        ));
        assert_eq!(
            RegionSchedulePolicy::from_target_fps(0, IDLE_REFRESH, KEYFRAME, Duration::ZERO),
            Err(RegionSchedulePolicyError::ZeroTargetFps)
        );
        let derived = RegionSchedulePolicy::from_target_fps(
            60,
            IDLE_REFRESH,
            KEYFRAME,
            Duration::from_millis(64),
        )
        .expect("policy");
        assert_eq!(derived.frame_interval(), Duration::from_nanos(16_666_666));
    }

    #[test]
    fn static_content_is_suppressed_between_bounded_idle_refreshes() {
        let region = owner(1, 1);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);

        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let mut serves = 0u32;
        let mut skips = 0u32;
        let mut longest_gap = Duration::ZERO;
        let mut gap = Duration::ZERO;
        for _ in 0..120 {
            let decision = scheduler
                .observe(
                    region,
                    frame(&pixels),
                    FRAME,
                    RegionScheduleSignals::default(),
                )
                .expect("static tick");
            gap = gap.saturating_add(FRAME);
            if decision.serves() {
                assert!(decision.is_mandatory(), "static serves must be mandatory");
                serves += 1;
                longest_gap = longest_gap.max(gap);
                gap = Duration::ZERO;
            } else {
                assert_eq!(decision.reason, RegionServiceReason::StaticSuppressed);
                assert_eq!(decision.cadence, CadenceRecommendation::Keepalive);
                skips += 1;
            }
        }

        assert!(skips > serves * 4, "static content must avoid encode work");
        assert!(
            longest_gap <= IDLE_REFRESH + FRAME,
            "static region starved for {longest_gap:?}"
        );
        let telemetry = scheduler.telemetry();
        assert_eq!(telemetry.ticks, 121);
        assert_eq!(telemetry.served + telemetry.skipped, telemetry.ticks);
        assert!(telemetry.idle_refreshes >= 1);
        assert!(
            telemetry.keyframes >= 2,
            "keyframe deadline must still fire"
        );
    }

    #[test]
    fn sustained_static_content_backs_capture_off_within_its_deadline() {
        let region = owner(1, 1);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);

        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let mut backed_off = false;
        for _ in 0..8 {
            let decision = scheduler
                .observe(
                    region,
                    frame(&pixels),
                    FRAME,
                    RegionScheduleSignals::default(),
                )
                .expect("static tick");
            assert!(decision.recommended_interval >= FRAME);
            assert!(
                decision.recommended_interval <= decision.deadline_remaining.max(FRAME),
                "backoff must never overshoot the service deadline"
            );
            backed_off |= decision.recommended_interval > FRAME;
        }
        assert!(backed_off, "sustained idle must reduce capture polling");
    }

    #[test]
    fn localized_motion_is_served_every_tick_without_keyframes() {
        let region = owner(3, 2);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);

        let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
        for step in 0..12u8 {
            paint_tiles(&mut pixels, 1, step.wrapping_add(1));
            let decision = scheduler
                .observe(
                    region,
                    frame(&pixels),
                    FRAME,
                    RegionScheduleSignals::default(),
                )
                .expect("sparse tick");
            assert!(decision.serves());
            assert_eq!(decision.reason, RegionServiceReason::Damage);
            assert!(!decision.keyframe, "localized motion must stay a delta");
            assert_eq!(decision.class(), ActivityClass::Sparse);
            assert_eq!(decision.cadence, CadenceRecommendation::Responsive);
            assert_eq!(decision.recommended_interval, FRAME);
        }
        assert_eq!(scheduler.telemetry().skipped, 0);
        assert_eq!(scheduler.telemetry().damage_serves, 12);
    }

    #[test]
    fn full_motion_is_served_continuously_at_the_frame_interval() {
        let region = owner(3, 2);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);

        let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let blocks = (WIDTH / 16) * (HEIGHT / 16);
        let mut full_motion = 0u32;
        for step in 0..12u8 {
            paint_tiles(&mut pixels, blocks, step.wrapping_mul(17).wrapping_add(1));
            let decision = scheduler
                .observe(
                    region,
                    frame(&pixels),
                    FRAME,
                    RegionScheduleSignals::default(),
                )
                .expect("full motion tick");
            assert!(decision.serves());
            assert_eq!(decision.recommended_interval, FRAME);
            if decision.class() == ActivityClass::FullMotion {
                full_motion += 1;
                assert_eq!(decision.cadence, CadenceRecommendation::Continuous);
            }
        }
        assert!(
            full_motion >= 10,
            "broad motion must classify as full motion"
        );
        assert_eq!(scheduler.telemetry().skipped, 0);
    }

    #[test]
    fn input_and_focus_activity_wake_a_static_region() {
        let region = owner(2, 5);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);

        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        for _ in 0..4 {
            let decision = scheduler
                .observe(
                    region,
                    frame(&pixels),
                    FRAME,
                    RegionScheduleSignals::default(),
                )
                .expect("static tick");
            assert!(!decision.serves());
        }

        let woken = scheduler
            .observe(
                region,
                frame(&pixels),
                FRAME,
                RegionScheduleSignals::input(),
            )
            .expect("input tick");
        assert!(woken.serves());
        assert_eq!(woken.reason, RegionServiceReason::InputWake);
        assert!(!woken.keyframe);
        assert_eq!(woken.recommended_interval, FRAME);

        // The wake grace keeps the region responsive without new pixels.
        let grace = scheduler
            .observe(
                region,
                frame(&pixels),
                FRAME,
                RegionScheduleSignals::default(),
            )
            .expect("grace tick");
        assert!(grace.serves());
        assert_eq!(grace.reason, RegionServiceReason::InputWake);

        // ...and expires, returning the static region to suppression.
        for _ in 0..4 {
            scheduler
                .observe(
                    region,
                    frame(&pixels),
                    Duration::from_millis(32),
                    RegionScheduleSignals::default(),
                )
                .expect("post grace tick");
        }
        let settled = scheduler
            .observe(
                region,
                frame(&pixels),
                Duration::from_millis(32),
                RegionScheduleSignals::default(),
            )
            .expect("settled tick");
        assert!(!settled.serves() || settled.reason == RegionServiceReason::MaxIdleRefresh);
        assert!(scheduler.telemetry().input_wakes >= 2);
    }

    #[test]
    fn keyframe_requests_and_recovery_override_activity_suppression() {
        let region = owner(4, 3);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);

        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        scheduler
            .observe(
                region,
                frame(&pixels),
                FRAME,
                RegionScheduleSignals::default(),
            )
            .expect("static tick");

        let forced = scheduler
            .observe(
                region,
                frame(&pixels),
                FRAME,
                RegionScheduleSignals::keyframe(ForcedKeyframe::ClientRequest),
            )
            .expect("forced keyframe");
        assert!(forced.serves());
        assert!(forced.keyframe);
        assert!(forced.is_mandatory());
        assert_eq!(
            forced.reason,
            RegionServiceReason::ForcedKeyframe(ForcedKeyframe::ClientRequest)
        );

        // A failed delivery must not silently downgrade the lost keyframe.
        scheduler.note_service_failed();
        assert!(scheduler.service_due());
        let retried = scheduler
            .observe(
                region,
                frame(&pixels),
                FRAME,
                RegionScheduleSignals::default(),
            )
            .expect("retry");
        assert!(retried.serves());
        assert!(retried.keyframe);
        assert_eq!(
            retried.reason,
            RegionServiceReason::ForcedKeyframe(ForcedKeyframe::Recovery)
        );
        assert_eq!(scheduler.telemetry().service_failures, 1);

        // Recovery reset re-establishes a startup baseline keyframe.
        scheduler.reset();
        assert!(scheduler.service_due());
        let recovered = scheduler
            .observe(
                region,
                frame(&pixels),
                FRAME,
                RegionScheduleSignals::default(),
            )
            .expect("recovered baseline");
        assert!(recovered.serves());
        assert!(recovered.keyframe);
        assert_eq!(recovered.reason, RegionServiceReason::StartupBaseline);
        assert!(recovered.activity.activity.baseline_refresh);
    }

    #[test]
    fn keyframe_deadline_fires_on_a_permanently_static_region() {
        let region = owner(1, 1);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);

        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let mut elapsed = Duration::ZERO;
        let mut keyframe_gap = Duration::ZERO;
        let mut longest_keyframe_gap = Duration::ZERO;
        while elapsed < Duration::from_secs(4) {
            let decision = scheduler
                .observe(
                    region,
                    frame(&pixels),
                    FRAME,
                    RegionScheduleSignals::default(),
                )
                .expect("static tick");
            elapsed = elapsed.saturating_add(FRAME);
            keyframe_gap = keyframe_gap.saturating_add(FRAME);
            if decision.keyframe {
                longest_keyframe_gap = longest_keyframe_gap.max(keyframe_gap);
                keyframe_gap = Duration::ZERO;
            }
        }
        assert!(scheduler.telemetry().keyframes >= 4);
        assert!(
            longest_keyframe_gap <= KEYFRAME + FRAME,
            "keyframe deadline overshot by {longest_keyframe_gap:?}"
        );
    }

    #[test]
    fn regions_are_isolated_and_a_busy_region_cannot_starve_a_static_one() {
        let busy_owner = owner(7, 1);
        let static_owner = owner(7, 2);
        let mut busy = scheduler(busy_owner);
        let mut quiet = scheduler(static_owner);
        baseline(&mut busy, busy_owner);
        baseline(&mut quiet, static_owner);

        let blocks = (WIDTH / 16) * (HEIGHT / 16);
        let mut busy_pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let static_pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let mut quiet_gap = Duration::ZERO;
        let mut longest_quiet_gap = Duration::ZERO;
        let mut quiet_serves = 0u32;

        for step in 0..120u8 {
            paint_tiles(
                &mut busy_pixels,
                blocks,
                step.wrapping_mul(31).wrapping_add(1),
            );
            let busy_decision = busy
                .observe(
                    busy_owner,
                    frame(&busy_pixels),
                    FRAME,
                    RegionScheduleSignals::default(),
                )
                .expect("busy tick");
            assert!(busy_decision.serves());

            let quiet_decision = quiet
                .observe(
                    static_owner,
                    frame(&static_pixels),
                    FRAME,
                    RegionScheduleSignals::default(),
                )
                .expect("quiet tick");
            quiet_gap = quiet_gap.saturating_add(FRAME);
            if quiet_decision.serves() {
                quiet_serves += 1;
                longest_quiet_gap = longest_quiet_gap.max(quiet_gap);
                quiet_gap = Duration::ZERO;
            }
        }

        assert!(
            quiet_serves >= 4,
            "static region must keep its refresh floor"
        );
        assert!(
            longest_quiet_gap <= IDLE_REFRESH + FRAME,
            "static region starved for {longest_quiet_gap:?} beside a busy region"
        );
        assert_eq!(busy.owner(), busy_owner);
        assert_eq!(quiet.owner(), static_owner);
        assert_eq!(
            quiet.telemetry().full_motion_ticks,
            1,
            "only the static region's own baseline may classify as full motion"
        );
        assert_eq!(quiet.telemetry().idle_ticks, 120);
        assert_eq!(busy.telemetry().idle_ticks, 0);
        assert_eq!(busy.telemetry().skipped, 0);
        assert!(quiet.telemetry().skipped > 0);
        assert!(
            quiet.telemetry().max_service_gap_micros
                <= u64::try_from((IDLE_REFRESH + FRAME).as_micros()).expect("micros")
        );
    }

    #[test]
    fn stale_or_wrong_region_frames_leave_schedule_state_untouched() {
        let region = owner(9, 4);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);
        let before = scheduler.telemetry();

        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        assert!(matches!(
            scheduler.observe(
                owner(8, 4),
                frame(&pixels),
                FRAME,
                RegionScheduleSignals::default()
            ),
            Err(RegionActivityError::StaleGeneration { .. })
        ));
        assert!(matches!(
            scheduler.observe(
                owner(9, 5),
                frame(&pixels),
                FRAME,
                RegionScheduleSignals::default()
            ),
            Err(RegionActivityError::RegionMismatch { .. })
        ));
        assert_eq!(scheduler.telemetry(), before);

        let next = owner(10, 4);
        scheduler.rebind(next).expect("rebind");
        assert_eq!(scheduler.owner(), next);
        assert!(scheduler.service_due());
        let rebound = scheduler
            .observe(
                next,
                frame(&pixels),
                FRAME,
                RegionScheduleSignals::default(),
            )
            .expect("rebound baseline");
        assert!(rebound.keyframe);
        assert_eq!(rebound.reason, RegionServiceReason::StartupBaseline);
    }

    #[test]
    fn telemetry_is_fixed_size_and_accounts_for_every_tick() {
        let region = owner(1, 1);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);

        let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
        for step in 0..64u8 {
            if step % 8 == 0 {
                paint_tiles(&mut pixels, 2, step.wrapping_add(1));
            }
            scheduler
                .observe(
                    region,
                    frame(&pixels),
                    FRAME,
                    RegionScheduleSignals::default(),
                )
                .expect("tick");
        }

        let telemetry = scheduler.telemetry();
        let fields = telemetry.fields();
        assert_eq!(fields.len(), REGION_SCHEDULE_TELEMETRY_FIELDS);
        assert!(fields.iter().all(|(key, _)| !key.is_empty()));
        assert_eq!(telemetry.ticks, 65);
        assert_eq!(telemetry.served + telemetry.skipped, telemetry.ticks);
        assert_eq!(
            telemetry.idle_ticks
                + telemetry.sparse_ticks
                + telemetry.scroll_ticks
                + telemetry.full_motion_ticks,
            telemetry.ticks
        );
        assert!(telemetry.max_skip_streak >= telemetry.current_skip_streak);

        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.owner, region);
        assert_eq!(snapshot.telemetry, telemetry);
        assert!(snapshot.deadline_remaining <= IDLE_REFRESH);
        // Bounded by construction: fixed-width counters only, no heap, no
        // per-tick or per-region growth.
        assert!(core::mem::size_of::<RegionScheduleTelemetry>() <= 128);
    }

    #[test]
    fn external_services_keep_deadlines_consistent() {
        let region = owner(1, 1);
        let mut scheduler = scheduler(region);
        baseline(&mut scheduler, region);

        scheduler.note_external_service(true);
        assert!(!scheduler.service_due());
        assert_eq!(scheduler.telemetry().keyframes, 2);
        assert_eq!(scheduler.deadline_remaining(), IDLE_REFRESH);
    }
}
