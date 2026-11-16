// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! OS-free native gesture validation, ordering, and coalescing, and the
//! gesture samples a Deck captures (an `NSEvent` monitor on a Mac,
//! `DirectManipulation` on Windows) with the one way both send them.

use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_protocol::messages::{
    GESTURE_MAGNIFY, GESTURE_ROTATE, GESTURE_SMART_ZOOM, GESTURE_SWIPE, GestureMagnifyMsg,
    GestureRotateMsg, GestureSmartZoomMsg, GestureSwipeMsg, GestureValidationError, ScrollPhaseMsg,
    SwipeDirectionMsg,
};

use crate::ScrollPhase;

/// One trackpad gesture step as a Deck's platform reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GestureSample {
    /// A pinch: the scale changed by `scale_delta` (new / old − 1).
    Magnify {
        scale_delta: f64,
        phase: ScrollPhaseMsg,
    },
    /// A two-finger rotation by `degrees_delta`.
    Rotate {
        degrees_delta: f64,
        phase: ScrollPhaseMsg,
    },
    /// A two-finger double tap.
    SmartZoom,
    /// A multi-finger swipe.
    Swipe {
        direction: SwipeDirectionMsg,
        fingers: u8,
        phase: ScrollPhaseMsg,
    },
}

impl GestureSample {
    /// The message a Deck sends for this sample (to a host that advertised
    /// gestures), with the session's next input `sequence`. Continuous
    /// steps are coalescable: a host may keep only the latest.
    #[must_use]
    pub fn wire(self, sequence: u64, timestamp_ns: u64) -> GestureWire {
        match self {
            Self::Magnify { scale_delta, phase } => GestureWire::Magnify(GestureMagnifyMsg {
                scale_delta,
                phase,
                sequence,
                timestamp_ns,
                coalescable: true,
                ..GestureMagnifyMsg::default()
            }),
            Self::Rotate {
                degrees_delta,
                phase,
            } => GestureWire::Rotate(GestureRotateMsg {
                degrees_delta,
                phase,
                sequence,
                timestamp_ns,
                coalescable: true,
                ..GestureRotateMsg::default()
            }),
            Self::SmartZoom => GestureWire::SmartZoom(GestureSmartZoomMsg {
                sequence,
                timestamp_ns,
                ..GestureSmartZoomMsg::default()
            }),
            Self::Swipe {
                direction,
                fingers,
                phase,
            } => GestureWire::Swipe(GestureSwipeMsg {
                direction,
                fingers,
                phase,
                sequence,
                timestamp_ns,
                coalescable: true,
                ..GestureSwipeMsg::default()
            }),
        }
    }
}

/// One gesture-v1 message a Deck sends.
#[derive(Debug, Clone, PartialEq)]
pub enum GestureWire {
    Magnify(GestureMagnifyMsg),
    Rotate(GestureRotateMsg),
    SmartZoom(GestureSmartZoomMsg),
    Swipe(GestureSwipeMsg),
}

impl GestureWire {
    /// The message's `type`, as input statistics count it.
    #[must_use]
    pub const fn input_type(&self) -> &'static str {
        self.as_ref().input_type()
    }

    /// The borrowed DTO.
    #[must_use]
    pub const fn as_ref(&self) -> GestureWireRef<'_> {
        match self {
            Self::Magnify(message) => GestureWireRef::Magnify(message),
            Self::Rotate(message) => GestureWireRef::Rotate(message),
            Self::SmartZoom(message) => GestureWireRef::SmartZoom(message),
            Self::Swipe(message) => GestureWireRef::Swipe(message),
        }
    }

    /// The JSON the session lane carries.
    ///
    /// # Errors
    ///
    /// A value JSON cannot hold (a non-finite delta).
    pub fn to_value(&self) -> serde_json::Result<serde_json::Value> {
        match self {
            Self::Magnify(message) => serde_json::to_value(message),
            Self::Rotate(message) => serde_json::to_value(message),
            Self::SmartZoom(message) => serde_json::to_value(message),
            Self::Swipe(message) => serde_json::to_value(message),
        }
    }
}

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

#[cfg(test)]
mod sample_tests {
    use super::*;

    #[test]
    fn a_sample_goes_as_the_message_the_mac_deck_always_sent() {
        let magnify = GestureSample::Magnify {
            scale_delta: 0.1,
            phase: ScrollPhaseMsg::Began,
        }
        .wire(7, 99);
        let value = magnify.to_value().expect("serializes");
        assert_eq!(value["type"], "gesture_magnify");
        assert_eq!(value["scale_delta"], 0.1);
        assert_eq!(value["phase"], "began");
        assert_eq!(value["sequence"], 7);
        assert_eq!(value["timestamp_ns"], 99);
        assert_eq!(value["coalescable"], true);
        assert_eq!(magnify.input_type(), GESTURE_MAGNIFY);
        assert!(magnify.as_ref().validate().is_ok());

        let swipe = GestureSample::Swipe {
            direction: SwipeDirectionMsg::Left,
            fingers: 3,
            phase: ScrollPhaseMsg::Changed,
        }
        .wire(8, 100);
        assert_eq!(swipe.input_type(), GESTURE_SWIPE);
        assert_eq!(swipe.to_value().expect("serializes")["fingers"], 3);
        let zoom = GestureSample::SmartZoom.wire(9, 101);
        assert_eq!(zoom.input_type(), GESTURE_SMART_ZOOM);
        assert_eq!(zoom.to_value().expect("serializes")["sequence"], 9);
        let rotate = GestureSample::Rotate {
            degrees_delta: -4.0,
            phase: ScrollPhaseMsg::Ended,
        }
        .wire(10, 102);
        assert_eq!(rotate.input_type(), GESTURE_ROTATE);
    }
}
