// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A generic HID pen digitizer: the report descriptor and the bytes one pen
//! sample becomes.
//!
//! A host that presents a pen as hardware — `IOHIDUserDevice` on macOS,
//! `uhid` on Linux — needs a descriptor that says "this is a pen" in the
//! vocabulary of the HID Usage Tables' Digitizers page (0x0D), and a packer
//! that turns a finished sample into an input report. Both are defined by the
//! USB HID specification rather than by any operating system, so they live
//! here once, beside the keyboard and pointer in [`crate::hid_reports`].
//!
//! The layout is the one pen digitizers commonly use, so a generic HID stack
//! recognises it without a vendor driver:
//!
//! | Bits | Usage |
//! | --- | --- |
//! | 1 | Tip Switch |
//! | 1 | Barrel Switch |
//! | 1 | Invert (eraser end in range) |
//! | 1 | Eraser (eraser end touching) |
//! | 1 | Secondary Barrel Switch |
//! | 1 | In Range |
//! | 2 | padding |
//! | 16 + 16 | X, Y over `0..=PEN_AXIS_MAX` (absolute) |
//! | 16 | Tip Pressure over `0..=PEN_PRESSURE_MAX` |
//! | 16 + 16 | X Tilt, Y Tilt, signed hundredths of a degree, ±90° |
//! | 16 | Twist, hundredths of a degree, `0..36000` |
//!
//! preceded by report id [`PEN_REPORT_ID`].
//!
//! What an operating system *does* with such a device is a separate question,
//! answered by measurement on each platform. On macOS 26 the HID system turns
//! it into real tablet proximity events and tablet-subtype pointer events, but
//! leaves pressure, tilt and rotation empty and decides the tip from a 75 %
//! pressure threshold rather than the Tip Switch. That is why the macOS Pier
//! still terminates Basic Tablet through `CGEvent` and uses this device only to
//! re-measure (`arcen-pier-macos probe-pen-hid`). See
//! `hosts/macos/AGENTS.md`, "A virtual pen digitizer, measured".

use crate::PenTool;
use crate::pen_axes;

/// Report id of the pen's only input report.
pub const PEN_REPORT_ID: u8 = 1;

/// Length of one pen input report, report id included.
pub const PEN_REPORT_LEN: usize = 14;

/// The largest value either position axis reports.
pub const PEN_AXIS_MAX: u16 = 0x7FFF;

/// The largest tip pressure the device reports: 13 bits, 8192 levels.
pub const PEN_PRESSURE_MAX: u16 = 8191;

/// The tilt range on either axis, in hundredths of a degree.
pub const PEN_TILT_LIMIT_CENTIDEGREES: i16 = 9000;

/// The largest twist, in hundredths of a degree (one step short of a turn).
pub const PEN_TWIST_MAX_CENTIDEGREES: u16 = 35_999;

/// Report descriptor for a pen digitizer with one input report.
pub const PEN_DESCRIPTOR: &[u8] = &[
    0x05,
    0x0D, // Usage Page (Digitizers)
    0x09,
    0x02, // Usage (Pen)
    0xA1,
    0x01, // Collection (Application)
    0x85,
    PEN_REPORT_ID, //   Report ID
    0x09,
    0x20, //   Usage (Stylus)
    0xA1,
    0x00, //   Collection (Physical)
    0x09,
    0x42, //     Usage (Tip Switch)
    0x09,
    0x44, //     Usage (Barrel Switch)
    0x09,
    0x3C, //     Usage (Invert)
    0x09,
    0x45, //     Usage (Eraser)
    0x09,
    0x5A, //     Usage (Secondary Barrel Switch)
    0x09,
    0x32, //     Usage (In Range)
    0x15,
    0x00, //     Logical Minimum (0)
    0x25,
    0x01, //     Logical Maximum (1)
    0x75,
    0x01, //     Report Size (1)
    0x95,
    0x06, //     Report Count (6)
    0x81,
    0x02, //     Input (Data, Variable, Absolute) — switches
    0x95,
    0x02, //     Report Count (2)
    0x81,
    0x03, //     Input (Constant) — padding
    0x05,
    0x01, //     Usage Page (Generic Desktop)
    0x09,
    0x30, //     Usage (X)
    0x09,
    0x31, //     Usage (Y)
    0x15,
    0x00, //     Logical Minimum (0)
    0x26,
    0xFF,
    0x7F, // Logical Maximum (32767)
    0x75,
    0x10, //     Report Size (16)
    0x95,
    0x02, //     Report Count (2)
    0x81,
    0x02, //     Input (Data, Variable, Absolute) — position
    0x05,
    0x0D, //     Usage Page (Digitizers)
    0x09,
    0x30, //     Usage (Tip Pressure)
    0x15,
    0x00, //     Logical Minimum (0)
    0x26,
    0xFF,
    0x1F, // Logical Maximum (8191)
    0x75,
    0x10, //     Report Size (16)
    0x95,
    0x01, //     Report Count (1)
    0x81,
    0x02, //     Input (Data, Variable, Absolute) — pressure
    0x09,
    0x3D, //     Usage (X Tilt)
    0x09,
    0x3E, //     Usage (Y Tilt)
    0x16,
    0xD8,
    0xDC, // Logical Minimum (-9000)
    0x26,
    0x28,
    0x23, // Logical Maximum (9000)
    0x65,
    0x14, //     Unit (English Rotation: degrees)
    0x55,
    0x0E, //     Unit Exponent (-2)
    0x75,
    0x10, //     Report Size (16)
    0x95,
    0x02, //     Report Count (2)
    0x81,
    0x02, //     Input (Data, Variable, Absolute) — tilt
    0x09,
    0x41, //     Usage (Twist)
    0x15,
    0x00, //     Logical Minimum (0)
    0x27,
    0x9F,
    0x8C,
    0x00,
    0x00, // Logical Maximum (35999), four bytes so it reads unsigned
    0x65,
    0x14, //     Unit (English Rotation: degrees)
    0x55,
    0x0E, //     Unit Exponent (-2)
    0x75,
    0x10, //     Report Size (16)
    0x95,
    0x01, //     Report Count (1)
    0x81,
    0x02, //     Input (Data, Variable, Absolute) — twist
    0x65,
    0x00, //     Unit (None)
    0x55,
    0x00, //     Unit Exponent (0)
    0xC0, //   End Collection
    0xC0, // End Collection
];

/// One finished pen sample, in the units the wire carries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenSample {
    /// Which end of the pen.
    pub tool: PenTool,
    /// Whether the pen is close enough to be tracked.
    pub in_proximity: bool,
    /// Whether the tip (or the eraser) is on the surface.
    pub touching: bool,
    /// Barrel buttons held, bit 0 the first.
    pub buttons: u8,
    /// Position, normalized `0.0..=1.0` across the surface.
    pub x: f64,
    /// Position, normalized `0.0..=1.0` down the surface.
    pub y: f64,
    /// Pressure, `0.0..=1.0`.
    pub pressure: f64,
    /// Tilt towards +X, degrees.
    pub tilt_x_degrees: f32,
    /// Tilt towards +Y, degrees.
    pub tilt_y_degrees: f32,
    /// Barrel rotation, degrees.
    pub rotation_degrees: f32,
}

/// A pen input report, before it becomes bytes.
///
/// The six switches are the report's own one-bit fields, named as the HID
/// usage tables name them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PenReport {
    /// Tip Switch.
    pub tip: bool,
    /// Barrel Switch.
    pub barrel: bool,
    /// Invert: the eraser end is the one in range.
    pub invert: bool,
    /// Eraser: the eraser end is touching.
    pub eraser: bool,
    /// Secondary Barrel Switch.
    pub secondary_barrel: bool,
    /// In Range.
    pub in_range: bool,
    /// X, `0..=PEN_AXIS_MAX`.
    pub x: u16,
    /// Y, `0..=PEN_AXIS_MAX`.
    pub y: u16,
    /// Tip pressure, `0..=PEN_PRESSURE_MAX`.
    pub pressure: u16,
    /// X tilt, hundredths of a degree.
    pub tilt_x: i16,
    /// Y tilt, hundredths of a degree.
    pub tilt_y: i16,
    /// Twist, hundredths of a degree.
    pub twist: u16,
}

impl PenReport {
    /// The report for a sample.
    ///
    /// Follows how pen digitizers report, so a generic HID stack reads it as a
    /// pen: the eraser end in range sets Invert, and touching with it sets
    /// Eraser instead of Tip Switch. A pen out of range reports no switches
    /// and no pressure but keeps its position, because an absolute report of
    /// zero would also be a move to the corner. Hover carries no pressure.
    /// Values are clamped, and a non-finite one reads as its neutral value.
    #[must_use]
    pub fn from_sample(sample: &PenSample) -> Self {
        let in_range = sample.in_proximity;
        let touching = in_range && sample.touching;
        let eraser_end = sample.tool == PenTool::Eraser;
        Self {
            tip: touching && !eraser_end,
            barrel: in_range && sample.buttons & 0b01 != 0,
            invert: in_range && eraser_end,
            eraser: touching && eraser_end,
            secondary_barrel: in_range && sample.buttons & 0b10 != 0,
            in_range,
            x: unit_to_axis(sample.x, PEN_AXIS_MAX),
            y: unit_to_axis(sample.y, PEN_AXIS_MAX),
            pressure: if touching {
                unit_to_axis(
                    f64::from(pen_axes::clamp_pressure(sample.pressure)),
                    PEN_PRESSURE_MAX,
                )
            } else {
                0
            },
            tilt_x: if in_range {
                centidegrees_tilt(sample.tilt_x_degrees)
            } else {
                0
            },
            tilt_y: if in_range {
                centidegrees_tilt(sample.tilt_y_degrees)
            } else {
                0
            },
            twist: if in_range {
                centidegrees_twist(sample.rotation_degrees)
            } else {
                0
            },
        }
    }

    /// The report that takes this pen out of range where it is.
    #[must_use]
    pub const fn released(&self) -> Self {
        Self {
            tip: false,
            barrel: false,
            invert: false,
            eraser: false,
            secondary_barrel: false,
            in_range: false,
            x: self.x,
            y: self.y,
            pressure: 0,
            tilt_x: 0,
            tilt_y: 0,
            twist: 0,
        }
    }

    /// The bytes of this report, report id first.
    #[must_use]
    pub fn bytes(&self) -> [u8; PEN_REPORT_LEN] {
        let switches = u8::from(self.tip)
            | u8::from(self.barrel) << 1
            | u8::from(self.invert) << 2
            | u8::from(self.eraser) << 3
            | u8::from(self.secondary_barrel) << 4
            | u8::from(self.in_range) << 5;
        let mut report = [0_u8; PEN_REPORT_LEN];
        report[0] = PEN_REPORT_ID;
        report[1] = switches;
        report[2..4].copy_from_slice(&self.x.to_le_bytes());
        report[4..6].copy_from_slice(&self.y.to_le_bytes());
        report[6..8].copy_from_slice(&self.pressure.to_le_bytes());
        report[8..10].copy_from_slice(&self.tilt_x.to_le_bytes());
        report[10..12].copy_from_slice(&self.tilt_y.to_le_bytes());
        report[12..14].copy_from_slice(&self.twist.to_le_bytes());
        report
    }
}

fn unit_to_axis(value: f64, max: u16) -> u16 {
    let value = if value.is_finite() { value } else { 0.0 };
    let scaled = (value.clamp(0.0, 1.0) * f64::from(max)).round();
    // Clamped into `0..=max` first, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        scaled as u16
    }
}

fn centidegrees_tilt(degrees: f32) -> i16 {
    let limit = f64::from(PEN_TILT_LIMIT_CENTIDEGREES);
    let degrees = if degrees.is_finite() {
        f64::from(degrees)
    } else {
        0.0
    };
    // Clamped into ±9000 first, so the cast is exact.
    #[allow(clippy::cast_possible_truncation)]
    {
        (degrees * 100.0).round().clamp(-limit, limit) as i16
    }
}

fn centidegrees_twist(degrees: f32) -> u16 {
    let wrapped = f64::from(pen_axes::wrap_rotation_degrees(f64::from(degrees)));
    // `wrap_rotation_degrees` returns `0.0..360.0` (non-finite reads 0), and
    // the clamp keeps a value that rounds up to a full turn inside the range.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (wrapped * 100.0)
            .round()
            .clamp(0.0, f64::from(PEN_TWIST_MAX_CENTIDEGREES)) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PenSample {
        PenSample {
            tool: PenTool::Tip,
            in_proximity: true,
            touching: true,
            buttons: 0,
            x: 0.5,
            y: 0.25,
            pressure: 0.5,
            tilt_x_degrees: 30.0,
            tilt_y_degrees: -45.5,
            rotation_degrees: 90.0,
        }
    }

    /// Walks the descriptor's main items and adds up the input bits, the way a
    /// HID parser does, so the declared layout and the packer cannot drift.
    fn declared_input_bits(descriptor: &[u8]) -> usize {
        let (mut size, mut count, mut bits, mut index) = (0_usize, 0_usize, 0_usize, 0_usize);
        while index < descriptor.len() {
            let prefix = descriptor[index];
            let length = match prefix & 0x03 {
                3 => 4,
                other => usize::from(other),
            };
            let data = &descriptor[index + 1..index + 1 + length];
            let value = data
                .iter()
                .rev()
                .fold(0_usize, |total, &byte| total << 8 | usize::from(byte));
            match prefix & 0xFC {
                0x74 => size = value,
                0x94 => count = value,
                0x80 => bits += size * count,
                _ => {}
            }
            index += 1 + length;
        }
        bits
    }

    #[test]
    fn the_descriptor_declares_exactly_the_report_the_packer_writes() {
        assert_eq!(&PEN_DESCRIPTOR[..4], &[0x05, 0x0D, 0x09, 0x02], "a pen");
        assert_eq!(&PEN_DESCRIPTOR[PEN_DESCRIPTOR.len() - 2..], &[0xC0, 0xC0]);
        // Everything after the report id byte.
        assert_eq!(
            declared_input_bits(PEN_DESCRIPTOR),
            (PEN_REPORT_LEN - 1) * 8
        );
    }

    #[test]
    fn a_touching_tip_reports_its_whole_surface() {
        let report = PenReport::from_sample(&sample());
        let bytes = report.bytes();
        assert_eq!(bytes[0], PEN_REPORT_ID);
        assert_eq!(bytes[1], 0b10_0001, "tip and in range");
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 16_384);
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 8_192);
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 4_096);
        assert_eq!(i16::from_le_bytes([bytes[8], bytes[9]]), 3_000);
        assert_eq!(i16::from_le_bytes([bytes[10], bytes[11]]), -4_550);
        assert_eq!(u16::from_le_bytes([bytes[12], bytes[13]]), 9_000);
    }

    #[test]
    fn hover_carries_tilt_but_no_pressure() {
        let mut hover = sample();
        hover.touching = false;
        let report = PenReport::from_sample(&hover);
        assert!(report.in_range && !report.tip);
        assert_eq!(report.pressure, 0);
        assert_eq!(report.tilt_x, 3_000);
    }

    #[test]
    fn the_eraser_end_inverts_and_erases_instead_of_tipping() {
        let mut eraser = sample();
        eraser.tool = PenTool::Eraser;
        let touching = PenReport::from_sample(&eraser);
        assert!(touching.invert && touching.eraser && !touching.tip);
        eraser.touching = false;
        let hovering = PenReport::from_sample(&eraser);
        assert!(hovering.invert && !hovering.eraser && !hovering.tip);
    }

    #[test]
    fn barrel_buttons_map_to_the_two_barrel_switches() {
        let mut held = sample();
        held.buttons = 0b11;
        assert_eq!(PenReport::from_sample(&held).bytes()[1], 0b11_0011);
        held.buttons = 0b100;
        assert_eq!(
            PenReport::from_sample(&held).bytes()[1],
            0b10_0001,
            "a third button has no usage and is not invented"
        );
    }

    #[test]
    fn out_of_range_lets_go_of_everything_but_keeps_its_place() {
        let mut away = sample();
        away.in_proximity = false;
        away.buttons = 1;
        let report = PenReport::from_sample(&away);
        assert_eq!(report.bytes()[1], 0, "no switch is held out of range");
        assert_eq!((report.pressure, report.tilt_x, report.twist), (0, 0, 0));
        assert_eq!(report.x, 16_384);
        let released = PenReport::from_sample(&sample()).released();
        assert_eq!(released, report);
    }

    #[test]
    fn hostile_values_are_clamped_not_forwarded() {
        let mut wild = sample();
        wild.x = -4.0;
        wild.y = f64::NAN;
        wild.pressure = 7.0;
        wild.tilt_x_degrees = 400.0;
        wild.tilt_y_degrees = f32::NEG_INFINITY;
        wild.rotation_degrees = -90.0;
        let report = PenReport::from_sample(&wild);
        assert_eq!((report.x, report.y), (0, 0));
        assert_eq!(report.pressure, PEN_PRESSURE_MAX);
        assert_eq!(report.tilt_x, PEN_TILT_LIMIT_CENTIDEGREES);
        assert_eq!(report.tilt_y, 0);
        assert_eq!(
            report.twist, 27_000,
            "rotation wraps rather than saturating"
        );
        wild.rotation_degrees = 359.999;
        assert_eq!(
            PenReport::from_sample(&wild).twist,
            PEN_TWIST_MAX_CENTIDEGREES
        );
    }
}
