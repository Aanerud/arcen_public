//! Applying a client's input to the local desktop.
//!
//! This is what turns a picture into a desktop someone can use. Messages
//! arrive as the same JSON the other hosts receive, are ordered through the
//! shared sequence tracker, and are injected with [`InputController`].
//!
//! Ordering matters more than it looks. Input arrives over a stream that can
//! reorder nothing but can certainly deliver a burst, and a button release
//! applied before its press leaves a button stuck down on someone's machine.
//! The shared tracker rejects anything that is not a strict advance, and
//! everything held is released when the session ends.

use arcen_input::{
    InputSequenceTracker, KeyboardEvent, LowLatencyMetadata, ModifierMask, PointerButton,
    PointerMotion, PointerScroll,
};
use arcen_protocol::messages::{
    KEY_RESET_MODIFIERS, KeyEventMsg, MOUSE_SCROLL, MouseButtonMsg, MouseMoveMsg, MouseScrollMsg,
    PEN_EVENT, PenEventMsg,
};

/// Wire names the shared crate does not expose as constants.
const MOUSE_MOVE: &str = "mouse_move";
const MOUSE_BUTTON: &str = "mouse_button";
const KEY_EVENT: &str = "key_event";
use serde::Serialize;

use crate::input::{DesktopBounds, InputController, InputError};

/// What a session did with the input it was sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct InputStats {
    /// Messages injected.
    pub applied: u64,
    /// Messages dropped because they arrived out of order.
    pub out_of_order: u64,
    /// Messages this host does not handle.
    pub unsupported: u64,
    /// Messages that were not valid JSON for their declared type.
    pub malformed: u64,
    /// Pen samples injected with their tablet surface intact.
    ///
    /// Counted separately from `applied` because a pen that is being received
    /// but not injected, and a pen that is not arriving at all, are different
    /// faults with the same symptom on screen.
    pub pen_samples: u64,
    /// Pen proximity transitions posted.
    pub pen_proximity_edges: u64,
    /// Keys this host has no macOS equivalent for.
    ///
    /// The controller refuses these rather than pressing the key that occupies
    /// the same physical position, so they must be reported: `applied` counted
    /// them as successes, which made a keyboard that silently drops a key look
    /// identical to one that works.
    pub unmapped_keys: u64,
    /// Pen samples refused because a field was outside its physical range.
    pub pen_rejected: u64,
    /// Scroll events posted to the desktop.
    ///
    /// Reported separately because `applied` counts every kind of input
    /// together, so a session where scrolling never arrives looks exactly like
    /// one where it works — which is not a distinction anyone can make from a
    /// total.
    pub scroll_events: u64,
}

/// Applies client input to the local desktop.
pub struct InputSession {
    controller: InputController,
    sequence: InputSequenceTracker,
    stats: InputStats,
}

impl std::fmt::Debug for InputSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputSession")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl InputSession {
    /// Creates a session mapping normalized coordinates onto `bounds`.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] when no event source is available.
    pub fn new(bounds: DesktopBounds) -> Result<Self, InputError> {
        Ok(Self {
            controller: InputController::new(bounds)?,
            sequence: InputSequenceTracker::default(),
            stats: InputStats::default(),
        })
    }

    /// Returns what has happened so far.
    #[must_use]
    pub fn stats(&self) -> InputStats {
        // The controller owns the pen counters because it is what posts the
        // events; reading them here keeps one source of truth rather than two
        // counters that can disagree.
        let controller = self.controller.stats();
        InputStats {
            pen_samples: controller.pen_samples,
            pen_proximity_edges: controller.pen_proximity_edges,
            unmapped_keys: controller.unmapped_keys,
            scroll_events: controller.scroll_events,
            ..self.stats
        }
    }

    /// Applies one message.
    ///
    /// Unknown message types and malformed payloads are counted and ignored
    /// rather than ending the session: a client sending something this host
    /// does not implement should lose that feature, not its desktop.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] only when injection itself fails.
    pub fn apply(&mut self, json: &str) -> Result<(), InputError> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        let Some(kind) = value.get("type").and_then(serde_json::Value::as_str) else {
            self.stats.malformed += 1;
            return Ok(());
        };

        match kind {
            MOUSE_MOVE => self.apply_move(&value),
            MOUSE_BUTTON => self.apply_button(&value),
            MOUSE_SCROLL => self.apply_scroll(&value),
            KEY_EVENT => self.apply_key(&value),
            PEN_EVENT => self.apply_pen(&value),
            // A client that has lost track of what it is holding asks for
            // everything to be let go. Honouring it is what keeps a modifier
            // from sticking after an alt-tab away from the client window.
            KEY_RESET_MODIFIERS => {
                self.controller.release_all()?;
                self.stats.applied += 1;
                Ok(())
            }
            _ => {
                self.stats.unsupported += 1;
                Ok(())
            }
        }
    }

    /// Returns whether `sequence` may be applied, counting it if not.
    fn accepts(&mut self, sequence: u64) -> bool {
        if self.sequence.accept(sequence) {
            true
        } else {
            self.stats.out_of_order += 1;
            false
        }
    }

    fn apply_move(&mut self, value: &serde_json::Value) -> Result<(), InputError> {
        let Ok(message) = serde_json::from_value::<MouseMoveMsg>(value.clone()) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        if !self.accepts(message.sequence) {
            return Ok(());
        }
        self.controller.pointer_motion(&PointerMotion {
            x: message.x,
            y: message.y,
            server_x: None,
            server_y: None,
            metadata: metadata(message.sequence, message.timestamp_ns),
        })?;
        self.stats.applied += 1;
        Ok(())
    }

    fn apply_button(&mut self, value: &serde_json::Value) -> Result<(), InputError> {
        let Ok(message) = serde_json::from_value::<MouseButtonMsg>(value.clone()) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        if !self.accepts(message.sequence) {
            return Ok(());
        }
        self.controller.pointer_button(&pointer_button(&message))?;
        self.stats.applied += 1;
        Ok(())
    }

    fn apply_scroll(&mut self, value: &serde_json::Value) -> Result<(), InputError> {
        let Ok(message) = serde_json::from_value::<MouseScrollMsg>(value.clone()) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        if !self.accepts(message.sequence) {
            return Ok(());
        }
        self.controller.pointer_scroll(&pointer_scroll(&message))?;
        self.stats.applied += 1;
        Ok(())
    }

    fn apply_key(&mut self, value: &serde_json::Value) -> Result<(), InputError> {
        let Ok(message) = serde_json::from_value::<KeyEventMsg>(value.clone()) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        if !self.accepts(message.sequence) {
            return Ok(());
        }
        let before_unmapped = self.controller.stats().unmapped_keys;
        self.controller.key_event(&KeyboardEvent {
            key_id: message.scan_code,
            pressed: message.pressed,
            modifiers: ModifierMask(message.modifiers),
            caps_lock_on: message.caps_lock_on,
            num_lock_on: None,
            scroll_lock_on: None,
            metadata: metadata(message.sequence, message.timestamp_ns),
        })?;
        // Only if a key was actually posted. The controller refuses a key it
        // has no macOS equivalent for, and counting that as applied made a
        // keyboard silently dropping keys indistinguishable from one that
        // works. The controller's own counter is the evidence.
        if self.controller.stats().unmapped_keys == before_unmapped {
            self.stats.applied += 1;
        }
        Ok(())
    }

    /// Applies a pen sample.
    ///
    /// The sample is validated before injection rather than trusted. A pen
    /// carries more fields than a mouse and each is a physical quantity with a
    /// range; a value outside it is a bug or a hostile peer, and injecting it
    /// puts an impossible pressure or tilt in front of a drawing application.
    fn apply_pen(&mut self, value: &serde_json::Value) -> Result<(), InputError> {
        let Ok(message) = serde_json::from_value::<PenEventMsg>(value.clone()) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        if message.validate().is_err() {
            // Counted as a pen rejection rather than as generic malformed
            // input: a client whose pen is out of range is a different report
            // from one sending unparseable JSON.
            self.stats.pen_rejected += 1;
            return Ok(());
        }
        if !self.accepts(message.sequence) {
            return Ok(());
        }
        self.controller.pen_event(&message)?;
        self.stats.applied += 1;
        Ok(())
    }

    /// Releases everything this session is holding.
    ///
    /// Called when the session ends, so a dropped client never leaves a key or
    /// button down on the physical machine.
    ///
    /// # Errors
    ///
    /// Returns the first [`InputError`] encountered, after attempting all
    /// releases.
    pub fn release_all(&mut self) -> Result<(), InputError> {
        self.controller.release_all()
    }
}

fn metadata(sequence: u64, timestamp_ns: u64) -> LowLatencyMetadata {
    LowLatencyMetadata {
        sequence,
        timestamp_ns,
        ..LowLatencyMetadata::default()
    }
}

fn pointer_motion(x: f64, y: f64, sequence: u64, timestamp_ns: u64) -> PointerMotion {
    PointerMotion {
        x,
        y,
        server_x: None,
        server_y: None,
        metadata: metadata(sequence, timestamp_ns),
    }
}

fn pointer_button(message: &MouseButtonMsg) -> PointerButton {
    PointerButton {
        button: message.button,
        pressed: message.pressed,
        motion_mode: pointer_motion_mode(message.motion_mode),
        position: pointer_motion(message.x, message.y, message.sequence, message.timestamp_ns),
    }
}

fn pointer_scroll(message: &MouseScrollMsg) -> PointerScroll {
    PointerScroll {
        delta_x: message.dx,
        delta_y: message.dy,
        unit: match message.unit {
            arcen_protocol::messages::ScrollUnitMsg::Line => arcen_input::ScrollUnit::Line,
            arcen_protocol::messages::ScrollUnitMsg::Point => arcen_input::ScrollUnit::Point,
        },
        phase: match message.phase {
            arcen_protocol::messages::ScrollPhaseMsg::None => arcen_input::ScrollPhase::None,
            arcen_protocol::messages::ScrollPhaseMsg::Began => arcen_input::ScrollPhase::Began,
            arcen_protocol::messages::ScrollPhaseMsg::Changed => arcen_input::ScrollPhase::Changed,
            arcen_protocol::messages::ScrollPhaseMsg::Ended => arcen_input::ScrollPhase::Ended,
            arcen_protocol::messages::ScrollPhaseMsg::Cancelled => {
                arcen_input::ScrollPhase::Cancelled
            }
        },
        motion_mode: pointer_motion_mode(message.motion_mode),
        position: pointer_motion(message.x, message.y, message.sequence, message.timestamp_ns),
    }
}

const fn pointer_motion_mode(
    mode: arcen_protocol::messages::PointerMotionMode,
) -> arcen_input::PointerMotionMode {
    match mode {
        arcen_protocol::messages::PointerMotionMode::Absolute => {
            arcen_input::PointerMotionMode::Absolute
        }
        arcen_protocol::messages::PointerMotionMode::Relative => {
            arcen_input::PointerMotionMode::Relative
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_key_with_no_mac_equivalent_is_reported_not_substituted() {
        // Insert used to be translated to Help, which occupies the same
        // physical position and is a different key. That does not give the
        // person Insert; it presses something else. And the unmapped result
        // was counted as `applied`, so a keyboard silently dropping keys
        // looked exactly like one that works.
        let Some(mut session) = session() else {
            return;
        };
        const QT_INSERT: u32 = 0x0100_0006;
        let event = serde_json::json!({
            "type": "key_event",
            "scan_code": QT_INSERT,
            "pressed": true,
            "modifiers": 0,
            "caps_lock_on": false,
            "sequence": 1,
            "timestamp_ns": 0,
        })
        .to_string();
        session.apply(&event).expect("no panic");
        let stats = session.stats();
        assert_eq!(stats.unmapped_keys, 1, "the gap must be visible");
        assert_eq!(stats.applied, 0, "nothing was pressed, so nothing applied");
    }
    use super::*;
    use serde_json::json;

    fn bounds() -> DesktopBounds {
        DesktopBounds::new(0.0, 0.0, 1920.0, 1080.0)
    }

    fn session() -> Option<InputSession> {
        // Without an event source there is nothing to test against; that is a
        // machine without Accessibility, not a failing assertion.
        InputSession::new(bounds()).ok()
    }

    #[test]
    fn relative_button_motion_mode_survives_wire_decode() {
        let Ok(message) = serde_json::from_value::<MouseButtonMsg>(json!({
            "type": MOUSE_BUTTON,
            "x": -12.0,
            "y": 7.0,
            "button": 1,
            "pressed": true,
            "motion_mode": "relative",
            "sequence": 42,
            "timestamp_ns": 99
        })) else {
            panic!("button message");
        };

        let event = pointer_button(&message);

        assert_eq!(event.motion_mode, arcen_input::PointerMotionMode::Relative);
        assert!((event.position.x + 12.0).abs() < f64::EPSILON);
        assert!((event.position.y - 7.0).abs() < f64::EPSILON);
    }

    #[test]
    fn scroll_position_and_motion_mode_survive_wire_decode() {
        let Ok(message) = serde_json::from_value::<MouseScrollMsg>(json!({
            "type": MOUSE_SCROLL,
            "x": 0.75,
            "y": 0.25,
            "dx": 1.0,
            "dy": -3.0,
            "motion_mode": "absolute",
            "sequence": 43,
            "timestamp_ns": 100
        })) else {
            panic!("scroll message");
        };

        let event = pointer_scroll(&message);

        assert_eq!(event.motion_mode, arcen_input::PointerMotionMode::Absolute);
        assert!((event.position.x - 0.75).abs() < f64::EPSILON);
        assert!((event.position.y - 0.25).abs() < f64::EPSILON);
        assert_eq!(event.position.metadata.sequence, 43);
    }

    #[test]
    fn unknown_message_types_are_counted_rather_than_fatal() {
        let Some(mut session) = session() else {
            return;
        };
        session
            .apply(&json!({"type": "something_else", "sequence": 1}).to_string())
            .expect("an unknown type must not end the session");
        assert_eq!(session.stats().unsupported, 1);
        assert_eq!(session.stats().applied, 0);
    }

    #[test]
    fn malformed_payloads_are_counted_rather_than_fatal() {
        let Some(mut session) = session() else {
            return;
        };
        session.apply("not json at all").expect("no panic");
        session
            .apply(&json!({"no_type": true}).to_string())
            .expect("no panic");
        // Right type, wrong shape.
        session
            .apply(&json!({"type": MOUSE_MOVE, "x": "left"}).to_string())
            .expect("no panic");
        assert_eq!(session.stats().malformed, 3);
    }

    #[test]
    fn out_of_order_input_is_rejected_rather_than_applied() {
        // A release applied before its press leaves a button stuck down on
        // someone's machine.
        let Some(mut session) = session() else {
            return;
        };
        let move_at = |sequence: u64| {
            json!({
                "type": MOUSE_MOVE,
                "x": 0.5, "y": 0.5,
                "server_x": 0, "server_y": 0,
                "sequence": sequence,
                "timestamp_ns": 0
            })
            .to_string()
        };
        session.apply(&move_at(10)).expect("first");
        session.apply(&move_at(5)).expect("stale");
        session.apply(&move_at(10)).expect("replay");
        assert_eq!(session.stats().applied, 1);
        assert_eq!(session.stats().out_of_order, 2);
    }

    #[test]
    fn a_reset_request_releases_everything() {
        let Some(mut session) = session() else {
            return;
        };
        session
            .apply(&json!({"type": KEY_RESET_MODIFIERS}).to_string())
            .expect("reset applies");
        assert_eq!(session.stats().applied, 1);
    }

    #[test]
    fn pointer_and_key_messages_are_accepted() {
        // Warps the pointer and scrolls the desktop this runs on.
        if !crate::desktop_tests_allowed() {
            return;
        }
        let Some(mut session) = session() else {
            return;
        };
        session
            .apply(
                &json!({
                    "type": MOUSE_MOVE, "x": 0.25, "y": 0.25,
                    "server_x": 0, "server_y": 0, "sequence": 1, "timestamp_ns": 0
                })
                .to_string(),
            )
            .expect("move");
        session
            .apply(
                &json!({
                    "type": MOUSE_SCROLL, "x": 0.25, "y": 0.25, "dx": 0.0, "dy": 3.0,
                    "server_x": 0, "server_y": 0, "sequence": 2, "timestamp_ns": 0
                })
                .to_string(),
            )
            .expect("scroll");
        assert_eq!(session.stats().applied, 2);
        assert_eq!(session.stats().malformed, 0);

        // Release anything the test may have left held.
        session.release_all().expect("release");
    }

    #[test]
    fn a_pen_sample_outside_its_physical_range_is_refused_not_injected() {
        // Pressure above one, or a tilt beyond ninety degrees, is not a
        // quantity a digitizer can produce. Injecting it would put an
        // impossible value in front of a drawing application; counting it as a
        // pen rejection rather than as malformed JSON keeps the two reports
        // distinguishable in a log.
        let Ok(mut session) = InputSession::new(DesktopBounds::new(0.0, 0.0, 100.0, 100.0)) else {
            // No event source on this machine; the mapping is covered by the
            // pen module's own tests.
            return;
        };
        let out_of_range = json!({
            "type": PEN_EVENT, "x": 0.5, "y": 0.5, "pressure": 4.0,
            "tilt_x_degrees": 0.0, "tilt_y_degrees": 0.0, "rotation_degrees": 0.0,
            "tool": "tip", "in_proximity": true, "touching": true, "buttons": 0,
            "server_x": 0, "server_y": 0, "sequence": 1, "timestamp_ns": 0
        })
        .to_string();
        session
            .apply(&out_of_range)
            .expect("refusal is not an error");
        assert_eq!(session.stats().pen_rejected, 1, "counted as a pen refusal");
        assert_eq!(session.stats().applied, 0, "and never injected");
        assert_eq!(session.stats().malformed, 0, "not confused with bad JSON");
        session.release_all().expect("release");
    }
}
