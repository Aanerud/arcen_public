use core::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmitMode {
    FirstFrame,
    Idr,
    Activity,
    Keepalive,
}

impl EmitMode {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FirstFrame => "first",
            Self::Idr => "idr",
            Self::Activity => "activity",
            Self::Keepalive => "keepalive",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleCadence {
    keepalive: Duration,
    has_frame: bool,
    dirty: bool,
    first: bool,
}

impl IdleCadence {
    #[must_use]
    pub const fn new(keepalive: Duration) -> Self {
        Self {
            keepalive,
            has_frame: false,
            dirty: false,
            first: true,
        }
    }

    pub const fn note_frame(&mut self) {
        self.has_frame = true;
        self.dirty = true;
    }

    /// Records a frame that arrived carrying no change.
    ///
    /// Some sources deliver on a cadence rather than only on change, and say
    /// separately whether anything moved — `ScreenCaptureKit` hands over a
    /// frame together with the rectangles the compositor redrew, which is
    /// legitimately empty for a still desktop.
    ///
    /// Such a frame is content, so a keepalive is owed on schedule and a
    /// pending recovery point can be served from it. It is not activity, so it
    /// must not cause an emission by itself: doing so would spend a stream's
    /// whole bandwidth re-sending a desktop that nobody has touched.
    pub const fn note_unchanged_frame(&mut self) {
        self.has_frame = true;
    }

    pub const fn reset(&mut self) {
        *self = Self::new(self.keepalive);
    }

    #[must_use]
    pub fn decision(self, idr_pending: bool, elapsed_since_emit: Duration) -> Option<EmitMode> {
        if !self.has_frame {
            None
        } else if self.first {
            Some(EmitMode::FirstFrame)
        } else if idr_pending {
            Some(EmitMode::Idr)
        } else if self.dirty {
            Some(EmitMode::Activity)
        } else if elapsed_since_emit >= self.keepalive {
            Some(EmitMode::Keepalive)
        } else {
            None
        }
    }

    pub const fn on_submitted(&mut self) {
        self.first = false;
        self.dirty = false;
    }
}

/// What a submission was for, from an encoder adapter's point of view.
///
/// The [`EmitMode`] cadence plus one encoder-pipeline concern: an encoder that
/// returns the access unit for a submission only on the next one (NVENC with a
/// one-deep output queue) needs a duplicate submission after any change, or the
/// newest frame stays queued until the next keepalive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmissionMode {
    FirstFrame,
    Idr,
    Activity,
    Keepalive,
    PipelineFlush,
}

impl SubmissionMode {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FirstFrame => "first",
            Self::Idr => "idr",
            Self::Activity => "activity",
            Self::Keepalive => "keepalive",
            Self::PipelineFlush => "pipeline_flush",
        }
    }
}

impl From<EmitMode> for SubmissionMode {
    fn from(value: EmitMode) -> Self {
        match value {
            EmitMode::FirstFrame => Self::FirstFrame,
            EmitMode::Idr => Self::Idr,
            EmitMode::Activity => Self::Activity,
            EmitMode::Keepalive => Self::Keepalive,
        }
    }
}

/// When a hardware encoder submits a frame: on change, on a requested keyframe,
/// once more to flush its pipeline, and otherwise only at keepalive.
///
/// Shared by every NVENC adapter so a still desktop costs a keepalive per
/// second rather than a full encode per frame on any host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmissionGate {
    cadence: IdleCadence,
    pipeline_flush_pending: bool,
}

impl SubmissionGate {
    #[must_use]
    pub const fn new(keepalive: Duration) -> Self {
        Self {
            cadence: IdleCadence::new(keepalive),
            pipeline_flush_pending: false,
        }
    }

    /// A new frame with changed content is ready.
    pub const fn note_frame(&mut self) {
        self.cadence.note_frame();
    }

    /// The capture source was recreated; nothing retained may be re-sent.
    pub const fn reset(&mut self) {
        self.cadence.reset();
        self.pipeline_flush_pending = false;
    }

    #[must_use]
    pub fn decision(
        self,
        idr_pending: bool,
        elapsed_since_emit: Duration,
    ) -> Option<SubmissionMode> {
        self.cadence
            .decision(idr_pending, elapsed_since_emit)
            .map(SubmissionMode::from)
            .or_else(|| {
                self.pipeline_flush_pending
                    .then_some(SubmissionMode::PipelineFlush)
            })
    }

    /// Records a submission. `output_ready` is whether the encoder returned an
    /// access unit for it.
    pub const fn on_submitted(&mut self, mode: SubmissionMode, output_ready: bool) {
        self.cadence.on_submitted();
        self.pipeline_flush_pending = !output_ready
            || matches!(
                mode,
                SubmissionMode::FirstFrame | SubmissionMode::Idr | SubmissionMode::Activity
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEPALIVE: Duration = Duration::from_secs(1);

    #[test]
    fn no_frame_never_emits() {
        let cadence = IdleCadence::new(KEEPALIVE);
        assert_eq!(cadence.decision(true, KEEPALIVE), None);
    }

    #[test]
    fn activity_idr_and_keepalive_are_immediate_at_their_due_tick() {
        let mut cadence = IdleCadence::new(KEEPALIVE);
        cadence.note_frame();
        assert_eq!(
            cadence.decision(false, Duration::ZERO),
            Some(EmitMode::FirstFrame)
        );
        cadence.on_submitted();
        assert_eq!(cadence.decision(false, Duration::ZERO), None);

        cadence.note_frame();
        assert_eq!(
            cadence.decision(false, Duration::ZERO),
            Some(EmitMode::Activity)
        );
        cadence.on_submitted();
        assert_eq!(cadence.decision(true, Duration::ZERO), Some(EmitMode::Idr));
        cadence.on_submitted();
        assert_eq!(
            cadence.decision(false, Duration::from_nanos(999_999_999)),
            None
        );
        assert_eq!(
            cadence.decision(false, KEEPALIVE),
            Some(EmitMode::Keepalive)
        );
    }

    #[test]
    fn idr_takes_priority_over_dirty_activity() {
        let mut cadence = IdleCadence::new(KEEPALIVE);
        cadence.note_frame();
        cadence.on_submitted();
        cadence.note_frame();
        assert_eq!(cadence.decision(true, Duration::ZERO), Some(EmitMode::Idr));
    }

    #[test]
    fn reset_invalidates_retained_frame_state() {
        let mut cadence = IdleCadence::new(KEEPALIVE);
        cadence.note_frame();
        cadence.on_submitted();
        cadence.reset();
        assert_eq!(cadence.decision(true, KEEPALIVE), None);
    }
}

#[cfg(test)]
mod unchanged_frame_tests {
    use super::{EmitMode, IdleCadence, SubmissionGate, SubmissionMode};
    use core::time::Duration;

    const KEEPALIVE: Duration = Duration::from_secs(1);

    #[test]
    fn an_unchanged_frame_alone_is_not_worth_sending() {
        let mut cadence = IdleCadence::new(KEEPALIVE);
        cadence.note_frame();
        cadence.on_submitted();
        cadence.note_unchanged_frame();
        assert_eq!(cadence.decision(false, Duration::ZERO), None);
    }

    #[test]
    fn an_unchanged_frame_still_owes_a_keepalive() {
        let mut cadence = IdleCadence::new(KEEPALIVE);
        cadence.note_frame();
        cadence.on_submitted();
        cadence.note_unchanged_frame();
        assert_eq!(
            cadence.decision(false, KEEPALIVE),
            Some(EmitMode::Keepalive)
        );
    }

    #[test]
    fn an_unchanged_frame_can_serve_a_pending_recovery_point() {
        // A Deck that cannot decode asks for a full frame. A still desktop is
        // exactly when it most needs answering, because nothing else will
        // produce one.
        let mut cadence = IdleCadence::new(KEEPALIVE);
        cadence.note_frame();
        cadence.on_submitted();
        cadence.note_unchanged_frame();
        assert_eq!(cadence.decision(true, Duration::ZERO), Some(EmitMode::Idr));
    }

    #[test]
    fn an_unchanged_first_frame_is_still_the_first_frame() {
        let mut cadence = IdleCadence::new(KEEPALIVE);
        cadence.note_unchanged_frame();
        assert_eq!(
            cadence.decision(false, Duration::ZERO),
            Some(EmitMode::FirstFrame)
        );
    }

    #[test]
    fn no_frame_at_all_is_not_an_unchanged_frame() {
        let cadence = IdleCadence::new(KEEPALIVE);
        assert_eq!(cadence.decision(true, KEEPALIVE), None);
    }

    #[test]
    fn activity_after_an_unchanged_run_emits_again() {
        let mut cadence = IdleCadence::new(KEEPALIVE);
        cadence.note_frame();
        cadence.on_submitted();
        cadence.note_unchanged_frame();
        assert_eq!(cadence.decision(false, Duration::ZERO), None);
        cadence.note_frame();
        assert_eq!(
            cadence.decision(false, Duration::ZERO),
            Some(EmitMode::Activity)
        );
    }

    fn primed_gate() -> SubmissionGate {
        let mut gate = SubmissionGate::new(KEEPALIVE);
        gate.note_frame();
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::FirstFrame)
        );
        gate.on_submitted(SubmissionMode::FirstFrame, false);
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::PipelineFlush)
        );
        gate.on_submitted(SubmissionMode::PipelineFlush, true);
        gate
    }

    #[test]
    fn no_frame_or_early_idle_tick_does_not_submit() {
        let gate = SubmissionGate::new(KEEPALIVE);
        assert_eq!(gate.decision(true, KEEPALIVE), None);

        let gate = primed_gate();
        assert_eq!(gate.decision(false, Duration::from_millis(999)), None);
    }

    #[test]
    fn activity_and_idr_submit_on_the_next_tick_then_flush_once() {
        let mut gate = primed_gate();
        gate.note_frame();
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::Activity)
        );
        gate.on_submitted(SubmissionMode::Activity, true);
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::PipelineFlush)
        );
        gate.on_submitted(SubmissionMode::PipelineFlush, true);

        assert_eq!(
            gate.decision(true, Duration::ZERO),
            Some(SubmissionMode::Idr)
        );
        gate.on_submitted(SubmissionMode::Idr, true);
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::PipelineFlush)
        );
    }

    #[test]
    fn continuous_activity_supersedes_pending_flush_and_keepalive_is_single() {
        let mut gate = primed_gate();
        gate.note_frame();
        gate.on_submitted(SubmissionMode::Activity, true);
        gate.note_frame();
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::Activity)
        );
        gate.on_submitted(SubmissionMode::Activity, true);
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::PipelineFlush)
        );
        gate.on_submitted(SubmissionMode::PipelineFlush, true);

        assert_eq!(
            gate.decision(false, KEEPALIVE),
            Some(SubmissionMode::Keepalive)
        );
        gate.on_submitted(SubmissionMode::Keepalive, true);
        assert_eq!(gate.decision(false, Duration::ZERO), None);
    }

    #[test]
    fn capture_recreate_discards_retained_frame_and_pending_flush() {
        let mut gate = primed_gate();
        gate.note_frame();
        gate.on_submitted(SubmissionMode::Activity, true);
        gate.reset();
        assert_eq!(gate.decision(true, KEEPALIVE), None);

        gate.note_frame();
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::FirstFrame)
        );
    }
}
