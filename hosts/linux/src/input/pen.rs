//! Portable, pure pen/tablet mapping and idempotent tablet-tool state machine.
//!
//! Kept free of any `evdev`/uinput dependency: `evdev` is a Linux-only Cargo
//! dependency (`[target.'cfg(target_os = "linux")'.dependencies]` in
//! `Cargo.toml`), so a module that imported it could not be unit tested on a
//! non-Linux development machine. `uinput.rs` (Linux-only) owns the real
//! `evdev::uinput::VirtualDevice` and converts the plain `(code, pressed)`
//! edges and mapped axis integers this module returns into real
//! `evdev::InputEvent`s; its own Linux-gated test module proves the local
//! `BTN_*`/`ABS_*` code constants below equal the real `evdev` constants.
//!
//! Every function here operates on an already [`PenEventMsg::validate`]d
//! sample — callers must validate before calling into this module (and
//! before advancing the shared [`arcen_input::InputSequenceTracker`]), so the
//! mapping functions clamp defensively but never need to reject input.

use arcen_input::{PenTool, RegionPenSample};
use arcen_protocol::messages::{PenEventMsg, PenToolMsg};

/// Inclusive maximum of the documented 13-bit Linux `ABS_PRESSURE` range
/// (`2^13 - 1`). This mirrors the magnitude Wacom's own Linux driver reports
/// for `ABS_PRESSURE` on modern pens (e.g. Pro Pen 2), so a generic virtual
/// tablet built on this range does not overclaim vendor-specific resolution
/// while still preserving the full source pressure precision Arcen's typed
/// pen contract can carry.
pub const PRESSURE_MAX_13BIT: i32 = 8_191;

/// Whole-degree tilt bounds. `uinput.rs` declares the `ABS_TILT_X`/
/// `ABS_TILT_Y` `AbsInfo` with this exact range, so the wire's `-90.0..=90.0`
/// degree convention passes straight through as whole evdev units instead of
/// inventing a new scale.
pub const TILT_MIN_DEGREES: i32 = -90;
pub const TILT_MAX_DEGREES: i32 = 90;

// evdev key codes for the tablet-tool device. Kept as local, documented
// constants (not imported from `evdev::KeyCode`) so this module stays
// portable; see the module doc comment above.
/// `BTN_TOOL_PEN` (0x140): tip tool in proximity.
pub const BTN_TOOL_PEN: u16 = 0x140;
/// `BTN_TOOL_RUBBER` (0x141): eraser tool in proximity.
pub const BTN_TOOL_RUBBER: u16 = 0x141;
/// `BTN_TOUCH` (0x14a): tip/eraser touching the surface.
pub const BTN_TOUCH: u16 = 0x14a;
/// `BTN_STYLUS` (0x14b): first barrel button.
pub const BTN_STYLUS: u16 = 0x14b;
/// `BTN_STYLUS2` (0x14c): second barrel button.
pub const BTN_STYLUS2: u16 = 0x14c;

/// Bit assigned to the first barrel button in `PenEventMsg::buttons`.
const BUTTON_STYLUS_BIT: u16 = 0x1;
/// Bit assigned to the second barrel button in `PenEventMsg::buttons`.
const BUTTON_STYLUS2_BIT: u16 = 0x2;

/// Maps a normalized `0.0..=1.0` axis value onto a fixed device-space
/// integer raster. Shared by the absolute-mouse device and the tablet-tool
/// device so both stay consistent with the compositor's single coordinate
/// mapping (the fixed X11 raster `InputController` already uses for the
/// mouse) rather than inventing a second, disagreeing coordinate space.
#[must_use]
pub fn normalized_axis(value: f64, extent: u32) -> i32 {
    let maximum = f64::from(extent.saturating_sub(1));
    #[allow(clippy::cast_possible_truncation)]
    let mapped = (value.clamp(0.0, 1.0) * maximum).round() as i32;
    mapped
}

/// Maps validated `0.0..=1.0` wire pressure onto the documented inclusive
/// 13-bit Linux `ABS_PRESSURE` range, clamping defensively at the boundary.
#[must_use]
pub fn pressure_to_13bit(pressure: f32) -> i32 {
    let clamped = pressure.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation)]
    let mapped = (clamped * PRESSURE_MAX_13BIT as f32).round() as i32;
    mapped.clamp(0, PRESSURE_MAX_13BIT)
}

/// Maps validated `-90.0..=90.0` degree tilt directly onto the whole-degree
/// `ABS_TILT_X`/`ABS_TILT_Y` axis declared with a matching `AbsInfo` range.
#[must_use]
pub fn tilt_to_evdev_degrees(tilt_degrees: f32) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let mapped = tilt_degrees.clamp(-90.0, 90.0).round() as i32;
    mapped.clamp(TILT_MIN_DEGREES, TILT_MAX_DEGREES)
}

/// Mapped tablet-tool absolute axis values ready for `EV_ABS` emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PenAxes {
    pub x: i32,
    pub y: i32,
    pub pressure: i32,
    pub tilt_x: i32,
    pub tilt_y: i32,
}

impl PenAxes {
    /// Maps every tablet axis from one validated `PenEventMsg` and the fixed
    /// device raster `width`/`height`.
    #[must_use]
    pub fn from_event(event: &PenEventMsg, width: u32, height: u32) -> Self {
        Self {
            x: normalized_axis(event.x, width),
            y: normalized_axis(event.y, height),
            pressure: pressure_to_13bit(event.pressure),
            tilt_x: tilt_to_evdev_degrees(event.tilt_x_degrees),
            tilt_y: tilt_to_evdev_degrees(event.tilt_y_degrees),
        }
    }

    /// Maps non-coordinate axes from a shared region pen sample while
    /// retaining the already-transformed Xorg raster position.
    #[must_use]
    pub fn from_region_sample(sample: &RegionPenSample, x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            pressure: pressure_to_13bit(sample.pressure),
            tilt_x: tilt_to_evdev_degrees(sample.tilt_x_degrees),
            tilt_y: tilt_to_evdev_degrees(sample.tilt_y_degrees),
        }
    }
}

/// Idempotent tablet-tool key state: which tool is asserted in proximity (if
/// any), tip contact, and the two barrel-button bits. This is exactly the
/// state a full teardown/reset/drop must release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PenToolState {
    pub tool: Option<PenToolMsg>,
    pub touching: bool,
    pub stylus: bool,
    pub stylus2: bool,
}

impl PenToolState {
    /// Every `BTN_*` code this state currently holds high, in a defensive
    /// release order (buttons and contact before the tool bit itself), for a
    /// full teardown release on reset/drop.
    #[must_use]
    pub fn held_codes(self) -> Vec<u16> {
        let mut codes = Vec::with_capacity(4);
        if self.stylus {
            codes.push(BTN_STYLUS);
        }
        if self.stylus2 {
            codes.push(BTN_STYLUS2);
        }
        if self.touching {
            codes.push(BTN_TOUCH);
        }
        if let Some(code) = tool_code(self.tool) {
            codes.push(code);
        }
        codes
    }
}

fn tool_code(tool: Option<PenToolMsg>) -> Option<u16> {
    match tool {
        Some(PenToolMsg::Tip) => Some(BTN_TOOL_PEN),
        Some(PenToolMsg::Eraser) => Some(BTN_TOOL_RUBBER),
        None => None,
    }
}

/// Plans the ordered, idempotent `EV_KEY` edges needed to move from
/// `previous` to the tool/contact/button truth carried by `event`, and
/// returns the new state to commit.
///
/// An out-of-proximity sample (`in_proximity = false`) is always treated as
/// fully released (no touch, no buttons) regardless of what a peer sent, so
/// a stale or malformed payload can never leave the tip or a barrel button
/// logically stuck once the tool has physically lifted away. Entering
/// proximity asserts the tool bit before any touch/button edge in the same
/// batch; leaving proximity releases touch/buttons before the tool bit, both
/// matching the order a physical tablet reports.
#[must_use]
pub fn plan_pen_edges(
    previous: PenToolState,
    event: &PenEventMsg,
) -> (Vec<(u16, bool)>, PenToolState) {
    plan_pen_edges_from_fields(
        previous,
        event.tool,
        event.in_proximity,
        event.touching,
        event.buttons,
    )
}

/// Plans Linux tablet-tool edges from the canonical shared region pen sample.
#[must_use]
pub fn plan_region_pen_edges(
    previous: PenToolState,
    sample: &RegionPenSample,
) -> (Vec<(u16, bool)>, PenToolState) {
    plan_pen_edges_from_fields(
        previous,
        wire_pen_tool(sample.tool),
        sample.in_proximity,
        sample.touching,
        sample.buttons,
    )
}

#[must_use]
pub const fn wire_pen_tool(tool: PenTool) -> PenToolMsg {
    match tool {
        PenTool::Tip => PenToolMsg::Tip,
        PenTool::Eraser => PenToolMsg::Eraser,
    }
}

fn plan_pen_edges_from_fields(
    previous: PenToolState,
    tool: PenToolMsg,
    in_proximity: bool,
    touching: bool,
    buttons: u16,
) -> (Vec<(u16, bool)>, PenToolState) {
    let requested_tool = in_proximity.then_some(tool);
    let (requested_touching, requested_stylus, requested_stylus2) = if in_proximity {
        (
            touching,
            buttons & BUTTON_STYLUS_BIT != 0,
            buttons & BUTTON_STYLUS2_BIT != 0,
        )
    } else {
        (false, false, false)
    };

    let entering = previous.tool.is_none() && requested_tool.is_some();
    let leaving = previous.tool.is_some() && requested_tool.is_none();
    let mut edges = Vec::with_capacity(4);

    if entering {
        if let Some(code) = tool_code(requested_tool) {
            edges.push((code, true));
        }
    } else if !leaving && previous.tool != requested_tool {
        // Tool swap without a proximity break (Tip <-> Eraser mid-hover).
        // Real hardware rarely does this, but stay idempotent and correct
        // regardless: release the old bit before asserting the new one.
        if let Some(code) = tool_code(previous.tool) {
            edges.push((code, false));
        }
        if let Some(code) = tool_code(requested_tool) {
            edges.push((code, true));
        }
    }

    if previous.stylus != requested_stylus {
        edges.push((BTN_STYLUS, requested_stylus));
    }
    if previous.stylus2 != requested_stylus2 {
        edges.push((BTN_STYLUS2, requested_stylus2));
    }
    if previous.touching != requested_touching {
        edges.push((BTN_TOUCH, requested_touching));
    }

    if leaving {
        if let Some(code) = tool_code(previous.tool) {
            edges.push((code, false));
        }
    }

    (
        edges,
        PenToolState {
            tool: requested_tool,
            touching: requested_touching,
            stylus: requested_stylus,
            stylus2: requested_stylus2,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tool: PenToolMsg, in_proximity: bool, touching: bool, buttons: u16) -> PenEventMsg {
        PenEventMsg {
            tool,
            in_proximity,
            touching,
            buttons,
            ..PenEventMsg::default()
        }
    }

    #[test]
    fn pressure_maps_full_13bit_range_and_clamps_out_of_bounds() {
        assert_eq!(pressure_to_13bit(0.0), 0);
        assert_eq!(pressure_to_13bit(1.0), PRESSURE_MAX_13BIT);
        assert_eq!(pressure_to_13bit(0.5), 4_096); // round(0.5 * 8191) = 4096
        assert_eq!(pressure_to_13bit(-1.0), 0);
        assert_eq!(pressure_to_13bit(2.0), PRESSURE_MAX_13BIT);
    }

    #[test]
    fn tilt_passes_through_whole_degrees_and_clamps() {
        assert_eq!(tilt_to_evdev_degrees(0.0), 0);
        assert_eq!(tilt_to_evdev_degrees(-90.0), -90);
        assert_eq!(tilt_to_evdev_degrees(90.0), 90);
        assert_eq!(tilt_to_evdev_degrees(45.4), 45);
        assert_eq!(tilt_to_evdev_degrees(45.6), 46);
        assert_eq!(tilt_to_evdev_degrees(-120.0), -90);
        assert_eq!(tilt_to_evdev_degrees(120.0), 90);
    }

    #[test]
    fn normalized_axis_clamps_to_device_extent() {
        assert_eq!(normalized_axis(-1.0, 1920), 0);
        assert_eq!(normalized_axis(0.5, 1920), 960);
        assert_eq!(normalized_axis(2.0, 1920), 1919);
    }

    #[test]
    fn axes_map_every_field_from_one_validated_event() {
        let event = PenEventMsg {
            x: 0.5,
            y: 0.25,
            pressure: 1.0,
            tilt_x_degrees: -45.0,
            tilt_y_degrees: 45.0,
            ..PenEventMsg::default()
        };
        let axes = PenAxes::from_event(&event, 1920, 1080);
        assert_eq!(axes.x, 960);
        assert_eq!(axes.y, 270);
        assert_eq!(axes.pressure, PRESSURE_MAX_13BIT);
        assert_eq!(axes.tilt_x, -45);
        assert_eq!(axes.tilt_y, 45);
    }

    #[test]
    fn proximity_in_asserts_tool_before_touch_and_buttons() {
        let previous = PenToolState::default();
        let event = sample(PenToolMsg::Tip, true, true, 0b11);
        let (edges, state) = plan_pen_edges(previous, &event);
        assert_eq!(
            edges,
            vec![
                (BTN_TOOL_PEN, true),
                (BTN_STYLUS, true),
                (BTN_STYLUS2, true),
                (BTN_TOUCH, true),
            ]
        );
        assert_eq!(
            state,
            PenToolState {
                tool: Some(PenToolMsg::Tip),
                touching: true,
                stylus: true,
                stylus2: true,
            }
        );
    }

    #[test]
    fn proximity_out_releases_touch_and_buttons_before_tool() {
        let previous = PenToolState {
            tool: Some(PenToolMsg::Tip),
            touching: true,
            stylus: true,
            stylus2: true,
        };
        let event = sample(PenToolMsg::Tip, false, false, 0);
        let (edges, state) = plan_pen_edges(previous, &event);
        assert_eq!(
            edges,
            vec![
                (BTN_STYLUS, false),
                (BTN_STYLUS2, false),
                (BTN_TOUCH, false),
                (BTN_TOOL_PEN, false),
            ]
        );
        assert_eq!(state, PenToolState::default());
    }

    #[test]
    fn identical_repeated_sample_is_fully_idempotent() {
        let previous = PenToolState::default();
        let event = sample(PenToolMsg::Tip, true, true, 0b01);
        let (_, state) = plan_pen_edges(previous, &event);
        let (edges, state2) = plan_pen_edges(state, &event);
        assert!(edges.is_empty(), "unchanged sample must emit no edges");
        assert_eq!(state, state2);
    }

    #[test]
    fn eraser_tool_uses_the_rubber_tool_code() {
        let previous = PenToolState::default();
        let event = sample(PenToolMsg::Eraser, true, false, 0);
        let (edges, state) = plan_pen_edges(previous, &event);
        assert_eq!(edges, vec![(BTN_TOOL_RUBBER, true)]);
        assert_eq!(state.tool, Some(PenToolMsg::Eraser));
    }

    #[test]
    fn tool_swap_mid_hover_releases_old_tool_before_asserting_new_tool() {
        let previous = PenToolState {
            tool: Some(PenToolMsg::Tip),
            touching: false,
            stylus: false,
            stylus2: false,
        };
        let event = sample(PenToolMsg::Eraser, true, false, 0);
        let (edges, state) = plan_pen_edges(previous, &event);
        assert_eq!(edges, vec![(BTN_TOOL_PEN, false), (BTN_TOOL_RUBBER, true)]);
        assert_eq!(state.tool, Some(PenToolMsg::Eraser));
    }

    #[test]
    fn out_of_proximity_sample_ignores_touch_and_button_bits_it_still_carries() {
        // A stale/malformed peer could still set touching/buttons alongside
        // in_proximity=false; the planner must not honor them.
        let previous = PenToolState::default();
        let event = sample(PenToolMsg::Tip, false, true, 0b11);
        let (edges, state) = plan_pen_edges(previous, &event);
        assert!(
            edges.is_empty(),
            "never-entered proximity holds nothing to release"
        );
        assert_eq!(state, PenToolState::default());
    }

    #[test]
    fn button_edges_are_independently_idempotent() {
        let mut state = PenToolState::default();
        let (edges, next) = plan_pen_edges(state, &sample(PenToolMsg::Tip, true, false, 0b01));
        assert_eq!(edges, vec![(BTN_TOOL_PEN, true), (BTN_STYLUS, true)]);
        state = next;

        let (edges, next) = plan_pen_edges(state, &sample(PenToolMsg::Tip, true, false, 0b11));
        assert_eq!(edges, vec![(BTN_STYLUS2, true)]);
        state = next;

        let (edges, _) = plan_pen_edges(state, &sample(PenToolMsg::Tip, true, false, 0b11));
        assert!(edges.is_empty());
    }

    #[test]
    fn held_codes_lists_every_asserted_bit_for_teardown() {
        let state = PenToolState {
            tool: Some(PenToolMsg::Eraser),
            touching: true,
            stylus: true,
            stylus2: false,
        };
        assert_eq!(
            state.held_codes(),
            vec![BTN_STYLUS, BTN_TOUCH, BTN_TOOL_RUBBER]
        );
        assert!(PenToolState::default().held_codes().is_empty());
    }
}
