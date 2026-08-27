//! Edge-preserving pen event dispatch.
//!
//! The AppKit monitor's [`super::sample::BoundedSampleQueue`] is a
//! drop-oldest mixed queue: it is the right shape for the *producer* side
//! (an AppKit callback that must never block), but production event
//! *delivery* to the host must not reuse that same drop-oldest behavior
//! unchanged, because a dropped sample there could silently discard a
//! proximity, contact, tool, or button transition. This module is the
//! consumer-side stage that sits between the drained, already-mapped
//! [`arcen_input::PenEvent`] stream and the transport send call:
//!
//! - **Motion is coalescible.** A run of samples that only differ in
//!   position/pressure/tilt/rotation (the digitizer's continuous axes) may
//!   be superseded by a newer one in the same run — only the latest queued
//!   motion sample is kept, exactly like `BoundedSampleQueue`'s producer-side
//!   coalescing, because an intermediate hover/move position is fully
//!   superseded by a later one before the host ever needs to see it.
//! - **Edges are never dropped.** A transition in `in_proximity`, `tool`,
//!   `touching`, or `buttons` is always preserved as its own queued sample,
//!   never merged into — or replaced by — a later one.
//! - **Bounded, fail-closed edge capacity.** The number of edges buffered in
//!   one dispatch batch is bounded. Reaching that bound is an explicit
//!   overflow ([`TabletDispatchOverflow`]), not a silent drop: the caller
//!   must react by resetting tablet authority (falling back to mouse
//!   emulation and reporting an error) rather than lose an edge.
#![forbid(unsafe_code)]

use arcen_input::{LowLatencyMetadata, PenEvent, PenTool};

/// Default bounded edge capacity per dispatch batch. Proximity/tool/contact/
/// button transitions arrive at a tiny fraction of the digitizer's point
/// rate (a person cannot press/release/hover-transition hundreds of times
/// per drained batch), so this comfortably covers real usage while still
/// bounding worst-case memory for a producer that must never block.
pub const DEFAULT_EDGE_CAPACITY: usize = 64;

/// The subset of [`PenEvent`] fields that define an "edge": a transition
/// that must never be dropped, as opposed to a continuous axis sample that
/// may be coalesced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EdgeState {
    in_proximity: bool,
    tool: PenTool,
    touching: bool,
    buttons: u16,
}

impl EdgeState {
    const fn from_event(event: &PenEvent) -> Self {
        Self {
            in_proximity: event.in_proximity,
            tool: event.tool,
            touching: event.touching,
            buttons: event.buttons,
        }
    }
}

/// The bounded edge queue's capacity was exhausted within one dispatch
/// batch. The caller must fail closed: reset tablet authority (drop back to
/// mouse-emulation fallback) and surface an error, rather than silently
/// drop the edge that could not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabletDispatchOverflow;

impl std::fmt::Display for TabletDispatchOverflow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tablet edge dispatch queue overflowed; tablet authority must reset"
        )
    }
}

impl std::error::Error for TabletDispatchOverflow {}

/// Stateful, edge-preserving, motion-coalescing pen event dispatcher.
///
/// Persists the last dispatched edge-state *across* `dispatch` calls (not
/// just within one batch) so a motion-only sample in a later batch is still
/// correctly recognized as a continuation rather than treated as an edge.
#[derive(Debug)]
pub struct TabletEventDispatcher {
    capacity: usize,
    last_state: Option<EdgeState>,
    last_position: (f64, f64),
    edge_count: u64,
    motion_coalesced_count: u64,
    overflow_count: u64,
}

impl Default for TabletEventDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl TabletEventDispatcher {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_EDGE_CAPACITY)
    }

    /// # Panics
    /// Panics if `capacity` is zero; a zero-capacity dispatcher could never
    /// deliver even a single edge.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "tablet dispatch edge capacity must be > 0");
        Self {
            capacity,
            last_state: None,
            last_position: (0.0, 0.0),
            edge_count: 0,
            motion_coalesced_count: 0,
            overflow_count: 0,
        }
    }

    /// Whether the last dispatched sample left the tool in proximity. Used
    /// to decide whether a teardown/suppression-restore path needs to
    /// synthesize a final out-of-proximity release (see
    /// [`Self::release_event`]).
    #[must_use]
    pub fn in_proximity(&self) -> bool {
        self.last_state.is_some_and(|state| state.in_proximity)
    }

    #[must_use]
    pub const fn edge_count(&self) -> u64 {
        self.edge_count
    }

    #[must_use]
    pub const fn motion_coalesced_count(&self) -> u64 {
        self.motion_coalesced_count
    }

    #[must_use]
    pub const fn overflow_count(&self) -> u64 {
        self.overflow_count
    }

    /// Fold one drained batch of already-mapped, already-validated
    /// [`PenEvent`]s into an ordered, edge-preserving, motion-coalesced
    /// output batch ready to send to the host in order. Also stamps each
    /// output event's `metadata.coalescable`: `false` on every edge
    /// (proximity/tool/contact/button transition — never supersedable),
    /// `true` on every motion continuation (freely supersedable), matching
    /// the same distinction this dispatcher itself enforces.
    ///
    /// # Errors
    /// Returns [`TabletDispatchOverflow`] if more distinct edges arrive in
    /// this batch than the bounded capacity allows. The dispatcher's
    /// internal state is left exactly as it was before the call that
    /// overflowed (via [`Self::reset`], which the caller should invoke as
    /// part of resetting tablet authority) so a retry after recovery starts
    /// from a clean slate rather than partial batch state.
    pub fn dispatch(
        &mut self,
        events: impl IntoIterator<Item = PenEvent>,
    ) -> Result<Vec<PenEvent>, TabletDispatchOverflow> {
        let mut out: Vec<PenEvent> = Vec::new();
        let mut working_state = self.last_state;
        let mut working_position = self.last_position;
        for mut event in events {
            let state = EdgeState::from_event(&event);
            let is_edge = working_state != Some(state);
            if is_edge {
                if out.len() >= self.capacity {
                    self.overflow_count = self.overflow_count.saturating_add(1);
                    return Err(TabletDispatchOverflow);
                }
                // An edge (proximity/tool/contact/button transition) must
                // never be treated as supersedable: mark it non-coalescable
                // so nothing downstream skips it in favor of a later sample.
                event.metadata.coalescable = false;
                working_position = (event.x, event.y);
                out.push(event);
                self.edge_count = self.edge_count.saturating_add(1);
            } else if let Some(last) = out.last_mut() {
                // A pure motion continuation within the same edge-state is
                // exactly the "supersedable motion" case: only the newest
                // position/pressure/tilt/rotation matters.
                event.metadata.coalescable = true;
                working_position = (event.x, event.y);
                *last = event;
                self.motion_coalesced_count = self.motion_coalesced_count.saturating_add(1);
            } else {
                // First sample of this batch continues a state dispatched
                // in a previous batch; it still must occupy one slot so the
                // host sees at least this batch's most current position,
                // and it is a motion continuation, not a new edge.
                event.metadata.coalescable = true;
                working_position = (event.x, event.y);
                out.push(event);
                self.motion_coalesced_count = self.motion_coalesced_count.saturating_add(1);
            }
            working_state = Some(state);
        }
        self.last_state = working_state;
        self.last_position = working_position;
        Ok(out)
    }

    /// Synthesize a final out-of-proximity release event from the last
    /// known state/position, for abnormal terminations that don't arrive
    /// via a natural proximity-leave sample: focus loss, the setting being
    /// disabled, disconnect/reconnect, or terminal teardown. Returns `None`
    /// when the dispatcher was not in proximity (nothing to release) so the
    /// caller never sends a spurious release with no prior authority.
    ///
    /// Leaves the dispatcher's edge-state as "out of proximity" so a
    /// subsequent real sample is correctly treated as a fresh proximity
    /// edge rather than a stale continuation.
    pub fn release_event(&mut self, mut metadata: LowLatencyMetadata) -> Option<PenEvent> {
        let state = self.last_state?;
        if !state.in_proximity {
            return None;
        }
        let (x, y) = self.last_position;
        metadata.coalescable = false;
        let event = PenEvent {
            x,
            y,
            pressure: 0.0,
            tilt_x_degrees: 0.0,
            tilt_y_degrees: 0.0,
            rotation_degrees: 0.0,
            tool: state.tool,
            in_proximity: false,
            touching: false,
            buttons: 0,
            metadata,
        };
        let released = EdgeState {
            in_proximity: false,
            tool: state.tool,
            touching: false,
            buttons: 0,
        };
        event.validate().ok()?;
        self.last_state = Some(released);
        self.edge_count = self.edge_count.saturating_add(1);
        Some(event)
    }

    /// Reset dispatcher state (used when tablet authority resets: overflow,
    /// disconnect, reconnect detach, focus loss, or the setting being
    /// disabled). Clears the remembered edge-state so the next sample is
    /// always treated as a fresh edge rather than a stale continuation.
    pub fn reset(&mut self) {
        self.last_state = None;
        self.last_position = (0.0, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_input::LowLatencyMetadata;

    fn metadata(sequence: u64) -> LowLatencyMetadata {
        LowLatencyMetadata {
            sequence,
            timestamp_ns: sequence,
            coalescable: sequence == 0,
        }
    }

    fn pen(x: f64, sequence: u64, in_proximity: bool, touching: bool, buttons: u16) -> PenEvent {
        PenEvent {
            x,
            y: 0.5,
            pressure: 0.3,
            tilt_x_degrees: 0.0,
            tilt_y_degrees: 0.0,
            rotation_degrees: 0.0,
            tool: PenTool::Tip,
            in_proximity,
            touching,
            buttons,
            metadata: metadata(sequence),
        }
    }

    #[test]
    fn pure_motion_run_coalesces_to_one_sample() {
        let mut dispatcher = TabletEventDispatcher::new();
        let batch = vec![
            pen(0.1, 1, true, true, 0),
            pen(0.2, 2, true, true, 0),
            pen(0.3, 3, true, true, 0),
        ];
        let dispatched = dispatcher.dispatch(batch).expect("no overflow");
        // The first sample establishes the (in_proximity, tool, touching,
        // buttons) edge and is queued; the next two are pure motion
        // continuations and supersede it in place.
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].x, 0.3);
        assert_eq!(dispatcher.motion_coalesced_count(), 2);
        assert_eq!(dispatcher.edge_count(), 1);
    }

    #[test]
    fn proximity_and_touching_edges_are_never_dropped() {
        let mut dispatcher = TabletEventDispatcher::new();
        let batch = vec![
            pen(0.1, 1, true, false, 0),  // enter proximity, hover
            pen(0.2, 2, true, false, 0),  // motion, still hover (coalesces)
            pen(0.3, 3, true, true, 0),   // touch-down edge
            pen(0.4, 4, true, true, 0),   // motion while touching (coalesces)
            pen(0.5, 5, true, false, 0),  // touch-up edge
            pen(0.0, 6, false, false, 0), // proximity-out edge
        ];
        let dispatched = dispatcher.dispatch(batch).expect("no overflow");
        // Four distinct edge-states occurred: hover, touching, hover again,
        // out-of-proximity. Every edge boundary must survive even though
        // some individual samples coalesced within a run.
        assert_eq!(dispatched.len(), 4);
        assert_eq!(dispatched[0].x, 0.2); // hover run coalesced to its last sample
        assert!(!dispatched[0].touching);
        assert_eq!(dispatched[1].x, 0.4); // touching run coalesced to its last sample
        assert!(dispatched[1].touching);
        assert_eq!(dispatched[2].x, 0.5); // touch-up edge preserved on its own
        assert!(!dispatched[2].touching);
        assert!(dispatched[2].in_proximity);
        assert_eq!(dispatched[3].x, 0.0); // proximity-out edge preserved on its own
        assert!(!dispatched[3].in_proximity);
        assert_eq!(dispatcher.edge_count(), 4);
        assert!(!dispatcher.in_proximity());
    }

    #[test]
    fn button_transition_is_an_edge_even_without_a_touching_change() {
        let mut dispatcher = TabletEventDispatcher::new();
        let batch = vec![pen(0.1, 1, true, true, 0), pen(0.1, 2, true, true, 1)];
        let dispatched = dispatcher.dispatch(batch).expect("no overflow");
        assert_eq!(dispatched.len(), 2);
        assert_eq!(dispatched[1].buttons, 1);
    }

    #[test]
    fn continuation_across_batches_still_coalesces() {
        let mut dispatcher = TabletEventDispatcher::new();
        dispatcher
            .dispatch(vec![pen(0.1, 1, true, true, 0)])
            .expect("no overflow");
        // A later batch whose first sample continues the same edge-state
        // must still occupy exactly one slot (never zero — the host must
        // still receive the freshest position) and count as coalesced, not
        // as a second edge.
        let dispatched = dispatcher
            .dispatch(vec![pen(0.2, 2, true, true, 0)])
            .expect("no overflow");
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].x, 0.2);
        assert_eq!(dispatcher.edge_count(), 1);
        assert_eq!(dispatcher.motion_coalesced_count(), 1);
    }

    #[test]
    fn overflow_is_reported_rather_than_dropping_an_edge() {
        let mut dispatcher = TabletEventDispatcher::with_capacity(2);
        // Three distinct edges in one batch: proximity-in, touch-down,
        // touch-up — one more edge than the bounded capacity allows.
        let batch = vec![
            pen(0.1, 1, true, false, 0),
            pen(0.2, 2, true, true, 0),
            pen(0.3, 3, true, false, 0),
        ];
        let result = dispatcher.dispatch(batch);
        assert_eq!(result, Err(TabletDispatchOverflow));
        assert_eq!(dispatcher.overflow_count(), 1);
    }

    #[test]
    fn reset_clears_remembered_edge_state() {
        let mut dispatcher = TabletEventDispatcher::new();
        dispatcher
            .dispatch(vec![pen(0.1, 1, true, true, 0)])
            .expect("no overflow");
        assert!(dispatcher.in_proximity());
        dispatcher.reset();
        assert!(!dispatcher.in_proximity());
        // After reset, the very next sample is treated as a fresh edge, not
        // a continuation, even though it has the same fields as before.
        let dispatched = dispatcher
            .dispatch(vec![pen(0.1, 2, true, true, 0)])
            .expect("no overflow");
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatcher.edge_count(), 2);
    }

    #[test]
    fn with_capacity_zero_panics() {
        let result = std::panic::catch_unwind(|| TabletEventDispatcher::with_capacity(0));
        assert!(result.is_err());
    }

    #[test]
    fn edges_are_stamped_non_coalescable_and_motion_is_stamped_coalescable() {
        let mut dispatcher = TabletEventDispatcher::new();
        let batch = vec![
            pen(0.1, 1, true, false, 0), // edge: proximity-in
            pen(0.2, 2, true, false, 0), // motion continuation supersedes slot 0
            pen(0.3, 3, true, true, 0),  // edge: touch-down, its own slot
        ];
        let dispatched = dispatcher.dispatch(batch).expect("no overflow");
        assert_eq!(dispatched.len(), 2);
        // Slot 0 ends up holding the *last* sample of the hover run — a
        // motion continuation, still supersedable by a later same-state
        // sample, so it is coalescable even though the run started on an
        // edge.
        assert!(dispatched[0].metadata.coalescable);
        // The touch-down edge itself was never overwritten within this
        // batch; it must remain non-coalescable.
        assert!(!dispatched[1].metadata.coalescable);
    }

    #[test]
    fn release_event_synthesizes_out_of_proximity_at_last_known_position() {
        let mut dispatcher = TabletEventDispatcher::new();
        dispatcher
            .dispatch(vec![pen(0.4, 1, true, true, 0)])
            .expect("no overflow");
        let release = dispatcher
            .release_event(metadata(2))
            .expect("dispatcher was in proximity");
        assert_eq!(release.x, 0.4);
        assert!(!release.in_proximity);
        assert!(!release.touching);
        assert_eq!(release.buttons, 0);
        assert!(!release.metadata.coalescable);
        // The dispatcher now believes it is out of proximity, so a further
        // release attempt has nothing left to release.
        assert!(dispatcher.release_event(metadata(3)).is_none());
    }

    #[test]
    fn release_event_is_none_when_never_in_proximity() {
        let mut dispatcher = TabletEventDispatcher::new();
        assert!(dispatcher.release_event(metadata(1)).is_none());
    }

    #[test]
    fn release_event_after_natural_proximity_leave_is_none() {
        let mut dispatcher = TabletEventDispatcher::new();
        dispatcher
            .dispatch(vec![pen(0.4, 1, true, true, 0)])
            .expect("no overflow");
        dispatcher
            .dispatch(vec![pen(0.4, 2, false, false, 0)])
            .expect("no overflow");
        // Already left proximity naturally; nothing left to synthesize.
        assert!(dispatcher.release_event(metadata(3)).is_none());
    }

    #[test]
    fn reset_clears_last_position_too() {
        let mut dispatcher = TabletEventDispatcher::new();
        dispatcher
            .dispatch(vec![pen(0.7, 1, true, true, 0)])
            .expect("no overflow");
        dispatcher.reset();
        assert!(dispatcher.release_event(metadata(2)).is_none());
    }
}
