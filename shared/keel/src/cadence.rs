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
    use super::{EmitMode, IdleCadence};
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
}
