//! Thin capture-loop binding for the shared per-region activity scheduler.
//!
//! Every `capenc` child serves exactly one region (one applied monitor), so
//! this module owns exactly one [`RegionActivityScheduler`] per process. It is
//! deliberately host neutral: the platform capture loops
//! (`linux_x11`, `win_mf`) only translate their own capture events into
//! [`RegionScheduleSignals`] and act on the returned decision. All damage,
//! activity, cadence, deadline, and telemetry logic lives in
//! `arcen_media::region_schedule`, so neither platform carries its own copy.
//!
//! The scheduler replaces the loops' bare `DamageTracker`: it *contains* that
//! tracker, so a frame is still hashed exactly once and the same 16x16 damage
//! map still drives selective BGRA->I420 conversion. The only addition is that
//! the measured changed-tile result now also produces an explicit serve/skip
//! decision plus bounded per-region diagnostics.
//!
//! When the region geometry cannot back an activity grid at all, the loops may
//! fall back to [`CaptureRegionScheduler::degraded`], which reports full damage
//! and serves every frame — the same conservative full-conversion behaviour the
//! loops used before this binding existed.

use std::time::{Duration, Instant};

use arcen_keel::{
    ActivityClass, ActivityHint, BgraFrame, DamageMap, DamageSummary, HashKernel, KernelPreference,
};
use arcen_media::{
    ForcedKeyframe, RegionActivityError, RegionActivityOwner, RegionActivityScheduler,
    RegionContractError, RegionGeneration, RegionId, RegionSchedulePolicy,
    RegionSchedulePolicyError, RegionScheduleSignals, RegionScheduleTelemetry,
};

/// Every `capenc` child is the first generation of its own region.
const REGION_GENERATION: u64 = 1;

/// Failure building a [`CaptureRegionScheduler`].
#[derive(Debug)]
pub(crate) enum CaptureScheduleError {
    Region(RegionContractError),
    Policy(RegionSchedulePolicyError),
    Activity(RegionActivityError),
}

impl std::fmt::Display for CaptureScheduleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Region(error) => std::fmt::Display::fmt(error, formatter),
            Self::Policy(error) => std::fmt::Display::fmt(error, formatter),
            Self::Activity(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

impl From<RegionContractError> for CaptureScheduleError {
    fn from(error: RegionContractError) -> Self {
        Self::Region(error)
    }
}

impl From<RegionSchedulePolicyError> for CaptureScheduleError {
    fn from(error: RegionSchedulePolicyError) -> Self {
        Self::Policy(error)
    }
}

impl From<RegionActivityError> for CaptureScheduleError {
    fn from(error: RegionActivityError) -> Self {
        Self::Activity(error)
    }
}

/// One capture tick's decision, flattened for the platform loops.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CaptureServiceDecision {
    /// Measured 16x16 changed-tile damage for this frame.
    pub(crate) summary: DamageSummary,
    /// Convert, encode, and emit this frame.
    pub(crate) serve: bool,
    /// This service must carry a complete picture.
    pub(crate) keyframe: bool,
    /// A downstream optimizer must not drop this service.
    pub(crate) mandatory: bool,
    /// Stable, bounded reason name for diagnostics.
    pub(crate) reason: &'static str,
    pub(crate) class: ActivityClass,
    /// Advisory capture interval until the next tick; never past a deadline.
    pub(crate) recommended_interval: Duration,
}

const DEGRADED_SUMMARY: DamageSummary = DamageSummary {
    dirty_blocks: 1,
    total_blocks: 1,
    dirty_block_rows: 1,
    total_block_rows: 1,
};

/// One region's scheduler plus the monotonic bookkeeping the shared crate
/// deliberately does not own.
#[derive(Debug)]
pub(crate) struct CaptureRegionScheduler {
    scheduler: Option<RegionActivityScheduler>,
    owner: RegionActivityOwner,
    frame_interval: Duration,
    last_observed: Option<Instant>,
}

impl CaptureRegionScheduler {
    /// Builds the region-owned activity scheduler for this capture geometry.
    ///
    /// `output_index` is the 0-based capture output this child serves; region
    /// identities are nonzero, so it is stored as `output_index + 1`.
    ///
    /// # Errors
    ///
    /// Returns a bounded contract error when the region identity, service
    /// policy, or grid geometry is invalid.
    pub(crate) fn try_new(
        output_index: u32,
        width: usize,
        height: usize,
        target_fps: u32,
        max_idle_refresh: Duration,
        keyframe_interval: Duration,
        input_wake_grace: Duration,
    ) -> Result<Self, CaptureScheduleError> {
        let owner = RegionActivityOwner::new(
            RegionGeneration::new(REGION_GENERATION)?,
            RegionId::new(output_index.saturating_add(1))?,
        );
        // Match `crate::frame_interval_from_fps`, so a caller-supplied target
        // the capture loop already tolerates cannot turn into a fatal region
        // scheduler error.
        let policy = RegionSchedulePolicy::from_target_fps(
            target_fps.clamp(1, 240),
            max_idle_refresh,
            keyframe_interval,
            input_wake_grace,
        )?;
        let scheduler =
            RegionActivityScheduler::new(owner, width, height, KernelPreference::Auto, policy)?;
        Ok(Self {
            scheduler: Some(scheduler),
            owner,
            frame_interval: policy.frame_interval(),
            last_observed: None,
        })
    }

    /// Conservative fallback: report full damage and serve every frame.
    pub(crate) fn degraded(frame_interval: Duration) -> Self {
        Self {
            scheduler: None,
            owner: Self::fallback_owner(),
            frame_interval,
            last_observed: None,
        }
    }

    fn fallback_owner() -> RegionActivityOwner {
        // Both identities are compile-time nonzero; the fallback never fails.
        RegionActivityOwner::new(
            RegionGeneration::new(REGION_GENERATION).unwrap_or_else(|_| unreachable!()),
            RegionId::new(1).unwrap_or_else(|_| unreachable!()),
        )
    }

    pub(crate) const fn is_active(&self) -> bool {
        self.scheduler.is_some()
    }

    pub(crate) fn kernel(&self) -> Option<HashKernel> {
        self.scheduler.as_ref().map(RegionActivityScheduler::kernel)
    }

    /// The changed-tile map backing the last observed frame, when active.
    pub(crate) fn damage_map(&self) -> Option<DamageMap<'_>> {
        self.scheduler
            .as_ref()
            .map(RegionActivityScheduler::damage_map)
    }

    /// True when this region must be served even without measured change.
    pub(crate) fn service_due(&self) -> bool {
        self.scheduler
            .as_ref()
            .is_none_or(RegionActivityScheduler::service_due)
    }

    /// Clears activity and deadline state; the next service is a keyframe.
    pub(crate) fn reset(&mut self) {
        if let Some(scheduler) = self.scheduler.as_mut() {
            scheduler.reset();
        }
        self.last_observed = None;
    }

    /// Re-arms after a served frame the pipeline could not deliver.
    pub(crate) fn note_service_failed(&mut self) {
        if let Some(scheduler) = self.scheduler.as_mut() {
            scheduler.note_service_failed();
        }
    }

    /// Records a service performed without a damage observation, such as a
    /// full-damage bypass frame that deliberately skipped the hash.
    pub(crate) fn note_external_service(&mut self, keyframe: bool) {
        if let Some(scheduler) = self.scheduler.as_mut() {
            scheduler.note_external_service(keyframe);
        }
    }

    /// Observes one captured frame and decides whether to serve this region.
    ///
    /// A rejected frame (geometry change) resets the scheduler and is served
    /// as a recovery keyframe rather than propagating an error into a capture
    /// loop that has no way to recover from one.
    pub(crate) fn observe(
        &mut self,
        frame: BgraFrame<'_>,
        now: Instant,
        forced_keyframe: Option<ForcedKeyframe>,
        input_activity: bool,
        hint: ActivityHint,
    ) -> CaptureServiceDecision {
        let elapsed = self
            .last_observed
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.last_observed = Some(now);

        let owner = self.owner;
        let frame_interval = self.frame_interval;
        let Some(scheduler) = self.scheduler.as_mut() else {
            return CaptureServiceDecision {
                summary: DEGRADED_SUMMARY,
                serve: true,
                keyframe: true,
                mandatory: true,
                reason: "degraded-full",
                class: ActivityClass::FullMotion,
                recommended_interval: frame_interval,
            };
        };

        let signals = RegionScheduleSignals {
            hint,
            forced_keyframe,
            input_activity,
        };
        match scheduler.observe(owner, frame, elapsed, signals) {
            Ok(decision) => CaptureServiceDecision {
                summary: decision.activity.activity.summary,
                serve: decision.serves(),
                keyframe: decision.keyframe,
                mandatory: decision.is_mandatory(),
                reason: decision.reason.name(),
                class: decision.class(),
                recommended_interval: decision.recommended_interval,
            },
            Err(_) => {
                scheduler.reset();
                CaptureServiceDecision {
                    summary: DEGRADED_SUMMARY,
                    serve: true,
                    keyframe: true,
                    mandatory: true,
                    reason: "geometry-recovery",
                    class: ActivityClass::FullMotion,
                    recommended_interval: frame_interval,
                }
            }
        }
    }

    pub(crate) fn telemetry(&self) -> RegionScheduleTelemetry {
        self.scheduler
            .as_ref()
            .map_or_else(RegionScheduleTelemetry::default, |scheduler| {
                scheduler.telemetry()
            })
    }

    /// One bounded `key=value` diagnostics fragment for the 1 Hz stats line.
    ///
    /// The field set is fixed size, so this string can never grow with session
    /// length, region count, or activity.
    pub(crate) fn telemetry_fragment(&self) -> String {
        let telemetry = self.telemetry();
        let class = self
            .scheduler
            .as_ref()
            .and_then(RegionActivityScheduler::last_decision)
            .map_or("none", |decision| class_name(decision.class()));
        let mut fragment = format!(
            "region={} activity_class={class} activity_active={}",
            self.owner.region_id().get(),
            self.scheduler.is_some()
        );
        for (key, value) in telemetry.fields() {
            fragment.push_str(&format!(" activity_{key}={value}"));
        }
        fragment
    }
}

const fn class_name(class: ActivityClass) -> &'static str {
    match class {
        ActivityClass::Idle => "idle",
        ActivityClass::Sparse => "sparse",
        ActivityClass::Scroll => "scroll",
        ActivityClass::FullMotion => "full-motion",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: usize = 64;
    const HEIGHT: usize = 64;
    const FPS: u32 = 60;
    const KEEPALIVE: Duration = Duration::from_millis(200);
    const KEYFRAME: Duration = Duration::from_secs(2);
    const WAKE_GRACE: Duration = Duration::from_millis(50);

    fn scheduler() -> CaptureRegionScheduler {
        CaptureRegionScheduler::try_new(0, WIDTH, HEIGHT, FPS, KEEPALIVE, KEYFRAME, WAKE_GRACE)
            .expect("capture scheduler")
    }

    fn frame(pixels: &[u8]) -> BgraFrame<'_> {
        BgraFrame::new(pixels, WIDTH, HEIGHT, WIDTH * 4).expect("frame")
    }

    fn tick(
        scheduler: &mut CaptureRegionScheduler,
        pixels: &[u8],
        now: Instant,
    ) -> CaptureServiceDecision {
        scheduler.observe(frame(pixels), now, None, false, ActivityHint::None)
    }

    #[test]
    fn first_observation_is_a_full_keyframe_and_exposes_a_damage_map() {
        let mut scheduler = scheduler();
        assert!(scheduler.is_active());
        assert!(scheduler.service_due());
        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let decision = tick(&mut scheduler, &pixels, Instant::now());
        assert!(decision.serve);
        assert!(decision.keyframe);
        assert!(decision.mandatory);
        assert_eq!(decision.reason, "startup-baseline");
        assert!(decision.summary.is_full_damage());
        assert!(scheduler.damage_map().is_some());
        assert!(scheduler.kernel().is_some());
        assert!(!scheduler.service_due());
    }

    #[test]
    fn static_frames_are_suppressed_but_still_refreshed_within_the_keepalive() {
        let mut scheduler = scheduler();
        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let mut now = Instant::now();
        tick(&mut scheduler, &pixels, now);

        let mut skipped = 0u32;
        let mut refreshed = false;
        for _ in 0..30 {
            now += Duration::from_millis(16);
            let decision = tick(&mut scheduler, &pixels, now);
            if decision.serve {
                assert!(decision.mandatory);
                refreshed = true;
            } else {
                assert_eq!(decision.reason, "static-suppressed");
                assert_eq!(decision.class, ActivityClass::Idle);
                skipped += 1;
            }
        }
        assert!(skipped >= 20, "static capture must avoid encode work");
        assert!(refreshed, "the keepalive refresh must still fire");
    }

    #[test]
    fn damage_and_forced_keyframes_always_serve() {
        let mut scheduler = scheduler();
        let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let mut now = Instant::now();
        tick(&mut scheduler, &pixels, now);

        pixels[0..64].fill(0xAA);
        now += Duration::from_millis(16);
        let dirty = tick(&mut scheduler, &pixels, now);
        assert!(dirty.serve);
        assert!(!dirty.keyframe);
        assert_eq!(dirty.reason, "damage");

        now += Duration::from_millis(16);
        let forced = scheduler.observe(
            frame(&pixels),
            now,
            Some(ForcedKeyframe::ClientRequest),
            false,
            ActivityHint::None,
        );
        assert!(forced.serve);
        assert!(forced.keyframe);
        assert!(forced.mandatory);
        assert_eq!(forced.reason, "client-request");

        now += Duration::from_millis(16);
        let woken = scheduler.observe(frame(&pixels), now, None, true, ActivityHint::None);
        assert!(woken.serve);
        assert!(!woken.keyframe);
        assert_eq!(woken.reason, "input-wake");
    }

    #[test]
    fn reset_forces_a_new_baseline_keyframe() {
        let mut scheduler = scheduler();
        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let mut now = Instant::now();
        tick(&mut scheduler, &pixels, now);
        now += Duration::from_millis(16);
        assert!(!tick(&mut scheduler, &pixels, now).serve);

        scheduler.reset();
        assert!(scheduler.service_due());
        now += Duration::from_millis(16);
        let recovered = tick(&mut scheduler, &pixels, now);
        assert!(recovered.serve);
        assert!(recovered.keyframe);
        assert_eq!(recovered.reason, "startup-baseline");
    }

    #[test]
    fn geometry_changes_recover_instead_of_failing_the_capture_loop() {
        let mut scheduler = scheduler();
        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let now = Instant::now();
        tick(&mut scheduler, &pixels, now);

        let narrow = vec![0u8; 32 * 32 * 4];
        let decision = scheduler.observe(
            BgraFrame::new(&narrow, 32, 32, 32 * 4).expect("frame"),
            now + Duration::from_millis(16),
            None,
            false,
            ActivityHint::None,
        );
        assert!(decision.serve);
        assert!(decision.keyframe);
        assert_eq!(decision.reason, "geometry-recovery");
        assert!(scheduler.service_due());
    }

    #[test]
    fn degraded_schedulers_serve_every_frame_as_a_full_picture() {
        let mut scheduler = CaptureRegionScheduler::degraded(Duration::from_millis(16));
        assert!(!scheduler.is_active());
        assert!(scheduler.service_due());
        assert!(scheduler.damage_map().is_none());
        assert!(scheduler.kernel().is_none());
        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        for step in 0..4 {
            let decision = tick(
                &mut scheduler,
                &pixels,
                Instant::now() + Duration::from_millis(16 * step),
            );
            assert!(decision.serve);
            assert!(decision.keyframe);
            assert!(decision.summary.is_full_damage());
        }
    }

    #[test]
    fn telemetry_fragment_is_bounded_and_stable() {
        let mut scheduler = scheduler();
        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let mut now = Instant::now();
        tick(&mut scheduler, &pixels, now);
        let short = scheduler.telemetry_fragment();

        for _ in 0..2_000 {
            now += Duration::from_millis(16);
            tick(&mut scheduler, &pixels, now);
        }
        let long = scheduler.telemetry_fragment();

        assert_eq!(
            short.matches('=').count(),
            long.matches('=').count(),
            "the diagnostics field set must not grow with session length"
        );
        assert!(
            long.len() < 512,
            "telemetry fragment grew to {}",
            long.len()
        );
        assert!(long.contains("activity_ticks=2001"));
        assert!(long.contains("region=1"));
        assert_eq!(scheduler.telemetry().ticks, 2001);
    }

    #[test]
    fn invalid_geometry_and_policy_are_rejected_instead_of_silently_degrading() {
        assert!(matches!(
            CaptureRegionScheduler::try_new(0, 0, HEIGHT, FPS, KEEPALIVE, KEYFRAME, WAKE_GRACE),
            Err(CaptureScheduleError::Activity(_))
        ));
        assert!(matches!(
            CaptureRegionScheduler::try_new(0, WIDTH, HEIGHT, 0, KEEPALIVE, KEYFRAME, WAKE_GRACE),
            Err(CaptureScheduleError::Policy(_))
        ));
        // A zero target is clamped exactly like the capture loop's own frame
        // interval helper, so it is rejected only where 1 fps genuinely
        // conflicts with the refresh bound — never merely because the caller
        // omitted a rate the loop already tolerates.
        assert!(CaptureRegionScheduler::try_new(
            0,
            WIDTH,
            HEIGHT,
            0,
            Duration::from_secs(1),
            KEYFRAME,
            WAKE_GRACE
        )
        .is_ok());
        assert!(CaptureRegionScheduler::try_new(
            u32::MAX,
            WIDTH,
            HEIGHT,
            FPS,
            KEEPALIVE,
            KEYFRAME,
            WAKE_GRACE
        )
        .is_ok());
    }
}
