// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One session's input: a Deck's [`DeckInputEvent`]s to the wire.
//!
//! [`SessionInput`] is what a platform shell hands each session window's
//! input adapter: [`SessionInput::sink`] gives one [`DeckInputSink`] per
//! window (per monitor), all sharing the session's sequence numbers, the
//! keyboard's timestamps and the region runtime. Keys, the host reset and
//! committed text are built as the macOS Deck builds them; pointer, wheel
//! and pen go through the shared [`DeckRegionRuntime`] (the region wire on
//! a negotiated multi-monitor session, its legacy adapter otherwise).
//! Events the Deck keeps (the input method's composition, Deck shortcuts,
//! the end of a pointer lock) and the host-led cursor go to the shell's own
//! sink, never to the host.
//!
//! A multi-window session's pointer follows a held-button drag from window
//! to window ([`PointerDragCapture`]): a window reports its positions as
//! fractions of its own picture, outside `0.0..=1.0` while it holds a drag
//! that left it, and the shell publishes where each window is on the Deck's
//! desktop ([`SessionInput::set_picture_surface`]).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_input::deck_events::{DeckInputEvent, DeckInputSink, LedCursor};
use arcen_input::deck_pen::ImageRect;
use arcen_input::deck_wheel::WheelScroll;
use arcen_input::keyboard::HostKeyEdge;
use arcen_input::{GestureSample, PenEvent, RegionInputWireMessage, ScrollPhase, ScrollUnit};
use arcen_media::{MonitorDesktopRect, RequestedMonitorTopology, SessionMonitorId};
use arcen_protocol::messages::{
    KeyEventMsg, KeyResetModifiersMsg, MouseMoveRelativeMsg, PointerMotionMode, ScrollUnitMsg,
    TextCommitMsg,
};

use super::pointer_drag::{DragGeometry, DragRoute, PictureSurface, PointerDragCapture};
use super::region_runtime::{
    AppliedTopologyView, DeckRegionRuntime, DeckRegionRuntimeError, RegionViewport,
};
use crate::runtime::session::{SessionCommand, SessionCommandSender};

/// Why the session ended a pointer lock it could not serve: the region wire
/// has no relative motion (the macOS Deck's reason).
pub const RELATIVE_UNAVAILABLE: &str = "region_relative_input_unavailable";

/// A session's input path to the host; cheap to clone, shared by every
/// window of the session.
#[derive(Clone)]
pub struct SessionInput {
    commands: SessionCommandSender,
    local: Arc<dyn DeckInputSink + Send + Sync>,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for SessionInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionInput")
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts of one session's input"
)]
struct State {
    runtime: Option<DeckRegionRuntime>,
    /// The stream size of a single-monitor (legacy wire) runtime.
    legacy_size: Option<(u32, u32)>,
    region_input_confirmed: bool,
    typed_pen: bool,
    /// The host advertised gestures (`supports_gestures_v1`).
    gestures: bool,
    sequence: u64,
    last_keyboard_ns: u64,
    /// The last legacy pointer position sent: normalized and host pixels.
    last_pointer: Option<(f64, f64, i32, i32)>,
    relative: bool,
    pen_touching: bool,
    /// The held-button drag across the session's windows.
    drag: PointerDragCapture,
    /// Where each window shows its monitor on the Deck's desktop.
    surfaces: Vec<PictureSurface>,
    /// The host's applied layout, the drag's fallback geometry.
    host_layout: Vec<(SessionMonitorId, MonitorDesktopRect)>,
}

/// What a pointer message does to the drag.
#[derive(Debug, Clone, Copy)]
enum PointerEdge {
    Motion,
    Button(u8, bool),
}

impl SessionInput {
    /// A session's input over its command sender; `local` receives what the
    /// Deck keeps (composition, Deck shortcuts, a pointer lock ending, the
    /// host-led cursor).
    #[must_use]
    pub fn new(
        commands: SessionCommandSender,
        local: Arc<dyn DeckInputSink + Send + Sync>,
    ) -> Self {
        Self {
            commands,
            local,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// A negotiated multi-monitor session's applied topology (the requested
    /// layout too, when the Deck asked for one); `region_input_confirmed` is
    /// the host's input-v4 (region wire) capability.
    ///
    /// # Errors
    ///
    /// The topology cannot form a region aggregate; input stays as it was.
    pub fn set_region_topology(
        &self,
        validated: &impl AppliedTopologyView,
        requested: Option<&RequestedMonitorTopology>,
        region_input_confirmed: bool,
    ) -> Result<(), DeckRegionRuntimeError> {
        let runtime = DeckRegionRuntime::from_validated_topology(validated, requested)?;
        let mut state = self.lock();
        state.runtime = Some(runtime);
        state.legacy_size = None;
        state.region_input_confirmed = region_input_confirmed;
        state.host_layout = validated
            .applied_monitors()
            .into_iter()
            .map(|(monitor, _, rect)| (monitor, rect))
            .collect();
        state.surfaces.clear();
        state.drag.reset();
        Ok(())
    }

    /// A single-monitor session streaming `width`×`height` host pixels
    /// (the legacy wire).
    ///
    /// # Errors
    ///
    /// The size cannot form a region.
    pub fn set_legacy_frame(&self, width: u32, height: u32) -> Result<(), DeckRegionRuntimeError> {
        let mut state = self.lock();
        if state.runtime.is_none() || state.legacy_size != Some((width, height)) {
            state.runtime = Some(DeckRegionRuntime::legacy_primary(width, height)?);
            state.legacy_size = Some((width, height));
            state.region_input_confirmed = false;
            state.host_layout.clear();
            state.surfaces.clear();
            state.drag.reset();
        }
        Ok(())
    }

    /// Whether the host takes typed pen samples (Tablet support); otherwise
    /// the pen moves and clicks the pointer.
    pub fn set_typed_pen(&self, typed: bool) {
        self.lock().typed_pen = typed;
    }

    /// Whether the host takes trackpad gestures (`supports_gestures_v1`);
    /// otherwise a pinch sends nothing, as on the macOS Deck.
    pub fn set_gestures(&self, gestures: bool) {
        self.lock().gestures = gestures;
    }

    /// Where the window showing `monitor` is on the Deck's desktop: its
    /// content area and its picture, in one space for every window
    /// (`None`: it cannot be placed now). A drag that leaves a window lands
    /// on the window under the cursor.
    pub fn set_picture_surface(
        &self,
        monitor: SessionMonitorId,
        placed: Option<(ImageRect, ImageRect)>,
    ) {
        let mut state = self.lock();
        state.surfaces.retain(|surface| surface.monitor != monitor);
        if let Some((window, picture)) = placed {
            state.surfaces.push(PictureSurface {
                monitor,
                window,
                picture,
            });
        }
    }

    /// The input sink of the window showing `monitor` (`None`: the primary,
    /// or the only, window).
    #[must_use]
    pub fn sink(&self, monitor: Option<SessionMonitorId>) -> Box<dyn DeckInputSink + Send> {
        Box::new(ViewportSink {
            input: self.clone(),
            monitor,
        })
    }

    /// One event from the window showing `monitor`.
    pub fn input(&self, monitor: Option<SessionMonitorId>, event: DeckInputEvent) {
        match event {
            DeckInputEvent::Keys(edges) => self.keys(&edges),
            DeckInputEvent::Reset { reason } => {
                let reset = KeyResetModifiersMsg {
                    reason: reason.to_owned(),
                    ..KeyResetModifiersMsg::default()
                };
                self.send_value(serde_json::to_value(reset));
            }
            DeckInputEvent::Text(text) => self.text(text),
            DeckInputEvent::PointerMove { x, y } => {
                self.lock().relative = false;
                let Some((monitor, at)) = self.place(monitor, (x, y), PointerEdge::Motion) else {
                    return;
                };
                self.pointer(
                    monitor,
                    at,
                    PointerMotionMode::Absolute,
                    |runtime, viewport, at, sequence, now| {
                        runtime.pointer_motion(viewport, at, sequence, now)
                    },
                );
            }
            DeckInputEvent::Button {
                button,
                pressed,
                x,
                y,
            } => self.button(monitor, button, pressed, (x, y)),
            DeckInputEvent::Scroll { scroll, x, y } => self.scroll(monitor, scroll, (x, y)),
            DeckInputEvent::PointerRelative { dx, dy } => self.relative(dx, dy),
            DeckInputEvent::Pen(event) => self.pen(monitor, event),
            DeckInputEvent::Gesture(sample) => self.gesture(sample),
            DeckInputEvent::PointerLockReleased { .. } => {
                self.lock().relative = false;
                self.local.input(event);
            }
            DeckInputEvent::Preedit(_)
            | DeckInputEvent::Shortcut(_)
            | DeckInputEvent::DismissModal => self.local.input(event),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn keys(&self, edges: &[HostKeyEdge]) {
        for edge in edges {
            let message = {
                let mut state = self.lock();
                let (server_x, server_y) = state
                    .last_pointer
                    .map_or((-1, -1), |(_, _, server_x, server_y)| (server_x, server_y));
                state.sequence = state.sequence.saturating_add(1);
                state.last_keyboard_ns =
                    now_epoch_ns().max(state.last_keyboard_ns.saturating_add(1));
                KeyEventMsg {
                    scan_code: edge.qt_key,
                    pressed: edge.pressed,
                    modifiers: edge.modifiers,
                    caps_lock_on: None,
                    num_lock_on: None,
                    scroll_lock_on: None,
                    server_x,
                    server_y,
                    sequence: state.sequence,
                    timestamp_ns: state.last_keyboard_ns,
                    coalescable: false,
                    ..KeyEventMsg::default()
                }
            };
            self.send_value(serde_json::to_value(message));
        }
    }

    fn text(&self, text: String) {
        let sequence = {
            let mut state = self.lock();
            state.sequence = state.sequence.saturating_add(1);
            state.sequence
        };
        let message = TextCommitMsg::new(text, sequence);
        if message.validate().is_err() {
            tracing::debug!(
                target: crate::log_target::INPUT,
                sequence,
                "committed text not sendable; dropped"
            );
            return;
        }
        self.send_value(serde_json::to_value(message));
    }

    fn button(&self, monitor: Option<SessionMonitorId>, button: u8, pressed: bool, at: (f64, f64)) {
        if self.lock().relative {
            // Locked: the button goes where the host pointer already is.
            let messages = {
                let mut state = self.lock();
                let State {
                    runtime, sequence, ..
                } = &mut *state;
                let Some(runtime) = runtime.as_mut() else {
                    return;
                };
                runtime.pointer_button_at_latest(button, pressed, sequence, now_epoch_ns())
            };
            match messages {
                Ok(message) => {
                    self.send_region(message.as_slice(), PointerMotionMode::Relative);
                }
                Err(error) => warn_rejected(&error, "button"),
            }
            return;
        }
        let Some((monitor, at)) = self.place(monitor, at, PointerEdge::Button(button, pressed))
        else {
            return;
        };
        self.pointer(
            monitor,
            at,
            PointerMotionMode::Absolute,
            |runtime, viewport, at, sequence, now| {
                runtime.pointer_button(viewport, at, button, pressed, sequence, now)
            },
        );
    }

    /// Which window's monitor a pointer message at `at` (a fraction of the
    /// picture of the window showing `monitor`) goes to, and where on it:
    /// the drag's rule in a multi-window session, the window's own picture
    /// otherwise. `None`: nowhere (another window holds the drag).
    fn place(
        &self,
        monitor: Option<SessionMonitorId>,
        at: (f64, f64),
        edge: PointerEdge,
    ) -> Option<(Option<SessionMonitorId>, (f64, f64))> {
        let Some(source) = monitor else {
            return (at.0.is_finite() && at.1.is_finite())
                .then(|| (None, (at.0.clamp(0.0, 1.0), at.1.clamp(0.0, 1.0))));
        };
        let mut state = self.lock();
        let State {
            drag,
            surfaces,
            host_layout,
            ..
        } = &mut *state;
        let geometry = DragGeometry {
            surfaces,
            host: host_layout,
        };
        let route = match edge {
            PointerEdge::Motion => drag.motion(&geometry, source, at),
            PointerEdge::Button(button, pressed) => {
                drag.button(&geometry, source, at, button, pressed)
            }
        };
        match route {
            DragRoute::To { monitor, fraction } => Some((Some(monitor), fraction)),
            DragRoute::Ignore => None,
        }
    }

    fn scroll(&self, monitor: Option<SessionMonitorId>, scroll: WheelScroll, at: (f64, f64)) {
        // While locked the host scrolls where its pointer is (the macOS rule).
        let mode = if self.lock().relative {
            PointerMotionMode::Relative
        } else {
            PointerMotionMode::Absolute
        };
        let Some((monitor, at)) = self.place(monitor, at, PointerEdge::Motion) else {
            return;
        };
        match scroll.unit {
            ScrollUnitMsg::Line => {
                let (Some(dx), Some(dy)) = (whole_ticks(scroll.dx), whole_ticks(scroll.dy)) else {
                    return;
                };
                self.pointer(monitor, at, mode, |runtime, viewport, at, sequence, now| {
                    runtime.pointer_scroll(viewport, at, dx, dy, sequence, now)
                });
            }
            ScrollUnitMsg::Point => {
                #[allow(clippy::cast_precision_loss, reason = "a small constant")]
                let units =
                    |value: f64| (value * arcen_media::LOGICAL_UNITS_PER_PIXEL as f64).round();
                let (dx, dy) = (units(scroll.dx), units(scroll.dy));
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "rounded and range-checked by the runtime"
                )]
                let (dx, dy) = (dx as i64, dy as i64);
                // A phase without travel still matters: a gesture's end is
                // what lets the host start its own momentum (the macOS rule).
                let (dx, dy) = match scroll.phase {
                    ScrollPhase::Began | ScrollPhase::Ended | ScrollPhase::Cancelled
                        if dx == 0 && dy == 0 =>
                    {
                        (0, 1)
                    }
                    _ => (dx, dy),
                };
                if dx == 0 && dy == 0 {
                    return;
                }
                let phase = scroll.phase;
                self.pointer(monitor, at, mode, |runtime, viewport, at, sequence, now| {
                    runtime.pointer_scroll_units_with_details(
                        viewport,
                        at,
                        dx,
                        dy,
                        ScrollUnit::Point,
                        phase,
                        sequence,
                        now,
                    )
                });
            }
        }
    }

    fn gesture(&self, sample: GestureSample) {
        let value = {
            let mut state = self.lock();
            if !state.gestures {
                return;
            }
            state.sequence = state.sequence.saturating_add(1);
            sample.wire(state.sequence, now_epoch_ns()).to_value()
        };
        self.send_value(value);
    }

    fn relative(&self, dx: i32, dy: i32) {
        let region = {
            let mut state = self.lock();
            state.relative = true;
            // A locked pointer has no window to follow.
            state.drag.reset();
            state
                .runtime
                .as_ref()
                .is_some_and(DeckRegionRuntime::uses_region_wire)
        };
        if region {
            self.local.input(DeckInputEvent::PointerLockReleased {
                reason: RELATIVE_UNAVAILABLE,
            });
            return;
        }
        if dx == 0 && dy == 0 {
            return;
        }
        let sequence = {
            let mut state = self.lock();
            state.sequence = state.sequence.saturating_add(1);
            state.sequence
        };
        let message = MouseMoveRelativeMsg {
            dx,
            dy,
            sequence,
            timestamp_ns: now_epoch_ns(),
            ..MouseMoveRelativeMsg::default()
        };
        self.send_value(serde_json::to_value(message));
    }

    fn pen(&self, monitor: Option<SessionMonitorId>, event: PenEvent) {
        if self.lock().typed_pen {
            let message = {
                let mut state = self.lock();
                let State {
                    runtime, sequence, ..
                } = &mut *state;
                let Some(runtime) = runtime.as_mut() else {
                    return;
                };
                let Some(viewport) = viewport(runtime, monitor) else {
                    return;
                };
                *sequence = sequence.saturating_add(1);
                let mut event = event;
                event.metadata.sequence = *sequence;
                runtime.pen(viewport, event)
            };
            match message {
                Ok(message) => self.send_region(&[message], PointerMotionMode::Absolute),
                Err(error) => warn_rejected(&error, "pen"),
            }
            return;
        }
        // No typed pen on this host: the pen is the pointer, its tip the
        // left button.
        if !event.in_proximity {
            let touching = std::mem::take(&mut self.lock().pen_touching);
            if touching {
                self.button(monitor, 1, false, (event.x, event.y));
            }
            return;
        }
        let was_touching = self.lock().pen_touching;
        self.input(
            monitor,
            DeckInputEvent::PointerMove {
                x: event.x,
                y: event.y,
            },
        );
        if event.touching != was_touching {
            self.lock().pen_touching = event.touching;
            self.button(monitor, 1, event.touching, (event.x, event.y));
        }
    }

    /// A pointer message at `at` in the window showing `monitor`.
    fn pointer(
        &self,
        monitor: Option<SessionMonitorId>,
        at: (f64, f64),
        mode: PointerMotionMode,
        emit: impl FnOnce(
            &mut DeckRegionRuntime,
            RegionViewport,
            (f64, f64),
            &mut u64,
            u64,
        ) -> Result<Vec<RegionInputWireMessage>, DeckRegionRuntimeError>,
    ) {
        let messages = {
            let mut state = self.lock();
            let State {
                runtime, sequence, ..
            } = &mut *state;
            let Some(runtime) = runtime.as_mut() else {
                return;
            };
            let Some(viewport) = viewport(runtime, monitor) else {
                return;
            };
            emit(runtime, viewport, at, sequence, now_epoch_ns())
        };
        match messages {
            Ok(messages) => self.send_region(&messages, mode),
            Err(error) => warn_rejected(&error, "pointer"),
        }
    }

    /// The macOS Deck's rule: the region wire only when the host confirmed
    /// it, the legacy adapter for a single-monitor session.
    fn send_region(&self, messages: &[RegionInputWireMessage], mode: PointerMotionMode) {
        let values: Vec<serde_json::Value> = {
            let mut state = self.lock();
            let Some(runtime) = state.runtime.as_ref() else {
                return;
            };
            if runtime.uses_region_wire() {
                if !state.region_input_confirmed {
                    tracing::error!(
                        target: crate::log_target::INPUT,
                        "region input send blocked because input-v4 capability was not negotiated",
                    );
                    return;
                }
                messages
                    .iter()
                    .filter_map(|message| message.to_json_value().ok())
                    .collect()
            } else {
                let mut values = Vec::with_capacity(messages.len());
                let mut last_pointer = None;
                for message in messages {
                    match runtime.adapt_legacy(message, mode) {
                        Ok(Some(adapted)) => {
                            last_pointer = adapted.pointer_position.or(last_pointer);
                            values.push(adapted.value);
                        }
                        Ok(None) => {}
                        Err(error) => warn_rejected(&error, "legacy adapter"),
                    }
                }
                if last_pointer.is_some() {
                    state.last_pointer = last_pointer;
                }
                values
            }
        };
        for value in values {
            self.send_value(Ok(value));
        }
    }

    fn send_value(&self, value: Result<serde_json::Value, serde_json::Error>) {
        let Ok(value) = value else {
            return;
        };
        if self.commands.send(SessionCommand::Json(value)).is_err() {
            tracing::debug!(
                target: crate::log_target::INPUT,
                "input after the session ended; dropped"
            );
        }
    }
}

/// The input sink of one session window.
struct ViewportSink {
    input: SessionInput,
    monitor: Option<SessionMonitorId>,
}

impl DeckInputSink for ViewportSink {
    fn input(&self, event: DeckInputEvent) {
        self.input.input(self.monitor, event);
    }

    fn led_cursor(&self, led: Option<LedCursor>) {
        self.input.local.led_cursor(led);
    }
}

fn viewport(
    runtime: &DeckRegionRuntime,
    monitor: Option<SessionMonitorId>,
) -> Option<RegionViewport> {
    match monitor {
        Some(monitor) => runtime.viewport_for_monitor(monitor),
        None => Some(runtime.primary_viewport()),
    }
}

fn whole_ticks(value: f64) -> Option<i32> {
    let rounded = value.round();
    if !rounded.is_finite() || rounded.abs() > f64::from(i32::MAX) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, reason = "range-checked above")]
    Some(rounded as i32)
}

fn warn_rejected(error: &DeckRegionRuntimeError, what: &str) {
    tracing::warn!(
        target: crate::log_target::INPUT,
        %error,
        what,
        "session input was rejected",
    );
}

fn now_epoch_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use arcen_input::keyboard::DeckShortcut;
    use arcen_input::{LowLatencyMetadata, PenTool};
    use arcen_media::video::EncoderBackend;
    use arcen_media::{
        BitrateBudgetKbps, DesktopRect, MediaStreamEpoch, MonitorDesktopRect, RegionMediaPlan,
        RegionMediaRoster, ResolvedAppliedMonitor, TopologyGeneration, ValidatedAppliedTopology,
        VideoConfiguration,
    };
    use arcen_protocol::messages::MultiMonitorCarrierMsg;
    use serde_json::Value;
    use tokio::sync::mpsc;

    #[derive(Default)]
    struct Local(Mutex<Vec<DeckInputEvent>>);

    impl DeckInputSink for Local {
        fn input(&self, event: DeckInputEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn session() -> (
        SessionInput,
        mpsc::UnboundedReceiver<SessionCommand>,
        Arc<Local>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let local = Arc::new(Local::default());
        let input = SessionInput::new(SessionCommandSender::for_test(sender), local.clone());
        (input, receiver, local)
    }

    fn sent(receiver: &mut mpsc::UnboundedReceiver<SessionCommand>) -> Vec<Value> {
        let mut values = Vec::new();
        while let Ok(command) = receiver.try_recv() {
            if let SessionCommand::Json(value) = command {
                values.push(value);
            }
        }
        values
    }

    fn kind(value: &Value) -> &str {
        value["type"].as_str().unwrap_or_default()
    }

    fn plan(monitor: u16, width: u32, height: u32) -> RegionMediaPlan {
        RegionMediaPlan::new(
            SessionMonitorId::new(monitor).unwrap(),
            MediaStreamEpoch::new(u64::from(monitor) + 40).unwrap(),
            EncoderBackend::OpenH264,
            VideoConfiguration::legacy_h264(),
            width,
            height,
            60,
            BitrateBudgetKbps::nominal_for_geometry(width, height, 60),
        )
        .unwrap()
    }

    /// Two 1920×1080 monitors side by side.
    fn two_monitors() -> ValidatedAppliedTopology<u32> {
        let monitor = |id: u16, x: i64| ResolvedAppliedMonitor {
            session_monitor_id: SessionMonitorId::new(id).unwrap(),
            native_display_id: u32::from(id) + 10,
            rect: MonitorDesktopRect {
                x,
                y: 0,
                width_px: 1920,
                height_px: 1080,
            },
        };
        ValidatedAppliedTopology {
            generation: TopologyGeneration::new(3).unwrap(),
            carrier: MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors: vec![monitor(1, 0), monitor(2, 1920)],
            media_roster: Box::new(
                RegionMediaRoster::new(vec![plan(1, 1920, 1080), plan(2, 1920, 1080)]).unwrap(),
            ),
            desktop: DesktopRect {
                x: 0,
                y: 0,
                width_px: 3840,
                height_px: 1080,
            },
        }
    }

    fn edge(qt_key: u32, pressed: bool) -> HostKeyEdge {
        HostKeyEdge {
            qt_key,
            pressed,
            modifiers: 0,
        }
    }

    #[test]
    fn a_single_monitor_session_speaks_the_legacy_wire_in_order() {
        let (input, mut receiver, local) = session();
        input.set_legacy_frame(1920, 1080).unwrap();
        let sink = input.sink(None);
        sink.input(DeckInputEvent::PointerMove { x: 0.5, y: 0.5 });
        sink.input(DeckInputEvent::Keys(vec![
            edge(0x41, true),
            edge(0x41, false),
        ]));
        sink.input(DeckInputEvent::Reset {
            reason: "focus_out",
        });
        sink.input(DeckInputEvent::Text("é".to_owned()));
        sink.input(DeckInputEvent::Button {
            button: 1,
            pressed: true,
            x: 0.5,
            y: 0.5,
        });
        sink.input(DeckInputEvent::Scroll {
            scroll: WheelScroll {
                dx: 0.0,
                dy: 1.0,
                unit: ScrollUnitMsg::Line,
                phase: ScrollPhase::None,
            },
            x: 0.5,
            y: 0.5,
        });
        let values = sent(&mut receiver);
        let kinds: Vec<&str> = values.iter().map(kind).collect();
        assert_eq!(
            kinds,
            [
                "mouse_move",
                "key_event",
                "key_event",
                "key_reset_modifiers",
                "text_commit",
                "mouse_button",
                "mouse_scroll"
            ]
        );
        // Keys carry where the pointer is, as the macOS Deck's do.
        assert_eq!(values[1]["server_x"], values[0]["server_x"]);
        assert_eq!(values[1]["scan_code"], 0x41);
        assert_eq!(values[3]["reason"], "focus_out");
        assert_eq!(values[4]["text"], "é");
        assert!(values[6]["dy"].as_f64().unwrap() > 0.0, "{}", values[6]);
        let sequences: Vec<u64> = values
            .iter()
            .filter_map(|value| value["sequence"].as_u64())
            .collect();
        assert!(
            sequences.windows(2).all(|pair| pair[0] < pair[1]),
            "{sequences:?}"
        );
        assert!(values[1]["timestamp_ns"].as_u64() < values[2]["timestamp_ns"].as_u64());
        assert!(local.0.lock().unwrap().is_empty());
    }

    #[test]
    fn what_the_deck_keeps_never_reaches_the_host() {
        let (input, mut receiver, local) = session();
        input.set_legacy_frame(1920, 1080).unwrap();
        let sink = input.sink(None);
        sink.input(DeckInputEvent::Preedit(Some("にほ".to_owned())));
        sink.input(DeckInputEvent::Shortcut(DeckShortcut::Disconnect));
        sink.input(DeckInputEvent::PointerLockReleased { reason: "panic_f9" });
        assert!(sent(&mut receiver).is_empty());
        assert_eq!(local.0.lock().unwrap().len(), 3);
    }

    #[test]
    fn each_window_points_into_its_own_monitor_once_the_host_confirms_regions() {
        let (input, mut receiver, _) = session();
        input
            .set_region_topology(&two_monitors(), None, false)
            .unwrap();
        let second = input.sink(Some(SessionMonitorId::new(2).unwrap()));
        second.input(DeckInputEvent::PointerMove { x: 0.5, y: 0.5 });
        assert!(
            sent(&mut receiver).is_empty(),
            "region input before input-v4"
        );

        input
            .set_region_topology(&two_monitors(), None, true)
            .unwrap();
        second.input(DeckInputEvent::PointerMove { x: 0.5, y: 0.5 });
        let values = sent(&mut receiver);
        assert!(!values.is_empty());
        assert!(
            values
                .iter()
                .all(|value| kind(value).starts_with("region_"))
        );
        assert_eq!(kind(&values[0]), "region_pointer_enter", "{values:?}");
        assert_eq!(values[0]["region_id"], 2, "{values:?}");

        // The region wire has no relative motion: the Deck ends the lock.
        second.input(DeckInputEvent::PointerRelative { dx: 3, dy: 4 });
        assert!(sent(&mut receiver).is_empty());
    }

    /// The host lays monitor 2 to the right of monitor 1, the Deck shows
    /// it on a display to the left: the drag follows the Deck's displays.
    #[test]
    fn a_held_drag_crosses_into_the_window_under_the_cursor_and_is_released_there() {
        let (input, mut receiver, _) = session();
        input
            .set_region_topology(&two_monitors(), None, true)
            .unwrap();
        let (one, two) = (
            SessionMonitorId::new(1).unwrap(),
            SessionMonitorId::new(2).unwrap(),
        );
        let area = |left: f64| ImageRect {
            left,
            top: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        input.set_picture_surface(one, Some((area(0.0), area(0.0))));
        input.set_picture_surface(two, Some((area(-1920.0), area(-1920.0))));
        let (first, second) = (input.sink(Some(one)), input.sink(Some(two)));
        let regions = |values: &[Value], kind_of: &str| -> Vec<u64> {
            values
                .iter()
                .filter(|value| kind(value) == kind_of)
                .filter_map(|value| value["region_id"].as_u64())
                .collect()
        };

        first.input(DeckInputEvent::Button {
            button: 1,
            pressed: true,
            x: 0.1,
            y: 0.5,
        });
        let pressed = sent(&mut receiver);
        assert_eq!(
            regions(&pressed, "region_pointer_button"),
            vec![1],
            "{pressed:?}"
        );

        // Window 1 holds the drag and reports a position left of itself.
        first.input(DeckInputEvent::PointerMove { x: -0.5, y: 0.5 });
        let crossed = sent(&mut receiver);
        assert_eq!(
            regions(&crossed, "region_pointer_leave"),
            vec![1],
            "{crossed:?}"
        );
        assert_eq!(
            regions(&crossed, "region_pointer_enter"),
            vec![2],
            "{crossed:?}"
        );
        // Window 2 sees the pointer too: it sends nothing during the drag.
        second.input(DeckInputEvent::PointerMove { x: 0.5, y: 0.5 });
        assert!(sent(&mut receiver).is_empty());

        first.input(DeckInputEvent::Button {
            button: 1,
            pressed: false,
            x: -0.5,
            y: 0.5,
        });
        let released = sent(&mut receiver);
        assert_eq!(
            regions(&released, "region_pointer_button"),
            vec![2],
            "{released:?}"
        );

        // Free again: window 2 points into its own monitor.
        second.input(DeckInputEvent::PointerMove { x: 0.25, y: 0.5 });
        assert!(!sent(&mut receiver).is_empty());
    }

    #[test]
    fn relative_motion_goes_as_such_on_the_legacy_wire_and_ends_the_lock_on_regions() {
        let (input, mut receiver, local) = session();
        input.set_legacy_frame(1920, 1080).unwrap();
        input.input(None, DeckInputEvent::PointerRelative { dx: 3, dy: -4 });
        input.input(
            None,
            DeckInputEvent::Scroll {
                scroll: WheelScroll {
                    dx: 0.0,
                    dy: 1.0,
                    unit: ScrollUnitMsg::Line,
                    phase: ScrollPhase::None,
                },
                x: 0.5,
                y: 0.5,
            },
        );
        let values = sent(&mut receiver);
        assert_eq!(kind(&values[0]), "mouse_move_relative");
        assert_eq!(
            (values[0]["dx"].as_i64(), values[0]["dy"].as_i64()),
            (Some(3), Some(-4))
        );
        // A wheel under the lock scrolls where the host pointer is: no move first.
        let scrolls: Vec<&Value> = values
            .iter()
            .filter(|value| kind(value) == "mouse_scroll")
            .collect();
        assert_eq!(scrolls.len(), 1, "{values:?}");
        assert_eq!(scrolls[0]["motion_mode"], "relative", "{}", scrolls[0]);

        input
            .set_region_topology(&two_monitors(), None, true)
            .unwrap();
        input.input(None, DeckInputEvent::PointerRelative { dx: 3, dy: 4 });
        assert!(sent(&mut receiver).is_empty());
        assert_eq!(
            local.0.lock().unwrap().as_slice(),
            [DeckInputEvent::PointerLockReleased {
                reason: RELATIVE_UNAVAILABLE
            }]
        );
    }

    fn pen(touching: bool, in_proximity: bool) -> PenEvent {
        PenEvent {
            x: 0.25,
            y: 0.75,
            pressure: if touching { 0.5 } else { 0.0 },
            tilt_x_degrees: 30.0,
            tilt_y_degrees: -20.0,
            rotation_degrees: 90.0,
            tool: PenTool::Tip,
            in_proximity,
            touching,
            buttons: 0,
            metadata: LowLatencyMetadata::default(),
        }
    }

    #[test]
    fn a_pen_is_typed_where_the_host_takes_it_and_the_pointer_where_it_does_not() {
        let (input, mut receiver, _) = session();
        input.set_legacy_frame(1920, 1080).unwrap();
        for event in [
            pen(false, true),
            pen(true, true),
            pen(false, true),
            pen(false, false),
        ] {
            input.input(None, DeckInputEvent::Pen(event));
        }
        let kinds: Vec<String> = sent(&mut receiver)
            .iter()
            .map(|value| kind(value).to_owned())
            .collect();
        assert_eq!(
            kinds,
            ["mouse_move", "mouse_button", "mouse_button",],
            "unmoved repeats are not sent; the tip is the left button"
        );

        input.set_typed_pen(true);
        input.input(None, DeckInputEvent::Pen(pen(true, true)));
        let values = sent(&mut receiver);
        assert_eq!(values.len(), 1);
        assert_eq!(kind(&values[0]), "pen_event", "{}", values[0]);
        assert!((values[0]["pressure"].as_f64().unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_touchpad_scroll_keeps_its_phases_and_a_pinch_goes_only_where_taken() {
        use arcen_input::GestureSample;
        use arcen_protocol::messages::ScrollPhaseMsg;
        let (input, mut receiver, _) = session();
        input.set_legacy_frame(1920, 1080).unwrap();
        let scroll = |dy: f64, phase: ScrollPhase| DeckInputEvent::Scroll {
            scroll: WheelScroll {
                dx: 0.0,
                dy,
                unit: ScrollUnitMsg::Point,
                phase,
            },
            x: 0.5,
            y: 0.5,
        };
        input.input(None, scroll(12.0, ScrollPhase::Began));
        input.input(None, scroll(3.0, ScrollPhase::Changed));
        input.input(None, scroll(0.0, ScrollPhase::Changed));
        input.input(None, scroll(0.0, ScrollPhase::Ended));
        let values = sent(&mut receiver);
        let phases: Vec<&str> = values
            .iter()
            .filter(|value| kind(value) == "mouse_scroll")
            .map(|value| value["phase"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(phases, ["began", "changed", "ended"], "{values:?}");
        let pinch = DeckInputEvent::Gesture(GestureSample::Magnify {
            scale_delta: 0.1,
            phase: ScrollPhaseMsg::Began,
        });
        input.input(None, pinch.clone());
        assert!(sent(&mut receiver).is_empty(), "no gestures advertised");
        input.set_gestures(true);
        input.input(None, pinch);
        let values = sent(&mut receiver);
        assert_eq!(values.len(), 1);
        assert_eq!(kind(&values[0]), "gesture_magnify");
        assert!(values[0]["sequence"].as_u64().unwrap() > 0);
    }
}
