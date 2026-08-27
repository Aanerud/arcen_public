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
