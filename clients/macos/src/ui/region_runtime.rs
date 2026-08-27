//! Region-authoritative viewport and input runtime for Deck.
//!
//! Native windows only contribute a region identity plus a local normalized
//! position. This module turns that into the shared fixed-point
//! [`arcen_input::RegionLogicalPosition`] and hands it to the shared
//! [`arcen_input::RegionInputEmitter`], which owns the ordered
//! [`arcen_input::RegionInputState`], derives every enter/leave/motion
//! transition, and encodes the validated input-v4
//! [`arcen_input::RegionInputWireMessage`]. Deck holds no wire encoding or
//! input state of its own. [`TemporaryLegacyRegionInputAdapter`] is retained
//! only for non-region, single-monitor compatibility sessions.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_input::{
    PenEvent, RegionCoordinateTransformer, RegionInputEmitError, RegionInputEmitter,
    RegionInputState, RegionInputStateError, RegionInputWireMessage, RegionLogicalPosition,
    RegionPenSample,
};
use arcen_media::{
    AppliedPoint, AppliedRect, AppliedRegionSet, AppliedSize, LogicalPoint, LogicalRect,
    LogicalSize, OutputIdentity, PhysicalSize, RegionContractError, RegionGeneration, RegionId,
    RegionPlacement, RegionSet, RequestedMonitorTopology, Rotation, Scale120, SessionMonitorId,
    TransformConvention,
};

use crate::protocol::messages::{
    MouseButtonMsg, MouseMoveMsg, MouseScrollMsg, PenEventMsg, PointerMotionMode,
    RegionInputPositionMsg, RegionInputValidationError,
};
use crate::ui::multi_window_session::ValidatedAppliedTopology;

const LEGACY_REGION_GENERATION: u64 = 1;
const LEGACY_PRIMARY_REGION_ID: u32 = 1;

/// Explicit shared transform convention for Deck: the negotiated
/// multi-monitor-v1 stream arrives already compositor oriented, so its
/// width/height *are* the transform-normal footprint and informational legacy
/// panel-rotation metadata must never rotate it a second time.
const TRANSFORM_CONVENTION: TransformConvention = TransformConvention::AlreadyCompositorOriented;

/// One native window's identity in the shared region aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionViewport {
    region_id: RegionId,
    session_monitor_id: Option<SessionMonitorId>,
}

impl RegionViewport {
    #[must_use]
    pub const fn region_id(self) -> RegionId {
        self.region_id
    }

    #[must_use]
    pub const fn session_monitor_id(self) -> Option<SessionMonitorId> {
        self.session_monitor_id
    }
}

/// One adapted legacy message plus the state Deck mirrors for diagnostics and
/// keyboard positional context.
#[derive(Debug, Clone)]
pub struct LegacyRegionWireMessage {
    pub value: serde_json::Value,
    pub input_type: &'static str,
    pub pointer_position: Option<(f64, f64, i32, i32)>,
    pub button_state: Option<(u8, bool)>,
}

/// Explicit, temporary bridge for non-region single-monitor sessions.
///
/// The shared region logical position remains authoritative. This adapter is
/// the only code allowed to derive legacy normalized/desktop pixels.
#[derive(Debug, Clone, Copy)]
pub struct TemporaryLegacyRegionInputAdapter {
    desktop_origin: AppliedPoint,
    desktop_size: AppliedSize,
}

impl TemporaryLegacyRegionInputAdapter {
    fn new(regions: &AppliedRegionSet) -> Result<Self, DeckRegionRuntimeError> {
        let mut iter = regions.regions().iter();
        let first = iter
            .next()
            .ok_or(DeckRegionRuntimeError::EmptyAppliedRegionSet)?;
        let first_rect = first.applied_rect();
        let mut min_x = first_rect.origin().x;
        let mut min_y = first_rect.origin().y;
        let mut max_x = checked_applied_end(first_rect.origin().x, first_rect.size().width())?;
        let mut max_y = checked_applied_end(first_rect.origin().y, first_rect.size().height())?;
        for region in iter {
            let rect = region.applied_rect();
            min_x = min_x.min(rect.origin().x);
            min_y = min_y.min(rect.origin().y);
            max_x = max_x.max(checked_applied_end(rect.origin().x, rect.size().width())?);
            max_y = max_y.max(checked_applied_end(rect.origin().y, rect.size().height())?);
        }
        let width =
            u32::try_from(max_x - min_x).map_err(|_| DeckRegionRuntimeError::CoordinateOverflow)?;
        let height =
            u32::try_from(max_y - min_y).map_err(|_| DeckRegionRuntimeError::CoordinateOverflow)?;
        Ok(Self {
            desktop_origin: AppliedPoint::new(min_x, min_y),
            desktop_size: AppliedSize::new(width, height)?,
        })
    }

    /// Converts one accepted region DTO to the deployed legacy message.
    ///
    /// Region enter is represented as the legacy absolute move that
    /// establishes the same position. Leave has no legacy counterpart.
    pub fn adapt(
        self,
        regions: &AppliedRegionSet,
        message: &RegionInputWireMessage,
        motion_mode: PointerMotionMode,
    ) -> Result<Option<LegacyRegionWireMessage>, DeckRegionRuntimeError> {
        message.validate()?;
        let position = self.legacy_position(regions, message.position())?;
        let (x, y, server_x, server_y) = position;
        let adapted = match message {
            RegionInputWireMessage::PointerEnter(message) => {
                if motion_mode == PointerMotionMode::Relative {
                    None
                } else {
                    Some(LegacyRegionWireMessage {
                        value: serde_json::to_value(MouseMoveMsg {
                            x,
                            y,
                            server_x,
                            server_y,
                            sequence: message.metadata.sequence,
                            timestamp_ns: message.metadata.timestamp_ns,
                            coalescable: message.metadata.coalescable,
                            ..MouseMoveMsg::default()
                        })
                        .expect("MouseMoveMsg must serialize"),
                        input_type: "mouse_move",
                        pointer_position: Some(position),
                        button_state: None,
                    })
                }
            }
            RegionInputWireMessage::PointerMotion(message) => {
                if motion_mode == PointerMotionMode::Relative {
                    None
                } else {
                    Some(LegacyRegionWireMessage {
                        value: serde_json::to_value(MouseMoveMsg {
                            x,
                            y,
                            server_x,
                            server_y,
                            sequence: message.metadata.sequence,
                            timestamp_ns: message.metadata.timestamp_ns,
                            coalescable: message.metadata.coalescable,
                            ..MouseMoveMsg::default()
                        })
                        .expect("MouseMoveMsg must serialize"),
                        input_type: "mouse_move",
                        pointer_position: Some(position),
                        button_state: None,
                    })
                }
            }
            RegionInputWireMessage::PointerLeave(_) => None,
            RegionInputWireMessage::PointerButton(message) => Some(LegacyRegionWireMessage {
                value: serde_json::to_value(MouseButtonMsg {
                    x,
                    y,
                    button: message.button,
                    pressed: message.pressed,
                    server_x,
                    server_y,
                    sequence: message.metadata.sequence,
                    timestamp_ns: message.metadata.timestamp_ns,
                    coalescable: message.metadata.coalescable,
                    motion_mode,
                    ..MouseButtonMsg::default()
                })
                .expect("MouseButtonMsg must serialize"),
                input_type: "mouse_button",
                pointer_position: Some(position),
                button_state: Some((message.button, message.pressed)),
            }),
            RegionInputWireMessage::PointerScroll(message) => Some(LegacyRegionWireMessage {
                value: serde_json::to_value(MouseScrollMsg {
                    x,
                    y,
                    dx: message.delta_x as f64 / arcen_media::LOGICAL_UNITS_PER_PIXEL as f64,
                    dy: message.delta_y as f64 / arcen_media::LOGICAL_UNITS_PER_PIXEL as f64,
                    server_x,
                    server_y,
                    sequence: message.metadata.sequence,
                    timestamp_ns: message.metadata.timestamp_ns,
                    coalescable: message.metadata.coalescable,
                    motion_mode,
                    ..MouseScrollMsg::default()
                })
                .expect("MouseScrollMsg must serialize"),
                input_type: "mouse_scroll",
                pointer_position: Some(position),
                button_state: None,
            }),
            RegionInputWireMessage::Pen(message) => Some(LegacyRegionWireMessage {
                value: serde_json::to_value(PenEventMsg {
                    x,
                    y,
                    server_x,
                    server_y,
                    pressure: message.pressure,
                    tilt_x_degrees: message.tilt_x_degrees,
                    tilt_y_degrees: message.tilt_y_degrees,
                    rotation_degrees: message.rotation_degrees,
                    tool: message.tool,
                    in_proximity: message.in_proximity,
                    touching: message.touching,
                    buttons: message.buttons,
                    sequence: message.metadata.sequence,
                    timestamp_ns: message.metadata.timestamp_ns,
                    coalescable: message.metadata.coalescable,
                    ..PenEventMsg::default()
                })
                .expect("PenEventMsg must serialize"),
                input_type: "pen_event",
                pointer_position: None,
                button_state: None,
            }),
        };
        Ok(adapted)
    }

    fn legacy_position(
        self,
        regions: &AppliedRegionSet,
        position: RegionInputPositionMsg,
    ) -> Result<(f64, f64, i32, i32), DeckRegionRuntimeError> {
        let region_id = RegionId::new(position.region_id)?;
        let applied = RegionCoordinateTransformer::new(regions).logical_to_applied(
            region_id,
            LogicalPoint::new(position.logical_x, position.logical_y),
        )?;
        let relative_x = applied.x - self.desktop_origin.x;
        let relative_y = applied.y - self.desktop_origin.y;
        let denominator_x = i64::from(self.desktop_size.width().saturating_sub(1).max(1));
        let denominator_y = i64::from(self.desktop_size.height().saturating_sub(1).max(1));
        let x = (relative_x as f64 / denominator_x as f64).clamp(0.0, 1.0);
        let y = (relative_y as f64 / denominator_y as f64).clamp(0.0, 1.0);
        Ok((
            x,
            y,
            saturating_i64_to_i32(applied.x),
            saturating_i64_to_i32(applied.y),
        ))
    }
}

fn checked_applied_end(origin: i64, extent: u32) -> Result<i64, DeckRegionRuntimeError> {
    origin
        .checked_add(i64::from(extent))
        .ok_or(DeckRegionRuntimeError::CoordinateOverflow)
}

fn saturating_i64_to_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value.is_negative() {
        i32::MIN
    } else {
        i32::MAX
    })
}

/// Deck's authoritative per-session region aggregate.
#[derive(Debug, Clone)]
pub struct DeckRegionRuntime {
    requested: RegionSet,
    applied: AppliedRegionSet,
    primary_viewport: RegionViewport,
    monitor_viewports: BTreeMap<SessionMonitorId, RegionViewport>,
    emitter: RegionInputEmitter,
    legacy_adapter: Option<TemporaryLegacyRegionInputAdapter>,
}

impl DeckRegionRuntime {
    /// Builds the region aggregate from the host-applied topology and the
    /// exact client-requested logical topology retained in connection state.
    ///
    /// When a test or defensive recovery path lacks that retained request,
    /// logical geometry falls back to the applied pixel geometry at 1x. A
    /// present request must contain every applied output identity.
    pub fn from_validated_topology(
        validated: &ValidatedAppliedTopology,
        requested: Option<&RequestedMonitorTopology>,
    ) -> Result<Self, DeckRegionRuntimeError> {
        let generation = RegionGeneration::new(validated.generation.get())?;
        let mut placements = Vec::with_capacity(validated.monitors.len());
        let mut monitor_ids = Vec::with_capacity(validated.monitors.len());

        for (index, monitor) in validated.monitors.iter().enumerate() {
            let output = monitor.cg_display_id.to_string();
            let requested_monitor = requested.and_then(|topology| {
                topology
                    .monitors()
                    .iter()
                    .find(|candidate| candidate.monitor().identity.id == output)
            });
            if requested.is_some() && requested_monitor.is_none() {
                return Err(DeckRegionRuntimeError::MissingRequestedOutput(output));
            }
            let (logical_origin, logical_size, scale, rotation) =
                if let Some(requested_monitor) = requested_monitor {
                    (
                        LogicalPoint::from_pixels(
                            i64::from(requested_monitor.monitor().x),
                            i64::from(requested_monitor.monitor().y),
                        )?,
                        LogicalSize::from_pixels(
                            u64::from(requested_monitor.logical_width),
                            u64::from(requested_monitor.logical_height),
                        )?,
                        scale120_from_f32(requested_monitor.monitor().scale)?,
                        requested_monitor.monitor().rotation,
                    )
                } else {
                    (
                        LogicalPoint::from_pixels(monitor.rect.x, monitor.rect.y)?,
                        LogicalSize::from_pixels(
                            u64::from(monitor.rect.width_px),
                            u64::from(monitor.rect.height_px),
                        )?,
                        Scale120::new(120)?,
                        Rotation::Degrees0,
                    )
                };
            // Window/input geometry consumes only the roster's negotiated
            // physical extent. Stream epochs remain owned by media framing
            // and are validated from each carrier-supplied frame header.
            let media_plan = validated
                .media_roster
                .plan(monitor.session_monitor_id)
                .ok_or(DeckRegionRuntimeError::MissingMediaPlan(
                    monitor.session_monitor_id,
                ))?;
            if media_plan.width != monitor.rect.width_px
                || media_plan.height != monitor.rect.height_px
            {
                return Err(DeckRegionRuntimeError::MediaPlanSizeMismatch {
                    monitor_id: monitor.session_monitor_id,
                    media: (media_plan.width, media_plan.height),
                    applied: (monitor.rect.width_px, monitor.rect.height_px),
                });
            }
            placements.push(RegionPlacement {
                region_id: RegionId::new(u32::from(monitor.session_monitor_id.get()))?,
                output: OutputIdentity::new(output)?,
                logical_rect: LogicalRect::new(logical_origin, logical_size)?,
                stream_size: PhysicalSize::new(media_plan.width, media_plan.height)?,
                scale,
                // Retained for the shared convention to interpret. Under
                // `AlreadyCompositorOriented` the applied roster/media extent
                // has already absorbed the host transform, so the region
                // records `OutputTransform::Normal` regardless of this value.
                rotation,
                primary: index == 0,
                applied_rect: AppliedRect::new(
                    AppliedPoint::new(monitor.rect.x, monitor.rect.y),
                    AppliedSize::new(monitor.rect.width_px, monitor.rect.height_px)?,
                )?,
            });
            monitor_ids.push(monitor.session_monitor_id);
        }

        Self::from_parts(
            generation,
            placements,
            monitor_ids.into_iter().map(Some).collect(),
        )
    }

    /// Builds the one-region compatibility aggregate used by a legacy
    /// primary-only session.
    pub fn legacy_primary(width: u32, height: u32) -> Result<Self, DeckRegionRuntimeError> {
        let generation = RegionGeneration::new(LEGACY_REGION_GENERATION)?;
        let placement = RegionPlacement {
            region_id: RegionId::new(LEGACY_PRIMARY_REGION_ID)?,
            output: OutputIdentity::new("legacy-primary")?,
            logical_rect: LogicalRect::new(
                LogicalPoint::new(0, 0),
                LogicalSize::from_pixels(u64::from(width), u64::from(height))?,
            )?,
            stream_size: PhysicalSize::new(width, height)?,
            scale: Scale120::new(120)?,
            rotation: Rotation::Degrees0,
            primary: true,
            applied_rect: AppliedRect::new(
                AppliedPoint::new(0, 0),
                AppliedSize::new(width, height)?,
            )?,
        };
        Self::from_parts(generation, vec![placement], vec![None])
    }

    fn from_parts(
        generation: RegionGeneration,
        placements: Vec<RegionPlacement>,
        monitor_ids: Vec<Option<SessionMonitorId>>,
    ) -> Result<Self, DeckRegionRuntimeError> {
        if placements.len() != monitor_ids.len() {
            return Err(DeckRegionRuntimeError::MismatchedRegionParts);
        }
        let (requested, applied) =
            arcen_media::build_region_sets(generation, TRANSFORM_CONVENTION, &placements)?;
        let mut monitor_viewports = BTreeMap::new();
        let mut primary_viewport = None;
        for (descriptor, monitor_id) in requested.regions().iter().zip(monitor_ids) {
            let viewport = RegionViewport {
                region_id: descriptor.id(),
                session_monitor_id: monitor_id,
            };
            if descriptor.is_primary() {
                primary_viewport = Some(viewport);
            }
            if let Some(monitor_id) = monitor_id {
                monitor_viewports.insert(monitor_id, viewport);
            }
        }
        let primary_viewport =
            primary_viewport.ok_or(DeckRegionRuntimeError::MissingPrimaryViewport)?;
        let legacy_adapter = if primary_viewport.session_monitor_id.is_none() {
            Some(TemporaryLegacyRegionInputAdapter::new(&applied)?)
        } else {
            None
        };
        Ok(Self {
            requested,
            applied,
            primary_viewport,
            monitor_viewports,
            emitter: RegionInputEmitter::new(),
            legacy_adapter,
        })
    }

    #[must_use]
    pub const fn generation(&self) -> RegionGeneration {
        self.applied.generation()
    }

    #[must_use]
    pub const fn primary_viewport(&self) -> RegionViewport {
        self.primary_viewport
    }

    /// Whether this runtime came from a negotiated host region topology.
    ///
    /// The compatibility runtime created by [`Self::legacy_primary`] has no
    /// session monitor id and is the only path allowed to use the temporary
    /// legacy adapter.
    #[must_use]
    pub const fn uses_region_wire(&self) -> bool {
        self.primary_viewport.session_monitor_id.is_some()
    }

    #[must_use]
    pub fn viewport_for_monitor(&self, monitor_id: SessionMonitorId) -> Option<RegionViewport> {
        self.monitor_viewports.get(&monitor_id).copied()
    }

    #[must_use]
    pub const fn requested_regions(&self) -> &RegionSet {
        &self.requested
    }

    #[must_use]
    pub const fn applied_regions(&self) -> &AppliedRegionSet {
        &self.applied
    }

    pub fn logical_position(
        &self,
        viewport: RegionViewport,
        local_fraction: (f64, f64),
    ) -> Result<RegionLogicalPosition, DeckRegionRuntimeError> {
        let region = self
            .applied
            .get(viewport.region_id)
            .ok_or(DeckRegionRuntimeError::UnknownRegion(viewport.region_id))?;
        let size = region.descriptor().logical_rect().size();
        Ok(RegionLogicalPosition {
            region_id: viewport.region_id,
            point: LogicalPoint::new(
                fraction_to_logical(local_fraction.0, size.width())?,
                fraction_to_logical(local_fraction.1, size.height())?,
            ),
        })
    }

    /// The ordered region input state owned by the shared emitter.
    ///
    /// Diagnostics and tests observe held buttons, focus, latest position,
    /// and the last accepted sequence here; Deck keeps no second copy.
    #[must_use]
    pub const fn input_state(&self) -> &RegionInputState {
        self.emitter.state()
    }

    pub fn pointer_motion(
        &mut self,
        viewport: RegionViewport,
        local_fraction: (f64, f64),
        sequence: &mut u64,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, DeckRegionRuntimeError> {
        let position = self.logical_position(viewport, local_fraction)?;
        self.emit(sequence, |emitter, regions| {
            emitter.pointer_motion(regions, position, timestamp_ns)
        })
    }

    pub fn pointer_sample(
        &mut self,
        viewport: RegionViewport,
        local_fraction: (f64, f64),
        buttons: &[(u8, bool)],
        sequence: &mut u64,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, DeckRegionRuntimeError> {
        let position = self.logical_position(viewport, local_fraction)?;
        self.emit(sequence, |emitter, regions| {
            emitter.pointer_sample(regions, position, buttons, timestamp_ns)
        })
    }

    pub fn pointer_button(
        &mut self,
        viewport: RegionViewport,
        local_fraction: (f64, f64),
        button: u8,
        pressed: bool,
        sequence: &mut u64,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, DeckRegionRuntimeError> {
        let position = self.logical_position(viewport, local_fraction)?;
        self.emit(sequence, |emitter, regions| {
            emitter.pointer_button(regions, position, button, pressed, timestamp_ns)
        })
    }

    pub fn pointer_button_at_latest(
        &mut self,
        button: u8,
        pressed: bool,
        sequence: &mut u64,
        timestamp_ns: u64,
    ) -> Result<Option<RegionInputWireMessage>, DeckRegionRuntimeError> {
        self.emit(sequence, |emitter, regions| {
            emitter.pointer_button_at_latest(regions, button, pressed, timestamp_ns)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pointer_scroll(
        &mut self,
        viewport: RegionViewport,
        local_fraction: (f64, f64),
        delta_x_ticks: i32,
        delta_y_ticks: i32,
        sequence: &mut u64,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, DeckRegionRuntimeError> {
        // The host scroll contract is an i32 fixed-point delta: reject an
        // out-of-range gesture before any state or sequence advances.
        let delta_x = scroll_delta_to_wire(delta_x_ticks)?;
        let delta_y = scroll_delta_to_wire(delta_y_ticks)?;
        let position = self.logical_position(viewport, local_fraction)?;
        self.emit(sequence, |emitter, regions| {
            emitter.pointer_scroll(regions, position, delta_x, delta_y, timestamp_ns)
        })
    }

    /// Emits one Wacom pen sample through its authoritative region.
    ///
    /// The tablet pipeline already allocated this sample's sequence from the
    /// session-global input counter, so the emitter adopts it rather than
    /// allocating a new one.
    pub fn pen(
        &mut self,
        viewport: RegionViewport,
        event: PenEvent,
    ) -> Result<RegionInputWireMessage, DeckRegionRuntimeError> {
        let position = self.logical_position(viewport, (event.x, event.y))?;
        let sample = RegionPenSample {
            position,
            pressure: event.pressure,
            tilt_x_degrees: event.tilt_x_degrees,
            tilt_y_degrees: event.tilt_y_degrees,
            rotation_degrees: event.rotation_degrees,
            tool: event.tool,
            in_proximity: event.in_proximity,
            touching: event.touching,
            buttons: event.buttons,
        };
        Ok(self.emitter.pen_with_sequence(
            &self.applied,
            sample,
            event.metadata.sequence,
            event.metadata.timestamp_ns,
            event.metadata.coalescable,
        )?)
    }

    pub fn adapt_legacy(
        &self,
        message: &RegionInputWireMessage,
        motion_mode: PointerMotionMode,
    ) -> Result<Option<LegacyRegionWireMessage>, DeckRegionRuntimeError> {
        self.legacy_adapter
            .ok_or(DeckRegionRuntimeError::LegacyAdapterUnavailableForRegionSession)?
            .adapt(&self.applied, message, motion_mode)
    }

    pub fn release_all(&mut self) {
        let _ = self.emitter.release_all();
    }

    /// Runs one shared emission against this session's applied aggregate,
    /// continuing and returning the caller's session-global input sequence.
    ///
    /// The sequence is written back on rejection too, so a rejected
    /// transition consumes its allocated sequence exactly like an accepted
    /// one and no later message can reuse it.
    fn emit<T>(
        &mut self,
        sequence: &mut u64,
        emit: impl FnOnce(&mut RegionInputEmitter, &AppliedRegionSet) -> Result<T, RegionInputEmitError>,
    ) -> Result<T, DeckRegionRuntimeError> {
        self.emitter.advance_sequence_to(*sequence);
        let emitted = emit(&mut self.emitter, &self.applied);
        *sequence = self.emitter.sequence();
        Ok(emitted?)
    }
}

fn fraction_to_logical(fraction: f64, extent: u64) -> Result<i64, DeckRegionRuntimeError> {
    if !fraction.is_finite() {
        return Err(DeckRegionRuntimeError::NonFiniteLocalFraction);
    }
    let maximum = extent.saturating_sub(1);
    let coordinate = (fraction.clamp(0.0, 1.0) * maximum as f64).round() as u64;
    i64::try_from(coordinate).map_err(|_| DeckRegionRuntimeError::CoordinateOverflow)
}

fn scroll_delta_to_wire(ticks: i32) -> Result<i64, DeckRegionRuntimeError> {
    let delta = i64::from(ticks) * arcen_media::LOGICAL_UNITS_PER_PIXEL;
    i32::try_from(delta)
        .map(i64::from)
        .map_err(|_| DeckRegionRuntimeError::ScrollDeltaOverflow(delta))
}

/// Converts a client-visible fractional scale into the wire-exact
/// `Scale120` rational, using the shared media conversion so hosts and Deck
/// cannot drift on rounding or rejection boundaries.
fn scale120_from_f32(scale: f32) -> Result<Scale120, DeckRegionRuntimeError> {
    arcen_media::scale120_from_scale(scale).map_err(|_| DeckRegionRuntimeError::InvalidScale(scale))
}

/// Region runtime construction, validation, or dispatch failure.
#[derive(Debug)]
pub enum DeckRegionRuntimeError {
    Region(RegionContractError),
    Coordinate(arcen_input::CoordinateTransformError),
    Input(RegionInputStateError),
    Wire(RegionInputValidationError),
    EmptyAppliedRegionSet,
    MissingPrimaryViewport,
    MismatchedRegionParts,
    MissingRequestedOutput(String),
    MissingMediaPlan(SessionMonitorId),
    MediaPlanSizeMismatch {
        monitor_id: SessionMonitorId,
        media: (u32, u32),
        applied: (u32, u32),
    },
    UnknownRegion(RegionId),
    MissingPointerPosition,
    /// A future shared emission rejection Deck has no dedicated variant for.
    EmitterRejected(String),
    LegacyAdapterUnavailableForRegionSession,
    ScrollDeltaOverflow(i64),
    NonFiniteLocalFraction,
    InvalidScale(f32),
    CoordinateOverflow,
}

impl Display for DeckRegionRuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Region(error) => write!(formatter, "{error}"),
            Self::Coordinate(error) => write!(formatter, "{error}"),
            Self::Input(error) => write!(formatter, "{error}"),
            Self::Wire(error) => write!(formatter, "{error}"),
            Self::EmptyAppliedRegionSet => formatter.write_str("applied region set is empty"),
            Self::MissingPrimaryViewport => formatter.write_str("region set has no primary view"),
            Self::MismatchedRegionParts => {
                formatter.write_str("region descriptors, rectangles, and monitor ids differ")
            }
            Self::MissingRequestedOutput(output) => {
                write!(
                    formatter,
                    "applied output {output:?} was absent from the request"
                )
            }
            Self::MissingMediaPlan(monitor_id) => {
                write!(formatter, "monitor {} has no media plan", monitor_id.get())
            }
            Self::MediaPlanSizeMismatch {
                monitor_id,
                media,
                applied,
            } => write!(
                formatter,
                "monitor {} media size {}x{} differs from applied {}x{}",
                monitor_id.get(),
                media.0,
                media.1,
                applied.0,
                applied.1
            ),
            Self::UnknownRegion(region_id) => {
                write!(formatter, "unknown region {}", region_id.get())
            }
            Self::MissingPointerPosition => formatter.write_str("pointer position is unavailable"),
            Self::EmitterRejected(error) => formatter.write_str(error),
            Self::LegacyAdapterUnavailableForRegionSession => {
                formatter.write_str("legacy input adapter is unavailable for a region session")
            }
            Self::ScrollDeltaOverflow(delta) => {
                write!(formatter, "region scroll delta {delta} does not fit i32")
            }
            Self::NonFiniteLocalFraction => {
                formatter.write_str("viewport-local pointer fraction is non-finite")
            }
            Self::InvalidScale(scale) => write!(formatter, "invalid region scale {scale}"),
            Self::CoordinateOverflow => formatter.write_str("region coordinate overflow"),
        }
    }
}

impl Error for DeckRegionRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Region(error) => Some(error),
            Self::Coordinate(error) => Some(error),
            Self::Input(error) => Some(error),
            Self::Wire(error) => Some(error),
            _ => None,
        }
    }
}

impl From<RegionContractError> for DeckRegionRuntimeError {
    fn from(value: RegionContractError) -> Self {
        Self::Region(value)
    }
}

impl From<arcen_input::CoordinateTransformError> for DeckRegionRuntimeError {
    fn from(value: arcen_input::CoordinateTransformError) -> Self {
        Self::Coordinate(value)
    }
}

impl From<RegionInputStateError> for DeckRegionRuntimeError {
    fn from(value: RegionInputStateError) -> Self {
        Self::Input(value)
    }
}

impl From<RegionInputValidationError> for DeckRegionRuntimeError {
    fn from(value: RegionInputValidationError) -> Self {
        Self::Wire(value)
    }
}

/// Preserves the exact rejection each shared emission produces: ordered-state
/// violations, wire validation failures, region-contract failures, and the
/// "no pointer position accepted yet" case keep their own Deck variants.
impl From<RegionInputEmitError> for DeckRegionRuntimeError {
    fn from(value: RegionInputEmitError) -> Self {
        match value {
            RegionInputEmitError::State(error) => Self::Input(error),
            RegionInputEmitError::Wire(error) => Self::Wire(error),
            RegionInputEmitError::Contract(error) => Self::Region(error),
            RegionInputEmitError::MissingPointerPosition => Self::MissingPointerPosition,
            other => Self::EmitterRejected(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_input::{
        LowLatencyMetadata, MappedRegionInput, PenTool, RegionInputEvent, RegionInputPipeline,
        RegionPointMapper,
    };
    use arcen_media::{
        video::EncoderBackend, BitrateBudgetKbps, MediaStreamEpoch, Monitor, MonitorIdentity,
        OutputTransform, RegionMediaPlan, RegionMediaRoster, RequestedMonitor, TopologyGeneration,
        VideoConfiguration,
    };

    use crate::protocol::messages::MultiMonitorCarrierMsg;
    use crate::ui::multi_window_session::{
        map_local_fraction_to_wire_pointer, DesktopRect, MonitorDesktopRect, ResolvedAppliedMonitor,
    };

    const CROSS_COMPONENT_BASELINE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/runtime/region_input.json"
    ));

    /// Stands in for a host's native pointer mapping so the parity gate
    /// observes the shared transform result itself.
    #[derive(Debug, Clone, Copy)]
    struct IdentityMapper;

    impl RegionPointMapper for IdentityMapper {
        type Point = AppliedPoint;
        type Error = std::convert::Infallible;

        fn map_applied(&self, point: AppliedPoint) -> Result<AppliedPoint, Self::Error> {
            Ok(point)
        }
    }

    fn mapped_point(mapped: MappedRegionInput<AppliedPoint>) -> AppliedPoint {
        match mapped {
            MappedRegionInput::PointerEnter(point)
            | MappedRegionInput::PointerLeave(point)
            | MappedRegionInput::PointerMotion(point) => point,
            MappedRegionInput::PointerButton(button) => button.position,
            MappedRegionInput::PointerScroll(scroll) => scroll.position,
            MappedRegionInput::Pen(pen) => pen.position,
        }
    }

    fn media_plan(monitor_id: u16, stream_epoch: u64, width: u32, height: u32) -> RegionMediaPlan {
        RegionMediaPlan::new(
            SessionMonitorId::new(monitor_id).unwrap(),
            MediaStreamEpoch::new(stream_epoch).unwrap(),
            EncoderBackend::OpenH264,
            VideoConfiguration::legacy_h264(),
            width,
            height,
            60,
            BitrateBudgetKbps::nominal_for_geometry(width, height, 60),
        )
        .unwrap()
    }

    fn requested_monitor(
        id: u32,
        x: i32,
        y: i32,
        logical_width: u32,
        logical_height: u32,
        physical_width: u32,
        physical_height: u32,
        scale: f32,
        primary: bool,
    ) -> RequestedMonitor {
        RequestedMonitor::new(
            Monitor {
                identity: MonitorIdentity {
                    id: id.to_string(),
                    name: format!("Display {id}"),
                    vendor: 1,
                    model: 2,
                    serial: id,
                },
                x,
                y,
                width_px: physical_width,
                height_px: physical_height,
                scale,
                refresh_hz: 60,
                rotation: arcen_media::Rotation::Degrees0,
                primary,
                width_mm: 0.0,
                height_mm: 0.0,
            },
            logical_width,
            logical_height,
        )
        .unwrap()
    }

    fn mixed_runtime() -> DeckRegionRuntime {
        let primary_id = SessionMonitorId::new(1).unwrap();
        let secondary_id = SessionMonitorId::new(2).unwrap();
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor(11, 0, 0, 1800, 1169, 3600, 2338, 2.0, true),
            requested_monitor(22, -2560, 0, 2560, 1440, 2560, 1440, 1.0, false),
        ])
        .unwrap();
        let roster = RegionMediaRoster::new(vec![
            media_plan(1, 41, 3600, 2338),
            media_plan(2, 77, 2560, 1440),
        ])
        .unwrap();
        let validated = ValidatedAppliedTopology {
            generation: TopologyGeneration::new(7).unwrap(),
            carrier: MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors: vec![
                ResolvedAppliedMonitor {
                    session_monitor_id: primary_id,
                    cg_display_id: 11,
                    rect: MonitorDesktopRect {
                        x: 0,
                        y: 0,
                        width_px: 3600,
                        height_px: 2338,
                    },
                },
                ResolvedAppliedMonitor {
                    session_monitor_id: secondary_id,
                    cg_display_id: 22,
                    rect: MonitorDesktopRect {
                        x: -2560,
                        y: 0,
                        width_px: 2560,
                        height_px: 1440,
                    },
                },
            ],
            media_roster: Box::new(roster),
            desktop: DesktopRect {
                x: -2560,
                y: 0,
                width_px: 6160,
                height_px: 2338,
            },
        };
        DeckRegionRuntime::from_validated_topology(&validated, Some(&requested)).unwrap()
    }

    fn baseline_runtime() -> DeckRegionRuntime {
        let monitor_id = SessionMonitorId::new(1).unwrap();
        let requested = RequestedMonitorTopology::new(vec![requested_monitor(
            11, 0, 0, 1_920, 1_080, 1_920, 1_080, 1.0, true,
        )])
        .unwrap();
        let validated = ValidatedAppliedTopology {
            generation: TopologyGeneration::new(7).unwrap(),
            carrier: MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors: vec![ResolvedAppliedMonitor {
                session_monitor_id: monitor_id,
                cg_display_id: 11,
                rect: MonitorDesktopRect {
                    x: 0,
                    y: 0,
                    width_px: 1_920,
                    height_px: 1_080,
                },
            }],
            media_roster: Box::new(
                RegionMediaRoster::new(vec![media_plan(1, 41, 1_920, 1_080)]).unwrap(),
            ),
            desktop: DesktopRect {
                x: 0,
                y: 0,
                width_px: 1_920,
                height_px: 1_080,
            },
        };
        DeckRegionRuntime::from_validated_topology(&validated, Some(&requested)).unwrap()
    }

    #[test]
    fn cross_component_fixture_freezes_deck_encoding_state_and_endpoint() {
        let baseline: serde_json::Value = serde_json::from_str(CROSS_COMPONENT_BASELINE).unwrap();
        let runtime = baseline_runtime();
        let generation = runtime.generation();
        let region_id = RegionId::new(1).unwrap();
        let pointer = RegionLogicalPosition {
            region_id,
            point: LogicalPoint::new(60, 1_440),
        };
        let pen = RegionPenSample {
            position: RegionLogicalPosition {
                region_id,
                point: LogicalPoint::new(600, 900),
            },
            pressure: 0.75,
            tilt_x_degrees: -12.0,
            tilt_y_degrees: 10.0,
            rotation_degrees: 180.0,
            tool: PenTool::Tip,
            in_proximity: true,
            touching: true,
            buttons: 1,
        };
        let events = [
            (
                RegionInputEvent::PointerEnter {
                    generation,
                    position: RegionLogicalPosition {
                        region_id,
                        point: LogicalPoint::new(0, 0),
                    },
                    sequence: 40,
                },
                9_000,
                false,
            ),
            (
                RegionInputEvent::PointerMotion {
                    generation,
                    position: pointer,
                    sequence: 41,
                },
                9_001,
                true,
            ),
            (
                RegionInputEvent::PointerButton {
                    generation,
                    position: pointer,
                    button: 1,
                    pressed: true,
                    sequence: 42,
                },
                9_002,
                false,
            ),
            (
                RegionInputEvent::PointerScroll {
                    generation,
                    position: pointer,
                    delta_x: 120,
                    delta_y: -240,
                    sequence: 43,
                },
                9_003,
                false,
            ),
            (
                RegionInputEvent::Pen {
                    generation,
                    sample: pen,
                    sequence: 44,
                },
                9_004,
                true,
            ),
            (
                RegionInputEvent::PointerLeave {
                    generation,
                    position: pointer,
                    sequence: 45,
                },
                9_005,
                false,
            ),
        ];
        let mut state = RegionInputState::new();
        let encoded = events
            .into_iter()
            .map(|(event, timestamp_ns, coalescable)| {
                state.apply(runtime.applied_regions(), event).unwrap();
                let message = RegionInputWireMessage::encode(event, timestamp_ns, coalescable);
                message.validate().unwrap();
                message.to_json_value().unwrap()
            })
            .collect::<Vec<_>>();

        assert_eq!(encoded.as_slice(), baseline["events"].as_array().unwrap());
        let transformer = RegionCoordinateTransformer::new(runtime.applied_regions());
        assert_eq!(
            transformer
                .logical_to_applied(pointer.region_id, pointer.point)
                .unwrap(),
            AppliedPoint::new(
                baseline["pointer_endpoint"]["x"].as_i64().unwrap(),
                baseline["pointer_endpoint"]["y"].as_i64().unwrap(),
            )
        );
        assert_eq!(
            transformer
                .logical_to_applied(pen.position.region_id, pen.position.point)
                .unwrap(),
            AppliedPoint::new(
                baseline["pen_endpoint"]["x"].as_i64().unwrap(),
                baseline["pen_endpoint"]["y"].as_i64().unwrap(),
            )
        );
        assert_eq!(state.latest_pointer_position(), Some(pointer));
        assert_eq!(state.held_buttons().collect::<Vec<_>>(), vec![1]);
        assert_eq!(state.pen().unwrap().sample, pen);
        assert_eq!(state.last_sequence(), 45);
        assert!(!state.is_focused());
    }

    #[test]
    fn center_and_all_corners_use_one_region_coordinate_transformer() {
        let runtime = mixed_runtime();
        let viewport = runtime.primary_viewport();
        let region = runtime.applied_regions().get(viewport.region_id()).unwrap();
        let size = region.descriptor().logical_rect().size();

        let top_left = runtime.logical_position(viewport, (0.0, 0.0)).unwrap();
        let top_right = runtime.logical_position(viewport, (1.0, 0.0)).unwrap();
        let bottom_left = runtime.logical_position(viewport, (0.0, 1.0)).unwrap();
        let bottom_right = runtime.logical_position(viewport, (1.0, 1.0)).unwrap();
        let center = runtime.logical_position(viewport, (0.5, 0.5)).unwrap();

        assert_eq!(top_left.point, LogicalPoint::new(0, 0));
        assert_eq!(
            top_right.point,
            LogicalPoint::new(i64::try_from(size.width() - 1).unwrap(), 0)
        );
        assert_eq!(
            bottom_left.point,
            LogicalPoint::new(0, i64::try_from(size.height() - 1).unwrap())
        );
        assert_eq!(
            bottom_right.point,
            LogicalPoint::new(
                i64::try_from(size.width() - 1).unwrap(),
                i64::try_from(size.height() - 1).unwrap()
            )
        );
        assert_eq!(
            RegionCoordinateTransformer::new(runtime.applied_regions())
                .logical_to_applied(center.region_id, center.point)
                .unwrap(),
            AppliedPoint::new(1800, 1169)
        );
    }

    #[test]
    fn context_menu_button_repeats_the_latest_authoritative_motion_position() {
        let mut runtime = mixed_runtime();
        let viewport = runtime.primary_viewport();
        let mut sequence = 0;
        let motion = runtime
            .pointer_motion(viewport, (0.75, 0.25), &mut sequence, 10)
            .unwrap();
        let motion = motion.last().unwrap();
        let button = runtime
            .pointer_button_at_latest(3, true, &mut sequence, 11)
            .unwrap()
            .unwrap();

        assert_eq!(motion.position(), button.position());
        assert_eq!(button.button_state(), Some((3, true)));
        let motion = motion.to_json_value().unwrap();
        let button = button.to_json_value().unwrap();
        assert_eq!(motion["logical_x"], button["logical_x"]);
        assert_eq!(motion["logical_y"], button["logical_y"]);
        assert!(motion.get("server_x").is_none());
        assert!(motion.get("server_y").is_none());
        assert!(button.get("server_x").is_none());
        assert!(button.get("server_y").is_none());
    }

    #[test]
    fn negotiated_region_runtime_has_no_legacy_adapter() {
        let mut runtime = mixed_runtime();
        let viewport = runtime.primary_viewport();
        let mut sequence = 0;
        let messages = runtime
            .pointer_motion(viewport, (0.5, 0.5), &mut sequence, 10)
            .unwrap();

        assert!(matches!(
            runtime.adapt_legacy(messages.last().unwrap(), PointerMotionMode::Absolute),
            Err(DeckRegionRuntimeError::LegacyAdapterUnavailableForRegionSession),
        ));
    }

    /// Parity gate: everything a live Match My Layout session emits -- root
    /// and secondary motion, the cross-viewport leave/enter transition,
    /// buttons, scroll, Wacom pen, and the held-button release -- is
    /// produced by one shared emitter, is accepted in order by the shared
    /// host pipeline, and lands on exactly the applied point the one shared
    /// transformer computes. There is no second client mapping path left to
    /// diverge from this one.
    #[test]
    fn every_production_region_message_is_accepted_by_the_shared_host_pipeline_unchanged() {
        let mut runtime = mixed_runtime();
        let primary = runtime.primary_viewport();
        let secondary = runtime
            .viewport_for_monitor(SessionMonitorId::new(2).unwrap())
            .unwrap();
        // Deck's session-global input counter: keyboard and pen already
        // consumed 40 sequences before this pointer gesture.
        let mut sequence = 40;
        let mut emitted = Vec::new();

        emitted.extend(
            runtime
                .pointer_motion(primary, (0.25, 0.25), &mut sequence, 10)
                .unwrap(),
        );
        emitted.extend(
            runtime
                .pointer_sample(primary, (0.5, 0.5), &[(1, true)], &mut sequence, 11)
                .unwrap(),
        );
        emitted.extend(
            runtime
                .pointer_scroll(primary, (0.5, 0.5), 1, -2, &mut sequence, 12)
                .unwrap(),
        );
        // Crossing viewports must derive leave-then-enter from the same
        // emitter rather than a per-viewport mapping.
        emitted.extend(
            runtime
                .pointer_button(secondary, (0.75, 0.5), 1, false, &mut sequence, 13)
                .unwrap(),
        );
        let pen_sequence = {
            sequence = sequence.saturating_add(1);
            sequence
        };
        emitted.push(
            runtime
                .pen(
                    secondary,
                    PenEvent {
                        x: 0.25,
                        y: 0.75,
                        pressure: 0.6,
                        tilt_x_degrees: 12.0,
                        tilt_y_degrees: -8.0,
                        rotation_degrees: 30.0,
                        tool: PenTool::Tip,
                        in_proximity: true,
                        touching: true,
                        buttons: 2,
                        metadata: LowLatencyMetadata {
                            sequence: pen_sequence,
                            timestamp_ns: 14,
                            coalescable: true,
                        },
                    },
                )
                .unwrap(),
        );
        emitted.extend(
            runtime
                .pointer_button(secondary, (0.75, 0.5), 3, true, &mut sequence, 15)
                .unwrap(),
        );
        emitted.extend(
            runtime
                .pointer_button_at_latest(3, false, &mut sequence, 16)
                .unwrap(),
        );

        assert_eq!(
            emitted
                .iter()
                .map(RegionInputWireMessage::input_type)
                .collect::<Vec<_>>(),
            vec![
                "region_pointer_enter",
                "region_pointer_motion",
                "region_pointer_button",
                "region_pointer_scroll",
                "region_pointer_leave",
                "region_pointer_enter",
                "region_pointer_button",
                "region_pen_event",
                "region_pointer_button",
                "region_pointer_button",
            ],
        );

        let mut host = RegionInputPipeline::new(runtime.applied_regions().clone(), IdentityMapper);
        let transformer = RegionCoordinateTransformer::new(runtime.applied_regions());
        let mut previous_sequence = 0;
        for message in &emitted {
            let metadata = message.metadata();
            assert!(
                metadata.sequence > previous_sequence,
                "region sequences must stay strictly increasing",
            );
            previous_sequence = metadata.sequence;
            assert_eq!(
                message.position().region_generation,
                runtime.generation().get(),
            );
            let mapped = host.apply(message.as_ref()).unwrap();
            let expected = transformer
                .logical_to_applied(
                    RegionId::new(message.position().region_id).unwrap(),
                    LogicalPoint::new(message.position().logical_x, message.position().logical_y),
                )
                .unwrap();
            assert_eq!(mapped_point(mapped), expected);
        }

        // The client's session-global counter, the client emitter state, and
        // the host state all agree on the same accepted sequence.
        assert_eq!(sequence, previous_sequence);
        assert_eq!(runtime.input_state().last_sequence(), previous_sequence);
        assert_eq!(host.state().last_sequence(), previous_sequence);
        assert_eq!(
            runtime.input_state().latest_pointer_position(),
            host.state().latest_pointer_position(),
        );
        assert_eq!(
            runtime.input_state().held_buttons().collect::<Vec<_>>(),
            host.state().held_buttons().collect::<Vec<_>>(),
        );
        assert_eq!(
            runtime.input_state().is_focused(),
            host.state().is_focused()
        );
        assert_eq!(
            runtime.input_state().pen().map(|pen| pen.sample),
            host.state().pen().map(|pen| pen.sample),
        );

        runtime.release_all();
        assert!(runtime.input_state().held_buttons().next().is_none());
        assert!(!runtime.input_state().is_focused());
        assert_eq!(
            runtime.input_state().last_sequence(),
            previous_sequence,
            "release-all must never rewind the accepted sequence",
        );
    }

    /// Parity gate for the deleted live secondary affine path: the region
    /// transformer places a secondary viewport's pointer on exactly the
    /// desktop pixel `map_local_fraction_to_wire_pointer` used to derive, so
    /// removing that second mapping moved no cursor.
    #[test]
    fn secondary_region_mapping_matches_the_removed_affine_desktop_mapping() {
        let mut runtime = mixed_runtime();
        let viewport = runtime
            .viewport_for_monitor(SessionMonitorId::new(2).unwrap())
            .unwrap();
        let monitor_rect = MonitorDesktopRect {
            x: -2560,
            y: 0,
            width_px: 2560,
            height_px: 1440,
        };
        let desktop = DesktopRect {
            x: -2560,
            y: 0,
            width_px: 6160,
            height_px: 2338,
        };
        let transformer_regions = runtime.applied_regions().clone();
        let transformer = RegionCoordinateTransformer::new(&transformer_regions);
        let mut sequence = 0;

        for (index, fraction) in [(0.0, 0.0), (0.25, 0.75), (0.5, 0.5), (1.0, 1.0)]
            .into_iter()
            .enumerate()
        {
            let messages = runtime
                .pointer_motion(viewport, fraction, &mut sequence, 100 + index as u64)
                .unwrap();
            let position = messages.last().unwrap().position();
            let applied = transformer
                .logical_to_applied(
                    RegionId::new(position.region_id).unwrap(),
                    LogicalPoint::new(position.logical_x, position.logical_y),
                )
                .unwrap();
            let (_, _, server_x, server_y) =
                map_local_fraction_to_wire_pointer(fraction, monitor_rect, desktop)
                    .expect("the legacy affine mapping is defined for this rectangle");

            assert_eq!(
                (applied.x, applied.y),
                (i64::from(server_x), i64::from(server_y)),
                "region mapping diverged from the removed affine mapping at {fraction:?}",
            );
        }
    }

    #[test]
    fn scroll_delta_must_fit_the_host_i32_contract_before_state_or_sequence_advance() {
        let mut runtime = mixed_runtime();
        let viewport = runtime.primary_viewport();
        let mut sequence = 0;

        assert!(matches!(
            runtime.pointer_scroll(viewport, (0.5, 0.5), i32::MAX, 0, &mut sequence, 10),
            Err(DeckRegionRuntimeError::ScrollDeltaOverflow(_)),
        ));
        assert_eq!(sequence, 0);
        assert!(!runtime.input_state().is_focused());

        let maximum_ticks = i32::MAX / 120;
        let messages = runtime
            .pointer_scroll(viewport, (0.5, 0.5), maximum_ticks, 0, &mut sequence, 11)
            .unwrap();
        let RegionInputWireMessage::PointerScroll(message) = messages.last().unwrap() else {
            panic!("last message must be the bounded scroll");
        };
        assert_eq!(message.delta_x, i64::from(maximum_ticks) * 120);
        assert!(i32::try_from(message.delta_x).is_ok());
    }

    #[test]
    fn mixed_retina_and_non_retina_sizes_keep_logical_and_media_facts_distinct() {
        let runtime = mixed_runtime();
        let primary = runtime.requested_regions().primary();
        let secondary = runtime
            .requested_regions()
            .regions()
            .iter()
            .find(|region| !region.is_primary())
            .unwrap();

        assert_eq!(primary.scale().get(), 240);
        assert_eq!(primary.logical_rect().size().width(), 1800 * 120);
        assert_eq!(primary.physical_size().width(), 3600);
        assert_eq!(primary.physical_size().height(), 2338);
        assert_eq!(secondary.scale().get(), 120);
        assert_eq!(secondary.logical_rect().size().width(), 2560 * 120);
        assert_eq!(secondary.physical_size().width(), 2560);
    }

    #[test]
    fn deck_regions_use_already_compositor_oriented_convention() {
        let monitor_id = SessionMonitorId::new(1).unwrap();
        let requested = RequestedMonitorTopology::new(vec![RequestedMonitor::new(
            Monitor {
                identity: MonitorIdentity {
                    id: "33".to_owned(),
                    name: "Rotated Display".to_owned(),
                    vendor: 1,
                    model: 2,
                    serial: 33,
                },
                x: 0,
                y: 0,
                width_px: 1920,
                height_px: 1080,
                scale: 1.0,
                refresh_hz: 60,
                rotation: arcen_media::Rotation::Degrees90,
                primary: true,
                width_mm: 0.0,
                height_mm: 0.0,
            },
            1080,
            1920,
        )
        .unwrap()])
        .unwrap();
        let validated = ValidatedAppliedTopology {
            generation: TopologyGeneration::new(8).unwrap(),
            carrier: MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors: vec![ResolvedAppliedMonitor {
                session_monitor_id: monitor_id,
                cg_display_id: 33,
                rect: MonitorDesktopRect {
                    x: 0,
                    y: 0,
                    width_px: 1080,
                    height_px: 1920,
                },
            }],
            media_roster: Box::new(
                RegionMediaRoster::new(vec![media_plan(1, 88, 1080, 1920)]).unwrap(),
            ),
            desktop: DesktopRect {
                x: 0,
                y: 0,
                width_px: 1080,
                height_px: 1920,
            },
        };
        let runtime =
            DeckRegionRuntime::from_validated_topology(&validated, Some(&requested)).unwrap();
        let region = runtime.requested_regions().primary();

        // Deck AlreadyCompositorOriented: the applied roster/media extent
        // has already absorbed the host transform, so legacy panel rotation
        // metadata must not rotate the stream a second time.
        assert_eq!(region.transform(), OutputTransform::Normal);
        assert_eq!(
            region.physical_size(),
            PhysicalSize::new(1080, 1920).unwrap()
        );
        assert_eq!(
            region.expected_applied_size().unwrap(),
            AppliedSize::new(1080, 1920).unwrap()
        );
        assert_eq!(
            runtime.applied_regions().primary().applied_rect().size(),
            AppliedSize::new(1080, 1920).unwrap()
        );
    }

    #[test]
    fn negative_layout_maps_secondary_corners_without_root_space_divergence() {
        let mut runtime = mixed_runtime();
        let viewport = runtime
            .viewport_for_monitor(SessionMonitorId::new(2).unwrap())
            .unwrap();
        let mut sequence = 0;
        let top_left = runtime
            .pointer_motion(viewport, (0.0, 0.0), &mut sequence, 10)
            .unwrap();
        let top_left = top_left.last().unwrap();
        let top_left_position = top_left.position();
        let top_left_wire = top_left.to_json_value().unwrap();
        let bottom_right = runtime
            .pointer_motion(viewport, (1.0, 1.0), &mut sequence, 11)
            .unwrap();
        let bottom_right = bottom_right.last().unwrap();
        let bottom_right_position = bottom_right.position();
        let bottom_right_wire = bottom_right.to_json_value().unwrap();
        let transformer = RegionCoordinateTransformer::new(runtime.applied_regions());

        assert_eq!(
            transformer
                .logical_to_applied(
                    RegionId::new(top_left_position.region_id).unwrap(),
                    LogicalPoint::new(top_left_position.logical_x, top_left_position.logical_y),
                )
                .unwrap(),
            AppliedPoint::new(-2560, 0),
        );
        assert_eq!(
            transformer
                .logical_to_applied(
                    RegionId::new(bottom_right_position.region_id).unwrap(),
                    LogicalPoint::new(
                        bottom_right_position.logical_x,
                        bottom_right_position.logical_y,
                    ),
                )
                .unwrap(),
            AppliedPoint::new(-1, 1439),
        );
        for wire in [top_left_wire, bottom_right_wire] {
            assert_eq!(wire["region_id"], 2);
            assert!(wire.get("server_x").is_none());
            assert!(wire.get("server_y").is_none());
        }
    }

    #[test]
    fn wacom_pen_sample_routes_through_its_secondary_region() {
        let mut runtime = mixed_runtime();
        let viewport = runtime
            .viewport_for_monitor(SessionMonitorId::new(2).unwrap())
            .unwrap();
        let event = PenEvent {
            x: 0.25,
            y: 0.75,
            pressure: 0.6,
            tilt_x_degrees: 12.0,
            tilt_y_degrees: -8.0,
            rotation_degrees: 30.0,
            tool: PenTool::Tip,
            in_proximity: true,
            touching: true,
            buttons: 2,
            metadata: LowLatencyMetadata {
                sequence: 1,
                timestamp_ns: 99,
                coalescable: true,
            },
        };

        let message = runtime.pen(viewport, event).unwrap();
        let position = message.position();
        assert_eq!(position.region_id, 2);
        assert_eq!(
            RegionCoordinateTransformer::new(runtime.applied_regions())
                .logical_to_applied(
                    RegionId::new(position.region_id).unwrap(),
                    LogicalPoint::new(position.logical_x, position.logical_y),
                )
                .unwrap(),
            AppliedPoint::new(-1920, 1079),
        );
        let wire = message.to_json_value().unwrap();
        assert_eq!(wire["type"], "region_pen_event");
        assert_eq!(wire["region_id"], 2);
        assert!(wire.get("server_x").is_none());
        assert!(wire.get("server_y").is_none());
    }
}
