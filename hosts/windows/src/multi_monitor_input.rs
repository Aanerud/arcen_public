//! Shared-region input adapter for the committed Windows virtual desktop.
//!
//! Validation, wire-to-domain conversion, ordered state, and the region
//! coordinate transform all live in the shared [`RegionInputPipeline`]. This
//! module contributes only the Windows-specific steps: proving the committed
//! region bounds exactly describe the declared virtual desktop, and mapping one
//! applied pixel index to both native Windows coordinate domains (real signed
//! desktop pixels for `PT_PEN` and the `MOUSEEVENTF_ABSOLUTE |
//! MOUSEEVENTF_VIRTUALDESK` axis range for `SendInput`). The legacy normalized
//! single-monitor path remains isolated in `input.rs`; multi-monitor input
//! never reuses or duplicates its monitor-relative coordinate math.

use std::error::Error;
use std::fmt::{Display, Formatter};

#[cfg(test)]
use arcen_input::RegionInputState;
use arcen_input::{
    validate_aggregate_parity, CoordinateTransformError, RegionAggregateParityError,
    RegionInputPipeline, RegionInputPipelineError, RegionInputStateError, RegionPointMapper,
    ReleasedRegionInput,
};
use arcen_media::{AppliedPoint, AppliedRegionSet, RegionContractError, RegionSet};
use arcen_protocol::messages::{
    RegionInputValidationError, RegionPenEventMsg, RegionPointerButtonMsg, RegionPointerEnterMsg,
    RegionPointerLeaveMsg, RegionPointerMotionMsg, RegionPointerScrollMsg,
};

use crate::display::DesktopRect;
use crate::multi_monitor_topology::{WindowsTopologyError, WindowsTopologyPlan};

const SEND_INPUT_AXIS_MAX: i128 = 65_535;

/// Real signed Windows virtual-desktop pixel used by `PT_PEN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopPixelPoint {
    pub x: i32,
    pub y: i32,
}

/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` axis values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendInputPoint {
    pub x: i32,
    pub y: i32,
}

/// One shared logical position mapped to both Windows native coordinate
/// domains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedRegionPoint {
    pub desktop: DesktopPixelPoint,
    pub send_input: SendInputPoint,
}

/// Mouse buttons supported by the Windows `SendInput` adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsMouseButton {
    Left,
    Middle,
    Right,
}

impl WindowsMouseButton {
    #[must_use]
    pub const fn protocol_code(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
        }
    }
}

impl TryFrom<u8> for WindowsMouseButton {
    type Error = RegionAdapterError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Left),
            2 => Ok(Self::Middle),
            3 => Ok(Self::Right),
            _ => Err(RegionAdapterError::UnsupportedButton(value)),
        }
    }
}

/// Checked region-scoped button transition ready for an atomic
/// move-then-button `SendInput` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedRegionButton {
    pub position: MappedRegionPoint,
    pub button: WindowsMouseButton,
    pub pressed: bool,
}

/// Checked region-scoped wheel sample ready for `SendInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedRegionScroll {
    pub position: MappedRegionPoint,
    pub horizontal: i32,
    pub vertical: i32,
}

/// Checked region-scoped pen sample ready for `PT_PEN`.
pub type MappedRegionPen = arcen_input::MappedRegionPen<MappedRegionPoint>;

/// The only Windows-specific step of the shared region input pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowsDesktopMapper {
    desktop: DesktopRect,
}

impl RegionPointMapper for WindowsDesktopMapper {
    type Point = MappedRegionPoint;
    type Error = WindowsAxisError;

    fn map_applied(&self, point: AppliedPoint) -> Result<MappedRegionPoint, WindowsAxisError> {
        map_applied_point(point, self.desktop)
    }
}

/// Stateful adapter from shared region input to one committed Windows virtual
/// desktop.
#[derive(Debug, Clone)]
pub struct RegionInputAdapter {
    pipeline: RegionInputPipeline<WindowsDesktopMapper>,
}

impl RegionInputAdapter {
    /// Builds one adapter from the exact Windows topology committed for this
    /// desktop.
    ///
    /// # Errors
    ///
    /// Returns an error when shared region generation fails or the plan's
    /// region bounds do not exactly match its declared virtual desktop.
    pub fn from_plan(plan: &WindowsTopologyPlan) -> Result<Self, RegionAdapterError> {
        let (requested_regions, applied_regions) = plan.region_sets()?;
        let desktop = DesktopRect {
            left: plan.desktop_x,
            top: plan.desktop_y,
            width: i32::try_from(plan.desktop_width)
                .map_err(|_| RegionAdapterError::DesktopDimensionOverflow)?,
            height: i32::try_from(plan.desktop_height)
                .map_err(|_| RegionAdapterError::DesktopDimensionOverflow)?,
        };
        Self::from_region_sets(requested_regions, applied_regions, desktop)
    }

    /// Builds an adapter from explicit shared region aggregates. This is the
    /// provider-neutral seam used by physical outputs today and future IddCx
    /// output providers.
    ///
    /// # Errors
    ///
    /// Returns an error when requested/applied aggregates disagree or their
    /// applied bounds do not exactly describe `desktop`.
    pub fn from_region_sets(
        requested_regions: RegionSet,
        applied_regions: AppliedRegionSet,
        desktop: DesktopRect,
    ) -> Result<Self, RegionAdapterError> {
        validate_aggregate_parity(&requested_regions, &applied_regions)?;
        validate_desktop(&applied_regions, desktop)?;
        Ok(Self {
            pipeline: RegionInputPipeline::new(applied_regions, WindowsDesktopMapper { desktop }),
        })
    }

    #[cfg(test)]
    #[must_use]
    pub const fn state(&self) -> &RegionInputState {
        self.pipeline.state()
    }

    #[must_use]
    pub const fn desktop(&self) -> DesktopRect {
        self.pipeline.mapper().desktop
    }

    /// Clears semantic focus/button/pen state. Native release emission remains
    /// the responsibility of the paired `Injector` and `PenInjector`.
    #[must_use]
    pub fn release_all(&mut self) -> ReleasedRegionInput {
        self.pipeline.release_all()
    }

    /// Applies and maps a pointer-enter transition.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or Windows-axis error.
    pub fn pointer_enter(
        &mut self,
        message: &RegionPointerEnterMsg,
    ) -> Result<MappedRegionPoint, RegionAdapterError> {
        Ok(self.pipeline.pointer_enter(message)?)
    }

    /// Applies and maps a pointer-leave transition.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or Windows-axis error.
    pub fn pointer_leave(
        &mut self,
        message: &RegionPointerLeaveMsg,
    ) -> Result<MappedRegionPoint, RegionAdapterError> {
        Ok(self.pipeline.pointer_leave(message)?)
    }

    /// Applies and maps region-local absolute pointer motion.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or Windows-axis error.
    pub fn pointer_motion(
        &mut self,
        message: &RegionPointerMotionMsg,
    ) -> Result<MappedRegionPoint, RegionAdapterError> {
        Ok(self.pipeline.pointer_motion(message)?)
    }

    /// Applies a button edge only when its logical position is exactly the
    /// latest accepted pointer position.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, unsupported-button, or
    /// Windows-axis error.
    pub fn pointer_button(
        &mut self,
        message: &RegionPointerButtonMsg,
    ) -> Result<MappedRegionButton, RegionAdapterError> {
        message.validate()?;
        let button = WindowsMouseButton::try_from(message.button)?;
        let mapped = self.pipeline.pointer_button(message)?;
        Ok(MappedRegionButton {
            position: mapped.position,
            button,
            pressed: mapped.pressed,
        })
    }

    /// Applies a wheel sample only at the exact latest pointer position.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, wheel-range, or Windows-axis
    /// error.
    pub fn pointer_scroll(
        &mut self,
        message: &RegionPointerScrollMsg,
    ) -> Result<MappedRegionScroll, RegionAdapterError> {
        message.validate()?;
        let horizontal = i32::try_from(message.delta_x)
            .map_err(|_| RegionAdapterError::WheelDeltaOverflow(message.delta_x))?;
        let vertical = i32::try_from(message.delta_y)
            .map_err(|_| RegionAdapterError::WheelDeltaOverflow(message.delta_y))?;
        let mapped = self.pipeline.pointer_scroll(message)?;
        Ok(MappedRegionScroll {
            position: mapped.position,
            horizontal,
            vertical,
        })
    }

    /// Applies and maps a full region-scoped pen sample.
    ///
    /// # Errors
    ///
    /// Returns a wire, shared-state, transform, or Windows-axis error.
    pub fn pen(
        &mut self,
        message: &RegionPenEventMsg,
    ) -> Result<MappedRegionPen, RegionAdapterError> {
        Ok(self.pipeline.pen(message)?)
    }
}

fn validate_desktop(
    regions: &AppliedRegionSet,
    desktop: DesktopRect,
) -> Result<(), RegionAdapterError> {
    if desktop.width <= 0 || desktop.height <= 0 {
        return Err(RegionAdapterError::EmptyDesktop(desktop));
    }
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for region in regions.regions() {
        let rect = region.applied_rect();
        let origin = rect.origin();
        let right = origin
            .x
            .checked_add(i64::from(rect.size().width()))
            .ok_or(RegionAdapterError::DesktopDimensionOverflow)?;
        let bottom = origin
            .y
            .checked_add(i64::from(rect.size().height()))
            .ok_or(RegionAdapterError::DesktopDimensionOverflow)?;
        min_x = min_x.min(origin.x);
        min_y = min_y.min(origin.y);
        max_x = max_x.max(right);
        max_y = max_y.max(bottom);
    }
    let expected_right = i64::from(desktop.left) + i64::from(desktop.width);
    let expected_bottom = i64::from(desktop.top) + i64::from(desktop.height);
    if min_x != i64::from(desktop.left)
        || min_y != i64::from(desktop.top)
        || max_x != expected_right
        || max_y != expected_bottom
    {
        return Err(RegionAdapterError::DesktopBoundsMismatch {
            declared: desktop,
            actual_left: min_x,
            actual_top: min_y,
            actual_right: max_x,
            actual_bottom: max_y,
        });
    }
    Ok(())
}

fn map_applied_point(
    point: AppliedPoint,
    desktop: DesktopRect,
) -> Result<MappedRegionPoint, WindowsAxisError> {
    let x = map_send_input_axis(point.x, desktop.left, desktop.width)?;
    let y = map_send_input_axis(point.y, desktop.top, desktop.height)?;
    let desktop = DesktopPixelPoint {
        x: i32::try_from(point.x).map_err(|_| WindowsAxisError::DesktopPointOverflow(point))?,
        y: i32::try_from(point.y).map_err(|_| WindowsAxisError::DesktopPointOverflow(point))?,
    };
    Ok(MappedRegionPoint {
        desktop,
        send_input: SendInputPoint { x, y },
    })
}

fn map_send_input_axis(coordinate: i64, origin: i32, extent: i32) -> Result<i32, WindowsAxisError> {
    if extent <= 0 {
        return Err(WindowsAxisError::EmptyDesktop(DesktopRect {
            left: origin,
            top: 0,
            width: extent,
            height: 1,
        }));
    }
    let offset = i128::from(coordinate) - i128::from(origin);
    if offset < 0 || offset >= i128::from(extent) {
        return Err(WindowsAxisError::PointOutsideDesktop(coordinate));
    }
    if extent == 1 {
        return Ok(0);
    }
    let mapped = offset
        .checked_mul(SEND_INPUT_AXIS_MAX)
        .ok_or(WindowsAxisError::DesktopDimensionOverflow)?
        / i128::from(extent - 1);
    i32::try_from(mapped).map_err(|_| WindowsAxisError::DesktopDimensionOverflow)
}

/// Windows-specific mapping failure for one applied pixel index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsAxisError {
    EmptyDesktop(DesktopRect),
    PointOutsideDesktop(i64),
    DesktopPointOverflow(AppliedPoint),
    DesktopDimensionOverflow,
}

impl Display for WindowsAxisError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&RegionAdapterError::from(*self), formatter)
    }
}

impl Error for WindowsAxisError {}

/// Region-to-Windows adapter failure.
#[derive(Debug)]
pub enum RegionAdapterError {
    Topology(WindowsTopologyError),
    Wire(RegionInputValidationError),
    Contract(RegionContractError),
    State(RegionInputStateError),
    Transform(CoordinateTransformError),
    AggregateMismatch,
    DesktopDimensionOverflow,
    EmptyDesktop(DesktopRect),
    DesktopBoundsMismatch {
        declared: DesktopRect,
        actual_left: i64,
        actual_top: i64,
        actual_right: i64,
        actual_bottom: i64,
    },
    PointOutsideDesktop(i64),
    DesktopPointOverflow(AppliedPoint),
    UnsupportedButton(u8),
    WheelDeltaOverflow(i64),
}

impl Display for RegionAdapterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Topology(error) => write!(formatter, "{error}"),
            Self::Wire(error) => write!(formatter, "{error}"),
            Self::Contract(error) => write!(formatter, "{error}"),
            Self::State(error) => write!(formatter, "{error}"),
            Self::Transform(error) => write!(formatter, "{error}"),
            Self::AggregateMismatch => {
                formatter.write_str("requested and applied shared region aggregates do not match")
            }
            Self::DesktopDimensionOverflow => {
                formatter.write_str("Windows virtual-desktop geometry overflow")
            }
            Self::EmptyDesktop(desktop) => {
                write!(formatter, "Windows virtual desktop is empty: {desktop:?}")
            }
            Self::DesktopBoundsMismatch {
                declared,
                actual_left,
                actual_top,
                actual_right,
                actual_bottom,
            } => write!(
                formatter,
                "applied region bounds ({actual_left},{actual_top})..({actual_right},{actual_bottom}) do not match declared Windows desktop {declared:?}"
            ),
            Self::PointOutsideDesktop(coordinate) => {
                write!(
                    formatter,
                    "mapped coordinate {coordinate} is outside the Windows virtual desktop"
                )
            }
            Self::DesktopPointOverflow(point) => {
                write!(
                    formatter,
                    "mapped point {point:?} cannot be represented by Windows desktop pixels"
                )
            }
            Self::UnsupportedButton(button) => {
                write!(
                    formatter,
                    "Windows input does not support pointer button {button}"
                )
            }
            Self::WheelDeltaOverflow(delta) => {
                write!(
                    formatter,
                    "wheel delta {delta} exceeds the Windows input range"
                )
            }
        }
    }
}

impl Error for RegionAdapterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Topology(error) => Some(error),
            Self::Wire(error) => Some(error),
            Self::Contract(error) => Some(error),
            Self::State(error) => Some(error),
            Self::Transform(error) => Some(error),
            _ => None,
        }
    }
}

impl From<WindowsTopologyError> for RegionAdapterError {
    fn from(value: WindowsTopologyError) -> Self {
        Self::Topology(value)
    }
}

impl From<RegionInputValidationError> for RegionAdapterError {
    fn from(value: RegionInputValidationError) -> Self {
        Self::Wire(value)
    }
}

impl From<RegionAggregateParityError> for RegionAdapterError {
    fn from(_: RegionAggregateParityError) -> Self {
        Self::AggregateMismatch
    }
}

impl From<WindowsAxisError> for RegionAdapterError {
    fn from(value: WindowsAxisError) -> Self {
        match value {
            WindowsAxisError::EmptyDesktop(desktop) => Self::EmptyDesktop(desktop),
            WindowsAxisError::PointOutsideDesktop(coordinate) => {
                Self::PointOutsideDesktop(coordinate)
            }
            WindowsAxisError::DesktopPointOverflow(point) => Self::DesktopPointOverflow(point),
            WindowsAxisError::DesktopDimensionOverflow => Self::DesktopDimensionOverflow,
        }
    }
}

impl From<RegionInputPipelineError<WindowsAxisError>> for RegionAdapterError {
    fn from(value: RegionInputPipelineError<WindowsAxisError>) -> Self {
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
    use arcen_input::{PenTool, RegionLogicalPosition};
    use arcen_media::{
        AppliedRect, AppliedRegionDescriptor, LogicalPoint, LogicalRect, LogicalSize,
        OutputIdentity, OutputTransform, PhysicalSize, RegionDescriptor, RegionGeneration,
        RegionId, Scale120, TopologyGeneration, LOGICAL_UNITS_PER_PIXEL,
    };
    use arcen_protocol::messages::{
        PenToolMsg, RegionInputMetadataMsg, RegionInputPositionMsg, REGION_PEN_EVENT,
        REGION_POINTER_BUTTON, REGION_POINTER_ENTER, REGION_POINTER_LEAVE, REGION_POINTER_MOTION,
        REGION_POINTER_SCROLL,
    };

    use crate::multi_monitor_topology::WindowsMonitorPlan;
    use crate::nvapi::AdapterLuid;

    const CROSS_COMPONENT_BASELINE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/runtime/region_input.json"
    ));

    fn logical_rect(x: i64, y: i64, width: u64, height: u64) -> arcen_media::LogicalRect {
        LogicalRect::new(
            LogicalPoint::from_pixels(x, y).expect("origin"),
            LogicalSize::from_pixels(width, height).expect("size"),
        )
        .expect("logical rect")
    }

    fn monitor(
        id: u16,
        client_display_id: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        logical: LogicalRect,
        scale: u32,
        primary: bool,
    ) -> WindowsMonitorPlan {
        WindowsMonitorPlan {
            session_monitor_id: arcen_media::SessionMonitorId::new(id).expect("monitor id"),
            client_display_id: client_display_id.to_owned(),
            adapter_luid: AdapterLuid {
                low_part: u32::from(id),
                high_part: 0,
            },
            target_id: u32::from(id),
            adapter_output_index: u32::from(id),
            adapter_name: "Test Adapter".to_owned(),
            global_index: u32::from(id),
            device_name: format!(r"\\.\DISPLAY{id}"),
            x,
            y,
            width,
            height,
            mode_width: width,
            mode_height: height,
            logical_rect: logical,
            scale: Scale120::new(scale).expect("scale"),
            refresh_hz: 60,
            rotation: arcen_media::Rotation::Degrees0,
            primary,
        }
    }

    fn plan() -> WindowsTopologyPlan {
        WindowsTopologyPlan {
            generation: TopologyGeneration::new(7).expect("generation"),
            desktop_x: -1_280,
            desktop_y: -120,
            desktop_width: 3_200,
            desktop_height: 1_200,
            monitors: vec![
                monitor(
                    1,
                    "left",
                    -1_280,
                    -120,
                    1_280,
                    960,
                    logical_rect(-1_024, -96, 1_024, 768),
                    150,
                    false,
                ),
                monitor(
                    2,
                    "main",
                    0,
                    0,
                    1_920,
                    1_080,
                    logical_rect(0, 0, 1_920, 1_080),
                    120,
                    true,
                ),
            ],
            requires_custom_timing: false,
        }
    }

    fn baseline_plan() -> WindowsTopologyPlan {
        WindowsTopologyPlan {
            generation: TopologyGeneration::new(7).expect("generation"),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 1_920,
            desktop_height: 1_080,
            monitors: vec![monitor(
                1,
                "baseline",
                0,
                0,
                1_920,
                1_080,
                logical_rect(0, 0, 1_920, 1_080),
                120,
                true,
            )],
            requires_custom_timing: false,
        }
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
            msg_type: REGION_POINTER_ENTER.to_owned(),
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
            msg_type: REGION_POINTER_MOTION.to_owned(),
            position: position(generation, region_id, logical_x, logical_y),
            metadata: metadata(sequence),
        }
    }

    fn legacy_desktop_to_virtual_abs(point: DesktopPixelPoint, desktop: DesktopRect) -> (i32, i32) {
        let x = (f64::from(point.x) - f64::from(desktop.left))
            / f64::from(desktop.width.saturating_sub(1).max(1));
        let y = (f64::from(point.y) - f64::from(desktop.top))
            / f64::from(desktop.height.saturating_sub(1).max(1));
        (
            (x.clamp(0.0, 1.0) * 65_535.0) as i32,
            (y.clamp(0.0, 1.0) * 65_535.0) as i32,
        )
    }

    #[test]
    fn cross_component_fixture_freezes_windows_decode_state_and_endpoint() {
        let baseline: serde_json::Value = serde_json::from_str(CROSS_COMPONENT_BASELINE).unwrap();
        let mut adapter = RegionInputAdapter::from_plan(&baseline_plan()).expect("adapter");
        let pointer_endpoint = MappedRegionPoint {
            desktop: DesktopPixelPoint {
                x: i32::try_from(baseline["pointer_endpoint"]["x"].as_i64().unwrap()).unwrap(),
                y: i32::try_from(baseline["pointer_endpoint"]["y"].as_i64().unwrap()).unwrap(),
            },
            send_input: SendInputPoint {
                x: i32::try_from(
                    baseline["windows_send_input_pointer"]["x"]
                        .as_i64()
                        .unwrap(),
                )
                .unwrap(),
                y: i32::try_from(
                    baseline["windows_send_input_pointer"]["y"]
                        .as_i64()
                        .unwrap(),
                )
                .unwrap(),
            },
        };
        let pen_endpoint = MappedRegionPoint {
            desktop: DesktopPixelPoint {
                x: i32::try_from(baseline["pen_endpoint"]["x"].as_i64().unwrap()).unwrap(),
                y: i32::try_from(baseline["pen_endpoint"]["y"].as_i64().unwrap()).unwrap(),
            },
            send_input: SendInputPoint {
                x: i32::try_from(baseline["windows_send_input_pen"]["x"].as_i64().unwrap())
                    .unwrap(),
                y: i32::try_from(baseline["windows_send_input_pen"]["y"].as_i64().unwrap())
                    .unwrap(),
            },
        };

        for event in baseline["events"].as_array().unwrap() {
            match event["type"].as_str().unwrap() {
                "region_pointer_enter" => {
                    let message: RegionPointerEnterMsg =
                        serde_json::from_value(event.clone()).unwrap();
                    assert_eq!(
                        adapter.pointer_enter(&message).unwrap(),
                        MappedRegionPoint {
                            desktop: DesktopPixelPoint { x: 0, y: 0 },
                            send_input: SendInputPoint { x: 0, y: 0 },
                        }
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
                    assert_eq!(mapped.button, WindowsMouseButton::Left);
                    assert!(mapped.pressed);
                }
                "region_pointer_scroll" => {
                    let message: RegionPointerScrollMsg =
                        serde_json::from_value(event.clone()).unwrap();
                    let mapped = adapter.pointer_scroll(&message).unwrap();
                    assert_eq!(mapped.position, pointer_endpoint);
                    assert_eq!((mapped.horizontal, mapped.vertical), (120, -240));
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
    fn explicit_region_topology_preserves_mixed_scale_and_signed_coordinates() {
        let plan = plan();
        let (requested, applied) = plan.region_sets().expect("region sets");
        let adapter = RegionInputAdapter::from_plan(&plan).expect("adapter");
        assert_eq!(applied.generation().get(), 7);
        let left = requested
            .get(RegionId::new(1).expect("region id"))
            .expect("left region");
        assert_eq!(
            left.logical_rect().origin(),
            LogicalPoint::from_pixels(-1_024, -96).expect("origin")
        );
        assert_eq!(left.scale().get(), 150);
        assert_eq!(
            adapter.desktop(),
            DesktopRect {
                left: -1_280,
                top: -120,
                width: 3_200,
                height: 1_200,
            }
        );
    }

    #[test]
    fn mixed_scale_negative_desktop_maps_corners_and_centers() {
        let mut adapter = RegionInputAdapter::from_plan(&plan()).expect("adapter");
        let left_last_x = 1_024 * LOGICAL_UNITS_PER_PIXEL - 1;
        let left_last_y = 768 * LOGICAL_UNITS_PER_PIXEL - 1;
        let top_left = adapter
            .pointer_enter(&enter(7, 1, 0, 0, 1))
            .expect("top left");
        assert_eq!(top_left.desktop, DesktopPixelPoint { x: -1_280, y: -120 });
        assert_eq!(top_left.send_input, SendInputPoint { x: 0, y: 0 });
        let bottom_right = adapter
            .pointer_motion(&motion(7, 1, left_last_x, left_last_y, 2))
            .expect("bottom right");
        assert_eq!(bottom_right.desktop, DesktopPixelPoint { x: -1, y: 839 });

        adapter
            .pointer_leave(&RegionPointerLeaveMsg {
                msg_type: REGION_POINTER_LEAVE.to_owned(),
                position: position(7, 1, left_last_x, left_last_y),
                metadata: metadata(3),
            })
            .expect("leave left");
        let center = adapter
            .pointer_enter(&enter(
                7,
                2,
                1_920 * LOGICAL_UNITS_PER_PIXEL / 2,
                1_080 * LOGICAL_UNITS_PER_PIXEL / 2,
                4,
            ))
            .expect("main center");
        assert!((center.desktop.x - 960).abs() <= 1);
        assert!((center.desktop.y - 540).abs() <= 1);
    }

    #[test]
    fn shared_mapping_matches_removed_legacy_virtual_axis_math() {
        let mut adapter = RegionInputAdapter::from_plan(&plan()).expect("adapter");
        let desktop = adapter.desktop();
        let last_x = 1_024 * LOGICAL_UNITS_PER_PIXEL - 1;
        let last_y = 768 * LOGICAL_UNITS_PER_PIXEL - 1;
        for (sequence, x, y) in [(1, 0, 0), (2, last_x / 2, last_y / 2), (3, last_x, last_y)] {
            let mapped = if sequence == 1 {
                adapter
                    .pointer_enter(&enter(7, 1, x, y, sequence))
                    .expect("enter")
            } else {
                adapter
                    .pointer_motion(&motion(7, 1, x, y, sequence))
                    .expect("motion")
            };
            assert_eq!(
                (mapped.send_input.x, mapped.send_input.y),
                legacy_desktop_to_virtual_abs(mapped.desktop, desktop)
            );
        }
    }

    #[test]
    fn every_shared_output_transform_maps_exact_corners() {
        let cases = [
            (OutputTransform::Normal, (0, 0), (3, 2)),
            (OutputTransform::Rotate90, (2, 0), (0, 3)),
            (OutputTransform::Rotate180, (3, 2), (0, 0)),
            (OutputTransform::Rotate270, (0, 3), (2, 0)),
            (OutputTransform::Flipped, (3, 0), (0, 2)),
            (OutputTransform::Flipped90, (2, 3), (0, 0)),
            (OutputTransform::Flipped180, (0, 2), (3, 0)),
            (OutputTransform::Flipped270, (0, 0), (2, 3)),
        ];
        for (transform, expected_first, expected_last) in cases {
            let generation = RegionGeneration::new(9).expect("generation");
            let descriptor = RegionDescriptor::new(
                RegionId::new(1).expect("id"),
                OutputIdentity::new(format!("transform-{transform:?}")).expect("identity"),
                logical_rect(0, 0, 4, 3),
                PhysicalSize::new(4, 3).expect("physical"),
                Scale120::new(120).expect("scale"),
                transform,
                true,
            );
            let requested =
                RegionSet::new(generation, vec![descriptor.clone()]).expect("requested");
            let size = descriptor.expected_applied_size().expect("applied size");
            let applied = AppliedRegionSet::new(
                generation,
                vec![AppliedRegionDescriptor::new(
                    descriptor,
                    AppliedRect::new(AppliedPoint::new(-20, -10), size).expect("rect"),
                )
                .expect("applied descriptor")],
            )
            .expect("applied");
            let desktop = DesktopRect {
                left: -20,
                top: -10,
                width: i32::try_from(size.width()).expect("width"),
                height: i32::try_from(size.height()).expect("height"),
            };
            let mut adapter =
                RegionInputAdapter::from_region_sets(requested, applied, desktop).expect("adapter");
            let first = adapter
                .pointer_enter(&enter(9, 1, 0, 0, 1))
                .expect("first corner");
            let last = adapter
                .pointer_motion(&motion(
                    9,
                    1,
                    4 * LOGICAL_UNITS_PER_PIXEL - 1,
                    3 * LOGICAL_UNITS_PER_PIXEL - 1,
                    2,
                ))
                .expect("last corner");
            assert_eq!(
                first.desktop,
                DesktopPixelPoint {
                    x: -20 + expected_first.0,
                    y: -10 + expected_first.1,
                },
                "{transform:?} first"
            );
            assert_eq!(
                last.desktop,
                DesktopPixelPoint {
                    x: -20 + expected_last.0,
                    y: -10 + expected_last.1,
                },
                "{transform:?} last"
            );
            let upper_right = adapter
                .pointer_motion(&motion(9, 1, 4 * LOGICAL_UNITS_PER_PIXEL - 1, 0, 3))
                .expect("upper-right corner");
            let lower_left = adapter
                .pointer_motion(&motion(9, 1, 0, 3 * LOGICAL_UNITS_PER_PIXEL - 1, 4))
                .expect("lower-left corner");
            assert_eq!(
                std::collections::BTreeSet::from([
                    (first.desktop.x, first.desktop.y),
                    (last.desktop.x, last.desktop.y),
                    (upper_right.desktop.x, upper_right.desktop.y),
                    (lower_left.desktop.x, lower_left.desktop.y),
                ]),
                std::collections::BTreeSet::from([
                    (desktop.left, desktop.top),
                    (desktop.left + desktop.width - 1, desktop.top),
                    (desktop.left, desktop.top + desktop.height - 1),
                    (
                        desktop.left + desktop.width - 1,
                        desktop.top + desktop.height - 1,
                    ),
                ]),
                "{transform:?} corners"
            );
            let center = adapter
                .pointer_motion(&motion(
                    9,
                    1,
                    (4 * LOGICAL_UNITS_PER_PIXEL - 1) / 2,
                    (3 * LOGICAL_UNITS_PER_PIXEL - 1) / 2,
                    5,
                ))
                .expect("center");
            assert!(
                center.desktop.x >= desktop.left
                    && center.desktop.x < desktop.left + desktop.width
                    && center.desktop.y >= desktop.top
                    && center.desktop.y < desktop.top + desktop.height,
                "{transform:?} center"
            );
        }
    }

    #[test]
    fn context_menu_button_uses_the_exact_latest_position() {
        let mut adapter = RegionInputAdapter::from_plan(&plan()).expect("adapter");
        let logical_x = 31_337;
        let logical_y = 52_001;
        let entered = adapter
            .pointer_enter(&enter(7, 2, logical_x, logical_y, 1))
            .expect("enter");
        let mismatch = RegionPointerButtonMsg {
            msg_type: REGION_POINTER_BUTTON.to_owned(),
            position: position(7, 2, logical_x + 1, logical_y),
            button: 3,
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
            position: position(7, 2, logical_x, logical_y),
            ..mismatch
        };
        let context = adapter.pointer_button(&exact).expect("context menu");
        assert_eq!(context.button, WindowsMouseButton::Right);
        assert_eq!(context.position, entered);
        assert!(context.pressed);
    }

    #[test]
    fn scroll_preserves_exact_position_and_windows_wheel_units() {
        let mut adapter = RegionInputAdapter::from_plan(&plan()).expect("adapter");
        let entered = adapter
            .pointer_enter(&enter(7, 2, 12_000, 24_000, 1))
            .expect("enter");
        let scroll = adapter
            .pointer_scroll(&RegionPointerScrollMsg {
                msg_type: REGION_POINTER_SCROLL.to_owned(),
                position: position(7, 2, 12_000, 24_000),
                delta_x: -120,
                delta_y: 240,
                metadata: metadata(2),
            })
            .expect("scroll");
        assert_eq!(scroll.position, entered);
        assert_eq!(scroll.horizontal, -120);
        assert_eq!(scroll.vertical, 240);
    }

    #[test]
    fn pen_maps_to_signed_desktop_pixels_and_retains_professional_axes() {
        let mut adapter = RegionInputAdapter::from_plan(&plan()).expect("adapter");
        let message = RegionPenEventMsg {
            msg_type: REGION_PEN_EVENT.to_owned(),
            position: position(7, 1, 61_440, 46_080),
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
        assert!(mapped.position.desktop.x < 0);
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
    fn stale_topology_epoch_or_region_generation_is_rejected_atomically() {
        let mut current_plan = plan();
        current_plan.generation = TopologyGeneration::new(8).expect("generation");
        let mut adapter = RegionInputAdapter::from_plan(&current_plan).expect("adapter");
        assert!(matches!(
            adapter.pointer_enter(&enter(7, 2, 0, 0, 1)),
            Err(RegionAdapterError::State(
                RegionInputStateError::StaleGeneration { .. }
            ))
        ));
        assert_eq!(adapter.state().last_sequence(), 0);
        assert!(!adapter.state().is_focused());
        adapter
            .pointer_enter(&enter(8, 2, 0, 0, 1))
            .expect("current generation");
    }

    #[test]
    fn explicit_leave_then_enter_transitions_between_regions() {
        let mut adapter = RegionInputAdapter::from_plan(&plan()).expect("adapter");
        adapter
            .pointer_enter(&enter(7, 1, 0, 0, 1))
            .expect("enter left");
        assert_eq!(
            adapter.state().active_pointer_region(),
            Some(RegionId::new(1).expect("left id"))
        );
        adapter
            .pointer_leave(&RegionPointerLeaveMsg {
                msg_type: REGION_POINTER_LEAVE.to_owned(),
                position: position(7, 1, 0, 0),
                metadata: metadata(2),
            })
            .expect("leave left");
        adapter
            .pointer_enter(&enter(7, 2, 0, 0, 3))
            .expect("enter main");
        assert_eq!(
            adapter.state().active_pointer_region(),
            Some(RegionId::new(2).expect("main id"))
        );
    }
}
