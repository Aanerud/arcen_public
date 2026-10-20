//! Pen input that never waits for the UI pass.
//!
//! AppKit dispatches each tablet event to the local monitor in
//! `tablet/monitor.rs`, which offers it here at once. [`PenInput`] routes the
//! sample through the geometry the last UI pass published ([`PenRoute`]),
//! maps it, classifies it with the shared
//! [`arcen_input::PenForwardingDispatcher`], and hands it to the session's
//! input lane in the same call — stamped with the event's own time, not the
//! time a later pass happened to look at it. The UI pass only publishes
//! geometry and session state ([`PenLink`]) and drains anything the monitor
//! had to queue while this state was busy.
//!
//! Order across devices: keys, modifiers, scrolling and mouse buttons still
//! reach the host through the UI pass. When the monitor reports one
//! ([`TabletMonitorEvent::InputEdge`]) later pen samples are held, in order,
//! until that pass has sent it ([`PenInput::ui_pass_completed`]), so a
//! modifier pressed before a stroke always arrives before the stroke.
//!
//! Everything here runs on the main thread. The AppKit callback and the UI
//! pass never interleave, so the shared state is `Rc<RefCell<_>>`; the
//! callback only ever uses `try_borrow_mut` and falls back to the monitor's
//! queue.
#![forbid(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use arcen_input::{LowLatencyMetadata, PenEvent, PenForwardingDispatcher, PenTool};
use arcen_media::SessionMonitorId;

use crate::observability::ClientTelemetry;
use crate::protocol::messages::{PenEventMsg, PenToolMsg, PointerMotionMode, TabletModeMsg};
use crate::tablet::{
    RawTabletSample, TabletCapabilityProbe, TabletMapper, TabletMonitorEvent, ViewSize,
};
use crate::transport::websocket::{SessionCommand, SessionCommandSender};
use crate::ui::region_runtime::{DeckRegionRuntime, RegionViewport};

/// Region runtime shared by pointer input (UI pass) and pen input (AppKit
/// callback): one stateful emitter, one session-global sequence.
pub(crate) type SharedRegionRuntime = Rc<RefCell<Option<DeckRegionRuntime>>>;

/// Session-global input sequence shared by every input path.
pub(crate) type SharedInputSequence = Rc<Cell<u64>>;

/// Pen samples that may wait behind an ordering barrier. A barrier lasts at
/// most one UI pass, about 7 samples of a 200 Hz digitizer at 30 Hz; the
/// bound only exists so a stalled UI cannot grow memory without limit.
pub(crate) const HOLD_CAPACITY: usize = 512;

/// Where pen samples go, as of the last UI pass that drew the session view.
#[derive(Debug, Clone, Default)]
pub(crate) struct PenRoute {
    /// Root video rectangle within the root window. `None` while no session
    /// view is on screen: samples are then not forwarded.
    pub(crate) root_view: Option<ViewSize>,
    pub(crate) multi_window_active: bool,
    pub(crate) multi_window_committed: bool,
    pub(crate) root_window_number: Option<isize>,
    pub(crate) secondary_targets: Vec<(isize, SessionMonitorId, ViewSize)>,
    pub(crate) primary_viewport: Option<RegionViewport>,
    pub(crate) monitor_viewports: Vec<(SessionMonitorId, RegionViewport)>,
    pub(crate) left_handed: bool,
}

/// Session state pen forwarding depends on, refreshed by the UI.
#[derive(Clone, Default)]
pub(crate) struct PenLink {
    pub(crate) commands: Option<SessionCommandSender>,
    pub(crate) telemetry: Option<Arc<ClientTelemetry>>,
    pub(crate) remote_frame_size: Option<[usize; 2]>,
    pub(crate) tablet_mode_active: TabletModeMsg,
    pub(crate) pen_negotiated: bool,
    pub(crate) region_input_confirmed: bool,
}

/// Forwarding cadence over one telemetry window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PenForwardSummary {
    pub(crate) forwarded: u64,
    pub(crate) interval_p50_us: u32,
    pub(crate) held: u64,
}

#[derive(Debug, Default)]
struct PenForwardStats {
    forwarded: u64,
    held: u64,
    intervals_us: Vec<u32>,
    last_at: Option<Instant>,
}

impl PenForwardStats {
    const CAPACITY: usize = 4096;

    fn record(&mut self, at: Instant) {
        self.forwarded = self.forwarded.saturating_add(1);
        if let Some(previous) = self.last_at {
            if self.intervals_us.len() < Self::CAPACITY {
                let micros = at.saturating_duration_since(previous).as_micros();
                self.intervals_us
                    .push(u32::try_from(micros).unwrap_or(u32::MAX));
            }
        }
        self.last_at = Some(at);
    }

    fn take(&mut self) -> PenForwardSummary {
        let mut intervals = std::mem::take(&mut self.intervals_us);
        let summary = PenForwardSummary {
            forwarded: self.forwarded,
            interval_p50_us: crate::tablet::sample::percentile(&mut intervals, 50),
            held: self.held,
        };
        self.forwarded = 0;
        self.held = 0;
        summary
    }
}

/// Pen capture, routing and forwarding state. See the module docs.
pub(crate) struct PenInput {
    pub(crate) mapper: TabletMapper,
    pub(crate) dispatcher: PenForwardingDispatcher,
    pub(crate) probe: TabletCapabilityProbe,
    pub(crate) last_event: Option<PenEvent>,
    pub(crate) last_error: Option<String>,
    pub(crate) events_sent: u64,
    pub(crate) overflow_resets: u64,
    pub(crate) last_region_viewport: Option<RegionViewport>,
    route: PenRoute,
    link: PenLink,
    region_runtime: SharedRegionRuntime,
    sequence: SharedInputSequence,
    route_fresh: bool,
    barrier: bool,
    held: VecDeque<(RawTabletSample, u64)>,
    last_timestamp_ns: u64,
    pending_status: Option<String>,
    last_input_sent: Option<(&'static str, Instant)>,
    activity: bool,
    stats: PenForwardStats,
    edge_wake: Option<Box<dyn Fn()>>,
}

impl PenInput {
    pub(crate) fn new(region_runtime: SharedRegionRuntime, sequence: SharedInputSequence) -> Self {
        Self {
            mapper: TabletMapper::new(),
            dispatcher: PenForwardingDispatcher::new(),
            probe: TabletCapabilityProbe::default(),
            last_event: None,
            last_error: None,
            events_sent: 0,
            overflow_resets: 0,
            last_region_viewport: None,
            route: PenRoute::default(),
            link: PenLink::default(),
            region_runtime,
            sequence,
            route_fresh: false,
            barrier: false,
            held: VecDeque::new(),
            last_timestamp_ns: 0,
            pending_status: None,
            last_input_sent: None,
            activity: false,
            stats: PenForwardStats::default(),
            edge_wake: None,
        }
    }

    /// Runs after a pen edge (proximity, contact, tool, button) so the UI
    /// updates mouse-emulation suppression; plain motion never wakes the UI.
    pub(crate) fn set_edge_wake(&mut self, wake: impl Fn() + 'static) {
        self.edge_wake = Some(Box::new(wake));
    }

    #[cfg(test)]
    pub(crate) fn route(&self) -> &PenRoute {
        &self.route
    }

    /// Offers one monitor event from the AppKit callback. Returns `false`,
    /// leaving the event to the monitor's queue, when pen or region state
    /// is borrowed (a re-entrant dispatch) or a panic was caught.
    pub(crate) fn offer(pen: &RefCell<Self>, event: TabletMonitorEvent) -> bool {
        let Ok(mut pen) = pen.try_borrow_mut() else {
            return false;
        };
        if pen.region_runtime.try_borrow_mut().is_err() {
            return false;
        }
        pen.on_monitor_event(event);
        true
    }

    /// Processes events the monitor had to queue, oldest first.
    pub(crate) fn on_monitor_events(&mut self, events: Vec<TabletMonitorEvent>) {
        for event in events {
            self.on_monitor_event(event);
        }
    }

    pub(crate) fn on_monitor_event(&mut self, event: TabletMonitorEvent) {
        self.on_monitor_event_at(event, now_epoch_ns());
    }

    /// As [`Self::on_monitor_event`], with the current wall-clock time given.
    pub(crate) fn on_monitor_event_at(&mut self, event: TabletMonitorEvent, now_epoch_ns: u64) {
        match event {
            TabletMonitorEvent::InputEdge => {
                if self.route.root_view.is_some() {
                    self.barrier = true;
                }
            }
            TabletMonitorEvent::Sample {
                sample,
                dispatch_age,
            } => {
                let age_ns = u64::try_from(dispatch_age.as_nanos()).unwrap_or(u64::MAX);
                let timestamp_ns = self.next_timestamp(now_epoch_ns.saturating_sub(age_ns));
                if self.route.root_view.is_none() {
                    self.probe.observe(sample);
                    return;
                }
                if self.barrier {
                    if self.held.len() >= HOLD_CAPACITY {
                        let status = self.reset_authority(
                            "pen samples overflowed while waiting for keyboard input; \
                             pen input reset to mouse emulation"
                                .to_string(),
                        );
                        self.pending_status = Some(status);
                        return;
                    }
                    self.held.push_back((sample, timestamp_ns));
                    self.stats.held = self.stats.held.saturating_add(1);
                } else {
                    self.process_sample(sample, timestamp_ns);
                }
            }
        }
    }

    /// Starts a UI pass. A route the previous pass did not republish (no
    /// session view drawn) is dropped, and any barrier it left is released.
    pub(crate) fn begin_pass(&mut self) {
        if !self.route_fresh {
            self.route.root_view = None;
            self.release_barrier();
        }
        self.route_fresh = false;
    }

    pub(crate) fn publish_route(&mut self, route: PenRoute) {
        self.route = route;
        self.route_fresh = true;
    }

    pub(crate) fn publish_link(&mut self, link: PenLink) {
        self.link = link;
    }

    /// The UI pass has sent its keyboard and pointer-button input: release
    /// pen samples held behind it, in order.
    pub(crate) fn ui_pass_completed(&mut self) {
        self.release_barrier();
    }

    fn release_barrier(&mut self) {
        self.barrier = false;
        while let Some((sample, timestamp_ns)) = self.held.pop_front() {
            self.process_sample(sample, timestamp_ns);
        }
    }

    /// Whether the pen owns input, counting samples still held behind an
    /// ordering barrier. AppKit's mouse-emulation duplicates of a held pen
    /// stroke reach egui in the same pass, before the held samples are sent;
    /// they must already be suppressed then, or the host would get a mouse
    /// press whose release the pen path later swallows.
    pub(crate) fn holds_authority(&self) -> bool {
        self.dispatcher.in_proximity()
            || self.held.iter().any(|(sample, _)| match sample {
                RawTabletSample::Point(point) => point.tool.is_pen_or_eraser(),
                RawTabletSample::Proximity(proximity) => {
                    proximity.entering && proximity.tool.is_pen_or_eraser()
                }
            })
    }

    #[cfg(test)]
    pub(crate) fn has_held_samples(&self) -> bool {
        !self.held.is_empty()
    }

    /// Status-line message from a reset that happened outside a UI pass.
    pub(crate) fn take_pending_status(&mut self) -> Option<String> {
        self.pending_status.take()
    }

    /// Whether pen input arrived since the last call.
    pub(crate) fn take_activity(&mut self) -> bool {
        std::mem::take(&mut self.activity)
    }

    pub(crate) fn take_last_input_sent(&mut self) -> Option<(&'static str, Instant)> {
        self.last_input_sent.take()
    }

    pub(crate) fn take_forward_stats(&mut self) -> PenForwardSummary {
        self.stats.take()
    }

    fn next_sequence(&self) -> u64 {
        let next = self.sequence.get().saturating_add(1);
        self.sequence.set(next);
        next
    }

    fn next_timestamp(&mut self, candidate_ns: u64) -> u64 {
        let timestamp = candidate_ns.max(self.last_timestamp_ns.saturating_add(1));
        self.last_timestamp_ns = timestamp;
        timestamp
    }

    fn uses_region_wire(&self) -> bool {
        self.region_runtime
            .borrow()
            .as_ref()
            .is_some_and(DeckRegionRuntime::uses_region_wire)
    }

    fn send_allowed(&self) -> bool {
        matches!(
            self.link.tablet_mode_active,
            TabletModeMsg::LocalTermination
        ) && self.link.pen_negotiated
    }

    fn process_sample(&mut self, raw: RawTabletSample, timestamp_ns: u64) {
        self.probe.observe(raw);
        self.activity = true;
        let Some(root_view) = self.route.root_view else {
            return;
        };
        let target = if tablet_root_fallback_allowed(self.route.multi_window_active) {
            Some((root_view, None))
        } else {
            resolve_tablet_sample_target(
                raw.window_number(),
                self.route.root_window_number,
                root_view,
                &self.route.secondary_targets,
            )
        };
        let Some((view, monitor_id)) = target else {
            tracing::warn!(
                target: crate::logging::target::INPUT,
                "dropped native tablet sample whose window matched neither root nor any committed secondary viewport",
            );
            return;
        };
        if !tablet_sample_forward_allowed(monitor_id, self.route.multi_window_committed) {
            tracing::debug!(
                target: crate::logging::target::INPUT,
                "dropped native tablet sample resolved to a secondary viewport that has not yet committed",
            );
            return;
        }
        let metadata = LowLatencyMetadata {
            sequence: self.next_sequence(),
            timestamp_ns,
            coalescable: true,
        };
        let mapped = match raw {
            RawTabletSample::Point(point) => self.mapper.on_point(point, view, metadata),
            RawTabletSample::Proximity(proximity) => {
                self.mapper.on_proximity(proximity, view, metadata)
            }
        };
        let Some(mut event) = mapped else {
            tracing::debug!(
                target: crate::logging::target::INPUT,
                "native tablet sample produced no valid pen event after mapping"
            );
            return;
        };
        if self.route.left_handed {
            event.x = 1.0 - event.x;
            event.tilt_x_degrees = -event.tilt_x_degrees;
        }
        let region_viewport = match monitor_id {
            Some(monitor_id) => self
                .route
                .monitor_viewports
                .iter()
                .find(|(candidate, _)| *candidate == monitor_id)
                .map(|(_, viewport)| *viewport),
            None => self.route.primary_viewport,
        };
        let event = self.dispatcher.forward(event);
        self.last_region_viewport = region_viewport;
        self.last_event = Some(event);
        if self.link.tablet_mode_active == TabletModeMsg::UsbTabletBridge {
            if !self.send_hard_usb(event) {
                self.last_error = Some("Hard USB pen state failed to reach transport".to_string());
            }
        } else if self.send_allowed() {
            self.send_routed(event, region_viewport);
        } else {
            tracing::trace!(
                target: crate::logging::target::INPUT,
                tablet_mode_active = ?self.link.tablet_mode_active,
                pen_negotiated = self.link.pen_negotiated,
                "pen sample mapped locally but NOT sent (gate closed)",
            );
        }
        if !event.metadata.coalescable {
            if let Some(wake) = &self.edge_wake {
                wake();
            }
        }
    }

    pub(crate) fn send_routed(&mut self, event: PenEvent, region_viewport: Option<RegionViewport>) {
        if let Some(region_viewport) = region_viewport {
            self.send_region(event, region_viewport);
        } else if self.uses_region_wire() {
            tracing::error!(
                target: crate::logging::target::INPUT,
                sequence = event.metadata.sequence,
                "region pen event dropped because no authoritative viewport was resolved",
            );
            self.last_error = Some("region pen event had no authoritative viewport".to_string());
        } else {
            self.send_legacy(event);
        }
    }

    fn send_region(&mut self, event: PenEvent, viewport: RegionViewport) {
        let message = {
            let mut runtime = self.region_runtime.borrow_mut();
            let Some(runtime) = runtime.as_mut() else {
                return;
            };
            runtime.pen(viewport, event)
        };
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                self.last_error = Some(format!("region pen event rejected: {error}"));
                tracing::warn!(
                    target: crate::logging::target::INPUT,
                    %error,
                    "region pen event was rejected",
                );
                return;
            }
        };
        let outgoing = if self.uses_region_wire() {
            if self.link.region_input_confirmed {
                match message.to_json_value() {
                    Ok(value) => Some((value, message.input_type())),
                    Err(error) => {
                        tracing::error!(
                            target: crate::logging::target::INPUT,
                            %error,
                            input_type = message.input_type(),
                            "validated region input could not be serialized",
                        );
                        None
                    }
                }
            } else {
                tracing::error!(
                    target: crate::logging::target::INPUT,
                    "region input send blocked because input-v4 capability was not negotiated",
                );
                None
            }
        } else {
            let adapted = self
                .region_runtime
                .borrow()
                .as_ref()
                .map(|runtime| runtime.adapt_legacy(&message, PointerMotionMode::Absolute));
            match adapted {
                Some(Ok(Some(legacy))) => Some((legacy.value, legacy.input_type)),
                Some(Err(error)) => {
                    tracing::error!(
                        target: crate::logging::target::INPUT,
                        %error,
                        "accepted region input could not be adapted to the temporary legacy wire",
                    );
                    None
                }
                Some(Ok(None)) | None => None,
            }
        };
        let sent = outgoing.is_some_and(|(value, input_type)| self.send_json(value, input_type));
        if sent {
            tracing::debug!(
                target: crate::logging::target::INPUT,
                sequence = event.metadata.sequence,
                region_id = viewport.region_id().get(),
                tool = ?event.tool,
                in_proximity = event.in_proximity,
                touching = event.touching,
                buttons = event.buttons,
                pressure = event.pressure,
                "region pen event forwarded",
            );
            self.note_sent(event);
        } else {
            tracing::warn!(
                target: crate::logging::target::INPUT,
                sequence = event.metadata.sequence,
                region_id = viewport.region_id().get(),
                "region pen event failed to enqueue on session transport",
            );
            self.last_error = Some("pen event failed to enqueue on session transport".to_string());
        }
    }

    fn send_legacy(&mut self, event: PenEvent) {
        let message = self.pen_event_msg(&event);
        if message.validate().is_err() {
            // `TabletMapper` and the dispatcher validate every event first;
            // fail closed rather than send a malformed sample.
            let status = self.reset_authority("pen event failed wire validation".to_string());
            self.pending_status = Some(status);
            return;
        }
        let value = serde_json::to_value(&message).expect("PenEventMsg must serialize to JSON");
        if self.send_json(value, "pen_event") {
            tracing::debug!(
                target: crate::logging::target::INPUT,
                sequence = message.sequence,
                tool = ?message.tool,
                in_proximity = message.in_proximity,
                touching = message.touching,
                buttons = message.buttons,
                pressure = message.pressure,
                "pen event forwarded to host transport",
            );
            self.note_sent(event);
        } else {
            tracing::warn!(
                target: crate::logging::target::INPUT,
                sequence = message.sequence,
                tool = ?message.tool,
                in_proximity = message.in_proximity,
                touching = message.touching,
                buttons = message.buttons,
                "pen event failed to enqueue on session transport",
            );
            self.last_error = Some("pen event failed to enqueue on session transport".to_string());
        }
    }

    fn note_sent(&mut self, event: PenEvent) {
        self.last_event = Some(event);
        self.events_sent = self.events_sent.saturating_add(1);
        self.last_error = None;
        self.stats.record(Instant::now());
    }

    fn send_json(&mut self, value: serde_json::Value, input_type: &'static str) -> bool {
        let started = Instant::now();
        let Some(commands) = &self.link.commands else {
            return false;
        };
        let sent = commands.send(SessionCommand::Json(value)).is_ok();
        if sent {
            if let Some(telemetry) = &self.link.telemetry {
                telemetry.record_input_send(started.elapsed());
            }
            self.last_input_sent = Some((input_type, Instant::now()));
        }
        sent
    }

    fn send_hard_usb(&self, event: PenEvent) -> bool {
        self.link
            .commands
            .as_ref()
            .is_some_and(|commands| commands.send(SessionCommand::HardUsbPen(event)).is_ok())
    }

    /// The legacy single-monitor wire message for one pen event, mapping the
    /// server coordinate exactly like mouse input does (same
    /// `remote_frame_size` scaling, same `-1` "unmapped" sentinel).
    pub(crate) fn pen_event_msg(&self, event: &PenEvent) -> PenEventMsg {
        pen_event_msg(event, self.link.remote_frame_size)
    }

    /// Emits a final synthetic out-of-proximity sample if the dispatcher
    /// still believes the tool is in proximity, so a host is never left with
    /// a pen held down. Reuses the region of the latest real sample.
    pub(crate) fn emit_final_release_if_in_proximity(&mut self) {
        if !self.dispatcher.in_proximity() {
            return;
        }
        let metadata = LowLatencyMetadata {
            sequence: self.next_sequence(),
            timestamp_ns: self.next_timestamp(now_epoch_ns()),
            coalescable: false,
        };
        let region_viewport = self.last_region_viewport;
        if let Some(release) = self.dispatcher.release_event(metadata) {
            if self.send_allowed() {
                self.send_routed(release, region_viewport);
            } else {
                self.last_event = Some(release);
            }
        }
        self.last_region_viewport = None;
    }

    /// Fail-closed reset: releases the pen if needed, forgets dispatcher and
    /// mapper state, counts it, and returns the status-line message.
    pub(crate) fn reset_authority(&mut self, message: String) -> String {
        self.held.clear();
        self.barrier = false;
        self.emit_final_release_if_in_proximity();
        self.dispatcher.reset();
        self.mapper = TabletMapper::new();
        self.overflow_resets = self.overflow_resets.saturating_add(1);
        tracing::error!(
            target: crate::logging::target::INPUT,
            %message,
            "tablet authority reset",
        );
        self.last_error = Some(message.clone());
        format!("Tablet input reset: {message}")
    }

    /// Releases pen authority for an abnormal end (focus loss, setting off,
    /// disconnect). Returns whether live authority actually ended.
    pub(crate) fn release_authority(&mut self, reason: &'static str) -> bool {
        let had_authority = self.dispatcher.in_proximity();
        self.held.clear();
        self.barrier = false;
        self.emit_final_release_if_in_proximity();
        self.dispatcher.reset();
        self.mapper = TabletMapper::new();
        if had_authority {
            tracing::debug!(
                target: crate::logging::target::INPUT,
                reason,
                "tablet pen authority released",
            );
        }
        had_authority
    }

    /// Starts a new session's pen state; capabilities seen in an earlier
    /// session do not carry over.
    pub(crate) fn reset_for_session(&mut self) {
        self.held.clear();
        self.barrier = false;
        self.dispatcher.reset();
        self.mapper = TabletMapper::new();
        self.probe = TabletCapabilityProbe::default();
    }
}

pub(crate) fn pen_event_msg(
    event: &PenEvent,
    remote_frame_size: Option<[usize; 2]>,
) -> PenEventMsg {
    let (server_x, server_y) = remote_frame_size
        .map(|[width, height]| {
            (
                (event.x * (width.saturating_sub(1)) as f64).round() as i32,
                (event.y * (height.saturating_sub(1)) as f64).round() as i32,
            )
        })
        .unwrap_or((-1, -1));
    PenEventMsg {
        x: event.x,
        y: event.y,
        server_x,
        server_y,
        pressure: event.pressure,
        tilt_x_degrees: event.tilt_x_degrees,
        tilt_y_degrees: event.tilt_y_degrees,
        rotation_degrees: event.rotation_degrees,
        tool: match event.tool {
            PenTool::Tip => PenToolMsg::Tip,
            PenTool::Eraser => PenToolMsg::Eraser,
        },
        in_proximity: event.in_proximity,
        touching: event.touching,
        buttons: event.buttons,
        sequence: event.metadata.sequence,
        timestamp_ns: event.metadata.timestamp_ns,
        coalescable: event.metadata.coalescable,
        ..PenEventMsg::default()
    }
}

pub(crate) fn now_epoch_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Resolves which viewport's own `ViewSize` (and, for a secondary viewport,
/// which monitor) a native tablet sample's carried `window_number` belongs
/// to: root first, then a committed secondary. `None` when neither matches —
/// a sample from a window this pass does not recognise (a just-closed
/// viewport, or one racing ahead of `secondary_targets`) is dropped rather
/// than misrouted through root's rect.
#[must_use]
pub(crate) fn resolve_tablet_sample_target(
    sample_window_number: isize,
    root_window_number: Option<isize>,
    root_view: ViewSize,
    secondary_targets: &[(isize, SessionMonitorId, ViewSize)],
) -> Option<(ViewSize, Option<SessionMonitorId>)> {
    if root_window_number == Some(sample_window_number) {
        return Some((root_view, None));
    }
    secondary_targets
        .iter()
        .find(|(window_number, _, _)| *window_number == sample_window_number)
        .map(|&(_, monitor_id, view)| (view, Some(monitor_id)))
}

/// Whether a sample may skip window-number resolution and be treated as
/// root's own: only when no multi-window attempt exists at all, so exactly
/// one native window can exist. The instant an attempt begins (pending or
/// committed) every sample must be positively resolved, even before
/// `secondary_targets` has its first entry.
#[must_use]
pub(crate) const fn tablet_root_fallback_allowed(multi_window_attempt_active: bool) -> bool {
    !multi_window_attempt_active
}

/// Whether a sample resolved to `monitor_id` may be forwarded given whether
/// the owning multi-window transaction has committed. Root's own samples
/// are never gated; a secondary viewport's are dropped until commit, the
/// same "input paused until commit" rule secondary mouse and keyboard input
/// follow.
#[must_use]
pub(crate) const fn tablet_sample_forward_allowed(
    monitor_id: Option<SessionMonitorId>,
    multi_window_committed: bool,
) -> bool {
    monitor_id.is_none() || multi_window_committed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tablet::{
        NativeTabletPoint, NativeTabletProximity, NativeTabletTool, TabletButtonMask,
    };
    use std::time::Duration;
    use tokio::sync::mpsc;

    const ROOT_WINDOW: isize = 1;
    const WIDTH: f64 = 1000.0;
    const HEIGHT: f64 = 500.0;
    const SAMPLE_PERIOD_NS: u64 = 5_000_000;
    const START_NS: u64 = 1_700_000_000_000_000_000;

    fn point(x: f64, pressure: f32, tip: bool, window_number: isize) -> RawTabletSample {
        RawTabletSample::Point(NativeTabletPoint {
            window_x: x,
            window_y: HEIGHT / 2.0,
            pressure,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation_degrees: 0.0,
            buttons: TabletButtonMask::new(if tip { TabletButtonMask::TIP } else { 0 }),
            device_id: 1,
            tool: NativeTabletTool::Pen,
            window_number,
        })
    }

    fn proximity(entering: bool) -> RawTabletSample {
        RawTabletSample::Proximity(NativeTabletProximity {
            window_x: 10.0,
            window_y: HEIGHT / 2.0,
            entering,
            tool: NativeTabletTool::Pen,
            vendor_id: 0,
            tablet_id: 0,
            pointing_device_id: 0,
            system_tablet_id: 0,
            vendor_pointing_device_type: 0,
            unique_id: 0,
            capability_mask: 0,
            device_id: 1,
            window_number: ROOT_WINDOW,
        })
    }

    fn sample(raw: RawTabletSample) -> TabletMonitorEvent {
        TabletMonitorEvent::Sample {
            sample: raw,
            dispatch_age: Duration::ZERO,
        }
    }

    fn route() -> PenRoute {
        PenRoute {
            root_view: Some(ViewSize::new(WIDTH, HEIGHT)),
            root_window_number: Some(ROOT_WINDOW),
            ..PenRoute::default()
        }
    }

    fn pen_with_channel() -> (PenInput, mpsc::UnboundedReceiver<SessionCommand>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut pen = PenInput::new(Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
        pen.publish_link(PenLink {
            commands: Some(SessionCommandSender::for_test(sender)),
            remote_frame_size: Some([1920, 1080]),
            tablet_mode_active: TabletModeMsg::LocalTermination,
            pen_negotiated: true,
            ..PenLink::default()
        });
        pen.begin_pass();
        pen.publish_route(route());
        (pen, receiver)
    }

    fn pen_messages(receiver: &mut mpsc::UnboundedReceiver<SessionCommand>) -> Vec<PenEventMsg> {
        let mut messages = Vec::new();
        while let Ok(command) = receiver.try_recv() {
            if let SessionCommand::Json(value) = command {
                messages.push(serde_json::from_value(value).expect("pen_event JSON"));
            }
        }
        messages
    }

    /// One second of a 200 Hz digitizer: proximity in, hover, a 161-sample
    /// stroke with varying pressure, hover, proximity out.
    fn digitizer_second() -> Vec<RawTabletSample> {
        (0..200u32)
            .map(|index| match index {
                0 => proximity(true),
                199 => proximity(false),
                20..=180 => point(
                    10.0 + f64::from(index) * 4.0,
                    0.1 + f32::from(u16::try_from(index - 20).unwrap()) * 0.005,
                    true,
                    ROOT_WINDOW,
                ),
                _ => point(10.0 + f64::from(index) * 4.0, 0.0, false, ROOT_WINDOW),
            })
            .collect()
    }

    /// Feeds [`digitizer_second`] at 200 Hz while the UI loop runs at
    /// `ui_hz`, and returns every forwarded message with the simulated time
    /// it left. Asserts each sample left in the same call it arrived.
    fn run_at_ui_rate(ui_hz: u64) -> Vec<(u64, PenEventMsg)> {
        let (mut pen, mut receiver) = pen_with_channel();
        let ui_period_ns = 1_000_000_000 / ui_hz;
        let mut next_pass_ns = START_NS + ui_period_ns;
        let mut forwarded = Vec::new();
        for (index, raw) in digitizer_second().into_iter().enumerate() {
            let now_ns = START_NS + u64::try_from(index).unwrap() * SAMPLE_PERIOD_NS;
            while next_pass_ns <= now_ns {
                pen.begin_pass();
                pen.publish_route(route());
                pen.ui_pass_completed();
                next_pass_ns += ui_period_ns;
            }
            pen.on_monitor_event_at(sample(raw), now_ns);
            let messages = pen_messages(&mut receiver);
            assert_eq!(
                messages.len(),
                1,
                "sample {index} must leave at once, not wait for a UI pass"
            );
            forwarded.push((now_ns, messages[0].clone()));
        }
        forwarded
    }

    #[test]
    fn pen_samples_at_200_hz_are_forwarded_at_200_hz_while_the_ui_runs_at_30_hz() {
        let forwarded = run_at_ui_rate(30);
        assert_eq!(forwarded.len(), 200);

        // Each message carries its own arrival time, 5 ms apart.
        for ((sent_ns, message), index) in forwarded.iter().zip(0u64..) {
            assert_eq!(message.timestamp_ns, *sent_ns);
            assert_eq!(message.timestamp_ns, START_NS + index * SAMPLE_PERIOD_NS);
        }
        let intervals: Vec<u64> = forwarded
            .windows(2)
            .map(|pair| pair[1].1.timestamp_ns - pair[0].1.timestamp_ns)
            .collect();
        assert!(intervals
            .iter()
            .all(|interval| *interval == SAMPLE_PERIOD_NS));

        // Strictly increasing sequence.
        assert!(forwarded
            .windows(2)
            .all(|pair| pair[1].1.sequence > pair[0].1.sequence));

        // Every contact sample keeps its pressure, in order.
        let pressures: Vec<f32> = forwarded
            .iter()
            .filter(|(_, message)| message.touching)
            .map(|(_, message)| message.pressure)
            .collect();
        assert_eq!(pressures.len(), 161);
        for (index, pressure) in pressures.iter().enumerate() {
            let expected = 0.1 + f32::from(u16::try_from(index).unwrap()) * 0.005;
            assert!((pressure - expected).abs() < 1e-4, "{index}: {pressure}");
        }

        // Edges appear exactly where the stroke changes state.
        let edges: Vec<usize> = forwarded
            .iter()
            .enumerate()
            .filter(|(_, (_, message))| !message.coalescable)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(edges, vec![0, 20, 181, 199]);
        assert!(!forwarded[199].1.in_proximity);
    }

    #[test]
    fn forwarding_does_not_depend_on_the_ui_rate() {
        let payload = |run: Vec<(u64, PenEventMsg)>| -> Vec<PenEventMsg> {
            run.into_iter().map(|(_, message)| message).collect()
        };
        let at_30 = payload(run_at_ui_rate(30));
        assert_eq!(payload(run_at_ui_rate(10)), at_30);
        assert_eq!(payload(run_at_ui_rate(60)), at_30);
        assert_eq!(payload(run_at_ui_rate(120)), at_30);
    }

    #[test]
    fn the_event_timestamp_is_the_dispatch_time_minus_its_age() {
        let (mut pen, mut receiver) = pen_with_channel();
        pen.on_monitor_event_at(
            TabletMonitorEvent::Sample {
                sample: proximity(true),
                dispatch_age: Duration::from_millis(30),
            },
            START_NS,
        );
        let messages = pen_messages(&mut receiver);
        assert_eq!(messages[0].timestamp_ns, START_NS - 30_000_000);
        // A later sample that waited less can never be stamped earlier.
        pen.on_monitor_event_at(
            TabletMonitorEvent::Sample {
                sample: point(20.0, 0.0, false, ROOT_WINDOW),
                dispatch_age: Duration::from_millis(40),
            },
            START_NS + 1_000_000,
        );
        let messages = pen_messages(&mut receiver);
        assert_eq!(messages[0].timestamp_ns, START_NS - 30_000_000 + 1);
    }

    #[test]
    fn samples_after_a_keyboard_edge_wait_for_the_ui_pass_and_keep_their_order() {
        let (mut pen, mut receiver) = pen_with_channel();
        pen.on_monitor_event_at(sample(proximity(true)), START_NS);
        assert_eq!(pen_messages(&mut receiver).len(), 1);

        pen.on_monitor_event_at(TabletMonitorEvent::InputEdge, START_NS + 1);
        for index in 0..6u64 {
            pen.on_monitor_event_at(
                sample(point(20.0 + index as f64, 0.5, true, ROOT_WINDOW)),
                START_NS + (index + 1) * SAMPLE_PERIOD_NS,
            );
        }
        assert!(
            pen_messages(&mut receiver).is_empty(),
            "pen samples must not overtake the key the UI pass has not sent yet"
        );
        assert!(pen.has_held_samples());

        // The UI pass sends the key, then releases the held stroke in order.
        pen.ui_pass_completed();
        let released = pen_messages(&mut receiver);
        assert_eq!(released.len(), 6);
        assert!(released
            .windows(2)
            .all(|pair| pair[1].sequence > pair[0].sequence));
        assert!(released
            .windows(2)
            .all(|pair| pair[1].timestamp_ns > pair[0].timestamp_ns));

        // Without a new edge nothing is held.
        pen.on_monitor_event_at(
            sample(point(40.0, 0.5, true, ROOT_WINDOW)),
            START_NS + 10 * SAMPLE_PERIOD_NS,
        );
        assert_eq!(pen_messages(&mut receiver).len(), 1);
    }

    #[test]
    fn a_pen_held_behind_a_barrier_already_holds_authority() {
        let (mut pen, mut receiver) = pen_with_channel();
        pen.on_monitor_event_at(TabletMonitorEvent::InputEdge, START_NS);
        assert!(!pen.holds_authority());
        pen.on_monitor_event_at(sample(proximity(true)), START_NS + 1);
        pen.on_monitor_event_at(sample(point(20.0, 0.5, true, ROOT_WINDOW)), START_NS + 2);
        assert!(pen_messages(&mut receiver).is_empty());
        assert!(!pen.dispatcher.in_proximity());
        assert!(
            pen.holds_authority(),
            "held pen samples must suppress mouse emulation before they are sent"
        );
        pen.ui_pass_completed();
        assert_eq!(pen_messages(&mut receiver).len(), 2);
        assert!(pen.holds_authority());

        // A held proximity-out alone does not claim authority.
        let (mut pen, _receiver) = pen_with_channel();
        pen.on_monitor_event_at(TabletMonitorEvent::InputEdge, START_NS);
        pen.on_monitor_event_at(sample(proximity(false)), START_NS + 1);
        assert!(!pen.holds_authority());
    }

    #[test]
    fn a_held_backlog_beyond_its_bound_fails_closed_with_a_release() {
        let (mut pen, mut receiver) = pen_with_channel();
        pen.on_monitor_event_at(sample(point(20.0, 0.5, true, ROOT_WINDOW)), START_NS);
        assert_eq!(pen_messages(&mut receiver).len(), 1);
        pen.on_monitor_event_at(TabletMonitorEvent::InputEdge, START_NS + 1);
        for index in 0..=HOLD_CAPACITY as u64 {
            pen.on_monitor_event_at(
                sample(point(21.0, 0.5, true, ROOT_WINDOW)),
                START_NS + 2 + index,
            );
        }
        assert_eq!(pen.overflow_resets, 1);
        assert!(pen.take_pending_status().is_some());
        assert!(!pen.has_held_samples());
        let released = pen_messages(&mut receiver);
        assert_eq!(released.len(), 1, "only the synthetic release is sent");
        assert!(!released[0].in_proximity && !released[0].touching);
    }

    #[test]
    fn a_busy_pen_state_leaves_events_to_the_queue_which_drains_them_in_order() {
        let (pen, mut receiver) = pen_with_channel();
        let pen = RefCell::new(pen);
        let mut queued = Vec::new();
        {
            let _busy = pen.borrow_mut();
            for raw in [proximity(true), point(30.0, 0.0, false, ROOT_WINDOW)] {
                let event = sample(raw);
                if !PenInput::offer(&pen, event) {
                    queued.push(event);
                }
            }
        }
        assert_eq!(queued.len(), 2);
        assert!(pen_messages(&mut receiver).is_empty());
        pen.borrow_mut().on_monitor_events(queued);
        let messages = pen_messages(&mut receiver);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].sequence < messages[1].sequence);
        assert!(!messages[0].coalescable && messages[1].coalescable);
        // Once free, the next event is taken directly.
        assert!(PenInput::offer(
            &pen,
            sample(point(31.0, 0.0, false, ROOT_WINDOW))
        ));
        assert_eq!(pen_messages(&mut receiver).len(), 1);
    }

    #[test]
    fn nothing_is_forwarded_once_the_session_view_is_no_longer_drawn() {
        let (mut pen, mut receiver) = pen_with_channel();
        pen.on_monitor_event_at(sample(proximity(true)), START_NS);
        assert_eq!(pen_messages(&mut receiver).len(), 1);
        // A pass that does not draw the session view drops the route.
        pen.begin_pass();
        pen.begin_pass();
        assert!(pen.route().root_view.is_none());
        pen.on_monitor_event_at(sample(point(30.0, 0.0, false, ROOT_WINDOW)), START_NS + 1);
        assert!(pen_messages(&mut receiver).is_empty());
    }

    fn multi_window_route(committed: bool, monitor_id: SessionMonitorId) -> PenRoute {
        PenRoute {
            multi_window_active: true,
            multi_window_committed: committed,
            secondary_targets: vec![(2, monitor_id, ViewSize::new(WIDTH * 2.0, HEIGHT))],
            ..route()
        }
    }

    #[test]
    fn multi_window_samples_route_by_window_and_wait_for_commit() {
        let monitor_id = SessionMonitorId::new(2).expect("valid id");
        let (mut pen, mut receiver) = pen_with_channel();

        // Uncommitted: root still works, the secondary is paused.
        pen.publish_route(multi_window_route(false, monitor_id));
        pen.on_monitor_event_at(sample(point(500.0, 0.0, false, ROOT_WINDOW)), START_NS);
        let root = pen_messages(&mut receiver);
        assert_eq!(root.len(), 1);
        assert!((root[0].x - 0.5).abs() < 1e-3);
        pen.on_monitor_event_at(sample(point(500.0, 0.0, false, 2)), START_NS + 1);
        assert!(pen_messages(&mut receiver).is_empty());

        // Committed: the secondary's own rect maps the sample.
        pen.publish_route(multi_window_route(true, monitor_id));
        pen.on_monitor_event_at(sample(point(500.0, 0.0, false, 2)), START_NS + 2);
        let secondary = pen_messages(&mut receiver);
        assert_eq!(secondary.len(), 1);
        assert!((secondary[0].x - 0.25).abs() < 1e-3);

        // A window that is neither root nor a known secondary is dropped.
        pen.on_monitor_event_at(sample(point(500.0, 0.0, false, 9)), START_NS + 3);
        assert!(pen_messages(&mut receiver).is_empty());
    }

    #[test]
    fn edges_wake_the_ui_and_motion_does_not() {
        let (mut pen, _receiver) = pen_with_channel();
        let wakes = Rc::new(Cell::new(0u32));
        let counter = Rc::clone(&wakes);
        pen.set_edge_wake(move || counter.set(counter.get() + 1));
        pen.on_monitor_event_at(sample(proximity(true)), START_NS);
        for index in 0..10u64 {
            pen.on_monitor_event_at(
                sample(point(20.0 + index as f64, 0.0, false, ROOT_WINDOW)),
                START_NS + 1 + index,
            );
        }
        pen.on_monitor_event_at(sample(point(40.0, 0.4, true, ROOT_WINDOW)), START_NS + 20);
        assert_eq!(wakes.get(), 2);
    }

    #[test]
    fn a_closed_gate_maps_locally_but_sends_nothing() {
        let (mut pen, mut receiver) = pen_with_channel();
        pen.publish_link(PenLink {
            pen_negotiated: false,
            ..pen.link.clone()
        });
        pen.on_monitor_event_at(sample(proximity(true)), START_NS);
        assert!(pen_messages(&mut receiver).is_empty());
        assert!(pen.last_event.is_some());
        assert!(pen.dispatcher.in_proximity());
    }
}
