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

use std::collections::BTreeMap;

use arcen_input::{
    InputSequenceTracker, KeyboardEvent, LowLatencyMetadata, ModifierMask, PointerButton,
    PointerMotion, PointerScroll, RegionInputPipeline, RegionInputPipelineError, RegionPointMapper,
};
use arcen_protocol::messages::{
    GESTURE_MAGNIFY, GESTURE_ROTATE, GESTURE_SMART_ZOOM, GESTURE_SWIPE, GestureMagnifyMsg,
    GestureRotateMsg, GestureSmartZoomMsg, GestureSwipeMsg, KEY_RESET_MODIFIERS, KeyEventMsg,
    MOUSE_SCROLL, MouseButtonMsg, MouseMoveMsg, MouseScrollMsg, PEN_EVENT, PenEventMsg,
    REGION_PEN_EVENT, REGION_POINTER_BUTTON, REGION_POINTER_ENTER, REGION_POINTER_LEAVE,
    REGION_POINTER_MOTION, REGION_POINTER_SCROLL, RegionPenEventMsg, RegionPointerButtonMsg,
    RegionPointerEnterMsg, RegionPointerLeaveMsg, RegionPointerMotionMsg, RegionPointerScrollMsg,
};

/// Wire names the shared crate does not expose as constants.
const MOUSE_MOVE: &str = "mouse_move";
const MOUSE_BUTTON: &str = "mouse_button";
const KEY_EVENT: &str = "key_event";
use serde::Serialize;

use crate::input::{DesktopBounds, InputController, InputError, NativePoint};

/// Which input coordinate contract this stream accepts.
#[derive(Debug, Clone)]
pub enum InputMode {
    /// Legacy normalized full-desktop input.
    Legacy(DesktopBounds),
    /// Region-scoped input from multi-monitor-v1.
    Region(RegionInputSession),
}

impl InputMode {
    /// Starts the selected input session.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] when the native injector cannot be created.
    pub fn start(self) -> Result<InputSession, InputError> {
        match self {
            Self::Legacy(bounds) => InputSession::new(bounds),
            Self::Region(region) => InputSession::new_region(region),
        }
    }
}

/// Region-scoped multi-monitor input contract for one stream.
#[derive(Debug, Clone)]
pub struct RegionInputSession {
    applied_regions: arcen_media::AppliedRegionSet,
    bounds_by_region: BTreeMap<u32, DesktopBounds>,
}

impl RegionInputSession {
    #[must_use]
    pub fn new(
        applied_regions: arcen_media::AppliedRegionSet,
        bounds: Vec<(arcen_media::SessionMonitorId, DesktopBounds)>,
    ) -> Self {
        let bounds_by_region = bounds
            .into_iter()
            .map(|(monitor_id, bounds)| (u32::from(monitor_id.get()), bounds))
            .collect();
        Self {
            applied_regions,
            bounds_by_region,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct MacRegionPointMapper {
    regions: [Option<MacRegionNativeMap>; arcen_media::MAX_MULTI_MONITOR_COUNT],
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct MacRegionNativeMap {
    left: i64,
    top: i64,
    width: u32,
    height: u32,
    bounds: DesktopBounds,
}

impl MacRegionPointMapper {
    fn new(
        applied_regions: &arcen_media::AppliedRegionSet,
        bounds_by_region: &BTreeMap<u32, DesktopBounds>,
    ) -> Self {
        let mut regions = [None; arcen_media::MAX_MULTI_MONITOR_COUNT];
        for (index, region) in applied_regions.regions().iter().enumerate() {
            let rect = region.applied_rect();
            let origin = rect.origin();
            let size = rect.size();
            regions[index] = bounds_by_region
                .get(&region.id().get())
                .copied()
                .map(|bounds| MacRegionNativeMap {
                    left: origin.x,
                    top: origin.y,
                    width: size.width(),
                    height: size.height(),
                    bounds,
                });
        }
        Self { regions }
    }
}

impl RegionPointMapper for MacRegionPointMapper {
    type Point = NativePoint;
    type Error = RegionMappingError;

    fn map_applied(
        &self,
        point: arcen_media::AppliedPoint,
    ) -> Result<NativePoint, RegionMappingError> {
        for region in self.regions.into_iter().flatten() {
            let right = region.left + i64::from(region.width);
            let bottom = region.top + i64::from(region.height);
            if point.x >= region.left
                && point.y >= region.top
                && point.x < right
                && point.y < bottom
            {
                let local_x = (point.x - region.left) as f64;
                let local_y = (point.y - region.top) as f64;
                return Ok(NativePoint {
                    x: region.bounds.origin_x
                        + local_x * region.bounds.width / f64::from(region.width),
                    y: region.bounds.origin_y
                        + local_y * region.bounds.height / f64::from(region.height),
                });
            }
        }
        Err(RegionMappingError::PointOutsideDisplays(point))
    }
}

/// macOS-specific mapping failure for one region input point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionMappingError {
    PointOutsideDisplays(arcen_media::AppliedPoint),
}

impl std::fmt::Display for RegionMappingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PointOutsideDisplays(point) => {
                write!(
                    formatter,
                    "mapped region point {point:?} is outside the virtual displays"
                )
            }
        }
    }
}

impl std::error::Error for RegionMappingError {}

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
    region: Option<RegionInputPipeline<MacRegionPointMapper>>,
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
            region: None,
            stats: InputStats::default(),
        })
    }

    /// Creates a session accepting region-scoped multi-monitor input.
    ///
    /// # Errors
    ///
    /// Returns [`InputError`] when no event source is available.
    pub fn new_region(region: RegionInputSession) -> Result<Self, InputError> {
        let bounds = region
            .bounds_by_region
            .values()
            .next()
            .copied()
            .ok_or(InputError::NoDesktopBounds)?;
        Ok(Self {
            controller: InputController::new(bounds)?,
            sequence: InputSequenceTracker::default(),
            region: Some(RegionInputPipeline::new(
                region.applied_regions.clone(),
                MacRegionPointMapper::new(&region.applied_regions, &region.bounds_by_region),
            )),
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
            REGION_POINTER_ENTER
            | REGION_POINTER_LEAVE
            | REGION_POINTER_MOTION
            | REGION_POINTER_BUTTON
            | REGION_POINTER_SCROLL
            | REGION_PEN_EVENT => self.apply_region(kind, &value),
            MOUSE_MOVE => self.apply_move(&value),
            MOUSE_BUTTON => self.apply_button(&value),
            MOUSE_SCROLL => self.apply_scroll(&value),
            KEY_EVENT => self.apply_key(&value),
            PEN_EVENT => self.apply_pen(&value),
            GESTURE_MAGNIFY => self.apply_gesture_magnify(&value),
            GESTURE_ROTATE => self.apply_gesture_rotate(&value),
            GESTURE_SMART_ZOOM => self.apply_gesture_smart_zoom(&value),
            GESTURE_SWIPE => self.apply_gesture_swipe(&value),
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

    fn apply_gesture_magnify(&mut self, value: &serde_json::Value) -> Result<(), InputError> {
        let Ok(message) = serde_json::from_value::<GestureMagnifyMsg>(value.clone()) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        if !self.accepts(message.sequence) {
            return Ok(());
        }
        self.controller.gesture_magnify(&message)?;
        self.stats.applied += 1;
        Ok(())
    }

    fn apply_gesture_rotate(&mut self, value: &serde_json::Value) -> Result<(), InputError> {
        let Ok(message) = serde_json::from_value::<GestureRotateMsg>(value.clone()) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        if !self.accepts(message.sequence) {
            return Ok(());
        }
        self.controller.gesture_rotate(&message)?;
        self.stats.applied += 1;
        Ok(())
    }

    fn apply_gesture_smart_zoom(&mut self, value: &serde_json::Value) -> Result<(), InputError> {
        let Ok(message) = serde_json::from_value::<GestureSmartZoomMsg>(value.clone()) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        if !self.accepts(message.sequence) {
            return Ok(());
        }
        self.controller.gesture_smart_zoom(&message)?;
        self.stats.applied += 1;
        Ok(())
    }

    fn apply_gesture_swipe(&mut self, value: &serde_json::Value) -> Result<(), InputError> {
        let Ok(message) = serde_json::from_value::<GestureSwipeMsg>(value.clone()) else {
            self.stats.malformed += 1;
            return Ok(());
        };
        if !self.accepts(message.sequence) {
            return Ok(());
        }
        self.controller.gesture_swipe(&message)?;
        self.stats.applied += 1;
        Ok(())
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

    fn apply_region(&mut self, kind: &str, value: &serde_json::Value) -> Result<(), InputError> {
        let Some(pipeline) = self.region.as_mut() else {
            self.stats.unsupported += 1;
            return Ok(());
        };
        let result = match kind {
            REGION_POINTER_ENTER => {
                let Ok(message) = serde_json::from_value::<RegionPointerEnterMsg>(value.clone())
                else {
                    self.stats.malformed += 1;
                    return Ok(());
                };
                pipeline
                    .pointer_enter(&message)
                    .map(|point| RegionAction::Motion(point))
            }
            REGION_POINTER_LEAVE => {
                let Ok(message) = serde_json::from_value::<RegionPointerLeaveMsg>(value.clone())
                else {
                    self.stats.malformed += 1;
                    return Ok(());
                };
                pipeline
                    .pointer_leave(&message)
                    .map(|point| RegionAction::Motion(point))
            }
            REGION_POINTER_MOTION => {
                let Ok(message) = serde_json::from_value::<RegionPointerMotionMsg>(value.clone())
                else {
                    self.stats.malformed += 1;
                    return Ok(());
                };
                pipeline
                    .pointer_motion(&message)
                    .map(|point| RegionAction::Motion(point))
            }
            REGION_POINTER_BUTTON => {
                let Ok(message) = serde_json::from_value::<RegionPointerButtonMsg>(value.clone())
                else {
                    self.stats.malformed += 1;
                    return Ok(());
                };
                pipeline.pointer_button(&message).map(RegionAction::Button)
            }
            REGION_POINTER_SCROLL => {
                let Ok(message) = serde_json::from_value::<RegionPointerScrollMsg>(value.clone())
                else {
                    self.stats.malformed += 1;
                    return Ok(());
                };
                pipeline.pointer_scroll(&message).map(RegionAction::Scroll)
            }
            REGION_PEN_EVENT => {
                let Ok(message) = serde_json::from_value::<RegionPenEventMsg>(value.clone()) else {
                    self.stats.malformed += 1;
                    return Ok(());
                };
                pipeline.pen(&message).map(RegionAction::Pen)
            }
            _ => unreachable!("region kind was prefiltered"),
        };
        match result {
            Ok(action) => self.inject_region_action(action),
            Err(RegionInputPipelineError::State(_)) => {
                self.stats.out_of_order += 1;
                Ok(())
            }
            Err(RegionInputPipelineError::Wire(_) | RegionInputPipelineError::Contract(_)) => {
                self.stats.malformed += 1;
                Ok(())
            }
            Err(RegionInputPipelineError::Transform(_) | RegionInputPipelineError::Mapping(_)) => {
                self.stats.malformed += 1;
                Ok(())
            }
        }
    }

    fn inject_region_action(&mut self, action: RegionAction) -> Result<(), InputError> {
        match action {
            RegionAction::Motion(point) => {
                self.controller.post_native_motion(point)?;
                self.stats.applied += 1;
            }
            RegionAction::Button(button) => {
                self.controller.native_pointer_button(
                    button.button,
                    button.pressed,
                    button.position,
                )?;
                self.stats.applied += 1;
            }
            RegionAction::Scroll(scroll) => {
                let denom = arcen_media::LOGICAL_UNITS_PER_PIXEL as f64;
                self.controller.native_pointer_scroll(
                    scroll.position,
                    scroll.delta_x as f64 / denom,
                    scroll.delta_y as f64 / denom,
                    scroll.unit,
                    scroll.phase,
                )?;
                self.stats.applied += 1;
            }
            RegionAction::Pen(pen) => {
                let message = PenEventMsg {
                    x: 0.0,
                    y: 0.0,
                    pressure: pen.sample.pressure,
                    tilt_x_degrees: pen.sample.tilt_x_degrees,
                    tilt_y_degrees: pen.sample.tilt_y_degrees,
                    rotation_degrees: pen.sample.rotation_degrees,
                    tool: arcen_input::wire_pen_tool(pen.sample.tool),
                    in_proximity: pen.sample.in_proximity,
                    touching: pen.sample.touching,
                    buttons: pen.sample.buttons,
                    sequence: 0,
                    ..PenEventMsg::default()
                };
                self.controller.native_pen_event(&message, pen.position)?;
                self.stats.applied += 1;
            }
        }
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

enum RegionAction {
    Motion(NativePoint),
    Button(arcen_input::MappedRegionButton<NativePoint>),
    Scroll(arcen_input::MappedRegionScroll<NativePoint>),
    Pen(arcen_input::MappedRegionPen<NativePoint>),
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

    fn applied_regions_for_mapping() -> arcen_media::AppliedRegionSet {
        let generation = arcen_media::RegionGeneration::new(1).expect("generation");
        let descriptor = |id: u32, primary: bool| {
            arcen_media::RegionDescriptor::new(
                arcen_media::RegionId::new(id).expect("region id"),
                arcen_media::OutputIdentity::new(format!("display-{id}")).expect("identity"),
                arcen_media::LogicalRect::new(
                    arcen_media::LogicalPoint::from_pixels(0, 0).expect("origin"),
                    arcen_media::LogicalSize::from_pixels(1920, 1080).expect("size"),
                )
                .expect("logical rect"),
                arcen_media::PhysicalSize::new(1920, 1080).expect("physical"),
                arcen_media::Scale120::new(120).expect("scale"),
                arcen_media::OutputTransform::Normal,
                primary,
            )
        };
        let first = descriptor(1, true);
        let second = descriptor(2, false);
        arcen_media::AppliedRegionSet::new(
            generation,
            vec![
                arcen_media::AppliedRegionDescriptor::new(
                    first,
                    arcen_media::AppliedRect::new(
                        arcen_media::AppliedPoint::new(0, 0),
                        arcen_media::AppliedSize::new(1920, 1080).expect("size"),
                    )
                    .expect("rect"),
                )
                .expect("first"),
                arcen_media::AppliedRegionDescriptor::new(
                    second,
                    arcen_media::AppliedRect::new(
                        arcen_media::AppliedPoint::new(1920, 0),
                        arcen_media::AppliedSize::new(1920, 1080).expect("size"),
                    )
                    .expect("rect"),
                )
                .expect("second"),
            ],
        )
        .expect("applied regions")
    }

    #[test]
    fn region_coordinate_mapping_targets_the_matching_global_display() {
        let regions = applied_regions_for_mapping();
        let mut bounds = BTreeMap::new();
        bounds.insert(1, DesktopBounds::new(-1920.0, 0.0, 1920.0, 1080.0));
        bounds.insert(2, DesktopBounds::new(0.0, 0.0, 1920.0, 1080.0));
        let mapper = MacRegionPointMapper::new(&regions, &bounds);

        let left = mapper
            .map_applied(arcen_media::AppliedPoint::new(100, 50))
            .expect("left display");
        let right = mapper
            .map_applied(arcen_media::AppliedPoint::new(2020, 50))
            .expect("right display");

        assert_eq!(
            left,
            NativePoint {
                x: -1820.0,
                y: 50.0
            }
        );
        assert_eq!(right, NativePoint { x: 100.0, y: 50.0 });
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
