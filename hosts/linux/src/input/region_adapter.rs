//! Shared-region input adapter for the Linux Xorg virtual raster.
//!
//! Validation, wire-to-domain conversion, ordered state, and the region
//! coordinate transform all live in the shared [`RegionInputPipeline`]. This
//! module contributes only the Linux-specific step: turning one applied pixel
//! index into the integer `ABS_X`/`ABS_Y` axes declared by the uinput devices,
//! and proving the committed [`LinuxTopologyPlan`] fits the declared Xorg
//! raster. No monitor-local normalized coordinate path exists here.

#[cfg(test)]
use arcen_input::RegionLogicalPosition;
use arcen_input::{
    validate_aggregate_parity, CoordinateTransformError, RegionAggregateParityError,
    RegionInputPipeline, RegionInputPipelineError, RegionInputState, RegionInputStateError,
    RegionPointMapper, ReleasedRegionInput,
};
use arcen_media::{AppliedPoint, AppliedRegionSet, RegionContractError, RegionId, RegionSet};
use arcen_protocol::messages::{
    RegionInputValidationError, RegionPenEventMsg, RegionPointerButtonMsg, RegionPointerEnterMsg,
    RegionPointerLeaveMsg, RegionPointerMotionMsg, RegionPointerScrollMsg,
};
use thiserror::Error;

use crate::display::topology::{LinuxTopologyError, LinuxTopologyPlan};

/// Integer absolute-axis position inside the committed Xorg virtual raster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XorgAxisPoint {
    pub x: i32,
    pub y: i32,
}

/// Checked region-scoped pointer button transition ready for uinput.
pub type MappedRegionButton = arcen_input::MappedRegionButton<XorgAxisPoint>;

/// Checked region-scoped scroll sample ready for uinput.
pub type MappedRegionScroll = arcen_input::MappedRegionScroll<XorgAxisPoint>;

/// Checked region-scoped pen sample ready for Linux tablet-axis conversion.
pub type MappedRegionPen = arcen_input::MappedRegionPen<XorgAxisPoint>;

/// The only Linux-specific step of the shared region input pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct XorgRasterMapper {
    width: u32,
    height: u32,
}

impl RegionPointMapper for XorgRasterMapper {
    type Point = XorgAxisPoint;
    type Error = XorgRasterError;

    fn map_applied(&self, point: AppliedPoint) -> Result<XorgAxisPoint, XorgRasterError> {
        if point.x < 0
            || point.y < 0
            || i128::from(point.x) >= i128::from(self.width)
            || i128::from(point.y) >= i128::from(self.height)
        {
            return Err(XorgRasterError::PointOutsideRaster {
                point,
                width: self.width,
                height: self.height,
            });
        }
        Ok(XorgAxisPoint {
            x: i32::try_from(point.x).map_err(|_| XorgRasterError::AxisOverflow(point))?,
            y: i32::try_from(point.y).map_err(|_| XorgRasterError::AxisOverflow(point))?,
        })
    }
}

/// Stateful adapter from shared region input to one Linux Xorg virtual raster.
#[derive(Debug, Clone)]
pub struct RegionInputAdapter {
    requested_regions: RegionSet,
    pipeline: RegionInputPipeline<XorgRasterMapper>,
}

impl RegionInputAdapter {
    /// Builds one adapter from the exact topology committed for this desktop.
    ///
    /// # Errors
    ///
    /// Returns an error when the topology cannot satisfy the shared region
    /// contract or any applied output lies outside the declared Xorg raster.
    pub fn from_plan(plan: &LinuxTopologyPlan) -> Result<Self, RegionAdapterError> {
        let (requested_regions, applied_regions) = plan.region_sets()?;
        validate_aggregate_parity(&requested_regions, &applied_regions)?;
        validate_raster(&applied_regions, plan.virtual_width, plan.virtual_height)?;
        Ok(Self {
            requested_regions,
            pipeline: RegionInputPipeline::new(
                applied_regions,
                XorgRasterMapper {
                    width: plan.virtual_width,
                    height: plan.virtual_height,
                },
            ),
        })
    }

    #[must_use]
    pub const fn requested_regions(&self) -> &RegionSet {
        &self.requested_regions
    }

    #[must_use]
    pub const fn applied_regions(&self) -> &AppliedRegionSet {
        self.pipeline.applied_regions()
    }

    #[must_use]
    pub const fn state(&self) -> &RegionInputState {
        self.pipeline.state()
    }

    #[must_use]
    pub const fn raster_size(&self) -> (u32, u32) {
        let mapper = self.pipeline.mapper();
        (mapper.width, mapper.height)
    }

    /// Clears shared focus/button/pen state and returns the releases mirrored
    /// by `InputController::reset_held` on the native devices.
    #[must_use]
    pub fn release_all(&mut self) -> ReleasedRegionInput {
        self.pipeline.release_all()
    }

    /// Applies and maps a region pointer-enter transition.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or raster validation error.
    pub fn pointer_enter(
        &mut self,
        message: &RegionPointerEnterMsg,
    ) -> Result<XorgAxisPoint, RegionAdapterError> {
        Ok(self.pipeline.pointer_enter(message)?)
    }

    /// Applies and maps a region pointer-leave transition.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or raster validation error.
    pub fn pointer_leave(
        &mut self,
        message: &RegionPointerLeaveMsg,
    ) -> Result<XorgAxisPoint, RegionAdapterError> {
        Ok(self.pipeline.pointer_leave(message)?)
    }

    /// Applies and maps region-local absolute pointer motion.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or raster validation error.
    pub fn pointer_motion(
        &mut self,
        message: &RegionPointerMotionMsg,
    ) -> Result<XorgAxisPoint, RegionAdapterError> {
        Ok(self.pipeline.pointer_motion(message)?)
    }

    /// Applies a button edge only when its logical position is exactly the
    /// latest accepted pointer position, then maps that position for uinput.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or raster validation error.
    pub fn pointer_button(
        &mut self,
        message: &RegionPointerButtonMsg,
    ) -> Result<MappedRegionButton, RegionAdapterError> {
        Ok(self.pipeline.pointer_button(message)?)
    }

    /// Applies a scroll sample at the exact latest pointer position.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or raster validation error.
    pub fn pointer_scroll(
        &mut self,
        message: &RegionPointerScrollMsg,
    ) -> Result<MappedRegionScroll, RegionAdapterError> {
        Ok(self.pipeline.pointer_scroll(message)?)
    }

    /// Applies and maps a full region-scoped pen sample.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or raster validation error.
    pub fn pen(
        &mut self,
        message: &RegionPenEventMsg,
    ) -> Result<MappedRegionPen, RegionAdapterError> {
        Ok(self.pipeline.pen(message)?)
    }

    #[cfg(test)]
    fn map(&self, position: RegionLogicalPosition) -> Result<XorgAxisPoint, RegionAdapterError> {
        Ok(self.pipeline.map(position)?)
    }
}

fn validate_raster(
    regions: &AppliedRegionSet,
    width: u32,
    height: u32,
) -> Result<(), RegionAdapterError> {
    if width == 0 || height == 0 {
        return Err(RegionAdapterError::EmptyRaster(width, height));
    }
    for region in regions.regions() {
        let rect = region.applied_rect();
        let origin = rect.origin();
        let right = i128::from(origin.x) + i128::from(rect.size().width());
        let bottom = i128::from(origin.y) + i128::from(rect.size().height());
        if origin.x < 0 || origin.y < 0 || right > i128::from(width) || bottom > i128::from(height)
        {
            return Err(RegionAdapterError::RegionOutsideRaster(region.id()));
        }
    }
    Ok(())
}

/// Linux-specific mapping failure for one applied pixel index.
#[derive(Debug, Error)]
pub enum XorgRasterError {
    #[error("mapped point {point:?} lies outside the Xorg input raster {width}x{height}")]
    PointOutsideRaster {
        point: AppliedPoint,
        width: u32,
        height: u32,
    },
    #[error("mapped point {0:?} cannot be represented by Linux absolute axes")]
    AxisOverflow(AppliedPoint),
}

/// Region-to-Xorg adapter failure.
#[derive(Debug, Error)]
pub enum RegionAdapterError {
    #[error(transparent)]
    Topology(#[from] LinuxTopologyError),
    #[error(transparent)]
    Wire(RegionInputValidationError),
    #[error(transparent)]
    Contract(RegionContractError),
    #[error(transparent)]
    State(RegionInputStateError),
    #[error(transparent)]
    Transform(CoordinateTransformError),
    #[error("requested and applied shared region aggregates do not match")]
    AggregateMismatch,
    #[error("Xorg input raster is empty ({0}x{1})")]
    EmptyRaster(u32, u32),
    #[error("applied region {0:?} lies outside the Xorg input raster")]
    RegionOutsideRaster(RegionId),
    #[error("mapped point {point:?} lies outside the Xorg input raster {width}x{height}")]
    PointOutsideRaster {
        point: AppliedPoint,
        width: u32,
        height: u32,
    },
    #[error("mapped point {0:?} cannot be represented by Linux absolute axes")]
    AxisOverflow(AppliedPoint),
}

impl From<XorgRasterError> for RegionAdapterError {
    fn from(value: XorgRasterError) -> Self {
        match value {
            XorgRasterError::PointOutsideRaster {
                point,
                width,
                height,
            } => Self::PointOutsideRaster {
                point,
                width,
                height,
            },
            XorgRasterError::AxisOverflow(point) => Self::AxisOverflow(point),
        }
    }
}

impl From<RegionAggregateParityError> for RegionAdapterError {
    fn from(_: RegionAggregateParityError) -> Self {
        Self::AggregateMismatch
    }
}

impl From<RegionInputPipelineError<XorgRasterError>> for RegionAdapterError {
    fn from(value: RegionInputPipelineError<XorgRasterError>) -> Self {
        match value {
            RegionInputPipelineError::Wire(error) => Self::Wire(error),
            RegionInputPipelineError::Contract(error) => Self::Contract(error),
            RegionInputPipelineError::State(error) => Self::State(error),
            RegionInputPipelineError::Transform(error) => Self::Transform(error),
            RegionInputPipelineError::Mapping(error) => Self::from(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_input::{domain_position, PenTool};
    use arcen_media::{
        LogicalPoint, Monitor, MonitorIdentity, RequestedMonitor, RequestedMonitorTopology,
        Rotation, LOGICAL_UNITS_PER_PIXEL,
    };
    use arcen_protocol::messages::{PenToolMsg, RegionInputMetadataMsg, RegionInputPositionMsg};

    use crate::display::topology::{plan_topology, HeadInventory};

    const CROSS_COMPONENT_BASELINE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/runtime/region_input.json"
    ));

    fn requested_monitor(
        id: &str,
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
                    id: id.to_owned(),
                    name: id.to_owned(),
                    ..MonitorIdentity::default()
                },
                x,
                y,
                width_px: physical_width,
                height_px: physical_height,
                scale,
                refresh_hz: 60,
                rotation: Rotation::Degrees0,
                primary,
                width_mm: 0.0,
                height_mm: 0.0,
            },
            logical_width,
            logical_height,
        )
        .expect("requested monitor")
    }

    fn plan() -> LinuxTopologyPlan {
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor("left", -1024, -120, 1024, 768, 1280, 960, 1.25, false),
            requested_monitor("main", 0, 0, 1920, 1080, 1920, 1080, 1.0, true),
        ])
        .expect("requested topology");
        let heads = HeadInventory::uniform(["DFP-0", "DFP-1"]).expect("head inventory");
        plan_topology(
            &requested,
            arcen_media::TopologyGeneration::new(7).expect("generation"),
            &heads,
        )
        .expect("Linux topology")
    }

    fn baseline_plan() -> LinuxTopologyPlan {
        let requested = RequestedMonitorTopology::new(vec![requested_monitor(
            "baseline", 0, 0, 1_920, 1_080, 1_920, 1_080, 1.0, true,
        )])
        .expect("requested topology");
        let heads = HeadInventory::uniform(["DFP-0"]).expect("head inventory");
        plan_topology(
            &requested,
            arcen_media::TopologyGeneration::new(7).expect("generation"),
            &heads,
        )
        .expect("Linux topology")
    }

    fn region_id(plan: &LinuxTopologyPlan, display_id: &str) -> u32 {
        u32::from(
            plan.monitors
                .iter()
                .find(|monitor| monitor.client_display_id == display_id)
                .expect("planned monitor")
                .session_monitor_id
                .get(),
        )
    }

    const fn metadata(sequence: u64) -> RegionInputMetadataMsg {
        RegionInputMetadataMsg {
            sequence,
            timestamp_ns: 0,
            coalescable: false,
        }
    }

    const fn position(
        generation: u64,
        region_id: u32,
        logical_x: i64,
        logical_y: i64,
    ) -> RegionInputPositionMsg {
        RegionInputPositionMsg {
            region_generation: generation,
            region_id,
            logical_x,
            logical_y,
        }
    }

    fn enter(
        generation: u64,
        region_id: u32,
        logical_x: i64,
        logical_y: i64,
        sequence: u64,
    ) -> RegionPointerEnterMsg {
        RegionPointerEnterMsg {
            msg_type: arcen_protocol::messages::REGION_POINTER_ENTER.to_owned(),
            position: position(generation, region_id, logical_x, logical_y),
            metadata: metadata(sequence),
        }
    }

    fn motion(
        generation: u64,
        region_id: u32,
        logical_x: i64,
        logical_y: i64,
        sequence: u64,
    ) -> RegionPointerMotionMsg {
        RegionPointerMotionMsg {
            msg_type: arcen_protocol::messages::REGION_POINTER_MOTION.to_owned(),
            position: position(generation, region_id, logical_x, logical_y),
            metadata: metadata(sequence),
        }
    }

    #[test]
    fn cross_component_fixture_freezes_linux_decode_state_and_endpoint() {
        let baseline: serde_json::Value = serde_json::from_str(CROSS_COMPONENT_BASELINE).unwrap();
        let mut adapter = RegionInputAdapter::from_plan(&baseline_plan()).expect("adapter");
        let pointer_endpoint = XorgAxisPoint {
            x: i32::try_from(baseline["pointer_endpoint"]["x"].as_i64().unwrap()).unwrap(),
            y: i32::try_from(baseline["pointer_endpoint"]["y"].as_i64().unwrap()).unwrap(),
        };
        let pen_endpoint = XorgAxisPoint {
            x: i32::try_from(baseline["pen_endpoint"]["x"].as_i64().unwrap()).unwrap(),
            y: i32::try_from(baseline["pen_endpoint"]["y"].as_i64().unwrap()).unwrap(),
        };

        for event in baseline["events"].as_array().unwrap() {
            match event["type"].as_str().unwrap() {
                "region_pointer_enter" => {
                    let message: RegionPointerEnterMsg =
                        serde_json::from_value(event.clone()).unwrap();
                    assert_eq!(
                        adapter.pointer_enter(&message).unwrap(),
                        XorgAxisPoint { x: 0, y: 0 }
                    );
                }
                "region_pointer_motion" => {
                    let message: RegionPointerMotionMsg =
                        serde_json::from_value(event.clone()).unwrap();
                    assert_eq!(adapter.pointer_motion(&message).unwrap(), pointer_endpoint);
                }
                "region_pointer_button" => {
                    let message: RegionPointerButtonMsg =
                        serde_json::from_value(event.clone()).unwrap();
                    let mapped = adapter.pointer_button(&message).unwrap();
                    assert_eq!(mapped.position, pointer_endpoint);
                    assert_eq!((mapped.button, mapped.pressed), (1, true));
                }
                "region_pointer_scroll" => {
                    let message: RegionPointerScrollMsg =
                        serde_json::from_value(event.clone()).unwrap();
                    let mapped = adapter.pointer_scroll(&message).unwrap();
                    assert_eq!(mapped.position, pointer_endpoint);
                    assert_eq!((mapped.delta_x, mapped.delta_y), (120, -240));
                }
                "region_pen_event" => {
                    let message: RegionPenEventMsg = serde_json::from_value(event.clone()).unwrap();
                    let mapped = adapter.pen(&message).unwrap();
                    assert_eq!(mapped.position, pen_endpoint);
                    assert_eq!(mapped.sample.tool, PenTool::Tip);
                    assert_eq!(mapped.sample.pressure, 0.75);
                }
                "region_pointer_leave" => {
                    let message: RegionPointerLeaveMsg =
                        serde_json::from_value(event.clone()).unwrap();
                    assert_eq!(adapter.pointer_leave(&message).unwrap(), pointer_endpoint);
                }
                event_type => panic!("unexpected baseline event {event_type}"),
            }
        }

        assert_eq!(
            adapter.state().latest_pointer_position(),
            Some(RegionLogicalPosition {
                region_id: RegionId::new(1).unwrap(),
                point: LogicalPoint::new(60, 1_440),
            })
        );
        assert_eq!(adapter.state().held_buttons().collect::<Vec<_>>(), vec![1]);
        assert_eq!(
            adapter.state().pen().unwrap().sample.position.point,
            LogicalPoint::new(600, 900)
        );
        assert_eq!(adapter.state().last_sequence(), 45);
        assert!(!adapter.state().is_focused());
    }

    #[test]
    fn mixed_scale_negative_layout_maps_corners_and_centers_through_shared_regions() {
        let plan = plan();
        let left_id = region_id(&plan, "left");
        let main_id = region_id(&plan, "main");
        let mut adapter = RegionInputAdapter::from_plan(&plan).expect("adapter");

        let left = adapter
            .requested_regions()
            .get(RegionId::new(left_id).expect("left id"))
            .expect("left region");
        assert_eq!(
            left.logical_rect().origin(),
            LogicalPoint::from_pixels(-1024, -120).expect("logical origin")
        );
        assert_eq!(left.scale().get(), 150);

        let left_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "left")
            .expect("left plan");
        let main_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "main")
            .expect("main plan");
        let left_last_x = i64::from(1024) * LOGICAL_UNITS_PER_PIXEL - 1;
        let left_last_y = i64::from(768) * LOGICAL_UNITS_PER_PIXEL - 1;
        let top_left = adapter
            .pointer_enter(&enter(7, left_id, 0, 0, 1))
            .expect("left top-left");
        assert_eq!(
            top_left,
            XorgAxisPoint {
                x: left_plan.x,
                y: left_plan.y,
            }
        );
        let bottom_right = adapter
            .pointer_motion(&motion(7, left_id, left_last_x, left_last_y, 2))
            .expect("left bottom-right");
        assert_eq!(
            bottom_right,
            XorgAxisPoint {
                x: left_plan.x + i32::try_from(left_plan.width).expect("width") - 1,
                y: left_plan.y + i32::try_from(left_plan.height).expect("height") - 1,
            }
        );

        adapter
            .pointer_leave(&RegionPointerLeaveMsg {
                msg_type: arcen_protocol::messages::REGION_POINTER_LEAVE.to_owned(),
                position: position(7, left_id, left_last_x, left_last_y),
                metadata: metadata(3),
            })
            .expect("leave left");
        let main_center_x = i64::from(1920) * LOGICAL_UNITS_PER_PIXEL / 2;
        let main_center_y = i64::from(1080) * LOGICAL_UNITS_PER_PIXEL / 2;
        let center = adapter
            .pointer_enter(&enter(7, main_id, main_center_x, main_center_y, 4))
            .expect("main center");
        assert!((center.x - (main_plan.x + 960)).abs() <= 1);
        assert!((center.y - (main_plan.y + 540)).abs() <= 1);
    }

    #[test]
    fn shared_transform_matches_removed_monitor_normalization_at_key_points() {
        let plan = plan();
        let left = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "left")
            .expect("left monitor");
        let left_id = u32::from(left.session_monitor_id.get());
        let mut adapter = RegionInputAdapter::from_plan(&plan).expect("adapter");
        let logical_width = left.logical_rect.size().width();
        let logical_height = left.logical_rect.size().height();

        for (sequence, x, y) in [
            (1, 0, 0),
            (
                2,
                i64::try_from((logical_width - 1) / 2).expect("center x"),
                i64::try_from((logical_height - 1) / 2).expect("center y"),
            ),
            (
                3,
                i64::try_from(logical_width - 1).expect("last x"),
                i64::try_from(logical_height - 1).expect("last y"),
            ),
        ] {
            let mapped = if sequence == 1 {
                adapter
                    .pointer_enter(&enter(7, left_id, x, y, sequence))
                    .expect("enter")
            } else {
                adapter
                    .pointer_motion(&motion(7, left_id, x, y, sequence))
                    .expect("motion")
            };
            let expected_x = i64::from(left.x)
                + rounded_ratio(
                    u128::try_from(x).expect("nonnegative x") * u128::from(left.width - 1),
                    u128::from(logical_width - 1),
                );
            let expected_y = i64::from(left.y)
                + rounded_ratio(
                    u128::try_from(y).expect("nonnegative y") * u128::from(left.height - 1),
                    u128::from(logical_height - 1),
                );
            assert_eq!(i64::from(mapped.x), expected_x);
            assert_eq!(i64::from(mapped.y), expected_y);
        }
    }

    #[test]
    fn button_requires_and_preserves_the_exact_authoritative_position() {
        let plan = plan();
        let main_id = region_id(&plan, "main");
        let mut adapter = RegionInputAdapter::from_plan(&plan).expect("adapter");
        adapter
            .pointer_enter(&enter(7, main_id, 12_000, 24_000, 1))
            .expect("enter");

        let mismatch = RegionPointerButtonMsg {
            msg_type: arcen_protocol::messages::REGION_POINTER_BUTTON.to_owned(),
            position: position(7, main_id, 12_001, 24_000),
            button: 1,
            pressed: true,
            metadata: metadata(2),
        };
        assert!(matches!(
            adapter.pointer_button(&mismatch),
            Err(RegionAdapterError::State(
                RegionInputStateError::ButtonPositionMismatch { .. }
            ))
        ));
        assert_eq!(adapter.state().last_sequence(), 1);

        let exact = RegionPointerButtonMsg {
            position: position(7, main_id, 12_000, 24_000),
            ..mismatch
        };
        let mapped = adapter.pointer_button(&exact).expect("exact button");
        assert_eq!(
            mapped.position,
            adapter
                .map(domain_position(exact.position).unwrap())
                .unwrap()
        );
        assert_eq!(adapter.state().held_buttons().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn pen_maps_region_position_and_retains_professional_axes() {
        let plan = plan();
        let left_id = region_id(&plan, "left");
        let mut adapter = RegionInputAdapter::from_plan(&plan).expect("adapter");
        let message = RegionPenEventMsg {
            msg_type: arcen_protocol::messages::REGION_PEN_EVENT.to_owned(),
            position: position(7, left_id, 61_440, 46_080),
            pressure: 0.625,
            tilt_x_degrees: -17.0,
            tilt_y_degrees: 23.0,
            rotation_degrees: 270.0,
            tool: PenToolMsg::Eraser,
            in_proximity: true,
            touching: true,
            buttons: 2,
            metadata: metadata(1),
        };
        let mapped = adapter.pen(&message).expect("mapped pen");
        assert_eq!(mapped.sample.tool, PenTool::Eraser);
        assert_eq!(mapped.sample.pressure, 0.625);
        assert_eq!(mapped.sample.tilt_x_degrees, -17.0);
        assert_eq!(mapped.sample.tilt_y_degrees, 23.0);
        assert_eq!(mapped.sample.rotation_degrees, 270.0);
        assert!(mapped.sample.touching);
        assert_eq!(
            adapter.state().pen().expect("pen state").sample,
            mapped.sample
        );
    }

    #[test]
    fn stale_generation_is_rejected_without_advancing_state() {
        let plan = plan();
        let main_id = region_id(&plan, "main");
        let mut adapter = RegionInputAdapter::from_plan(&plan).expect("adapter");
        assert!(matches!(
            adapter.pointer_enter(&enter(6, main_id, 0, 0, 1)),
            Err(RegionAdapterError::State(
                RegionInputStateError::StaleGeneration { .. }
            ))
        ));
        assert_eq!(adapter.state().last_sequence(), 0);
        assert!(!adapter.state().is_focused());
        adapter
            .pointer_enter(&enter(7, main_id, 0, 0, 1))
            .expect("current generation");
    }

    #[test]
    fn explicit_leave_then_enter_transitions_between_regions() {
        let plan = plan();
        let left_id = region_id(&plan, "left");
        let main_id = region_id(&plan, "main");
        let mut adapter = RegionInputAdapter::from_plan(&plan).expect("adapter");
        adapter
            .pointer_enter(&enter(7, left_id, 0, 0, 1))
            .expect("enter left");
        assert_eq!(
            adapter.state().active_pointer_region(),
            Some(RegionId::new(left_id).expect("left id"))
        );
        adapter
            .pointer_leave(&RegionPointerLeaveMsg {
                msg_type: arcen_protocol::messages::REGION_POINTER_LEAVE.to_owned(),
                position: position(7, left_id, 0, 0),
                metadata: metadata(2),
            })
            .expect("leave left");
        adapter
            .pointer_enter(&enter(7, main_id, 0, 0, 3))
            .expect("enter main");
        assert_eq!(
            adapter.state().active_pointer_region(),
            Some(RegionId::new(main_id).expect("main id"))
        );
    }

    fn rounded_ratio(numerator: u128, denominator: u128) -> i64 {
        i64::try_from((numerator + denominator / 2) / denominator).expect("mapped axis")
    }
}
