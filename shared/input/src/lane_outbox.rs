//! Ordered outbox for the input/cursor lane (ADR 0014).
//!
//! The lane writer's queue is bounded and reports a full queue instead of
//! dropping. An input owner holds what did not fit here, in order, and
//! retries. Edges (keys, buttons, scroll, pen, gestures, enter/leave,
//! held-state resets, text) are never dropped, merged or reordered. Pointer
//! motion waiting directly behind motion of the same kind is coalesced:
//!
//! - an absolute position (`mouse_move`) or a region position
//!   (`region_pointer_motion`) replaces the previous position of the same
//!   kind, because only the latest position matters;
//! - relative motion (`mouse_move_relative`) is summed into the previous
//!   relative record, keeping the newest sequence and timestamp, because
//!   every delta moves the pointer.
//!
//! Motion of different kinds is never merged: a relative delta never
//! replaces an absolute warp, and an absolute position never swallows a
//! delta.
//!
//! Pen samples (`pen_event`, `region_pen_event`) follow the shared pen
//! forwarding policy ([`crate::PenForwardingDispatcher`]):
//!
//! - a pen edge (proximity, tool, contact or button change, or anything the
//!   sender marked not coalescable) is an [`InputRecordClass::Edge`];
//! - a hovering sample (in proximity, tip up) replaces a hovering sample of
//!   the same tool, buttons and region directly ahead of it, like a pointer
//!   position;
//! - a sample taken while the tip is down is ink: its pressure and position
//!   are kept. Only when more than [`PEN_CONTACT_BACKLOG_CAP`] of them are
//!   waiting behind a stalled lane is the oldest adjacent pair of the same
//!   stroke folded (newer wins), and every fold is counted in
//!   [`InputLaneOutbox::pen_contact_decimated`].

use std::collections::VecDeque;

use arcen_protocol::messages::{MOUSE_MOVE_RELATIVE, MouseMoveRelativeMsg, REGION_POINTER_MOTION};
use serde_json::Value;

/// How a queued input record may be treated under back-pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRecordClass {
    /// Must be delivered, in order.
    Edge,
    /// Absolute desktop position: replaces an absolute position directly
    /// ahead of it.
    AbsoluteMotion,
    /// Region position: replaces a region position directly ahead of it.
    RegionMotion,
    /// Relative delta: summed into a relative delta directly ahead of it.
    RelativeMotion,
    /// Hovering pen sample: replaces a hovering sample of the same stroke
    /// state directly ahead of it.
    PenHoverMotion,
    /// Pen sample with the tip down: kept, folded only above
    /// [`PEN_CONTACT_BACKLOG_CAP`].
    PenContactMotion,
}

/// Contact pen samples allowed to wait in the outbox before the oldest
/// adjacent pair of the same stroke is folded. About 1.3 s of a 200 Hz
/// digitizer: long enough that a short lane stall loses no ink, bounded so
/// a stalled lane cannot grow memory without limit.
pub const PEN_CONTACT_BACKLOG_CAP: usize = 256;

const PEN_EVENT: &str = "pen_event";
const REGION_PEN_EVENT: &str = "region_pen_event";

impl InputRecordClass {
    /// Class of a session input message type.
    #[must_use]
    pub fn of_message_type(msg_type: &str) -> Self {
        match msg_type {
            "mouse_move" => Self::AbsoluteMotion,
            REGION_POINTER_MOTION => Self::RegionMotion,
            MOUSE_MOVE_RELATIVE => Self::RelativeMotion,
            _ => Self::Edge,
        }
    }

    /// Class of a session input message.
    #[must_use]
    pub fn of_message(message: &Value) -> Self {
        let Some(msg_type) = message.get("type").and_then(Value::as_str) else {
            return Self::Edge;
        };
        if msg_type == PEN_EVENT || msg_type == REGION_PEN_EVENT {
            return Self::of_pen_message(message);
        }
        Self::of_message_type(msg_type)
    }

    fn of_pen_message(message: &Value) -> Self {
        let flag = |name: &str| message.get(name).and_then(Value::as_bool);
        // A sample is motion only if its sender said so; an absent flag is
        // treated as an edge so nothing ambiguous is ever folded.
        if flag("coalescable") != Some(true) || flag("in_proximity") != Some(true) {
            return Self::Edge;
        }
        if flag("touching") == Some(true) {
            Self::PenContactMotion
        } else {
            Self::PenHoverMotion
        }
    }
}

/// Input messages waiting for room on the input lane, in send order.
#[derive(Debug, Clone, Default)]
pub struct InputLaneOutbox {
    pending: VecDeque<(Value, InputRecordClass)>,
    coalesced: u64,
    pen_contact_pending: usize,
    pen_contact_decimated: u64,
}

impl InputLaneOutbox {
    /// Queues a message behind everything already waiting, coalescing it
    /// with motion of the same kind that is last in line.
    pub fn push(&mut self, message: Value) {
        let class = InputRecordClass::of_message(&message);
        if let Some((last, last_class)) = self.pending.back_mut() {
            if *last_class == class {
                match class {
                    InputRecordClass::AbsoluteMotion | InputRecordClass::RegionMotion => {
                        *last = message;
                        self.coalesced = self.coalesced.saturating_add(1);
                        return;
                    }
                    InputRecordClass::RelativeMotion => {
                        if let Some(summed) = sum_relative(last, &message) {
                            *last = summed;
                            self.coalesced = self.coalesced.saturating_add(1);
                            return;
                        }
                    }
                    InputRecordClass::PenHoverMotion => {
                        if same_pen_stroke(last, &message) {
                            *last = message;
                            self.coalesced = self.coalesced.saturating_add(1);
                            return;
                        }
                    }
                    InputRecordClass::PenContactMotion | InputRecordClass::Edge => {}
                }
            }
        }
        self.pending.push_back((message, class));
        if class == InputRecordClass::PenContactMotion {
            self.pen_contact_pending += 1;
            if self.pen_contact_pending > PEN_CONTACT_BACKLOG_CAP {
                self.fold_oldest_contact_pair();
            }
        }
    }

    /// Folds the oldest adjacent pair of contact samples from the same
    /// stroke, keeping the newer one.
    fn fold_oldest_contact_pair(&mut self) {
        let pair = (1..self.pending.len()).find(|&index| {
            let (older, older_class) = &self.pending[index - 1];
            let (newer, newer_class) = &self.pending[index];
            *older_class == InputRecordClass::PenContactMotion
                && *newer_class == InputRecordClass::PenContactMotion
                && same_pen_stroke(older, newer)
        });
        if let Some(index) = pair {
            self.pending.remove(index - 1);
            self.pen_contact_pending -= 1;
            self.pen_contact_decimated = self.pen_contact_decimated.saturating_add(1);
        }
    }

    /// The next message to send.
    #[must_use]
    pub fn front(&self) -> Option<&Value> {
        self.pending.front().map(|(message, _)| message)
    }

    /// Removes the message just sent.
    pub fn pop_front(&mut self) -> Option<Value> {
        let (message, class) = self.pending.pop_front()?;
        if class == InputRecordClass::PenContactMotion {
            self.pen_contact_pending -= 1;
        }
        Some(message)
    }

    /// Messages waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Motion messages folded into an earlier one so far.
    #[must_use]
    pub const fn coalesced(&self) -> u64 {
        self.coalesced
    }

    /// Contact pen samples folded because the backlog exceeded
    /// [`PEN_CONTACT_BACKLOG_CAP`].
    #[must_use]
    pub const fn pen_contact_decimated(&self) -> u64 {
        self.pen_contact_decimated
    }
}

/// Whether two pen samples belong to the same unchanged stroke state: same
/// message type, tool, buttons, contact, proximity and region.
fn same_pen_stroke(older: &Value, newer: &Value) -> bool {
    const KEYS: [&str; 7] = [
        "type",
        "tool",
        "buttons",
        "touching",
        "in_proximity",
        "region_id",
        "region_generation",
    ];
    KEYS.iter().all(|key| older.get(key) == newer.get(key))
}

/// `older` and `newer` as one relative delta, or `None` when either cannot
/// be merged (malformed, or marked not coalescable by the sender).
fn sum_relative(older: &Value, newer: &Value) -> Option<Value> {
    let older: MouseMoveRelativeMsg = serde_json::from_value(older.clone()).ok()?;
    let newer: MouseMoveRelativeMsg = serde_json::from_value(newer.clone()).ok()?;
    if !older.coalescable || !newer.coalescable {
        return None;
    }
    let summed = MouseMoveRelativeMsg {
        dx: older.dx.checked_add(newer.dx)?,
        dy: older.dy.checked_add(newer.dy)?,
        ..newer
    };
    serde_json::to_value(summed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drain(outbox: &mut InputLaneOutbox) -> Vec<Value> {
        let mut sent = Vec::new();
        while let Some(message) = outbox.pop_front() {
            sent.push(message);
        }
        sent
    }

    fn relative(dx: i32, dy: i32, sequence: u64) -> Value {
        json!({"type": "mouse_move_relative", "dx": dx, "dy": dy, "sequence": sequence})
    }

    #[test]
    fn edges_are_kept_in_order_and_only_trailing_positions_coalesce() {
        let mut outbox = InputLaneOutbox::default();
        outbox.push(json!({"type": "mouse_move", "x": 1}));
        outbox.push(json!({"type": "mouse_move", "x": 2}));
        outbox.push(json!({"type": "mouse_button", "down": true}));
        outbox.push(json!({"type": "mouse_move", "x": 3}));
        outbox.push(json!({"type": "mouse_move", "x": 4}));
        outbox.push(json!({"type": "mouse_button", "down": false}));
        let sent = drain(&mut outbox);
        assert_eq!(
            sent,
            [
                json!({"type": "mouse_move", "x": 2}),
                json!({"type": "mouse_button", "down": true}),
                json!({"type": "mouse_move", "x": 4}),
                json!({"type": "mouse_button", "down": false}),
            ]
        );
        assert_eq!(outbox.coalesced(), 2);
    }

    #[test]
    fn relative_deltas_are_summed_not_replaced() {
        let mut outbox = InputLaneOutbox::default();
        outbox.push(relative(3, -1, 10));
        outbox.push(relative(4, 2, 11));
        outbox.push(relative(-2, 5, 12));
        let sent = drain(&mut outbox);
        assert_eq!(sent.len(), 1);
        let summed: MouseMoveRelativeMsg = serde_json::from_value(sent[0].clone()).unwrap();
        assert_eq!((summed.dx, summed.dy), (5, 6));
        assert_eq!(summed.sequence, 12);
    }

    #[test]
    fn relative_and_absolute_motion_are_never_merged() {
        let mut outbox = InputLaneOutbox::default();
        outbox.push(json!({"type": "mouse_move", "x": 100, "y": 100}));
        outbox.push(relative(5, 5, 1));
        outbox.push(json!({"type": "mouse_move", "x": 7, "y": 7}));
        outbox.push(json!({"type": "region_pointer_motion", "logical_x": 1}));
        outbox.push(relative(1, 1, 2));
        assert_eq!(outbox.len(), 5);
        assert_eq!(outbox.coalesced(), 0);
    }

    #[test]
    fn a_delta_marked_not_coalescable_is_kept_whole() {
        let mut outbox = InputLaneOutbox::default();
        outbox.push(relative(1, 1, 1));
        let mut exact = relative(2, 2, 2);
        exact["coalescable"] = json!(false);
        outbox.push(exact);
        assert_eq!(outbox.len(), 2);
    }

    #[test]
    fn classes_of_message_types() {
        assert_eq!(
            InputRecordClass::of_message_type("mouse_move"),
            InputRecordClass::AbsoluteMotion
        );
        assert_eq!(
            InputRecordClass::of_message_type("region_pointer_motion"),
            InputRecordClass::RegionMotion
        );
        assert_eq!(
            InputRecordClass::of_message_type("mouse_move_relative"),
            InputRecordClass::RelativeMotion
        );
        for edge in [
            "key_event",
            "mouse_button",
            "mouse_scroll",
            "region_pointer_enter",
            "gesture_swipe",
            "key_reset_modifiers",
            "text_commit",
        ] {
            assert_eq!(
                InputRecordClass::of_message_type(edge),
                InputRecordClass::Edge
            );
        }
    }

    const CAP: i64 = 256;

    #[test]
    fn test_cap_matches_the_policy_cap() {
        assert_eq!(usize::try_from(CAP).unwrap(), PEN_CONTACT_BACKLOG_CAP);
    }

    fn pen(x: i64, touching: bool, coalescable: bool) -> Value {
        json!({
            "type": "region_pen_event",
            "region_generation": 3,
            "region_id": 1,
            "logical_x": x,
            "logical_y": 0,
            "pressure": 0.5,
            "tool": "tip",
            "in_proximity": true,
            "touching": touching,
            "buttons": 0,
            "sequence": x,
            "coalescable": coalescable,
        })
    }

    #[test]
    fn pen_messages_are_classed_by_contact_and_coalescable() {
        assert_eq!(
            InputRecordClass::of_message(&pen(1, false, true)),
            InputRecordClass::PenHoverMotion
        );
        assert_eq!(
            InputRecordClass::of_message(&pen(1, true, true)),
            InputRecordClass::PenContactMotion
        );
        assert_eq!(
            InputRecordClass::of_message(&pen(1, true, false)),
            InputRecordClass::Edge
        );
        let mut out_of_proximity = pen(1, false, true);
        out_of_proximity["in_proximity"] = json!(false);
        assert_eq!(
            InputRecordClass::of_message(&out_of_proximity),
            InputRecordClass::Edge
        );
        assert_eq!(
            InputRecordClass::of_message(&json!({"type": "pen_event", "touching": true})),
            InputRecordClass::Edge
        );
        assert_eq!(
            InputRecordClass::of_message(&json!({
                "type": "pen_event", "in_proximity": true, "touching": false, "coalescable": true
            })),
            InputRecordClass::PenHoverMotion
        );
    }

    #[test]
    fn hovering_pen_samples_coalesce_only_behind_the_same_stroke_state() {
        let mut outbox = InputLaneOutbox::default();
        outbox.push(pen(1, false, false));
        outbox.push(pen(2, false, true));
        outbox.push(pen(3, false, true));
        let mut pressed_button = pen(4, false, true);
        pressed_button["buttons"] = json!(1);
        outbox.push(pressed_button);
        let mut other_region = pen(5, false, true);
        other_region["region_id"] = json!(2);
        outbox.push(other_region);
        let sent = drain(&mut outbox);
        let xs: Vec<i64> = sent
            .iter()
            .map(|m| m["logical_x"].as_i64().unwrap())
            .collect();
        assert_eq!(xs, vec![1, 3, 4, 5]);
        assert_eq!(outbox.coalesced(), 1);
    }

    #[test]
    fn contact_pen_samples_are_never_coalesced_below_the_cap() {
        let mut outbox = InputLaneOutbox::default();
        outbox.push(pen(0, true, false));
        for x in 1..=CAP {
            outbox.push(pen(x, true, true));
        }
        assert_eq!(outbox.len(), PEN_CONTACT_BACKLOG_CAP + 1);
        assert_eq!(outbox.coalesced(), 0);
        assert_eq!(outbox.pen_contact_decimated(), 0);
        let sent = drain(&mut outbox);
        let xs: Vec<i64> = sent
            .iter()
            .map(|m| m["logical_x"].as_i64().unwrap())
            .collect();
        assert_eq!(xs, (0..=CAP).collect::<Vec<_>>());
    }

    #[test]
    fn a_contact_backlog_above_the_cap_folds_the_oldest_pair_and_counts_it() {
        let mut outbox = InputLaneOutbox::default();
        outbox.push(pen(0, true, false));
        let extra = 10i64;
        for x in 1..=CAP + extra {
            outbox.push(pen(x, true, true));
        }
        outbox.push(json!({"type": "key_event", "pressed": true}));
        assert_eq!(outbox.pen_contact_decimated(), 10);
        let sent = drain(&mut outbox);
        // The touch-down edge and the key edge are intact; the newest ink
        // survives; only the oldest contact samples were folded.
        assert_eq!(sent.first().unwrap()["logical_x"], json!(0));
        assert_eq!(sent.last().unwrap()["type"], json!("key_event"));
        let xs: Vec<i64> = sent[1..sent.len() - 1]
            .iter()
            .map(|m| m["logical_x"].as_i64().unwrap())
            .collect();
        assert_eq!(xs.len(), PEN_CONTACT_BACKLOG_CAP);
        assert_eq!(*xs.first().unwrap(), extra + 1);
        assert_eq!(*xs.last().unwrap(), CAP + extra);
    }

    #[test]
    fn draining_releases_contact_backlog_room() {
        let mut outbox = InputLaneOutbox::default();
        for round in 0..4i64 {
            for x in 0..CAP {
                outbox.push(pen(round * 1000 + x, true, true));
            }
            drain(&mut outbox);
        }
        assert_eq!(outbox.pen_contact_decimated(), 0);
    }

    #[test]
    fn pen_and_pointer_records_keep_their_relative_order() {
        let mut outbox = InputLaneOutbox::default();
        outbox.push(json!({"type": "mouse_move", "x": 1}));
        outbox.push(pen(1, false, true));
        outbox.push(json!({"type": "mouse_move", "x": 2}));
        outbox.push(pen(2, false, true));
        assert_eq!(outbox.len(), 4);
        assert_eq!(outbox.coalesced(), 0);
    }
}
