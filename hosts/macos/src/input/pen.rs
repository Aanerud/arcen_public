// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Pen injection: Basic Tablet termination on macOS.
//!
//! In Basic Tablet the Deck's own tablet driver reads the pen and sends
//! finished samples. This host's job is to put them on the local desktop with
//! their professional surface intact — pressure, tilt, rotation, eraser,
//! proximity and barrel buttons — so an application here sees a tablet rather
//! than a mouse.
//!
//! macOS has no separate tablet event. A tablet is a mouse event carrying a
//! *subtype* and a set of tablet fields, which is why this module produces
//! ordinary mouse events and then annotates them. Two details in that mapping
//! are not guessable from the API and were read from the SDK headers rather
//! than assumed:
//!
//! * the subtype field is `kCGMouseEventSubtype`, which is **7**, not one of
//!   the `kCGTabletEvent*` numbers it sits beside;
//! * tilt is a **normalized** double where 1 is maximum tilt, while the wire
//!   and `arcen-input` carry degrees. Passing degrees through unconverted
//!   would report every sample as fully tilted.
//!
//! Three more were measured with an `AppKit` window reading the events back
//! (macOS 26.7), and each had been wrong while every counter said the pen
//! worked:
//!
//! * `-[NSEvent pressure]` reads the *mouse* pressure field,
//!   `kCGMouseEventPressure` (2), not `kCGTabletEventPointPressure`.
//!   `CGEventCreateMouseEvent` sets it to 1.0 for a press or drag, so with
//!   only the tablet field set every stroke read as full pressure. Both are
//!   set now: the mouse field carries 256 levels, which is all `NSEvent`
//!   offers, and the tablet field the full-precision value for applications
//!   that read it from the `CGEvent`.
//! * A proximity edge must be its own event type, `kCGEventTabletProximity`
//!   (24). A mouse-moved event with the proximity subtype reaches `AppKit` as
//!   an ordinary move, so `tabletProximity:` never ran and an application
//!   never learnt a pen — or an eraser — had arrived.
//! * The proximity event must carry a capability mask. With none, a toolkit
//!   that sizes its tablet support from `-[NSEvent capabilityMask]` concludes
//!   the pen has no pressure or tilt.
//!
//! A virtual HID digitizer does not replace any of this on macOS: measured,
//! the HID system delivers its proximity but empties pressure, tilt and
//! rotation (`arcen-pier-macos probe-pen-hid`).
//!
//! Anything about *when* an edge happens — that a tool leaving the tablet
//! releases what it held, that entering asserts the tool before a press — is
//! shared policy in [`arcen_input::plan_pen_edges`], because it is identical
//! on Linux and would otherwise be written twice and drift.

use arcen_input::{PenEdge, PenTool, PenToolState, pen_axes, plan_pen_edges};
use arcen_protocol::messages::{PenEventMsg, PenToolMsg};

/// `kCGMouseEventSubtype`. Read from `CGEventTypes.h`; it is 7, which is not
/// adjacent to the tablet field numbers and is easy to get wrong by guessing.
pub const K_CG_MOUSE_EVENT_SUBTYPE: u32 = 7;

/// `kCGEventMouseSubtypeTabletPoint`.
pub const K_CG_MOUSE_SUBTYPE_TABLET_POINT: i64 = 1;
/// `kCGEventMouseSubtypeTabletProximity`.
pub const K_CG_MOUSE_SUBTYPE_TABLET_PROXIMITY: i64 = 2;

/// `kCGMouseEventPressure`, a double in 0.0..=1.0. What `-[NSEvent pressure]`
/// returns, for tablet events too.
pub const K_CG_MOUSE_EVENT_PRESSURE: u32 = 2;

/// `kCGEventTabletProximity`, the event type of a proximity edge.
pub const K_CG_EVENT_TABLET_PROXIMITY: u32 = 24;

/// `kCGTabletEventPointPressure`, a double in 0.0..=1.0.
pub const K_CG_TABLET_EVENT_POINT_PRESSURE: u32 = 19;
/// `kCGTabletEventTiltX`, a normalized double in -1.0..=1.0.
pub const K_CG_TABLET_EVENT_TILT_X: u32 = 20;
/// `kCGTabletEventTiltY`, a normalized double in -1.0..=1.0.
pub const K_CG_TABLET_EVENT_TILT_Y: u32 = 21;
/// `kCGTabletEventRotation`, a double in degrees.
pub const K_CG_TABLET_EVENT_ROTATION: u32 = 22;
/// `kCGTabletEventPointButtons`.
pub const K_CG_TABLET_EVENT_POINT_BUTTONS: u32 = 18;
/// `kCGTabletEventDeviceID`.
pub const K_CG_TABLET_EVENT_DEVICE_ID: u32 = 24;
/// `kCGTabletProximityEventDeviceID`.
pub const K_CG_TABLET_PROXIMITY_EVENT_DEVICE_ID: u32 = 31;
/// `kCGTabletProximityEventPointerType`.
pub const K_CG_TABLET_PROXIMITY_EVENT_POINTER_TYPE: u32 = 37;
/// `kCGTabletProximityEventEnterProximity`.
pub const K_CG_TABLET_PROXIMITY_EVENT_ENTER_PROXIMITY: u32 = 38;
/// `kCGTabletProximityEventVendorID`.
pub const K_CG_TABLET_PROXIMITY_EVENT_VENDOR_ID: u32 = 28;
/// `kCGTabletProximityEventSystemTabletID`.
pub const K_CG_TABLET_PROXIMITY_EVENT_SYSTEM_TABLET_ID: u32 = 32;
/// `kCGTabletProximityEventTabletID`.
pub const K_CG_TABLET_PROXIMITY_EVENT_TABLET_ID: u32 = 29;
/// `kCGTabletProximityEventPointerID`.
pub const K_CG_TABLET_PROXIMITY_EVENT_POINTER_ID: u32 = 30;
/// `kCGTabletProximityEventCapabilityMask`.
pub const K_CG_TABLET_PROXIMITY_EVENT_CAPABILITY_MASK: u32 = 36;

/// `NX_TABLET_CAPABILITY_*` bits from `IOLLEvent.h`, for the fields this
/// host fills on every tablet point: device id, absolute X and Y, buttons,
/// both tilts, pressure and rotation. Not Z, and not tangential pressure,
/// which no wire sample carries.
pub const TABLET_CAPABILITIES: i64 = NX_TABLET_CAPABILITY_DEVICEID
    | NX_TABLET_CAPABILITY_ABSX
    | NX_TABLET_CAPABILITY_ABSY
    | NX_TABLET_CAPABILITY_BUTTONS
    | NX_TABLET_CAPABILITY_TILTX
    | NX_TABLET_CAPABILITY_TILTY
    | NX_TABLET_CAPABILITY_PRESSURE
    | NX_TABLET_CAPABILITY_ROTATION;
const NX_TABLET_CAPABILITY_DEVICEID: i64 = 0x0001;
const NX_TABLET_CAPABILITY_ABSX: i64 = 0x0002;
const NX_TABLET_CAPABILITY_ABSY: i64 = 0x0004;
const NX_TABLET_CAPABILITY_BUTTONS: i64 = 0x0040;
const NX_TABLET_CAPABILITY_TILTX: i64 = 0x0080;
const NX_TABLET_CAPABILITY_TILTY: i64 = 0x0100;
const NX_TABLET_CAPABILITY_PRESSURE: i64 = 0x0400;
const NX_TABLET_CAPABILITY_ROTATION: i64 = 0x2000;

/// `NX_TABLET_POINTER_PEN`, from `IOLLEvent.h`.
pub const NX_TABLET_POINTER_PEN: i64 = 1;
/// `NX_TABLET_POINTER_ERASER`, from `IOLLEvent.h`.
pub const NX_TABLET_POINTER_ERASER: i64 = 3;

/// The vendor id this host's virtual pen reports in tablet proximity events.
///
/// It is `0xFFFF`, the USB-IF reserved id that is never assigned to any
/// company, and the same id Arcen uses for every synthetic device
/// (`arcen_usb_bridge::ARCEN_LAB_VENDOR_ID`). It used to be Wacom's id, on
/// the reasoning that the pen samples came from a Wacom digitizer; with any
/// tablet now supported that would be a false claim for most pens, and for a
/// Wacom it is still not the device the application is talking to.
///
/// Nothing on macOS requires a particular vendor here. `AppKit` and
/// `CoreGraphics` deliver tablet pressure, tilt, rotation and proximity from the
/// event fields regardless of vendor, and real tablets' point events already
/// arrive with vendor `0` (measured on the Deck: `-[NSEvent vendorID]` reads
/// 0 for `TabletPoint`-subtype mouse events). The id stays non-zero so one
/// Arcen session reads as one identified tablet in proximity events.
pub const ARCEN_TABLET_VENDOR_ID: i64 = 0xFFFF;
/// Device id for the single pen this host presents.
pub const ARCEN_TABLET_DEVICE_ID: i64 = 1;

/// The tablet annotations for one sample, already in CoreGraphics units.
///
/// Separated from posting so the conversion can be tested without a window
/// server: everything here is arithmetic, and it is the arithmetic that has
/// the unit mismatch in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabletPoint {
    /// Pressure, 0.0..=1.0.
    pub pressure: f64,
    /// Normalized horizontal tilt, -1.0..=1.0.
    pub tilt_x: f64,
    /// Normalized vertical tilt, -1.0..=1.0.
    pub tilt_y: f64,
    /// Rotation in degrees, 0.0..360.0.
    pub rotation_degrees: f64,
    /// `kCGTabletEventPointButtons`, in the layout `-[NSEvent buttonMask]`
    /// returns: see [`tablet_button_mask`].
    pub buttons: i64,
    /// `NX_TABLET_POINTER_*` for the tool in use.
    pub pointer_type: i64,
}

/// Converts a wire sample into CoreGraphics tablet annotations.
///
/// Degrees become the normalized -1..=1 CoreGraphics tilt. Values are clamped
/// rather than trusted: `PenEventMsg` is validated at the edge, but this
/// conversion also runs on samples rebuilt from a resumed session, and a
/// non-finite or out-of-range value reaching `CGEventSetDoubleValueField`
/// produces an unpredictable pointer rather than a rejected message.
#[must_use]
pub fn tablet_point(event: &PenEventMsg) -> TabletPoint {
    TabletPoint {
        pressure: f64::from(pen_axes::clamp_pressure(f64::from(event.pressure))),
        tilt_x: pen_axes::tilt_degrees_to_unit(event.tilt_x_degrees),
        tilt_y: pen_axes::tilt_degrees_to_unit(event.tilt_y_degrees),
        rotation_degrees: f64::from(pen_axes::wrap_rotation_degrees(f64::from(
            event.rotation_degrees,
        ))),
        buttons: tablet_button_mask(event.touching, event.buttons),
        pointer_type: pointer_type(event.tool),
    }
}

/// `NSEventButtonMaskPenTip`: the tip (or eraser) is on the surface.
pub const PEN_BUTTON_MASK_TIP: i64 = 1 << 0;
/// `NSEventButtonMaskPenLowerSide`: the first barrel button.
pub const PEN_BUTTON_MASK_LOWER_SIDE: i64 = 1 << 1;
/// `NSEventButtonMaskPenUpperSide`: the second barrel button.
pub const PEN_BUTTON_MASK_UPPER_SIDE: i64 = 1 << 2;

/// The tablet button field for one sample.
///
/// `-[NSEvent buttonMask]` returns `kCGTabletEventPointButtons` unchanged, and
/// `AppKit`'s layout is not the wire's: bit 0 is tip contact and the barrel
/// buttons are bits 1 and 2, while the wire's `buttons` carries only barrel
/// buttons from bit 0. Copied across verbatim, every stroke read as no button
/// and the first barrel button as the tip (measured on macOS 26.7: a tip-down
/// drag `buttonMask=0`, a barrel click `buttonMask=0x1`). Barrel buttons past
/// the second have no `AppKit` bit and are left out, as they are for clicks.
#[must_use]
pub const fn tablet_button_mask(touching: bool, wire_buttons: u16) -> i64 {
    let mut mask = 0;
    if touching {
        mask |= PEN_BUTTON_MASK_TIP;
    }
    if wire_buttons & 0b01 != 0 {
        mask |= PEN_BUTTON_MASK_LOWER_SIDE;
    }
    if wire_buttons & 0b10 != 0 {
        mask |= PEN_BUTTON_MASK_UPPER_SIDE;
    }
    mask
}

/// Annotations for a tool that is leaving the tablet.
///
/// Everything a pen can hold reads as released, because a tool out of
/// proximity is holding nothing. Used when a session ends without the client
/// having sent a final sample, so the releases that go out carry tablet
/// annotations rather than looking like bare mouse events.
#[must_use]
pub const fn tablet_point_away(pointer_type: i64) -> TabletPoint {
    TabletPoint {
        pressure: 0.0,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rotation_degrees: 0.0,
        buttons: 0,
        pointer_type,
    }
}

/// Returns the `NX_TABLET_POINTER_*` value for a wire tool.
#[must_use]
pub const fn pointer_type(tool: PenToolMsg) -> i64 {
    match tool {
        PenToolMsg::Tip => NX_TABLET_POINTER_PEN,
        PenToolMsg::Eraser => NX_TABLET_POINTER_ERASER,
    }
}

/// Converts a wire tool to the shared enum.
#[must_use]
pub const fn shared_tool(tool: PenToolMsg) -> PenTool {
    match tool {
        PenToolMsg::Tip => PenTool::Tip,
        PenToolMsg::Eraser => PenTool::Eraser,
    }
}

/// Plans the edges for a sample against `previous`.
///
/// A thin wrapper that keeps the wire-to-shared conversion in one place.
#[must_use]
pub fn plan(previous: PenToolState, event: &PenEventMsg) -> (Vec<PenEdge>, PenToolState) {
    plan_pen_edges(
        previous,
        shared_tool(event.tool),
        event.in_proximity,
        event.touching,
        event.buttons,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PenEventMsg {
        PenEventMsg {
            msg_type: "pen_event".to_string(),
            x: 0.5,
            y: 0.5,
            server_x: 0,
            server_y: 0,
            pressure: 0.5,
            tilt_x_degrees: 45.0,
            tilt_y_degrees: -90.0,
            rotation_degrees: 90.0,
            tool: PenToolMsg::Tip,
            in_proximity: true,
            touching: true,
            buttons: 0,
            sequence: 1,
            timestamp_ns: 0,
            coalescable: true,
        }
    }

    #[test]
    fn tilt_degrees_become_normalized_coregraphics_tilt() {
        // The bug this guards: passing 45 degrees straight through reports
        // maximum tilt, because CoreGraphics reads 1.0 as full scale.
        let point = tablet_point(&sample());
        assert!((point.tilt_x - 0.5).abs() < f64::EPSILON);
        assert!((point.tilt_y + 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn out_of_range_and_non_finite_values_are_clamped_not_forwarded() {
        let mut event = sample();
        event.pressure = f32::NAN;
        event.tilt_x_degrees = 1_000.0;
        event.tilt_y_degrees = f32::NEG_INFINITY;
        event.rotation_degrees = f32::INFINITY;
        let point = tablet_point(&event);
        assert!((point.pressure - 0.0).abs() < f64::EPSILON);
        assert!((point.tilt_x - 1.0).abs() < f64::EPSILON);
        assert!((point.tilt_y - 0.0).abs() < f64::EPSILON);
        assert!((point.rotation_degrees - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rotation_wraps_rather_than_saturating() {
        let mut event = sample();
        event.rotation_degrees = 450.0;
        assert!((tablet_point(&event).rotation_degrees - 90.0).abs() < 1e-9);
        event.rotation_degrees = -90.0;
        assert!((tablet_point(&event).rotation_degrees - 270.0).abs() < 1e-9);
    }

    #[test]
    fn the_eraser_reports_as_an_eraser_not_a_pen() {
        let mut event = sample();
        event.tool = PenToolMsg::Eraser;
        assert_eq!(tablet_point(&event).pointer_type, NX_TABLET_POINTER_ERASER);
        assert_eq!(tablet_point(&sample()).pointer_type, NX_TABLET_POINTER_PEN);
    }

    #[test]
    fn the_virtual_pen_claims_no_tablet_makers_vendor_id() {
        // Reserved, never-assigned, and Arcen's id for every synthetic device.
        assert_eq!(ARCEN_TABLET_VENDOR_ID, 0xFFFF);
        assert_ne!(ARCEN_TABLET_VENDOR_ID, 0);
    }

    #[test]
    fn pressure_passes_through_the_shared_normalisation_unchanged() {
        for pressure in [0.0_f32, 0.25, 0.5, 1.0] {
            let mut event = sample();
            event.pressure = pressure;
            assert!((tablet_point(&event).pressure - f64::from(pressure)).abs() < 1e-6);
        }
    }

    #[test]
    fn the_tablet_button_field_uses_appkits_pen_layout_not_the_wires() {
        let mut event = sample();
        assert_eq!(tablet_point(&event).buttons, PEN_BUTTON_MASK_TIP);

        event.touching = false;
        event.buttons = 0b01;
        assert_eq!(tablet_point(&event).buttons, PEN_BUTTON_MASK_LOWER_SIDE);

        event.buttons = 0b10;
        assert_eq!(tablet_point(&event).buttons, PEN_BUTTON_MASK_UPPER_SIDE);

        event.touching = true;
        event.buttons = 0b1111;
        assert_eq!(
            tablet_point(&event).buttons,
            PEN_BUTTON_MASK_TIP | PEN_BUTTON_MASK_LOWER_SIDE | PEN_BUTTON_MASK_UPPER_SIDE
        );

        event.touching = false;
        event.buttons = 0;
        assert_eq!(tablet_point(&event).buttons, 0);
        assert_eq!(tablet_point_away(NX_TABLET_POINTER_PEN).buttons, 0);
    }

    #[test]
    fn planning_delegates_to_the_shared_policy() {
        let (edges, state) = plan(PenToolState::default(), &sample());
        assert_eq!(edges, vec![PenEdge::ToolIn(PenTool::Tip), PenEdge::TipDown]);
        assert!(state.touching);
    }
}
