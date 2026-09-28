//! OS-free native gesture validation, ordering, and coalescing.

use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_protocol::messages::{
    GESTURE_MAGNIFY, GESTURE_ROTATE, GESTURE_SMART_ZOOM, GESTURE_SWIPE, GestureMagnifyMsg,
    GestureRotateMsg, GestureSmartZoomMsg, GestureSwipeMsg, GestureValidationError,
    SwipeDirectionMsg,
};

use crate::ScrollPhase;

/// One borrowed gesture-v1 protocol DTO.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GestureWireRef<'a> {
    Magnify(&'a GestureMagnifyMsg),
    Rotate(&'a GestureRotateMsg),
    SmartZoom(&'a GestureSmartZoomMsg),
    Swipe(&'a GestureSwipeMsg),
}

impl GestureWireRef<'_> {
    /// Validates the DTO's physical ranges and sequence.
    ///
    /// # Errors
    ///
    /// Returns the first invalid wire field.
    pub fn validate(self) -> Result<(), GestureValidationError> {
        match self {
            Self::Magnify(message) => message.validate(),
            Self::Rotate(message) => message.validate(),
            Self::SmartZoom(message) => message.validate(),
            Self::Swipe(message) => message.validate(),
        }
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        match self {
            Self::Magnify(message) => message.sequence,
            Self::Rotate(message) => message.sequence,
            Self::SmartZoom(message) => message.sequence,
            Self::Swipe(message) => message.sequence,
        }
    }

    #[must_use]
    pub const fn input_type(self) -> &'static str {
        match self {
            Self::Magnify(_) => GESTURE_MAGNIFY,
            Self::Rotate(_) => GESTURE_ROTATE,
            Self::SmartZoom(_) => GESTURE_SMART_ZOOM,
            Self::Swipe(_) => GESTURE_SWIPE,
        }
    }

    /// Decodes into the canonical gesture event.
    ///
    /// # Errors
    ///
    /// Returns the first invalid wire field.
    pub fn decode(self) -> Result<GestureEvent, GestureValidationError> {
        self.validate()?;
        Ok(match self {
            Self::Magnify(message) => GestureEvent::Magnify {
                scale_delta: message.scale_delta,
                phase: message.phase.into(),
                sequence: message.sequence,
            },
            Self::Rotate(message) => GestureEvent::Rotate {
                degrees_delta: message.degrees_delta,
                phase: message.phase.into(),
                sequence: message.sequence,
            },
            Self::SmartZoom(message) => GestureEvent::SmartZoom {
                sequence: message.sequence,
            },
            Self::Swipe(message) => GestureEvent::Swipe {
                direction: message.direction,
                fingers: message.fingers,
                phase: message.phase.into(),
                sequence: message.sequence,
            },
        })
    }
}

/// Canonical gesture event after checked wire validation.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum GestureEvent {
    Magnify {
        scale_delta: f64,
        phase: ScrollPhase,
        sequence: u64,
    },
    Rotate {
        degrees_delta: f64,
        phase: ScrollPhase,
        sequence: u64,
    },
    SmartZoom {
        sequence: u64,
    },
    Swipe {
        direction: SwipeDirectionMsg,
        fingers: u8,
        phase: ScrollPhase,
        sequence: u64,
    },
}

impl GestureEvent {
    #[must_use]
    pub const fn sequence(self) -> u64 {
        match self {
            Self::Magnify { sequence, .. }
            | Self::Rotate { sequence, .. }
            | Self::SmartZoom { sequence }
            | Self::Swipe { sequence, .. } => sequence,
        }
    }

    #[must_use]
    const fn coalesce_class(self) -> Option<GestureCoalesceClass> {
        match self {
            Self::Magnify {
                phase: ScrollPhase::Changed,
                ..
            } => Some(GestureCoalesceClass::Magnify),
            Self::Rotate {
                phase: ScrollPhase::Changed,
                ..
            } => Some(GestureCoalesceClass::Rotate),
            Self::Swipe {
                phase: ScrollPhase::Changed,
                ..
            } => Some(GestureCoalesceClass::Swipe),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureCoalesceClass {
    Magnify,
    Rotate,
    Swipe,
}

/// Dependency-safe ordered gesture state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GestureState {
    last_sequence: u64,
}

impl GestureState {
    #[must_use]
    pub const fn new() -> Self {
        Self { last_sequence: 0 }
    }

    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    /// Applies one validated event atomically.
    ///
    /// # Errors
    ///
    /// Returns [`GestureStateError::InvalidSequence`] without changing state
    /// when ordering is not strictly increasing.
    pub fn apply(&mut self, event: GestureEvent) -> Result<(), GestureStateError> {
        let sequence = event.sequence();
        if sequence == 0 || sequence <= self.last_sequence {
            return Err(GestureStateError::InvalidSequence {
                last: self.last_sequence,
                received: sequence,
            });
        }
        self.last_sequence = sequence;
        Ok(())
    }
}

/// Rejected semantic gesture transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GestureStateError {
    InvalidSequence { last: u64, received: u64 },
}

impl Display for GestureStateError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSequence { last, received } => {
                write!(
                    formatter,
                    "gesture sequence {received} does not follow {last}"
                )
            }
        }
    }
}

impl Error for GestureStateError {}

/// Coalesces replaceable continuous gesture changes while preserving order.
#[must_use]
pub fn coalesce_gestures(events: &[GestureEvent]) -> Vec<GestureEvent> {
    let mut retained = Vec::with_capacity(events.len());
    for &event in events {
        if let Some(class) = event.coalesce_class() {
            if let Some(index) = retained
                .iter()
                .rposition(|candidate: &GestureEvent| candidate.coalesce_class() == Some(class))
            {
                retained[index] = event;
                continue;
            }
        }
        retained.push(event);
    }
    retained
}
