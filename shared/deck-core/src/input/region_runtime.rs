// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
//!
//! Shared by every Deck (moved from the macOS Deck): a platform hands its
//! applied topology through [`AppliedTopologyView`].

// Moved verbatim from the macOS Deck (ui/region_runtime.rs); the strict
// shared lints are tightened over time, as for the session engine.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_input::{
    PenEvent, RegionCoordinateTransformer, RegionInputEmitError, RegionInputEmitter,
    RegionInputState, RegionInputStateError, RegionInputWireMessage, RegionLogicalPosition,
    RegionPenSample, ScrollPhase, ScrollUnit,
};
use arcen_media::{
    AppliedPoint, AppliedRect, AppliedRegionSet, AppliedSize, LogicalPoint, LogicalRect,
    LogicalSize, OutputIdentity, PhysicalSize, RegionContractError, RegionGeneration, RegionId,
    RegionPlacement, RegionSet, RequestedMonitorTopology, Rotation, Scale120, SessionMonitorId,
    TransformConvention,
};

use arcen_media::{MonitorDesktopRect, RegionMediaRoster, TopologyGeneration};
use arcen_protocol::messages::{
    MouseButtonMsg, MouseMoveMsg, MouseScrollMsg, PenEventMsg, PointerMotionMode,
    RegionInputPositionMsg, RegionInputValidationError,
};

const LEGACY_REGION_GENERATION: u64 = 1;
const LEGACY_PRIMARY_REGION_ID: u32 = 1;

/// Explicit shared transform convention for Deck: the negotiated
/// multi-monitor-v1 stream arrives already compositor oriented, so its
/// width/height *are* the transform-normal footprint and informational legacy
/// panel-rotation metadata must never rotate it a second time.
const TRANSFORM_CONVENTION: TransformConvention = TransformConvention::AlreadyCompositorOriented;

/// The facts of a host-applied, validated multi-monitor topology the region
/// runtime needs; each Deck's projection of
/// [`arcen_media::ValidatedAppliedTopology`] provides them.
pub trait AppliedTopologyView {
    /// The committed topology generation.
    fn topology_generation(&self) -> TopologyGeneration;
    /// Every applied monitor, negotiated primary first: its session monitor
    /// id, the local output identity it resolved to (as text), and its
    /// applied desktop rectangle.
    fn applied_monitors(&self) -> Vec<(SessionMonitorId, String, MonitorDesktopRect)>;
    /// The host-authoritative per-monitor media roster.
    fn media_roster(&self) -> &RegionMediaRoster;
}

impl<D: std::fmt::Display> AppliedTopologyView for arcen_media::ValidatedAppliedTopology<D> {
    fn topology_generation(&self) -> TopologyGeneration {
        self.generation
    }

    fn applied_monitors(&self) -> Vec<(SessionMonitorId, String, MonitorDesktopRect)> {
        self.monitors
            .iter()
            .map(|monitor| {
                (
                    monitor.session_monitor_id,
                    monitor.native_display_id.to_string(),
                    monitor.rect,
                )
            })
            .collect()
    }

    fn media_roster(&self) -> &RegionMediaRoster {
        &self.media_roster
    }
}

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
                    unit: message.unit,
                    phase: message.phase,
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
        validated: &impl AppliedTopologyView,
        requested: Option<&RequestedMonitorTopology>,
    ) -> Result<Self, DeckRegionRuntimeError> {
        let generation = RegionGeneration::new(validated.topology_generation().get())?;
        let monitors = validated.applied_monitors();
        let mut placements = Vec::with_capacity(monitors.len());
        let mut monitor_ids = Vec::with_capacity(monitors.len());

        for (index, (session_monitor_id, output, rect)) in monitors.into_iter().enumerate() {
            let requested_monitor = requested.and_then(|topology| {
                topology
                    .monitors()
                    .iter()
                    .find(|candidate| candidate.monitor().identity.id == output)
            });
            if requested.is_some() && requested_monitor.is_none() {
                return Err(DeckRegionRuntimeError::MissingRequestedOutput(output));
            }
            let (logical_origin, logical_size, scale, rotation) = if let Some(requested_monitor) =
                requested_monitor
            {
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
                    LogicalPoint::from_pixels(rect.x, rect.y)?,
                    LogicalSize::from_pixels(u64::from(rect.width_px), u64::from(rect.height_px))?,
                    Scale120::new(120)?,
                    Rotation::Degrees0,
                )
            };
            // Window/input geometry consumes only the roster's negotiated
            // physical extent. Stream epochs remain owned by media framing
            // and are validated from each carrier-supplied frame header.
            let media_plan = validated
                .media_roster()
                .plan(session_monitor_id)
                .ok_or(DeckRegionRuntimeError::MissingMediaPlan(session_monitor_id))?;
            if media_plan.width != rect.width_px || media_plan.height != rect.height_px {
                return Err(DeckRegionRuntimeError::MediaPlanSizeMismatch {
                    monitor_id: session_monitor_id,
                    media: (media_plan.width, media_plan.height),
                    applied: (rect.width_px, rect.height_px),
                });
            }
            placements.push(RegionPlacement {
                region_id: RegionId::new(u32::from(session_monitor_id.get()))?,
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
                    AppliedPoint::new(rect.x, rect.y),
                    AppliedSize::new(rect.width_px, rect.height_px)?,
                )?,
            });
            monitor_ids.push(session_monitor_id);
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

    /// Emits a continuous scroll whose deltas are already in wire units — the
    /// same fixed point [`Self::pointer_scroll`] produces from whole ticks,
    /// but carrying a trackpad's fractional travel instead of rounding it to
    /// notches.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::pointer_scroll`].
    pub fn pointer_scroll_units(
        &mut self,
        viewport: RegionViewport,
        local_fraction: (f64, f64),
        delta_x_units: i64,
        delta_y_units: i64,
        sequence: &mut u64,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, DeckRegionRuntimeError> {
        self.pointer_scroll_units_with_details(
            viewport,
            local_fraction,
            delta_x_units,
            delta_y_units,
            ScrollUnit::Line,
            ScrollPhase::None,
            sequence,
            timestamp_ns,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pointer_scroll_units_with_details(
        &mut self,
        viewport: RegionViewport,
        local_fraction: (f64, f64),
        delta_x_units: i64,
        delta_y_units: i64,
        unit: ScrollUnit,
        phase: ScrollPhase,
        sequence: &mut u64,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, DeckRegionRuntimeError> {
        let bound = |delta: i64| {
            i32::try_from(delta)
                .map(i64::from)
                .map_err(|_| DeckRegionRuntimeError::ScrollDeltaOverflow(delta))
        };
        let delta_x = bound(delta_x_units)?;
        let delta_y = bound(delta_y_units)?;
        let position = self.logical_position(viewport, local_fraction)?;
        self.emit(sequence, |emitter, regions| {
            emitter.pointer_scroll_with_details(
                regions,
                position,
                delta_x,
                delta_y,
                unit,
                phase,
                timestamp_ns,
            )
        })
    }

    /// Emits one pen sample through its authoritative region.
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
