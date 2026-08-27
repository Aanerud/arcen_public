//! Transport-independent keyboard, pointer, and pen contracts.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{Display, Formatter};

mod region;
mod region_emitter;
mod region_pipeline;
mod region_state;
mod region_wire;

pub use region::{CoordinateTransformError, RegionCoordinateTransformer};
pub use region_emitter::{RegionInputEmitError, RegionInputEmitter};
pub use region_pipeline::{
    MappedRegionButton, MappedRegionInput, MappedRegionPen, MappedRegionScroll,
    RegionAggregateParityError, RegionInputPipeline, RegionInputPipelineError, RegionPointMapper,
    validate_aggregate_parity,
};
pub use region_state::{
    RegionInputEvent, RegionInputState, RegionInputStateError, RegionLogicalPosition,
    RegionPenSample, RegionPenState, ReleasedRegionInput,
};
pub use region_wire::{
    RegionInputDecodeError, RegionInputWireError, RegionInputWireMessage, RegionInputWireRef,
    domain_generation, domain_pen_tool, domain_position, wire_pen_tool, wire_position,
};

/// Hardware input capability truth reported by an endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityAvailability {
    /// The endpoint probed and found the capability.
    Available,
    /// The endpoint probed and did not find the capability.
    Unavailable,
    /// The endpoint did not establish whether the capability exists.
    #[default]
    Unknown,
}

/// Input device capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputCapabilities {
    /// Keyboard event availability.
    #[serde(default)]
    pub keyboard: CapabilityAvailability,
    /// Absolute pointer availability.
    #[serde(default)]
    pub pointer: CapabilityAvailability,
    /// Relative pointer availability.
    #[serde(default)]
    pub relative_pointer: CapabilityAvailability,
    /// Host-rendered cursor availability.
    #[serde(default)]
    pub host_cursor: CapabilityAvailability,
    /// Pen digitizer availability.
    #[serde(default)]
    pub pen: CapabilityAvailability,
    /// Pen pressure availability.
    #[serde(default)]
    pub pen_pressure: CapabilityAvailability,
    /// Pen tilt availability.
    #[serde(default)]
    pub pen_tilt: CapabilityAvailability,
    /// Pen barrel rotation availability.
    #[serde(default)]
    pub pen_rotation: CapabilityAvailability,
    /// Pen eraser availability.
    #[serde(default)]
    pub pen_eraser: CapabilityAvailability,
    /// Pen proximity availability.
    #[serde(default)]
    pub pen_proximity: CapabilityAvailability,
}

/// Pointer coordinate transport used by an input event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerMotionMode {
    /// Normalized absolute coordinates.
    #[default]
    Absolute,
    /// Signed relative deltas.
    Relative,
}

/// Authority responsible for rendering the visible cursor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorMode {
    /// The client renders the cursor.
    #[default]
    Local,
    /// The host capture path includes the cursor.
    Host,
}

/// Requested or active tablet input mode for a connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabletMode {
    /// Local AppKit/local-injection termination over input-v3.
    #[default]
    LocalTermination,
    /// Full USB bridge for native host-driver/device semantics.
    WacomUsbBridge,
    /// Disable tablet redirection and keep normal mouse compatibility.
    DisabledMouseCompat,
}

/// Deterministic result of matching a cursor request to host capability truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorModeNegotiation {
    /// Requested cursor authority.
    pub requested: CursorMode,
    /// Cursor authority that can truthfully be active.
    pub active: CursorMode,
    /// Whether the request was accepted exactly.
    pub accepted: bool,
}

/// Deterministic result of matching a tablet-mode request to host capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabletModeNegotiation {
    /// Requested tablet mode.
    pub requested: TabletMode,
    /// Mode that can truthfully be active.
    pub active: TabletMode,
    /// Whether the request was accepted exactly.
    pub accepted: bool,
}

/// Intersects two endpoint capability claims without turning unknown into
/// authority. Both endpoints must prove availability; either endpoint may
/// explicitly make the combined capability unavailable.
#[must_use]
pub const fn mutual_capability(
    first: CapabilityAvailability,
    second: CapabilityAvailability,
) -> CapabilityAvailability {
    match (first, second) {
        (CapabilityAvailability::Available, CapabilityAvailability::Available) => {
            CapabilityAvailability::Available
        }
        (CapabilityAvailability::Unavailable, _) | (_, CapabilityAvailability::Unavailable) => {
            CapabilityAvailability::Unavailable
        }
        _ => CapabilityAvailability::Unknown,
    }
}

/// Matches a cursor request to proven host cursor capability.
#[must_use]
pub const fn negotiate_cursor_mode(
    requested: CursorMode,
    host_cursor: CapabilityAvailability,
) -> CursorModeNegotiation {
    match (requested, host_cursor) {
        (CursorMode::Host, CapabilityAvailability::Available) => CursorModeNegotiation {
            requested,
            active: CursorMode::Host,
            accepted: true,
        },
        (CursorMode::Host, _) => CursorModeNegotiation {
            requested,
            active: CursorMode::Local,
            accepted: false,
        },
        (CursorMode::Local, _) => CursorModeNegotiation {
            requested,
            active: CursorMode::Local,
            accepted: true,
        },
    }
}

/// Matches a tablet-mode request to proven host capability truth.
#[must_use]
pub const fn negotiate_tablet_mode(
    requested: TabletMode,
    local_termination: CapabilityAvailability,
    wacom_usb_bridge: CapabilityAvailability,
) -> TabletModeNegotiation {
    match requested {
        TabletMode::LocalTermination => {
            if matches!(local_termination, CapabilityAvailability::Available) {
                TabletModeNegotiation {
                    requested,
                    active: TabletMode::LocalTermination,
                    accepted: true,
                }
            } else {
                TabletModeNegotiation {
                    requested,
                    active: TabletMode::DisabledMouseCompat,
                    accepted: false,
                }
            }
        }
        TabletMode::WacomUsbBridge => {
            if matches!(wacom_usb_bridge, CapabilityAvailability::Available) {
                TabletModeNegotiation {
                    requested,
                    active: TabletMode::WacomUsbBridge,
                    accepted: true,
                }
            } else {
                // A native bridge is a distinct ownership/driver mode. Never
                // substitute local termination for an unavailable bridge:
                // the user must explicitly choose the WAN-compatible mode.
                TabletModeNegotiation {
                    requested,
                    active: TabletMode::DisabledMouseCompat,
                    accepted: false,
                }
            }
        }
        TabletMode::DisabledMouseCompat => TabletModeNegotiation {
            requested,
            active: TabletMode::DisabledMouseCompat,
            accepted: true,
        },
    }
}

/// Sequence and clock metadata for latency-sensitive input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LowLatencyMetadata {
    /// Monotonic sequence assigned by the sender.
    pub sequence: u64,
    /// Sender monotonic timestamp in nanoseconds.
    pub timestamp_ns: u64,
    /// Whether an intermediate event may be replaced by a newer event.
    pub coalescable: bool,
}

/// Compact modifier mask used by the protocol-v3 compatibility contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModifierMask(pub u32);

impl ModifierMask {
    /// Shift modifier.
    pub const SHIFT: u32 = 0x01;
    /// Control modifier.
    pub const CONTROL: u32 = 0x02;
    /// Alt/Option modifier.
    pub const ALT: u32 = 0x04;
    /// Meta/Command modifier.
    pub const META: u32 = 0x08;
    /// Keypad modifier.
    pub const KEYPAD: u32 = 0x10;

    /// Returns whether all requested modifier bits are set.
    #[must_use]
    pub const fn contains(self, bits: u32) -> bool {
        self.0 & bits == bits
    }
}

/// Keyboard event. `key_id` is protocol-defined rather than OS-native.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardEvent {
    /// Protocol key identifier.
    pub key_id: u32,
    /// Whether the key is pressed.
    pub pressed: bool,
    /// Destination-policy modifier mask.
    pub modifiers: ModifierMask,
    /// Observable lock states; `None` means unknown.
    pub caps_lock_on: Option<bool>,
    /// Observable lock states; `None` means unknown.
    pub num_lock_on: Option<bool>,
    /// Observable lock states; `None` means unknown.
    pub scroll_lock_on: Option<bool>,
    /// Low-latency metadata.
    pub metadata: LowLatencyMetadata,
}

/// Pointer motion in normalized and optional server coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointerMotion {
    /// Normalized horizontal coordinate.
    pub x: f64,
    /// Normalized vertical coordinate.
    pub y: f64,
    /// Server horizontal coordinate when mapped.
    pub server_x: Option<i32>,
    /// Server vertical coordinate when mapped.
    pub server_y: Option<i32>,
    /// Low-latency metadata.
    pub metadata: LowLatencyMetadata,
}

/// Signed pointer motion independent of an absolute desktop position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointerRelativeMotion {
    /// Horizontal relative delta.
    pub dx: i32,
    /// Vertical relative delta.
    pub dy: i32,
    /// Low-latency metadata.
    pub metadata: LowLatencyMetadata,
}

/// Pointer button transition.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointerButton {
    /// Pointer button identifier.
    pub button: u8,
    /// Whether the button is pressed.
    pub pressed: bool,
    /// Whether the position is authoritative for this transition.
    #[serde(default)]
    pub motion_mode: PointerMotionMode,
    /// Pointer position at the transition.
    pub position: PointerMotion,
}

/// Pointer scroll delta.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointerScroll {
    /// Horizontal wheel or trackpad delta.
    pub delta_x: f64,
    /// Vertical wheel or trackpad delta.
    pub delta_y: f64,
    /// Whether the position is authoritative for this scroll edge.
    #[serde(default)]
    pub motion_mode: PointerMotionMode,
    /// Pointer position at the event.
    pub position: PointerMotion,
}

/// Tracks the one globally ordered sequence shared by every input event type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputSequenceTracker {
    last_nonzero: u64,
}

impl InputSequenceTracker {
    /// Accepts legacy sequence zero without advancing ordering state, otherwise
    /// accepts only a strict increase.
    pub fn accept(&mut self, sequence: u64) -> bool {
        if sequence == 0 {
            return true;
        }
        if sequence <= self.last_nonzero {
            return false;
        }
        self.last_nonzero = sequence;
        true
    }

    /// Last accepted nonzero sequence.
    #[must_use]
    pub const fn last_nonzero(self) -> u64 {
        self.last_nonzero
    }
}

/// Whole relative motion emitted for one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccumulatedRelativeMotion {
    /// Whole horizontal delta.
    pub dx: i32,
    /// Whole vertical delta.
    pub dy: i32,
}

/// Allocation-free accumulator that retains fractional native pointer motion.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FractionalMotionAccumulator {
    residual_x: f64,
    residual_y: f64,
}

impl FractionalMotionAccumulator {
    /// Adds raw deltas and returns the whole signed movement ready for the wire.
    ///
    /// # Errors
    ///
    /// Returns [`InputContractError::NonFinite`] for non-finite native input.
    pub fn accumulate(
        &mut self,
        dx: f64,
        dy: f64,
    ) -> Result<AccumulatedRelativeMotion, InputContractError> {
        let mut residual_x = self.residual_x;
        let mut residual_y = self.residual_y;
        let accumulated = AccumulatedRelativeMotion {
            dx: accumulate_axis(&mut residual_x, dx, "dx")?,
            dy: accumulate_axis(&mut residual_y, dy, "dy")?,
        };
        self.residual_x = residual_x;
        self.residual_y = residual_y;
        Ok(accumulated)
    }

    /// Clears retained subpixel motion.
    pub fn clear(&mut self) {
        self.residual_x = 0.0;
        self.residual_y = 0.0;
    }

    /// Current retained fractions, exposed for deterministic tests.
    #[must_use]
    pub const fn residual(self) -> (f64, f64) {
        (self.residual_x, self.residual_y)
    }
}

fn accumulate_axis(
    residual: &mut f64,
    delta: f64,
    field: &'static str,
) -> Result<i32, InputContractError> {
    if !delta.is_finite() {
        return Err(InputContractError::NonFinite(field));
    }
    let total = *residual + delta;
    if !total.is_finite() {
        return Err(InputContractError::NonFinite(field));
    }
    if total >= f64::from(i32::MAX) {
        *residual = 0.0;
        return Ok(i32::MAX);
    }
    if total <= f64::from(i32::MIN) {
        *residual = 0.0;
        return Ok(i32::MIN);
    }
    let whole = total.trunc();
    *residual = total - whole;
    #[allow(clippy::cast_possible_truncation)]
    Ok(whole as i32)
}

/// Pen tool touching or approaching the digitizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PenTool {
    /// Pen tip.
    Tip,
    /// Eraser end.
    Eraser,
}

/// Pen sample with the full professional digitizer surface.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PenEvent {
    /// Normalized horizontal coordinate.
    pub x: f64,
    /// Normalized vertical coordinate.
    pub y: f64,
    /// Normalized pressure in the inclusive range 0..=1.
    pub pressure: f32,
    /// X tilt in degrees, inclusive -90..=90.
    pub tilt_x_degrees: f32,
    /// Y tilt in degrees, inclusive -90..=90.
    pub tilt_y_degrees: f32,
    /// Barrel rotation in degrees, inclusive 0..360.
    pub rotation_degrees: f32,
    /// Tip or eraser tool.
    pub tool: PenTool,
    /// Whether the tool is in digitizer proximity.
    pub in_proximity: bool,
    /// Whether the tip is touching the surface.
    pub touching: bool,
    /// Bitset of barrel and auxiliary buttons.
    pub buttons: u16,
    /// Low-latency metadata.
    pub metadata: LowLatencyMetadata,
}

impl PenEvent {
    /// Validates normalized coordinates and physical pen ranges.
    ///
    /// # Errors
    ///
    /// Returns the first invalid pen field.
    pub fn validate(&self) -> Result<(), InputContractError> {
        validate_unit("x", self.x)?;
        validate_unit("y", self.y)?;
        validate_f32("pressure", self.pressure, 0.0, 1.0)?;
        validate_f32("tilt_x_degrees", self.tilt_x_degrees, -90.0, 90.0)?;
        validate_f32("tilt_y_degrees", self.tilt_y_degrees, -90.0, 90.0)?;
        validate_f32("rotation_degrees", self.rotation_degrees, 0.0, 360.0)
    }
}

fn validate_unit(field: &'static str, value: f64) -> Result<(), InputContractError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(InputContractError::OutOfRange(field))
    }
}

fn validate_f32(
    field: &'static str,
    value: f32,
    minimum: f32,
    maximum: f32,
) -> Result<(), InputContractError> {
    if value.is_finite() && (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(InputContractError::OutOfRange(field))
    }
}

/// Unified low-latency input event.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    /// Keyboard transition.
    Keyboard(KeyboardEvent),
    /// Pointer motion.
    PointerMotion(PointerMotion),
    /// Relative pointer motion.
    PointerRelativeMotion(PointerRelativeMotion),
    /// Pointer button transition.
    PointerButton(PointerButton),
    /// Pointer scrolling.
    PointerScroll(PointerScroll),
    /// Pen sample.
    Pen(PenEvent),
}

impl InputEvent {
    /// Sequence participating in the globally ordered input stream.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        match self {
            Self::Keyboard(event) => event.metadata.sequence,
            Self::PointerMotion(event) => event.metadata.sequence,
            Self::PointerRelativeMotion(event) => event.metadata.sequence,
            Self::PointerButton(event) => event.position.metadata.sequence,
            Self::PointerScroll(event) => event.position.metadata.sequence,
            Self::Pen(event) => event.metadata.sequence,
        }
    }
}

/// Input contract validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputContractError {
    /// A field was non-finite or outside its allowed range.
    OutOfRange(&'static str),
    /// A native relative-motion delta was NaN or infinite.
    NonFinite(&'static str),
}

impl Display for InputContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfRange(field) => write!(formatter, "{field} is outside its allowed range"),
            Self::NonFinite(field) => write!(formatter, "{field} is not finite"),
        }
    }
}

impl Error for InputContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn pen() -> PenEvent {
        PenEvent {
            x: 0.5,
            y: 0.25,
            pressure: 0.75,
            tilt_x_degrees: -12.0,
            tilt_y_degrees: 10.0,
            rotation_degrees: 180.0,
            tool: PenTool::Tip,
            in_proximity: true,
            touching: true,
            buttons: 1,
            metadata: LowLatencyMetadata::default(),
        }
    }

    #[test]
    fn complete_pen_sample_validates() {
        assert_eq!(pen().validate(), Ok(()));
    }

    #[test]
    fn invalid_pen_pressure_is_explicit() {
        let mut event = pen();
        event.pressure = 1.1;
        assert_eq!(
            event.validate(),
            Err(InputContractError::OutOfRange("pressure"))
        );
    }

    #[test]
    fn unknown_hardware_capability_stays_unknown() {
        let capabilities = InputCapabilities {
            keyboard: CapabilityAvailability::Available,
            pointer: CapabilityAvailability::Available,
            ..InputCapabilities::default()
        };
        let json = serde_json::to_string(&capabilities).expect("serializes");
        assert!(json.contains("\"pen\":\"unknown\""));
    }

    #[test]
    fn old_capability_payload_defaults_new_truth_to_unknown() {
        let capabilities: InputCapabilities =
            serde_json::from_str(r#"{"keyboard":"available","pointer":"available"}"#)
                .expect("legacy capabilities parse");
        assert_eq!(
            capabilities.relative_pointer,
            CapabilityAvailability::Unknown
        );
        assert_eq!(capabilities.host_cursor, CapabilityAvailability::Unknown);
    }

    #[test]
    fn cursor_negotiation_never_claims_unproven_host_authority() {
        assert_eq!(
            negotiate_cursor_mode(CursorMode::Host, CapabilityAvailability::Unknown),
            CursorModeNegotiation {
                requested: CursorMode::Host,
                active: CursorMode::Local,
                accepted: false,
            }
        );
        assert_eq!(
            negotiate_cursor_mode(CursorMode::Host, CapabilityAvailability::Available).active,
            CursorMode::Host
        );
        assert!(
            negotiate_cursor_mode(CursorMode::Local, CapabilityAvailability::Unavailable).accepted
        );
    }

    #[test]
    fn tablet_bridge_never_silently_falls_back_to_local_termination() {
        assert_eq!(
            negotiate_tablet_mode(
                TabletMode::WacomUsbBridge,
                CapabilityAvailability::Available,
                CapabilityAvailability::Unavailable,
            ),
            TabletModeNegotiation {
                requested: TabletMode::WacomUsbBridge,
                active: TabletMode::DisabledMouseCompat,
                accepted: false,
            }
        );
    }

    #[test]
    fn tablet_local_and_disabled_modes_negotiate_explicitly() {
        assert!(
            negotiate_tablet_mode(
                TabletMode::LocalTermination,
                CapabilityAvailability::Available,
                CapabilityAvailability::Unavailable,
            )
            .accepted
        );
        assert_eq!(
            negotiate_tablet_mode(
                TabletMode::LocalTermination,
                CapabilityAvailability::Unknown,
                CapabilityAvailability::Unavailable,
            )
            .active,
            TabletMode::DisabledMouseCompat
        );
        assert!(
            negotiate_tablet_mode(
                TabletMode::DisabledMouseCompat,
                CapabilityAvailability::Unknown,
                CapabilityAvailability::Unknown,
            )
            .accepted
        );
    }

    #[test]
    fn mutual_capability_requires_both_endpoints_to_prove_availability() {
        assert_eq!(
            mutual_capability(
                CapabilityAvailability::Available,
                CapabilityAvailability::Available,
            ),
            CapabilityAvailability::Available
        );
        assert_eq!(
            mutual_capability(
                CapabilityAvailability::Available,
                CapabilityAvailability::Unknown,
            ),
            CapabilityAvailability::Unknown
        );
        assert_eq!(
            mutual_capability(
                CapabilityAvailability::Unknown,
                CapabilityAvailability::Unavailable,
            ),
            CapabilityAvailability::Unavailable
        );
    }

    #[test]
    fn sequence_zero_is_legacy_and_nonzero_order_is_global() {
        let mut tracker = InputSequenceTracker::default();
        assert!(tracker.accept(0));
        assert!(tracker.accept(4));
        assert!(tracker.accept(0));
        assert!(!tracker.accept(4));
        assert!(!tracker.accept(3));
        assert_eq!(tracker.last_nonzero(), 4);
        assert!(tracker.accept(5));
    }

    #[test]
    fn rejected_sequence_does_not_advance_tracker() {
        let mut tracker = InputSequenceTracker::default();
        assert!(tracker.accept(10));
        assert!(!tracker.accept(9));
        assert_eq!(tracker.last_nonzero(), 10);
        assert!(tracker.accept(11));
    }

    #[test]
    fn fractional_motion_retains_subpixel_deltas_and_clears() {
        let mut accumulator = FractionalMotionAccumulator::default();
        assert_eq!(
            accumulator.accumulate(0.4, -0.4).expect("finite"),
            AccumulatedRelativeMotion { dx: 0, dy: 0 }
        );
        assert_eq!(
            accumulator.accumulate(0.8, -0.8).expect("finite"),
            AccumulatedRelativeMotion { dx: 1, dy: -1 }
        );
        let (x, y) = accumulator.residual();
        assert!((x - 0.2).abs() < f64::EPSILON * 4.0);
        assert!((y + 0.2).abs() < f64::EPSILON * 4.0);
        accumulator.clear();
        assert_eq!(accumulator.residual(), (0.0, 0.0));
    }

    #[test]
    fn fractional_motion_fails_closed_for_non_finite_input() {
        let mut accumulator = FractionalMotionAccumulator::default();
        accumulator.accumulate(0.5, 0.5).expect("finite");
        assert_eq!(
            accumulator.accumulate(0.75, f64::NAN),
            Err(InputContractError::NonFinite("dy"))
        );
        assert_eq!(accumulator.residual(), (0.5, 0.5));
    }

    #[test]
    fn relative_event_exposes_global_sequence() {
        let event = InputEvent::PointerRelativeMotion(PointerRelativeMotion {
            dx: -4,
            dy: 7,
            metadata: LowLatencyMetadata {
                sequence: 42,
                ..LowLatencyMetadata::default()
            },
        });
        assert_eq!(event.sequence(), 42);
    }
}
