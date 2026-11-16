// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A Deck's own pen samples to the typed pen wire ([`PenEvent`]).
//!
//! A Deck reads its pen from the operating system (`AppKit` tablet events on
//! a Mac, `WM_POINTER` with `POINTER_PEN_INFO` on Windows) and sends the
//! vendor-neutral sample every host accepts: a position normalized into the
//! video's image rectangle, pressure `0..=1`, tilt in degrees, rotation
//! `0..360`, pen or eraser, proximity, contact and barrel buttons. This
//! module holds the portable half: the image-rectangle normalization both
//! Decks share, and the Windows Ink reading (whose ranges are Microsoft's
//! published ones, converted through [`crate::pen_axes`]).

use crate::pen_axes::{
    WINDOWS_INK_PRESSURE_RANGE, normalize_logical, tilt_logical_to_degrees, wrap_rotation_degrees,
};
use crate::{LowLatencyMetadata, PenEvent, PenTool};

/// Where the video is drawn in the window, top-left origin, in the same
/// units as the samples (points on a Mac, client pixels on Windows).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageRect {
    /// Left edge.
    pub left: f64,
    /// Top edge.
    pub top: f64,
    /// Width; positive.
    pub width: f64,
    /// Height; positive.
    pub height: f64,
}

impl ImageRect {
    /// A window-relative point (top-left origin) as a normalized position in
    /// the image, clamped to its edges as the mouse is. `None` for a
    /// degenerate rectangle or a non-finite point.
    #[must_use]
    pub fn normalize(self, x: f64, y: f64) -> Option<(f64, f64)> {
        let valid = [self.left, self.top, self.width, self.height, x, y]
            .iter()
            .all(|value| value.is_finite())
            && self.width > 0.0
            && self.height > 0.0;
        if !valid {
            return None;
        }
        Some((
            ((x - self.left) / self.width).clamp(0.0, 1.0),
            ((y - self.top) / self.height).clamp(0.0, 1.0),
        ))
    }
}

/// `POINTER_FLAG_INRANGE`.
pub const POINTER_FLAG_INRANGE: u32 = 0x0000_0002;
/// `POINTER_FLAG_INCONTACT`.
pub const POINTER_FLAG_INCONTACT: u32 = 0x0000_0004;
/// `POINTER_FLAG_SECONDBUTTON`: a pen's barrel button.
pub const POINTER_FLAG_SECONDBUTTON: u32 = 0x0000_0020;
/// `PEN_FLAG_BARREL`.
pub const PEN_FLAG_BARREL: u32 = 0x0000_0001;
/// `PEN_FLAG_INVERTED`: the eraser end is towards the screen.
pub const PEN_FLAG_INVERTED: u32 = 0x0000_0002;
/// `PEN_FLAG_ERASER`: the eraser is pressed to the screen.
pub const PEN_FLAG_ERASER: u32 = 0x0000_0004;
/// `PEN_MASK_PRESSURE`.
pub const PEN_MASK_PRESSURE: u32 = 0x0000_0001;
/// `PEN_MASK_ROTATION`.
pub const PEN_MASK_ROTATION: u32 = 0x0000_0002;
/// `PEN_MASK_TILT_X`.
pub const PEN_MASK_TILT_X: u32 = 0x0000_0004;
/// `PEN_MASK_TILT_Y`.
pub const PEN_MASK_TILT_Y: u32 = 0x0000_0008;

/// Windows Ink's tilt range, whole degrees `-90..=90`.
const WINDOWS_INK_TILT_RANGE: crate::pen_axes::LogicalRange =
    match crate::pen_axes::LogicalRange::new(-90, 90) {
        Some(range) => range,
        None => panic!("tilt range"),
    };

/// Pressure for a pen in contact that reports none, as the W3C Pointer
/// Events default for a pressed button without pressure support.
const CONTACT_WITHOUT_PRESSURE: f32 = 0.5;

/// One pen sample as `GetPointerPenInfo` (or each entry of
/// `GetPointerPenInfoHistory`, oldest first) reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowsInkSample {
    /// Client-area X (top-left origin), in pixels; fractional when the
    /// adapter refined it from the `HIMETRIC` location.
    pub x: f64,
    /// Client-area Y.
    pub y: f64,
    /// `POINTER_INFO::pointerFlags`.
    pub pointer_flags: u32,
    /// `POINTER_PEN_INFO::penFlags`.
    pub pen_flags: u32,
    /// `POINTER_PEN_INFO::penMask`: which of the axes below are valid.
    pub pen_mask: u32,
    /// `0..=1024`.
    pub pressure: u32,
    /// Degrees, `0..=359`.
    pub rotation: u32,
    /// Degrees, `-90..=90`.
    pub tilt_x: i32,
    /// Degrees, `-90..=90`.
    pub tilt_y: i32,
}

/// Windows Ink samples to [`PenEvent`]s, with the proximity a single sample
/// cannot carry.
///
/// A sample in range is in proximity; the first sample out of range (or
/// [`Self::leave`], for `WM_POINTERLEAVE` and capture loss) is a clean
/// zeroed leave, as a Mac Deck sends on leaving proximity. The tool is the
/// eraser while the pen is inverted or erasing, the tip otherwise. Contact
/// is `POINTER_FLAG_INCONTACT`, or positive pressure.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsInkMapper {
    in_proximity: bool,
    eraser: bool,
}

impl WindowsInkMapper {
    /// A mapper with no pen in range.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            in_proximity: false,
            eraser: false,
        }
    }

    /// Whether a pen is in range.
    #[must_use]
    pub const fn in_proximity(self) -> bool {
        self.in_proximity
    }

    /// One sample; `None` when it cannot be placed, or when an out-of-range
    /// sample follows nothing in range.
    pub fn on_sample(
        &mut self,
        sample: WindowsInkSample,
        image: ImageRect,
        metadata: LowLatencyMetadata,
    ) -> Option<PenEvent> {
        let (x, y) = image.normalize(sample.x, sample.y)?;
        if sample.pointer_flags & POINTER_FLAG_INRANGE == 0 {
            return self.leave_at(x, y, metadata);
        }
        self.in_proximity = true;
        self.eraser = sample.pen_flags & (PEN_FLAG_INVERTED | PEN_FLAG_ERASER) != 0;
        let has = |mask| sample.pen_mask & mask != 0;
        let contact = sample.pointer_flags & POINTER_FLAG_INCONTACT != 0;
        let pressure = if has(PEN_MASK_PRESSURE) {
            normalize_logical(i64::from(sample.pressure), WINDOWS_INK_PRESSURE_RANGE)
        } else if contact {
            CONTACT_WITHOUT_PRESSURE
        } else {
            0.0
        };
        let tilt = |present, raw: i32| {
            if present {
                tilt_logical_to_degrees(i64::from(raw), WINDOWS_INK_TILT_RANGE, None)
            } else {
                0.0
            }
        };
        let barrel = sample.pen_flags & PEN_FLAG_BARREL != 0
            || sample.pointer_flags & POINTER_FLAG_SECONDBUTTON != 0;
        let event = PenEvent {
            x,
            y,
            pressure,
            tilt_x_degrees: tilt(has(PEN_MASK_TILT_X), sample.tilt_x),
            tilt_y_degrees: tilt(has(PEN_MASK_TILT_Y), sample.tilt_y),
            rotation_degrees: if has(PEN_MASK_ROTATION) {
                wrap_rotation_degrees(f64::from(sample.rotation))
            } else {
                0.0
            },
            tool: self.tool(),
            in_proximity: true,
            touching: contact || pressure > 0.0 && has(PEN_MASK_PRESSURE),
            buttons: u16::from(barrel),
            metadata,
        };
        event.validate().ok()?;
        Some(event)
    }

    /// The pen left (`WM_POINTERLEAVE`, `WM_POINTERCAPTURECHANGED`, focus
    /// loss) at a normalized position; `None` when none was in range.
    pub fn leave_at(&mut self, x: f64, y: f64, metadata: LowLatencyMetadata) -> Option<PenEvent> {
        if !self.in_proximity {
            return None;
        }
        self.in_proximity = false;
        let event = PenEvent {
            x,
            y,
            pressure: 0.0,
            tilt_x_degrees: 0.0,
            tilt_y_degrees: 0.0,
            rotation_degrees: 0.0,
            tool: self.tool(),
            in_proximity: false,
            touching: false,
            buttons: 0,
            metadata,
        };
        event.validate().ok()?;
        Some(event)
    }

    const fn tool(self) -> PenTool {
        if self.eraser {
            PenTool::Eraser
        } else {
            PenTool::Tip
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: ImageRect = ImageRect {
        left: 100.0,
        top: 50.0,
        width: 800.0,
        height: 400.0,
    };

    fn metadata() -> LowLatencyMetadata {
        LowLatencyMetadata {
            sequence: 1,
            timestamp_ns: 1,
            coalescable: true,
        }
    }

    fn sample(pointer_flags: u32, pressure: u32) -> WindowsInkSample {
        WindowsInkSample {
            x: 500.0,
            y: 250.0,
            pointer_flags,
            pen_flags: 0,
            pen_mask: PEN_MASK_PRESSURE | PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            pressure,
            rotation: 0,
            tilt_x: 0,
            tilt_y: 0,
        }
    }

    const IN_RANGE: u32 = POINTER_FLAG_INRANGE;
    const IN_CONTACT: u32 = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT;

    #[test]
    fn the_image_rectangle_places_the_pen_as_it_places_the_mouse() {
        assert_eq!(IMAGE.normalize(100.0, 50.0), Some((0.0, 0.0)));
        assert_eq!(IMAGE.normalize(500.0, 250.0), Some((0.5, 0.5)));
        assert_eq!(IMAGE.normalize(0.0, 1000.0), Some((0.0, 1.0)));
        let empty = ImageRect {
            width: 0.0,
            ..IMAGE
        };
        assert_eq!(empty.normalize(1.0, 1.0), None);
        assert_eq!(IMAGE.normalize(f64::NAN, 1.0), None);
    }

    #[test]
    fn windows_ink_axes_reach_the_wire_normalized() {
        let mut mapper = WindowsInkMapper::new();
        let mut raw = sample(IN_CONTACT, 1024);
        raw.tilt_x = -45;
        raw.tilt_y = 90;
        raw.rotation = 359;
        let event = mapper.on_sample(raw, IMAGE, metadata()).expect("placed");
        assert!((event.pressure - 1.0).abs() < f32::EPSILON);
        assert!((event.tilt_x_degrees + 45.0).abs() < 0.01);
        assert!((event.tilt_y_degrees - 90.0).abs() < 0.01);
        assert!((event.rotation_degrees - 359.0).abs() < 0.01);
        assert!(event.touching && event.in_proximity);
        assert_eq!((event.x, event.y), (0.5, 0.5));
        let half = mapper
            .on_sample(sample(IN_CONTACT, 512), IMAGE, metadata())
            .expect("placed");
        assert!((half.pressure - 0.5).abs() < 0.001);
    }

    #[test]
    fn hover_is_proximity_without_contact() {
        let mut mapper = WindowsInkMapper::new();
        let event = mapper
            .on_sample(sample(IN_RANGE, 0), IMAGE, metadata())
            .expect("placed");
        assert!(event.in_proximity && !event.touching);
        assert_eq!(event.tool, PenTool::Tip);
    }

    #[test]
    fn leaving_range_is_one_clean_leave() {
        let mut mapper = WindowsInkMapper::new();
        mapper.on_sample(sample(IN_CONTACT, 300), IMAGE, metadata());
        let leave = mapper
            .on_sample(sample(0, 300), IMAGE, metadata())
            .expect("a leave");
        assert!(!leave.in_proximity && !leave.touching);
        assert!(leave.pressure.abs() < f32::EPSILON && leave.buttons == 0);
        assert_eq!(mapper.on_sample(sample(0, 0), IMAGE, metadata()), None);
        assert_eq!(mapper.leave_at(0.5, 0.5, metadata()), None);
    }

    #[test]
    fn the_eraser_and_the_barrel_button() {
        let mut mapper = WindowsInkMapper::new();
        let mut raw = sample(IN_RANGE, 0);
        raw.pen_flags = PEN_FLAG_INVERTED;
        let event = mapper.on_sample(raw, IMAGE, metadata()).expect("placed");
        assert_eq!(event.tool, PenTool::Eraser);
        raw.pen_flags = PEN_FLAG_BARREL;
        let event = mapper.on_sample(raw, IMAGE, metadata()).expect("placed");
        assert_eq!((event.tool, event.buttons), (PenTool::Tip, 1));
        let leave = mapper.leave_at(0.0, 0.0, metadata()).expect("leave");
        assert_eq!(leave.tool, PenTool::Tip);
    }

    #[test]
    fn missing_axes_are_neutral_and_contact_still_presses() {
        let mut mapper = WindowsInkMapper::new();
        let mut raw = sample(IN_CONTACT, 900);
        raw.pen_mask = 0;
        raw.tilt_x = 30;
        let event = mapper.on_sample(raw, IMAGE, metadata()).expect("placed");
        assert!((event.pressure - 0.5).abs() < f32::EPSILON);
        assert!(event.tilt_x_degrees.abs() < f32::EPSILON && event.touching);
        raw.pointer_flags = IN_RANGE;
        let hover = mapper.on_sample(raw, IMAGE, metadata()).expect("placed");
        assert!(!hover.touching && hover.pressure.abs() < f32::EPSILON);
    }
}
