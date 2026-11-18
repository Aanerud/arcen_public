// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What a Pier that cannot inject a trackpad gesture does with one: the
//! one rule every such Pier follows, so a pinch from either Deck zooms the
//! same way on a Linux and a Windows host.
//!
//! A pinch (`gesture_magnify`) becomes Control + wheel, the zoom every
//! desktop application on X11, Wayland and Windows already answers (a
//! Windows precision touchpad's own pinch reaches applications the same
//! way). Each message is one self-contained chord: Control down, the wheel
//! step, Control up, so nothing is left held when a gesture is cancelled,
//! its last message is lost or the session ends. A Control the Deck already
//! holds is used as it is and never released by a chord.
//!
//! Rotation, smart zoom and multi-finger swipes have no equivalent there:
//! they are declined, counted, and logged by the Pier, never an error that
//! ends the session.

use arcen_protocol::keys::{KEY_CONTROL, MOD_CTRL};
use arcen_protocol::messages::{
    GESTURE_ROTATE, GESTURE_SMART_ZOOM, GESTURE_SWIPE, GestureMagnifyMsg, GestureValidationError,
    KeyEventMsg, MouseScrollMsg, PointerMotionMode, ScrollUnitMsg,
};

/// The change of scale one wheel notch stands for: a pinch to twice the
/// size is about four notches, the zoom steps (110 %, 125 %, 150 %, 175 %,
/// 200 %) browsers take on Control + wheel.
pub const SCALE_PER_WHEEL_NOTCH: f64 = 1.2;

/// High-resolution wheel units in one notch (the Windows `WHEEL_DELTA`,
/// evdev's `REL_WHEEL_HI_RES` step).
pub const WHEEL_UNITS_PER_NOTCH: f64 = 120.0;

/// The most notches one pinch step scrolls, whatever its scale says: a
/// Deck reports a pinch in small steps, and a bogus one must not scroll a
/// document by pages.
pub const MAX_WHEEL_NOTCHES_PER_STEP: f64 = 4.0;

/// One input step of a gesture's fallback, in the messages a Pier already
/// injects: the key goes through its `key_event` path, the wheel through its
/// `mouse_scroll` path (relative: at the pointer, wherever it is) in
/// high-resolution units, 120 to a notch, so the small steps a pinch arrives
/// in add up to whole notches on every host instead of each rounding to one.
#[derive(Debug, Clone, PartialEq)]
pub enum GestureFallbackInput {
    /// Control pressed or released.
    Key(KeyEventMsg),
    /// The wheel step (`unit: point`, 120 to a notch; positive: up, which
    /// zooms in).
    Scroll(MouseScrollMsg),
}

/// What a declined gesture was, for the Pier's counters and its log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclinedGesture {
    /// A two-finger rotation.
    Rotate,
    /// A two-finger double tap.
    SmartZoom,
    /// A multi-finger swipe.
    Swipe,
}

impl DeclinedGesture {
    /// The gesture a message type names, when it is one a fallback Pier
    /// declines (`gesture_rotate`, `gesture_smart_zoom`, `gesture_swipe`).
    #[must_use]
    pub fn from_message_type(message_type: &str) -> Option<Self> {
        match message_type {
            GESTURE_ROTATE => Some(Self::Rotate),
            GESTURE_SMART_ZOOM => Some(Self::SmartZoom),
            GESTURE_SWIPE => Some(Self::Swipe),
            _ => None,
        }
    }

    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Rotate => "rotate",
            Self::SmartZoom => "smart_zoom",
            Self::Swipe => "swipe",
        }
    }
}

/// Wheel notches for a pinch step of `scale_delta` (new / old − 1):
/// logarithmic, so pinching out and back in returns to where it started,
/// and at most [`MAX_WHEEL_NOTCHES_PER_STEP`] either way.
#[must_use]
pub fn magnify_wheel_notches(scale_delta: f64) -> f64 {
    let scale = 1.0 + scale_delta;
    if !scale.is_finite() || scale <= 0.0 {
        return 0.0;
    }
    (scale.ln() / SCALE_PER_WHEEL_NOTCH.ln())
        .clamp(-MAX_WHEEL_NOTCHES_PER_STEP, MAX_WHEEL_NOTCHES_PER_STEP)
}

/// The chord one `gesture_magnify` becomes: Control down, the wheel step,
/// Control up; only the wheel step when `control_held` (the Deck already
/// holds Control); nothing for a step too small to scroll.
///
/// # Errors
///
/// Returns the message's own validation error.
pub fn magnify_chord(
    message: &GestureMagnifyMsg,
    control_held: bool,
) -> Result<Vec<GestureFallbackInput>, GestureValidationError> {
    message.validate()?;
    let notches = magnify_wheel_notches(message.scale_delta);
    if notches == 0.0 {
        return Ok(Vec::new());
    }
    let control = |pressed: bool| {
        GestureFallbackInput::Key(KeyEventMsg {
            scan_code: KEY_CONTROL,
            pressed,
            modifiers: if pressed { MOD_CTRL } else { 0 },
            sequence: message.sequence,
            timestamp_ns: message.timestamp_ns,
            ..KeyEventMsg::default()
        })
    };
    let wheel = GestureFallbackInput::Scroll(MouseScrollMsg {
        dy: notches * WHEEL_UNITS_PER_NOTCH,
        unit: ScrollUnitMsg::Point,
        motion_mode: PointerMotionMode::Relative,
        sequence: message.sequence,
        timestamp_ns: message.timestamp_ns,
        ..MouseScrollMsg::default()
    });
    Ok(if control_held {
        vec![wheel]
    } else {
        vec![control(true), wheel, control(false)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::ScrollPhaseMsg;

    fn pinch(scale_delta: f64) -> GestureMagnifyMsg {
        GestureMagnifyMsg {
            scale_delta,
            phase: ScrollPhaseMsg::Changed,
            sequence: 7,
            ..GestureMagnifyMsg::default()
        }
    }

    #[test]
    fn a_pinch_is_one_control_wheel_chord_that_leaves_nothing_held() {
        let chord = magnify_chord(&pinch(0.2), false).expect("chord");
        let [
            GestureFallbackInput::Key(down),
            GestureFallbackInput::Scroll(wheel),
            GestureFallbackInput::Key(up),
        ] = chord.as_slice()
        else {
            panic!("expected Control, wheel, Control: {chord:?}");
        };
        assert_eq!(
            (down.scan_code, down.pressed, down.modifiers),
            (KEY_CONTROL, true, MOD_CTRL)
        );
        assert_eq!(
            (up.scan_code, up.pressed, up.modifiers),
            (KEY_CONTROL, false, 0)
        );
        assert!(
            (wheel.dy - WHEEL_UNITS_PER_NOTCH).abs() < 1e-9,
            "20 % larger is one notch up"
        );
        assert_eq!(wheel.motion_mode, PointerMotionMode::Relative);
        assert_eq!(wheel.unit, ScrollUnitMsg::Point);
        // A step of 1 %, as a trackpad reports a pinch, is a small part of
        // a notch, not a whole one.
        let small = magnify_chord(&pinch(0.01), false).expect("chord");
        let [_, GestureFallbackInput::Scroll(wheel), _] = small.as_slice() else {
            panic!("expected Control, wheel, Control: {small:?}");
        };
        assert!(wheel.dy > 6.0 && wheel.dy < 7.0, "{}", wheel.dy);
    }

    #[test]
    fn pinching_in_scrolls_down_and_undoes_pinching_out() {
        let out = magnify_wheel_notches(0.44);
        let back = magnify_wheel_notches(1.0 / 1.44 - 1.0);
        assert!((out - 2.0).abs() < 1e-9);
        assert!((out + back).abs() < 1e-9);
    }

    #[test]
    fn a_control_the_deck_holds_is_never_released() {
        let chord = magnify_chord(&pinch(-0.1), true).expect("chord");
        assert!(
            matches!(chord.as_slice(), [GestureFallbackInput::Scroll(wheel)] if wheel.dy < 0.0)
        );
    }

    #[test]
    fn rotate_smart_zoom_and_swipe_are_declined_by_name() {
        assert_eq!(
            DeclinedGesture::from_message_type("gesture_swipe"),
            Some(DeclinedGesture::Swipe)
        );
        assert_eq!(DeclinedGesture::from_message_type("gesture_magnify"), None);
    }

    #[test]
    fn nothing_to_scroll_sends_nothing_and_invalid_steps_are_refused() {
        assert!(magnify_chord(&pinch(0.0), false).expect("chord").is_empty());
        assert!(
            magnify_chord(&pinch(-1.0), false)
                .expect("chord")
                .is_empty()
        );
        assert!(magnify_chord(&pinch(f64::NAN), false).is_err());
        assert!((magnify_wheel_notches(1e300) - MAX_WHEEL_NOTCHES_PER_STEP).abs() < 1e-9);
    }
}
