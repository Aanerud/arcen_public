//! Classifier tests over hand-written report descriptors.
//!
//! The descriptors below are written for these tests in the shapes the named
//! tablet families publish (standard Digitizers-page pens, vendor-mirrored
//! digitizer pages, push/pop global state, physical units, composite
//! interfaces). They are illustrative fixtures, not captures from specific
//! products, and every vendor name is informational: none of them is a gate.

use super::*;

/// Cintiq-style display tablet: standard Digitizers-page pen with tip, barrel,
/// eraser, invert, secondary barrel, in-range, 32-bit X/Y, 8192 pressure
/// levels, hundredth-degree tilt with a physical degree unit, and twist.
const CINTIQ_STYLE: &[u8] = &[
    0x05, 0x0D, // Usage Page (Digitizers)
    0x09, 0x02, // Usage (Pen)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x02, //   Report ID (2)
    0x09, 0x20, //   Usage (Stylus)
    0xA1, 0x00, //   Collection (Physical)
    0x09, 0x42, //     Tip Switch
    0x09, 0x44, //     Barrel Switch
    0x09, 0x45, //     Eraser
    0x09, 0x3C, //     Invert
    0x09, 0x5A, //     Secondary Barrel Switch
    0x09, 0x32, //     In Range
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x75, 0x01, //     Report Size (1)
    0x95, 0x06, //     Report Count (6)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x03, //     Input (Cnst) padding
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     X
    0x27, 0xA0, 0x86, 0x01, 0x00, // Logical Maximum (100000)
    0x75, 0x20, //     Report Size (32)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x02, //     Input
    0x09, 0x31, //     Y
    0x27, 0x50, 0xC3, 0x00, 0x00, // Logical Maximum (50000)
    0x81, 0x02, //     Input
    0x05, 0x0D, //     Usage Page (Digitizers)
    0x09, 0x30, //     Tip Pressure
    0x26, 0xFF, 0x1F, // Logical Maximum (8191)
    0x75, 0x10, //     Report Size (16)
    0x81, 0x02, //     Input
    0x09, 0x3D, //     X Tilt
    0x09, 0x3E, //     Y Tilt
    0x55, 0x0E, //     Unit Exponent (-2)
    0x65, 0x14, //     Unit (English Rotation: degrees)
    0x16, 0xD8, 0xDC, // Logical Minimum (-9000)
    0x26, 0x28, 0x23, // Logical Maximum (9000)
    0x36, 0xD8, 0xDC, // Physical Minimum (-9000)
    0x46, 0x28, 0x23, // Physical Maximum (9000)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x02, //     Input
    0x09, 0x41, //     Twist
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0x67, 0x01, // Logical Maximum (359)
    0x35, 0x00, //     Physical Minimum (0)
    0x46, 0x67, 0x01, // Physical Maximum (359)
    0x55, 0x00, //     Unit Exponent (0)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x02, //     Input
    0xC0, //   End Collection
    0xC0, // End Collection
];

/// Intuos-style pen interface on the vendor-mirrored digitizer page.
const INTUOS_STYLE_PEN_VENDOR_PAGE: &[u8] = &[
    0x06, 0x0D, 0xFF, // Usage Page (vendor-mirrored Digitizers, 0xFF0D)
    0x09, 0x02, // Usage (Pen)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x10, //   Report ID (16)
    0x09, 0x20, //   Usage (Stylus)
    0xA1, 0x00, //   Collection (Physical)
    0x09, 0x42, //     Tip Switch
    0x09, 0x44, //     Barrel Switch
    0x09, 0x45, //     Eraser
    0x09, 0x5A, //     Secondary Barrel Switch
    0x09, 0x32, //     In Range
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x05, 0x81, 0x02, //
    0x95, 0x03, 0x81, 0x03, // padding
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, 0x09, 0x31, // X, Y
    0x26, 0xFF, 0x7F, // Logical Maximum (32767)
    0x75, 0x10, 0x95, 0x02, 0x81, 0x02, //
    0x06, 0x0D, 0xFF, // Usage Page (0xFF0D)
    0x09, 0x30, //     Tip Pressure (mirrored)
    0x26, 0xFF, 0x0F, // Logical Maximum (4095)
    0x95, 0x01, 0x81, 0x02, //
    0x09, 0x3D, 0x09, 0x3E, // X/Y Tilt (mirrored)
    0x15, 0xC4, 0x25, 0x3C, // -60..60
    0x75, 0x08, 0x95, 0x02, 0x81, 0x02, //
    0xC0, 0xC0,
];

/// Intuos-style touch interface: a standard Digitizers-page touch pad.
const INTUOS_STYLE_TOUCH: &[u8] = &[
    0x05, 0x0D, // Usage Page (Digitizers)
    0x09, 0x05, // Usage (Touch Pad)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x03, //   Report ID (3)
    0x09, 0x22, //   Usage (Finger)
    0xA1, 0x02, //   Collection (Logical)
    0x09, 0x42, //     Tip Switch
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, //
    0x95, 0x07, 0x81, 0x03, //
    0x09, 0x51, //     Contact Identifier
    0x25, 0x0F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, // X, Y
    0x26, 0xFF, 0x0F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, //
    0xC0, //   End Collection
    0x05, 0x0D, 0x09, 0x54, // Contact Count
    0x25, 0x0A, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, //
    0xC0,
];

/// Opaque legacy pen interface: a boot-mouse collection followed by a
/// vendor-mirrored digitizer collection whose bytes are not described.
const LEGACY_OPAQUE_PEN: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, // Generic Desktop Mouse, Application
    0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, // Report ID 1, Pointer, Physical
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, // Buttons 1..3
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, //
    0x95, 0x01, 0x75, 0x05, 0x81, 0x03, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, // X, Y relative
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, //
    0xC0, 0xC0, //
    0x06, 0x0D, 0xFF, 0x09, 0x01, 0xA1, 0x01, // 0xFF0D Digitizer, Application
    0x85, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x01, // Report ID 2, opaque vendor usage
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x09, 0x81, 0x02, //
    0xC0,
];

/// Huion-style pen: Digitizer usage with nested Stylus, physical units under
/// Push/Pop, 8192 pressure levels, signed-byte tilt. The second interface is
/// the tablet's keypad, presented as a keyboard.
const HUION_STYLE_PEN: &[u8] = &[
    0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, // Digitizers Pen, Application
    0x85, 0x0A, 0x09, 0x20, 0xA1, 0x01, // Report ID 10, Stylus
    0x09, 0x42, 0x09, 0x44, 0x09, 0x45, // Tip, Barrel, Eraser
    0x95, 0x03, 0x75, 0x01, 0x15, 0x00, 0x25, 0x01, 0x81, 0x02, //
    0x95, 0x02, 0x81, 0x03, //
    0x09, 0x32, 0x95, 0x01, 0x81, 0x02, // In Range
    0x95, 0x02, 0x81, 0x03, //
    0x75, 0x10, 0x95, 0x01, //
    0xA4, // Push
    0x05, 0x01, 0x55, 0x0D, 0x65, 0x33, // Generic Desktop, exponent -3, inches
    0x09, 0x30, 0x35, 0x00, 0x46, 0x10, 0x27, 0x26, 0xFF, 0x7F, 0x81, 0x02, // X
    0x09, 0x31, 0x46, 0x70, 0x17, 0x26, 0xFF, 0x4F, 0x81, 0x02, // Y
    0xB4, // Pop
    0x09, 0x30, 0x26, 0xFF, 0x1F, 0x81, 0x02, // Tip Pressure 8191
    0x09, 0x3D, 0x09, 0x3E, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81,
    0x02, // Tilt
    0xC0, 0xC0,
];

const HUION_STYLE_KEYPAD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, // Generic Desktop Keyboard
    0x85, 0x03, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, // modifiers
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, //
    0x95, 0x06, 0x75, 0x08, 0x26, 0xFF, 0x00, 0x19, 0x00, 0x29, 0xFF, 0x81, 0x00, //
    0xC0,
];

/// XP-Pen-style pen: 16384 pressure levels, +/-60 degree tilt, no twist.
const XP_PEN_STYLE: &[u8] = &[
    0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, // Digitizers Pen
    0x85, 0x07, 0x09, 0x20, 0xA1, 0x00, // Report 7, Stylus
    0x09, 0x42, 0x09, 0x44, 0x09, 0x45, 0x09, 0x3C, // Tip, Barrel, Eraser, Invert
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x04, 0x81, 0x02, //
    0x95, 0x01, 0x81, 0x03, //
    0x09, 0x32, 0x95, 0x01, 0x81, 0x02, // In Range
    0x95, 0x02, 0x81, 0x03, //
    0x05, 0x01, 0x09, 0x30, 0x75, 0x10, 0x95, 0x01, 0x26, 0xFF, 0x7F, 0x81, 0x02, // X
    0x09, 0x31, 0x26, 0xFF, 0x4F, 0x81, 0x02, // Y
    0x05, 0x0D, 0x09, 0x30, 0x26, 0xFF, 0x3F, 0x81, 0x02, // Pressure 16383
    0x09, 0x3D, 0x09, 0x3E, 0x15, 0xC4, 0x25, 0x3C, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, //
    0xC0, 0xC0,
];

/// Gaomon-style pen: usage-range switches, 8192 levels, no tilt.
const GAOMON_STYLE: &[u8] = &[
    0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, // Digitizers Pen
    0x85, 0x08, 0x09, 0x20, 0xA1, 0x00, // Stylus
    0x19, 0x42, 0x29, 0x45, // Usage Minimum (Tip Switch) .. Maximum (Eraser)
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x04, 0x81, 0x02, //
    0x09, 0x32, 0x95, 0x01, 0x81, 0x02, // In Range
    0x95, 0x03, 0x81, 0x03, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81,
    0x02, //
    0x05, 0x0D, 0x09, 0x30, 0x26, 0xFF, 0x1F, 0x95, 0x01, 0x81, 0x02, //
    0xC0, 0xC0,
];

/// A generic pen from a vendor no list has heard of (think VID 0x1234):
/// Digitizer application usage, extended (32-bit) usages, 1024 levels.
const GENERIC_UNKNOWN_VENDOR: &[u8] = &[
    0x05, 0x0D, 0x09, 0x01, 0xA1, 0x01, // Digitizers Digitizer, Application
    0x09, 0x20, 0xA1, 0x00, // Stylus
    0x0B, 0x42, 0x00, 0x0D, 0x00, // Usage (Digitizers:Tip Switch), extended
    0x0B, 0x32, 0x00, 0x0D, 0x00, // Usage (Digitizers:In Range), extended
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02, 0x81, 0x02, //
    0x95, 0x06, 0x81, 0x03, //
    0x0B, 0x30, 0x00, 0x01, 0x00, // Usage (Generic Desktop:X), extended
    0x0B, 0x31, 0x00, 0x01, 0x00, // Usage (Generic Desktop:Y), extended
    0x26, 0xFF, 0x0F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, //
    0x09, 0x30, 0x26, 0xFF, 0x03, 0x95, 0x01, 0x81, 0x02, // Tip Pressure 1023
    0xC0, 0xC0,
];

/// Pressure-only pen: tip, X/Y and pressure; nothing else.
const PRESSURE_ONLY: &[u8] = &[
    0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, //
    0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, //
    0x95, 0x07, 0x81, 0x03, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x26, 0xFF, 0x0F, 0x75, 0x10, 0x95, 0x02, 0x81,
    0x02, //
    0x05, 0x0D, 0x09, 0x30, 0x26, 0xFF, 0x07, 0x95, 0x01, 0x81, 0x02, // 2047
    0xC0,
];

const KEYBOARD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x06, 0x75, 0x08,
    0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

const MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x03,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03,
    0x81, 0x06, 0xC0, 0xC0,
];

/// An absolute pointer: X/Y and buttons, the shape of a tablet in mouse mode.
/// Not a pen: it has no digitizer collection at all.
const ABSOLUTE_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x02,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x02, 0x75, 0x01, 0x81, 0x02, 0x95, 0x06, 0x81, 0x03, 0x05, 0x01,
    0x09, 0x30, 0x09, 0x31, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0xC0, 0xC0,
];

const GAMEPAD: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x10, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x10, 0x81, 0x02, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02, 0xC0,
];

const VENDOR_ONLY: &[u8] = &[
    0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x40,
    0x09, 0x01, 0x81, 0x02, 0x09, 0x01, 0x91, 0x02, 0xC0,
];

/// A touch screen with a finger collection and no stylus.
const TOUCH_SCREEN: &[u8] = &[
    0x05, 0x0D, 0x09, 0x04, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x22, 0xA1, 0x02, 0x09, 0x42, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x09, 0x32, 0x81, 0x02, 0x95, 0x06, 0x81, 0x03,
    0x09, 0x51, 0x25, 0x0A, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31,
    0x26, 0xFF, 0x0F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0xC0, 0x05, 0x0D, 0x09, 0x54, 0x25, 0x0A,
    0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0xC0,
];

/// A Digitizer-usage collection that is touch-shaped (finger, contact id),
/// which must not pass as a pen just because its usage is "Digitizer".
const DIGITIZER_USAGE_FINGER: &[u8] = &[
    0x05, 0x0D, 0x09, 0x01, 0xA1, 0x01, 0x09, 0x22, 0xA1, 0x02, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03, 0x09, 0x51, 0x25, 0x0A, 0x75, 0x08,
    0x81, 0x02, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x26, 0xFF, 0x0F, 0x75, 0x10, 0x95, 0x02, 0x81,
    0x02, 0xC0, 0xC0,
];

/// A Digitizers-page pen with a tip switch but no position.
const PEN_WITHOUT_POSITION: &[u8] = &[
    0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x20, 0xA1, 0x00, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03, 0xC0, 0xC0,
];

/// Position and pressure declared only as constant padding: not a pen.
const PEN_WITH_CONSTANT_AXES: &[u8] = &[
    0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x42, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x75, 0x10,
    0x95, 0x03, 0x81, 0x03, 0xC0,
];

fn classify(descriptors: &[&[u8]]) -> Result<PenTabletProfile, NotPenTablet> {
    classify_pen_tablet(descriptors.iter().copied())
}

#[test]
fn cintiq_style_pen_reports_its_full_surface() {
    let profile = classify(&[CINTIQ_STYLE]).unwrap();
    assert_eq!(profile.evidence, PenTabletEvidence::DigitizerPen);
    let caps = profile.capabilities;
    assert!(caps.tip_switch && caps.in_range && caps.eraser);
    assert_eq!(caps.barrel_buttons, 2);
    assert_eq!(caps.pressure_levels(), Some(8192));
    assert_eq!(caps.x.unwrap().logical_max, 100_000);
    assert_eq!(caps.y.unwrap().logical_max, 50_000);
    let tilt = caps.tilt_x.unwrap();
    assert_eq!((tilt.logical_min, tilt.logical_max), (-9000, 9000));
    assert_eq!(tilt.unit_exponent, -2);
    assert!(tilt.physical_unit_is_degrees());
    assert!(caps.has_tilt());
    let twist = caps.twist.unwrap();
    assert_eq!((twist.logical_min, twist.logical_max), (0, 359));
}

/// A collection on the vendor-defined Digitizers page whose usage is not
/// Digitizer or Pen (here: undefined). Never a tablet on its own.
const VENDOR_DIGITIZER_UNNAMED: &[u8] = &[
    0x06, 0x0D, 0xFF, 0x09, 0x00, 0xA1, 0x01, 0x09, 0x00, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08,
    0x95, 0x08, 0x81, 0x02, 0xC0,
];

#[test]
fn a_described_vendor_page_pen_is_a_tablet_and_keeps_its_declared_axes() {
    // Alone, beside a mouse, and beside a standard touch digitizer.
    for descriptors in [
        &[INTUOS_STYLE_PEN_VENDOR_PAGE][..],
        &[INTUOS_STYLE_PEN_VENDOR_PAGE, MOUSE][..],
    ] {
        let profile = classify(descriptors).unwrap();
        assert_eq!(profile.evidence, PenTabletEvidence::VendorDigitizerPen);
        assert_eq!(profile.capabilities.pressure_levels(), Some(4096));
        assert_eq!(profile.capabilities.barrel_buttons, 2);
        assert!(profile.capabilities.eraser && profile.capabilities.has_tilt());
    }
    let corroborated =
        classify(&[INTUOS_STYLE_PEN_VENDOR_PAGE, INTUOS_STYLE_TOUCH, MOUSE]).unwrap();
    assert_eq!(
        corroborated.evidence,
        PenTabletEvidence::VendorDigitizerWithStandardDigitizer
    );
}

#[test]
fn a_vendor_page_pen_with_express_key_keyboards_is_a_tablet() {
    assert_eq!(
        classify(&[INTUOS_STYLE_PEN_VENDOR_PAGE, MOUSE, KEYBOARD])
            .unwrap()
            .evidence,
        PenTabletEvidence::VendorDigitizerPen
    );
    // Express keys in the same descriptor as the pen.
    let mut combined = LEGACY_OPAQUE_PEN.to_vec();
    combined.extend_from_slice(KEYBOARD);
    assert!(classify(&[combined.as_slice()]).is_ok());
}

#[test]
fn a_vendor_page_pen_beside_a_game_controller_is_refused() {
    assert_eq!(
        classify(&[LEGACY_OPAQUE_PEN, GAMEPAD]),
        Err(NotPenTablet::DisqualifyingCollection)
    );
    let mut combined = LEGACY_OPAQUE_PEN.to_vec();
    combined.extend_from_slice(GAMEPAD);
    assert_eq!(
        classify(&[combined.as_slice()]),
        Err(NotPenTablet::DisqualifyingCollection)
    );
}

#[test]
fn a_keyboard_beside_a_touch_pad_is_not_a_tablet() {
    assert_eq!(
        classify(&[KEYBOARD, crate::descriptor_fixtures::TOUCH_PAD_ONLY]),
        Err(NotPenTablet::TouchOnly)
    );
    let mut combined = KEYBOARD.to_vec();
    combined.extend_from_slice(crate::descriptor_fixtures::TOUCH_PAD_ONLY);
    assert_eq!(
        classify(&[combined.as_slice()]),
        Err(NotPenTablet::TouchOnly)
    );
}

#[test]
fn an_opaque_vendor_pen_is_a_tablet_with_unknown_axes() {
    let profile = classify(&[LEGACY_OPAQUE_PEN]).unwrap();
    assert_eq!(profile.evidence, PenTabletEvidence::VendorDigitizerPen);
    assert_eq!(profile.capabilities, PenCapabilities::default());
    assert_eq!(profile.capabilities.pressure_levels(), None);
    assert!(!profile.capabilities.has_tilt());
}

#[test]
fn an_intuos5_style_tablet_is_a_tablet_with_unknown_axes() {
    use crate::descriptor_fixtures::{
        INTUOS5_STYLE_MOUSE_INTERFACE as MOUSE_IF, INTUOS5_STYLE_PEN_INTERFACE as PEN_IF,
        INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE as CONTROL_IF,
    };
    let profile = classify(&[PEN_IF, CONTROL_IF, MOUSE_IF]).unwrap();
    assert_eq!(profile.evidence, PenTabletEvidence::VendorDigitizerPen);
    assert_eq!(profile.capabilities, PenCapabilities::default());
    // The pen interface on its own (as raw HID offers it) qualifies too.
    assert!(classify(&[PEN_IF]).is_ok());
    // Its vendor control interface alone is a vendor-only device.
    assert_eq!(classify(&[CONTROL_IF]), Err(NotPenTablet::NoDigitizer));
    // Express keys presented as a keyboard are part of the tablet.
    assert!(classify(&[PEN_IF, crate::descriptor_fixtures::BOOT_KEYBOARD]).is_ok());
    assert_eq!(
        classify(&[PEN_IF, crate::descriptor_fixtures::GAMEPAD]),
        Err(NotPenTablet::DisqualifyingCollection)
    );
}

#[test]
fn an_unnamed_vendor_digitizer_still_needs_a_standard_digitizer() {
    assert_eq!(
        classify(&[VENDOR_DIGITIZER_UNNAMED]),
        Err(NotPenTablet::VendorDigitizerOnly)
    );
    assert_eq!(
        classify(&[VENDOR_DIGITIZER_UNNAMED, MOUSE]),
        Err(NotPenTablet::VendorDigitizerOnly)
    );
    let profile = classify(&[VENDOR_DIGITIZER_UNNAMED, INTUOS_STYLE_TOUCH]).unwrap();
    assert_eq!(
        profile.evidence,
        PenTabletEvidence::VendorDigitizerWithStandardDigitizer
    );
}

#[test]
fn huion_style_pen_with_keypad_is_a_tablet() {
    let profile = classify(&[HUION_STYLE_PEN, HUION_STYLE_KEYPAD]).unwrap();
    assert_eq!(profile.evidence, PenTabletEvidence::DigitizerPen);
    let caps = profile.capabilities;
    assert_eq!(caps.pressure_levels(), Some(8192));
    assert_eq!(caps.x.unwrap().logical_max, 32_767);
    assert_eq!(caps.y.unwrap().logical_max, 20_479);
    // Push/Pop restored the Digitizers page and cleared the inch unit.
    assert_eq!(caps.pressure.unwrap().unit, 0);
    let tilt = caps.tilt_y.unwrap();
    assert_eq!((tilt.logical_min, tilt.logical_max), (-127, 127));
    assert_eq!(caps.barrel_buttons, 1);
    assert!(caps.eraser);
}

#[test]
fn xp_pen_style_reports_sixteen_thousand_levels_and_tilt() {
    let caps = classify(&[XP_PEN_STYLE]).unwrap().capabilities;
    assert_eq!(caps.pressure_levels(), Some(16_384));
    let tilt = caps.tilt_x.unwrap();
    assert_eq!((tilt.logical_min, tilt.logical_max), (-60, 60));
    assert!(caps.twist.is_none());
    assert!(caps.eraser);
}

#[test]
fn gaomon_style_usage_ranges_are_understood() {
    let caps = classify(&[GAOMON_STYLE]).unwrap().capabilities;
    assert!(caps.tip_switch && caps.in_range && caps.eraser);
    assert_eq!(caps.barrel_buttons, 1);
    assert_eq!(caps.pressure_levels(), Some(8192));
    assert!(!caps.has_tilt());
}

#[test]
fn an_unknown_vendor_generic_digitizer_pen_is_a_tablet() {
    let profile = classify(&[GENERIC_UNKNOWN_VENDOR]).unwrap();
    assert_eq!(profile.evidence, PenTabletEvidence::DigitizerPen);
    assert_eq!(profile.capabilities.pressure_levels(), Some(1024));
    assert_eq!(profile.capabilities.x.unwrap().logical_max, 4095);
}

#[test]
fn a_pressure_only_pen_is_a_tablet_with_nothing_else_claimed() {
    let caps = classify(&[PRESSURE_ONLY]).unwrap().capabilities;
    assert_eq!(caps.pressure_levels(), Some(2048));
    assert!(caps.tip_switch);
    assert!(!caps.in_range && !caps.eraser && !caps.has_tilt());
    assert!(caps.twist.is_none());
    assert_eq!(caps.barrel_buttons, 0);
}

#[test]
fn the_synthetic_lab_tablet_is_a_tablet() {
    let caps = classify(&[crate::SYNTHETIC_TABLET_REPORT_DESCRIPTOR])
        .unwrap()
        .capabilities;
    assert_eq!(caps.pressure_levels(), Some(1024));
    assert_eq!(caps.barrel_buttons, 2);
    assert!(caps.eraser && caps.has_tilt());
}

#[test]
fn keyboards_mice_and_gamepads_are_not_tablets() {
    for descriptor in [KEYBOARD, MOUSE, ABSOLUTE_MOUSE, GAMEPAD] {
        assert_eq!(classify(&[descriptor]), Err(NotPenTablet::NoDigitizer));
    }
    assert_eq!(
        classify(&[KEYBOARD, MOUSE, GAMEPAD]),
        Err(NotPenTablet::NoDigitizer)
    );
}

#[test]
fn a_vendor_only_device_is_not_a_tablet() {
    assert_eq!(classify(&[VENDOR_ONLY]), Err(NotPenTablet::NoDigitizer));
}

#[test]
fn touch_only_digitizers_are_not_pen_tablets() {
    assert_eq!(classify(&[TOUCH_SCREEN]), Err(NotPenTablet::TouchOnly));
    assert_eq!(
        classify(&[INTUOS_STYLE_TOUCH]),
        Err(NotPenTablet::TouchOnly)
    );
    assert_eq!(
        classify(&[DIGITIZER_USAGE_FINGER]),
        Err(NotPenTablet::TouchOnly)
    );
}

#[test]
fn a_digitizer_that_cannot_drive_a_pen_is_refused() {
    assert_eq!(
        classify(&[PEN_WITHOUT_POSITION]),
        Err(NotPenTablet::NoPenCollection)
    );
    assert_eq!(
        classify(&[PEN_WITH_CONSTANT_AXES]),
        Err(NotPenTablet::NoPenCollection)
    );
}

#[test]
fn one_malformed_interface_rejects_the_whole_device() {
    let truncated = &CINTIQ_STYLE[..CINTIQ_STYLE.len() - 1];
    assert_eq!(
        classify(&[CINTIQ_STYLE, truncated]),
        Err(NotPenTablet::Malformed(
            HidDescriptorError::UnterminatedCollection
        ))
    );
}

#[test]
fn no_descriptors_is_not_a_tablet() {
    assert_eq!(classify(&[]), Err(NotPenTablet::NoDescriptors));
    assert_eq!(
        classify(&[&[]]),
        Err(NotPenTablet::Malformed(HidDescriptorError::Empty))
    );
}

#[test]
fn too_many_interfaces_are_refused() {
    let many = vec![CINTIQ_STYLE; MAX_INTERFACES + 1];
    assert_eq!(classify(&many), Err(NotPenTablet::TooManyDescriptors));
}

#[test]
fn malformed_descriptors_fail_closed() {
    assert_eq!(
        parse_report_descriptor(&[0x26, 0xFF]),
        Err(HidDescriptorError::Truncated { offset: 0 })
    );
    assert_eq!(
        parse_report_descriptor(&[0xC0]),
        Err(HidDescriptorError::UnbalancedCollection { offset: 0 })
    );
    assert_eq!(
        parse_report_descriptor(&[0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01]),
        Err(HidDescriptorError::UnterminatedCollection)
    );
    assert_eq!(
        parse_report_descriptor(&[0xB4]),
        Err(HidDescriptorError::GlobalStack)
    );
    assert_eq!(
        parse_report_descriptor(&[0xFE, 0x10, 0x00]),
        Err(HidDescriptorError::Truncated { offset: 0 })
    );
    let oversized = vec![0_u8; MAX_HID_REPORT_DESCRIPTOR_BYTES + 1];
    assert_eq!(
        parse_report_descriptor(&oversized),
        Err(HidDescriptorError::TooLarge {
            actual: MAX_HID_REPORT_DESCRIPTOR_BYTES + 1
        })
    );
}

#[test]
fn nesting_collection_and_usage_bounds_are_enforced() {
    let mut deep = Vec::new();
    for _ in 0..=MAX_COLLECTION_DEPTH {
        deep.extend_from_slice(&[0xA1, 0x00]);
    }
    assert_eq!(
        parse_report_descriptor(&deep),
        Err(HidDescriptorError::CollectionTooDeep)
    );

    let mut wide = Vec::new();
    for _ in 0..=MAX_TOP_LEVEL_COLLECTIONS {
        wide.extend_from_slice(&[0xA1, 0x01, 0xC0]);
    }
    assert_eq!(
        parse_report_descriptor(&wide),
        Err(HidDescriptorError::TooManyCollections)
    );

    let mut usages = vec![0x05, 0x0D];
    for _ in 0..=MAX_LOCAL_USAGES {
        usages.extend_from_slice(&[0x09, 0x42]);
    }
    assert_eq!(
        parse_report_descriptor(&usages),
        Err(HidDescriptorError::TooManyUsages)
    );

    let pushes = vec![0xA4_u8; MAX_GLOBAL_STACK_DEPTH + 1];
    assert_eq!(
        parse_report_descriptor(&pushes),
        Err(HidDescriptorError::GlobalStack)
    );
}

#[test]
fn long_items_are_skipped_by_their_declared_length() {
    let mut descriptor = vec![0xFE, 0x02, 0x10, 0xAA, 0xBB];
    descriptor.extend_from_slice(PRESSURE_ONLY);
    assert!(classify(&[descriptor.as_slice()]).is_ok());
}

#[test]
fn every_prefix_of_a_real_descriptor_is_handled_without_panicking() {
    for descriptor in [
        CINTIQ_STYLE,
        HUION_STYLE_PEN,
        XP_PEN_STYLE,
        GENERIC_UNKNOWN_VENDOR,
        LEGACY_OPAQUE_PEN,
    ] {
        for end in 0..descriptor.len() {
            let _ = parse_report_descriptor(&descriptor[..end]);
        }
    }
}

#[test]
fn unsigned_maxima_are_not_sign_extended() {
    // `Logical Maximum (65535)` written as two bytes with a zero minimum.
    let descriptor = [
        0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95,
        0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x26, 0xFF, 0xFF, 0x75, 0x10, 0x95,
        0x02, 0x81, 0x02, 0xC0,
    ];
    let caps = classify(&[&descriptor[..]]).unwrap().capabilities;
    assert_eq!(caps.x.unwrap().logical_max, 65_535);
}

#[test]
fn presence_hint_sees_pens_and_vendor_digitizers_only() {
    assert!(declares_pen_or_vendor_digitizer(CINTIQ_STYLE));
    assert!(declares_pen_or_vendor_digitizer(
        INTUOS_STYLE_PEN_VENDOR_PAGE
    ));
    assert!(declares_pen_or_vendor_digitizer(LEGACY_OPAQUE_PEN));
    for descriptor in [KEYBOARD, MOUSE, GAMEPAD, VENDOR_ONLY, TOUCH_SCREEN, &[][..]] {
        assert!(!declares_pen_or_vendor_digitizer(descriptor));
    }
}

#[test]
fn capture_filter_sees_every_digitizer_and_nothing_else() {
    for descriptor in [
        CINTIQ_STYLE,
        INTUOS_STYLE_TOUCH,
        TOUCH_SCREEN,
        LEGACY_OPAQUE_PEN,
    ] {
        assert!(declares_any_digitizer(descriptor));
    }
    for descriptor in [KEYBOARD, MOUSE, GAMEPAD, VENDOR_ONLY, &[0xC0][..]] {
        assert!(!declares_any_digitizer(descriptor));
    }
}

#[test]
fn hid_class_descriptor_yields_the_report_length() {
    // HID 1.11, country 0, one report descriptor of 0x012E bytes.
    let hid = [0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x2E, 0x01];
    assert_eq!(hid_report_descriptor_length(&hid), Some(0x012E));
    // Preceded by an unrelated class descriptor.
    let mut extra = vec![0x04, 0x24, 0x00, 0x00];
    extra.extend_from_slice(&hid);
    assert_eq!(hid_report_descriptor_length(&extra), Some(0x012E));
    // Zero, oversized, missing and malformed lengths are refused.
    assert_eq!(
        hid_report_descriptor_length(&[0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x00, 0x00]),
        None
    );
    assert_eq!(
        hid_report_descriptor_length(&[0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x01, 0x10]),
        None
    );
    assert_eq!(hid_report_descriptor_length(&[]), None);
    assert_eq!(hid_report_descriptor_length(&[0x01, 0x21]), None);
    assert_eq!(
        hid_report_descriptor_length(&[0x09, 0x21, 0x11, 0x01, 0x00, 0x02, 0x23, 0x10, 0x00]),
        None
    );
}

#[test]
fn hid_class_descriptors_must_name_exactly_one_report() {
    // Two Report entries, either order: ambiguous (hosts disagree on which).
    for hid in [
        [
            12, 0x21, 0x11, 0x01, 0, 2, 0x22, 0x87, 0x00, 0x22, 0x2F, 0x00,
        ],
        [
            12, 0x21, 0x11, 0x01, 0, 2, 0x22, 0x2F, 0x00, 0x22, 0x87, 0x00,
        ],
    ] {
        assert_eq!(
            parse_hid_class_descriptor(&hid),
            Err(HidClassDescriptorError::ReportEntries(2))
        );
        assert_eq!(hid_report_descriptor_length(&hid), None);
    }
    // Report beside Physical: fine.
    let with_physical = [
        12, 0x21, 0x11, 0x01, 0, 2, 0x23, 0x10, 0x00, 0x22, 0x87, 0x00,
    ];
    assert_eq!(parse_hid_class_descriptor(&with_physical), Ok(0x87));
    assert_eq!(hid_report_descriptor_length(&with_physical), Some(0x87));
    // bLength and bNumDescriptors must agree exactly.
    assert_eq!(
        parse_hid_class_descriptor(&[9, 0x21, 0x11, 0x01, 0, 2, 0x22, 0x87, 0x00]),
        Err(HidClassDescriptorError::InconsistentCount)
    );
    assert_eq!(
        parse_hid_class_descriptor(&[12, 0x21, 0x11, 0x01, 0, 1, 0x22, 0x87, 0x00, 0, 0, 0]),
        Err(HidClassDescriptorError::InconsistentCount)
    );
    assert_eq!(
        parse_hid_class_descriptor(&[10, 0x21, 0x11, 0x01, 0, 1, 0x22, 0x87, 0x00]),
        Err(HidClassDescriptorError::Malformed)
    );
    let too_many: Vec<u8> = [
        vec![u8::try_from(6 + 3 * 17).unwrap(), 0x21, 0x11, 0x01, 0, 17],
        [0x23_u8, 0x01, 0x00].repeat(17),
    ]
    .concat();
    assert_eq!(
        parse_hid_class_descriptor(&too_many),
        Err(HidClassDescriptorError::InconsistentCount)
    );
}

#[test]
fn report_descriptor_requests_round_trip() {
    let setup = hid_report_descriptor_request(2, 0x012E);
    assert_eq!(setup.request_type, 0x81);
    assert_eq!(setup.request, 0x06);
    assert_eq!(setup.value, 0x2200);
    assert_eq!(requested_report_descriptor_interface(setup), Some(2));
    let device_descriptor = SetupPacket {
        request_type: 0x80,
        request: 0x06,
        value: 0x0100,
        index: 0,
        length: 18,
    };
    assert_eq!(
        requested_report_descriptor_interface(device_descriptor),
        None
    );
    let out_of_range = SetupPacket {
        index: 0x0100,
        ..setup
    };
    assert_eq!(requested_report_descriptor_interface(out_of_range), None);
}

/// Wraps `inner` in an outer application collection opened by `prefix`.
fn wrapped(prefix: &[u8], inner: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    out.extend_from_slice(inner);
    out.push(0xC0);
    out
}

/// An opaque vendor-defined Digitizers "Pen" application (0xFF0D, usage 2).
const VENDOR_PEN_OPEN: &[u8] = &[0x06, 0x0D, 0xFF, 0x09, 0x02, 0xA1, 0x01];

/// The reviewed bypass: a Gamepad Application collection nested inside an
/// outer 0xFF0D Pen application was invisible to a top-level-only check, and
/// a host associates its inputs with the nearest Application collection.
#[test]
fn a_gamepad_nested_inside_a_vendor_pen_application_is_seen_and_refused() {
    let nested = wrapped(VENDOR_PEN_OPEN, GAMEPAD);
    assert_eq!(disqualifying_collection(&nested), Ok(Some(0x05)));
    // The outer "pen" wraps another device, so it is no pen at all.
    assert_eq!(
        classify(&[nested.as_slice()]),
        Err(NotPenTablet::NoDigitizer)
    );
    // Beside a genuine pen the device is a tablet, but the gamepad is found.
    assert!(classify(&[CINTIQ_STYLE, nested.as_slice()]).is_ok());
    assert_eq!(disqualifying_collection(&nested).unwrap(), Some(0x05));
}

#[test]
fn a_gamepad_nested_inside_a_standard_pen_application_is_seen() {
    // Outer standard pen with genuine fields; a gamepad application inside.
    let mut pen = CINTIQ_STYLE[..CINTIQ_STYLE.len() - 1].to_vec(); // drop final End Collection
    pen.extend_from_slice(GAMEPAD);
    pen.push(0xC0);
    let summary = parse_report_descriptor(&pen).unwrap();
    assert_eq!(summary.collections.len(), 2);
    assert!(
        summary
            .collections
            .iter()
            .any(|collection| collection.nested)
    );
    assert_eq!(disqualifying_collection(&pen), Ok(Some(0x05)));
    // The pen keeps its own fields; the gamepad's axes are not the pen's.
    let caps = classify(&[pen.as_slice()]).unwrap().capabilities;
    assert_eq!(caps.pressure_levels(), Some(8192));

    // Named by a physical collection instead of an application one.
    let mut physical = CINTIQ_STYLE[..CINTIQ_STYLE.len() - 1].to_vec();
    physical.extend_from_slice(&[0x05, 0x01, 0x09, 0x05, 0xA1, 0x00, 0xC0]);
    physical.push(0xC0);
    assert_eq!(disqualifying_collection(&physical), Ok(Some(0x05)));
}

/// A keyboard-only device wrapped in an outer 0xFF0D Pen application with no
/// pen fields of its own must not pass the opaque vendor-pen rule.
#[test]
fn a_keyboard_wrapped_in_a_vendor_pen_application_is_not_a_tablet() {
    let wrapped_keyboard = wrapped(VENDOR_PEN_OPEN, KEYBOARD);
    assert_eq!(
        classify(&[wrapped_keyboard.as_slice()]),
        Err(NotPenTablet::NoDigitizer)
    );
    assert!(!declares_pen_or_vendor_digitizer(&wrapped_keyboard));
    // Express keys beside (not inside) an opaque vendor pen remain allowed.
    let mut beside = LEGACY_OPAQUE_PEN.to_vec();
    beside.extend_from_slice(KEYBOARD);
    assert!(classify(&[beside.as_slice()]).is_ok());
}

#[test]
fn a_vendor_pen_may_nest_digitizer_applications() {
    let inner_digitizer = [
        0x06, 0x0D, 0xFF, 0x09, 0x20, 0xA1, 0x01, 0x09, 0x00, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
        0x08, 0x95, 0x04, 0x81, 0x02, 0xC0,
    ];
    let nested = wrapped(VENDOR_PEN_OPEN, &inner_digitizer);
    let profile = classify(&[nested.as_slice()]).unwrap();
    assert_eq!(profile.evidence, PenTabletEvidence::VendorDigitizerPen);
    assert_eq!(disqualifying_collection(&nested), Ok(None));
}

#[test]
fn inputs_belong_to_the_innermost_application_collection() {
    // A Digitizer application whose only pen-shaped fields sit inside a
    // nested Generic Desktop application is not a pen.
    let mut descriptor = vec![0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01];
    descriptor.extend_from_slice(&[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, // nested Mouse application
        0x05, 0x0D, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, //
        0x95, 0x07, 0x81, 0x03, //
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x26, 0xFF, 0x0F, 0x75, 0x10, 0x95, 0x02, 0x81,
        0x02, //
        0xC0,
    ]);
    descriptor.push(0xC0);
    assert_eq!(
        classify(&[descriptor.as_slice()]),
        Err(NotPenTablet::NoPenCollection)
    );
}
