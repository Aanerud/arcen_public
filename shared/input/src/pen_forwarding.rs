//! Edge-preserving pen sample forwarding.
//!
//! A client turns native digitizer reports into [`PenEvent`]s and hands them
//! to [`PenForwardingDispatcher`] on their way to the wire. The dispatcher
//! decides what is an *edge* and what is *motion*, and stamps
//! `metadata.coalescable` accordingly:
//!
//! - **Edges are never dropped.** A change of `in_proximity`, `tool`,
//!   `touching`, or `buttons` is always forwarded as its own sample and
//!   marked not coalescable.
//! - **Motion is coalescable, but not coalesced here by default.** A sample
//!   that only moves the continuous axes (position, pressure, tilt,
//!   rotation) is marked coalescable. In [`PenForwardMode::Immediate`] (the
//!   production mode) every sample is still forwarded, because pressure
//!   samples taken while the tip is down are the ink itself. Only real
//!   back-pressure downstream (the input lane outbox) may fold motion, and
//!   only by its own rules.
//! - [`PenForwardMode::BatchedLatestMotion`] keeps the older behaviour of
//!   folding a motion run inside one batch down to its newest sample. It
//!   exists for diagnostics and for clients that genuinely deliver input in
//!   batches; it must never be used to hide a slow UI loop.
//! - **Bounded, fail-closed edge capacity.** The number of edges accepted in
//!   one batch is bounded. Reaching that bound is an explicit
//!   [`PenDispatchOverflow`], not a silent drop: the caller must reset pen
//!   authority rather than lose an edge.

use crate::{LowLatencyMetadata, PenEvent, PenTool};

/// Default bounded edge capacity per batch. Proximity/tool/contact/button
/// transitions arrive at a tiny fraction of the digitizer's point rate.
pub const DEFAULT_PEN_EDGE_CAPACITY: usize = 64;

/// How motion samples within one batch are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PenForwardMode {
    /// Forward every sample. Production default.
    #[default]
    Immediate,
    /// Fold a motion run inside one batch to its newest sample.
    BatchedLatestMotion,
}

/// The subset of [`PenEvent`] fields whose change is an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PenEdgeState {
    /// Whether the tool is within digitizer proximity.
    pub in_proximity: bool,
    /// Which end of the tool is in use.
    pub tool: PenTool,
    /// Whether the tip touches the surface.
    pub touching: bool,
    /// Barrel buttons held.
    pub buttons: u16,
}

impl PenEdgeState {
    /// Edge state carried by `event`.
    #[must_use]
    pub const fn of(event: &PenEvent) -> Self {
        Self {
            in_proximity: event.in_proximity,
            tool: event.tool,
            touching: event.touching,
            buttons: event.buttons,
        }
    }
}

/// More edges arrived in one batch than the bounded capacity allows. The
/// caller must fail closed (reset pen authority, report it) rather than drop
/// the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PenDispatchOverflow;

impl std::fmt::Display for PenDispatchOverflow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "pen edge dispatch queue overflowed; pen authority must reset"
        )
    }
}

impl std::error::Error for PenDispatchOverflow {}

/// Stateful, edge-preserving pen dispatcher.
///
/// Remembers the last forwarded edge state *across* calls, so a motion
/// sample in a later call is recognised as a continuation rather than an
/// edge.
#[derive(Debug, Clone)]
pub struct PenForwardingDispatcher {
    capacity: usize,
    mode: PenForwardMode,
    last_state: Option<PenEdgeState>,
    last_position: (f64, f64),
    edge_count: u64,
    motion_forwarded_count: u64,
    motion_coalesced_count: u64,
    overflow_count: u64,
}

impl Default for PenForwardingDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl PenForwardingDispatcher {
    /// Immediate-mode dispatcher with the default edge capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_PEN_EDGE_CAPACITY)
    }

    /// Immediate-mode dispatcher with `capacity` edges per batch.
    ///
    /// # Panics
    /// Panics if `capacity` is zero; such a dispatcher could never deliver
    /// even a single edge.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_mode(capacity, PenForwardMode::Immediate)
    }

    /// Dispatcher with an explicit forwarding mode.
    ///
    /// # Panics
    /// Panics if `capacity` is zero.
    #[must_use]
    pub fn with_mode(capacity: usize, mode: PenForwardMode) -> Self {
        assert!(capacity > 0, "pen dispatch edge capacity must be > 0");
        Self {
            capacity,
            mode,
            last_state: None,
            last_position: (0.0, 0.0),
            edge_count: 0,
            motion_forwarded_count: 0,
            motion_coalesced_count: 0,
            overflow_count: 0,
        }
    }

    /// Forwarding mode.
    #[must_use]
    pub const fn mode(&self) -> PenForwardMode {
        self.mode
    }

    /// Whether the last forwarded sample left the tool in proximity.
    #[must_use]
    pub fn in_proximity(&self) -> bool {
        self.last_state.is_some_and(|state| state.in_proximity)
    }

    /// Last forwarded edge state, if any.
    #[must_use]
    pub const fn last_state(&self) -> Option<PenEdgeState> {
        self.last_state
    }

    /// Edges forwarded so far.
    #[must_use]
    pub const fn edge_count(&self) -> u64 {
        self.edge_count
    }

    /// Motion samples forwarded so far.
    #[must_use]
    pub const fn motion_forwarded_count(&self) -> u64 {
        self.motion_forwarded_count
    }

    /// Motion samples folded into a newer one so far (batched mode only).
    #[must_use]
    pub const fn motion_coalesced_count(&self) -> u64 {
        self.motion_coalesced_count
    }

    /// Batches rejected for exceeding the edge capacity.
    #[must_use]
    pub const fn overflow_count(&self) -> u64 {
        self.overflow_count
    }

    /// Classifies and stamps one sample and forwards it. A single sample can
    /// never overflow the edge capacity.
    pub fn forward(&mut self, mut event: PenEvent) -> PenEvent {
        let state = PenEdgeState::of(&event);
        if self.last_state == Some(state) {
            event.metadata.coalescable = true;
            self.motion_forwarded_count = self.motion_forwarded_count.saturating_add(1);
        } else {
            event.metadata.coalescable = false;
            self.edge_count = self.edge_count.saturating_add(1);
        }
        self.last_state = Some(state);
        self.last_position = (event.x, event.y);
        event
    }

    /// Folds one batch into an ordered, edge-preserving output batch.
    ///
    /// # Errors
    /// Returns [`PenDispatchOverflow`] if more distinct edges arrive in this
    /// batch than the capacity allows. Dispatcher state is left as before
    /// the call.
    pub fn dispatch(
        &mut self,
        events: impl IntoIterator<Item = PenEvent>,
    ) -> Result<Vec<PenEvent>, PenDispatchOverflow> {
        let mut out: Vec<PenEvent> = Vec::new();
        let mut working_state = self.last_state;
        let mut working_position = self.last_position;
        let mut edges = 0u64;
        let mut forwarded = 0u64;
        let mut coalesced = 0u64;
        let mut batch_edges = 0usize;
        for mut event in events {
            let state = PenEdgeState::of(&event);
            let is_edge = working_state != Some(state);
            working_position = (event.x, event.y);
            if is_edge {
                if batch_edges >= self.capacity {
                    self.overflow_count = self.overflow_count.saturating_add(1);
                    return Err(PenDispatchOverflow);
                }
                batch_edges += 1;
                event.metadata.coalescable = false;
                out.push(event);
                edges += 1;
            } else {
                event.metadata.coalescable = true;
                let fold = self.mode == PenForwardMode::BatchedLatestMotion
                    && out
                        .last()
                        .is_some_and(|last| PenEdgeState::of(last) == state);
                match out.last_mut() {
                    Some(last) if fold => {
                        *last = event;
                        coalesced += 1;
                    }
                    _ => {
                        out.push(event);
                        forwarded += 1;
                    }
                }
            }
            working_state = Some(state);
        }
        self.last_state = working_state;
        self.last_position = working_position;
        self.edge_count = self.edge_count.saturating_add(edges);
        self.motion_forwarded_count = self.motion_forwarded_count.saturating_add(forwarded);
        self.motion_coalesced_count = self.motion_coalesced_count.saturating_add(coalesced);
        Ok(out)
    }

    /// Synthesizes a final out-of-proximity release at the last known
    /// position, for terminations that do not arrive as a real
    /// proximity-leave sample (focus loss, disable, disconnect, overflow).
    /// Returns `None` when the tool was not in proximity.
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
        event.validate().ok()?;
        self.last_state = Some(PenEdgeState {
            in_proximity: false,
            tool: state.tool,
            touching: false,
            buttons: 0,
        });
        self.edge_count = self.edge_count.saturating_add(1);
        Some(event)
    }

    /// Forgets the remembered edge state so the next sample is a fresh edge.
    pub fn reset(&mut self) {
        self.last_state = None;
        self.last_position = (0.0, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}, got {actual}"
        );
    }

    fn batched() -> PenForwardingDispatcher {
        PenForwardingDispatcher::with_mode(
            DEFAULT_PEN_EDGE_CAPACITY,
            PenForwardMode::BatchedLatestMotion,
        )
    }

    #[test]
    fn immediate_mode_forwards_every_contact_sample_with_its_pressure() {
        let mut dispatcher = PenForwardingDispatcher::new();
        let mut batch = vec![pen(0.1, 1, true, true, 0)];
        for index in 2..=8u16 {
            let mut sample = pen(f64::from(index) / 10.0, u64::from(index), true, true, 0);
            sample.pressure = f32::from(index) / 10.0;
            batch.push(sample);
        }
        let out = dispatcher.dispatch(batch).expect("no overflow");
        assert_eq!(out.len(), 8);
        assert!(!out[0].metadata.coalescable);
        assert!(out[1..].iter().all(|event| event.metadata.coalescable));
        for (event, expected) in out[1..].iter().zip([0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]) {
            assert_close(f64::from(event.pressure), expected);
        }
        assert_eq!(dispatcher.edge_count(), 1);
        assert_eq!(dispatcher.motion_forwarded_count(), 7);
        assert_eq!(dispatcher.motion_coalesced_count(), 0);
    }

    #[test]
    fn forward_classifies_one_sample_at_a_time() {
        let mut dispatcher = PenForwardingDispatcher::new();
        assert!(
            !dispatcher
                .forward(pen(0.1, 1, true, false, 0))
                .metadata
                .coalescable
        );
        assert!(
            dispatcher
                .forward(pen(0.2, 2, true, false, 0))
                .metadata
                .coalescable
        );
        assert!(
            !dispatcher
                .forward(pen(0.2, 3, true, true, 0))
                .metadata
                .coalescable
        );
        assert!(
            dispatcher
                .forward(pen(0.3, 4, true, true, 0))
                .metadata
                .coalescable
        );
        assert!(
            !dispatcher
                .forward(pen(0.3, 5, true, true, 2))
                .metadata
                .coalescable
        );
        assert_eq!(dispatcher.edge_count(), 3);
        assert_eq!(dispatcher.motion_forwarded_count(), 2);
        assert!(dispatcher.in_proximity());
    }

    #[test]
    fn batched_motion_run_coalesces_to_one_sample() {
        let mut dispatcher = batched();
        let batch = vec![
            pen(0.1, 1, true, true, 0),
            pen(0.2, 2, true, true, 0),
            pen(0.3, 3, true, true, 0),
        ];
        let out = dispatcher.dispatch(batch).expect("no overflow");
        assert_eq!(out.len(), 1);
        assert_close(out[0].x, 0.3);
        assert_eq!(dispatcher.motion_coalesced_count(), 2);
        assert_eq!(dispatcher.edge_count(), 1);
    }

    #[test]
    fn batched_mode_never_drops_proximity_or_touching_edges() {
        let mut dispatcher = batched();
        let batch = vec![
            pen(0.1, 1, true, false, 0),
            pen(0.2, 2, true, false, 0),
            pen(0.3, 3, true, true, 0),
            pen(0.4, 4, true, true, 0),
            pen(0.5, 5, true, false, 0),
            pen(0.0, 6, false, false, 0),
        ];
        let out = dispatcher.dispatch(batch).expect("no overflow");
        assert_eq!(out.len(), 4);
        assert_close(out[0].x, 0.2);
        assert_close(out[1].x, 0.4);
        assert!(out[1].touching);
        assert_close(out[2].x, 0.5);
        assert!(!out[2].touching);
        assert_close(out[3].x, 0.0);
        assert!(!out[3].in_proximity);
        assert_eq!(dispatcher.edge_count(), 4);
        assert!(!dispatcher.in_proximity());
    }

    #[test]
    fn immediate_mode_keeps_every_sample_and_every_edge_in_order() {
        let mut dispatcher = PenForwardingDispatcher::new();
        let batch = vec![
            pen(0.1, 1, true, false, 0),
            pen(0.2, 2, true, false, 0),
            pen(0.3, 3, true, true, 0),
            pen(0.4, 4, true, true, 0),
            pen(0.5, 5, true, false, 0),
            pen(0.0, 6, false, false, 0),
        ];
        let out = dispatcher.dispatch(batch).expect("no overflow");
        let sequences: Vec<u64> = out.iter().map(|event| event.metadata.sequence).collect();
        assert_eq!(sequences, vec![1, 2, 3, 4, 5, 6]);
        let coalescable: Vec<bool> = out.iter().map(|event| event.metadata.coalescable).collect();
        assert_eq!(coalescable, vec![false, true, false, true, false, false]);
    }

    #[test]
    fn button_transition_is_an_edge_even_without_a_touching_change() {
        let mut dispatcher = batched();
        let batch = vec![pen(0.1, 1, true, true, 0), pen(0.1, 2, true, true, 1)];
        let out = dispatcher.dispatch(batch).expect("no overflow");
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].buttons, 1);
    }

    #[test]
    fn batched_continuation_across_batches_still_occupies_one_slot() {
        let mut dispatcher = batched();
        dispatcher
            .dispatch(vec![pen(0.1, 1, true, true, 0)])
            .expect("no overflow");
        let out = dispatcher
            .dispatch(vec![pen(0.2, 2, true, true, 0), pen(0.3, 3, true, true, 0)])
            .expect("no overflow");
        assert_eq!(out.len(), 1);
        assert_close(out[0].x, 0.3);
        assert_eq!(dispatcher.edge_count(), 1);
        assert_eq!(dispatcher.motion_forwarded_count(), 1);
        assert_eq!(dispatcher.motion_coalesced_count(), 1);
    }

    #[test]
    fn overflow_is_reported_rather_than_dropping_an_edge() {
        let mut dispatcher = PenForwardingDispatcher::with_capacity(2);
        let batch = vec![
            pen(0.1, 1, true, false, 0),
            pen(0.2, 2, true, true, 0),
            pen(0.3, 3, true, false, 0),
        ];
        assert_eq!(dispatcher.dispatch(batch), Err(PenDispatchOverflow));
        assert_eq!(dispatcher.overflow_count(), 1);
        assert!(dispatcher.last_state().is_none());
        assert_eq!(dispatcher.edge_count(), 0);
    }

    #[test]
    fn reset_clears_remembered_edge_state() {
        let mut dispatcher = PenForwardingDispatcher::new();
        dispatcher.forward(pen(0.1, 1, true, true, 0));
        assert!(dispatcher.in_proximity());
        dispatcher.reset();
        assert!(!dispatcher.in_proximity());
        assert!(
            !dispatcher
                .forward(pen(0.1, 2, true, true, 0))
                .metadata
                .coalescable
        );
        assert_eq!(dispatcher.edge_count(), 2);
    }

    #[test]
    fn with_capacity_zero_panics() {
        let result = std::panic::catch_unwind(|| PenForwardingDispatcher::with_capacity(0));
        assert!(result.is_err());
    }

    #[test]
    fn release_event_synthesizes_out_of_proximity_at_last_known_position() {
        let mut dispatcher = PenForwardingDispatcher::new();
        dispatcher.forward(pen(0.4, 1, true, true, 0));
        let release = dispatcher
            .release_event(metadata(2))
            .expect("dispatcher was in proximity");
        assert_close(release.x, 0.4);
        assert!(!release.in_proximity);
        assert!(!release.touching);
        assert_eq!(release.buttons, 0);
        assert!(!release.metadata.coalescable);
        assert!(dispatcher.release_event(metadata(3)).is_none());
    }

    #[test]
    fn release_event_is_none_when_never_in_proximity_or_after_leaving() {
        let mut dispatcher = PenForwardingDispatcher::new();
        assert!(dispatcher.release_event(metadata(1)).is_none());
        dispatcher.forward(pen(0.4, 1, true, true, 0));
        dispatcher.forward(pen(0.4, 2, false, false, 0));
        assert!(dispatcher.release_event(metadata(3)).is_none());
    }

    #[test]
    fn reset_clears_last_position_too() {
        let mut dispatcher = PenForwardingDispatcher::new();
        dispatcher.forward(pen(0.7, 1, true, true, 0));
        dispatcher.reset();
        assert!(dispatcher.release_event(metadata(2)).is_none());
    }
}
