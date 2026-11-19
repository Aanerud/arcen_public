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
use crate::pen_buttons::{DriverClick, PenButtonMap, PenButtonsHeld};
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
/// `POINTER_FLAG_SECONDBUTTON`: a pen's barrel button, in contact.
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
///
/// Barrel buttons go through the session's [`PenButtonMap`]: the Windows Ink
/// barrel (`PEN_FLAG_BARREL`, or `POINTER_FLAG_SECONDBUTTON` in contact) is
/// the button the tablet driver set to Right Click, and a right or middle
/// click the driver performs as a mouse click while the pen is in range
/// ([`Self::on_driver_click`]; a Wacom side switch set to Middle Click) is
/// that click on the pen.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsInkMapper {
    in_proximity: bool,
    eraser: bool,
    map: PenButtonMap,
    held: PenButtonsHeld,
    last: Option<PenEvent>,
}

impl WindowsInkMapper {
    /// A mapper with no pen in range and the default button mapping.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The session's button mapping (its settings and its host).
    pub const fn set_button_map(&mut self, map: PenButtonMap) {
        self.map = map;
    }

    /// Whether a pen is in range.
    #[must_use]
    pub const fn in_proximity(&self) -> bool {
        self.in_proximity
    }

    /// Whether the pen in range touches the surface.
    #[must_use]
    pub fn touching(&self) -> bool {
        self.in_proximity && self.last.is_some_and(|event| event.touching)
    }

    /// Whether the driver's `click` is held on the pen.
    #[must_use]
    pub const fn driver_held(&self, click: DriverClick) -> bool {
        match click {
            DriverClick::Right => self.held.driver_right,
            DriverClick::Middle => self.held.driver_middle,
        }
    }

    /// A right or middle click the tablet driver performed as a mouse click
    /// (`WM_RBUTTONDOWN`, `WM_MBUTTONUP`, ...) while the pen is in range: the
    /// last sample again, with the click on the pen. `None` when no pen is in
    /// range (the click is the mouse's) or nothing changed.
    pub fn on_driver_click(
        &mut self,
        click: DriverClick,
        pressed: bool,
        metadata: LowLatencyMetadata,
    ) -> Option<PenEvent> {
        if !self.in_proximity {
            return None;
        }
        let held = match click {
            DriverClick::Right => &mut self.held.driver_right,
            DriverClick::Middle => &mut self.held.driver_middle,
        };
        if *held == pressed {
            return None;
        }
        *held = pressed;
        let mut event = self.last?;
        event.buttons = self.map.wire_buttons(self.held);
        event.metadata = metadata;
        self.last = Some(event);
        Some(event)
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
        self.held.barrel = sample.pen_flags & PEN_FLAG_BARREL != 0
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
            buttons: self.map.wire_buttons(self.held),
            metadata,
        };
        event.validate().ok()?;
        self.last = Some(event);
        Some(event)
    }

    /// The pen left (`WM_POINTERLEAVE`, `WM_POINTERCAPTURECHANGED`, focus
    /// loss) at a normalized position; `None` when none was in range.
    pub fn leave_at(&mut self, x: f64, y: f64, metadata: LowLatencyMetadata) -> Option<PenEvent> {
        if !self.in_proximity {
            return None;
        }
        self.in_proximity = false;
        self.held = PenButtonsHeld::default();
        self.last = None;
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

/// `WM_LBUTTONDOWN` and its neighbours: the mouse messages a tablet driver
/// sends for a pen button set to a mouse click.
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
#[cfg(test)]
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_RBUTTONDBLCLK: u32 = 0x0206;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MBUTTONDBLCLK: u32 = 0x0209;

/// Where a mouse message goes while a pen is in range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenMouseRoute {
    /// The tablet driver's click on a pen button: the pen's
    /// ([`WindowsInkMapper::on_driver_click`]).
    PenClick(DriverClick, bool),
    /// The mouse's, as without a pen.
    Mouse,
    /// Nothing: the pen owns the pointer.
    Drop,
}

/// Where a mouse message that arrives while `pen` is in range goes.
///
/// The Deck answers every pen `WM_POINTER` message, so Windows promotes none
/// of them to mouse messages: a click that arrives now is one the tablet
/// driver performed for a pen button set to a mouse click (Wacom's Right
/// Click, Middle Click or Left Click), or another mouse's. Right and middle
/// clicks ride on the pen, so the host sees them from the pen at the pen;
/// a left click while hovering is the mouse's left button (the pen's own
/// left click is its tip, so one in contact is dropped); movement stays the
/// pen's, and a release whose press the pen did not take reaches the mouse.
#[must_use]
pub fn route_mouse_while_pen(msg: u32, pen: &WindowsInkMapper) -> PenMouseRoute {
    let release = |click| {
        if pen.driver_held(click) {
            PenMouseRoute::PenClick(click, false)
        } else {
            PenMouseRoute::Mouse
        }
    };
    match msg {
        WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => PenMouseRoute::PenClick(DriverClick::Right, true),
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => PenMouseRoute::PenClick(DriverClick::Middle, true),
        WM_RBUTTONUP => release(DriverClick::Right),
        WM_MBUTTONUP => release(DriverClick::Middle),
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK if pen.touching() => PenMouseRoute::Drop,
        WM_MOUSEMOVE => PenMouseRoute::Drop,
        // A hovering left click, a left release, the wheel (a Wacom
        // Pan/Scroll), the side buttons.
        _ => PenMouseRoute::Mouse,
    }
}

/// The Deck's one-time hint when a pen tablet is attached but its pen
/// reaches the Deck as a mouse (a Wacom with "Use Windows Ink" off for the
/// Deck): Tablet support then sends no pen, and the host sees no pressure.
pub const TABLET_AS_MOUSE_HINT: &str = "Your tablet is sending mouse input, so pressure can't \
     reach the host. In Wacom Tablet Properties \u{203a} Mapping, turn on Use Windows Ink \
     for this app (or Other Programs).";

/// Whether to show [`TABLET_AS_MOUSE_HINT`]: Tablet support is chosen, the
/// computer has a pen digitizer (a HID pen or digitizer collection, or a
/// Wacom device), yet Windows lists no pen pointer device and no pen sample
/// arrived. Windows lists a pen exactly when the tablet's driver routes it
/// through Windows Ink, so a digitizer it does not list is one its driver
/// turns into a mouse.
#[must_use]
pub const fn tablet_sends_mouse(facts: TabletFacts) -> bool {
    facts.tablet_support
        && facts.digitizer_present
        && !facts.pen_pointer_listed
        && !facts.pen_observed
}

/// What [`tablet_sends_mouse`] judges.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "four independent facts")]
pub struct TabletFacts {
    /// Tablet support is the chosen tablet mode.
    pub tablet_support: bool,
    /// A pen digitizer is attached ([`is_pen_digitizer_collection`]).
    pub digitizer_present: bool,
    /// The OS lists a pen pointer device.
    pub pen_pointer_listed: bool,
    /// A pen sample arrived this run.
    pub pen_observed: bool,
}

/// Whether a HID top-level collection is a pen digitizer: the Digitizers
/// page's digitizer or pen usage, or any collection of a Wacom device (whose
/// driver may hide the standard one behind its own).
#[must_use]
pub const fn is_pen_digitizer_collection(vendor_id: u32, usage_page: u16, usage: u16) -> bool {
    const WACOM: u32 = 0x056A;
    const DIGITIZERS_PAGE: u16 = 0x0D;
    const DIGITIZER: u16 = 0x01;
    const PEN: u16 = 0x02;
    vendor_id == WACOM || (usage_page == DIGITIZERS_PAGE && matches!(usage, DIGITIZER | PEN))
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_tablet_whose_driver_makes_it_a_mouse_gets_the_windows_ink_hint() {
        // The field case: a Wacom with Use Windows Ink off for other
        // programs; Windows lists no pen, and none arrives.
        let field = TabletFacts {
            tablet_support: true,
            digitizer_present: true,
            ..TabletFacts::default()
        };
        assert!(tablet_sends_mouse(field));
        // Windows Ink on: Windows lists the pen (or one was seen).
        for ink in [
            TabletFacts {
                pen_pointer_listed: true,
                ..field
            },
            TabletFacts {
                pen_observed: true,
                ..field
            },
            // No tablet, or the user chose another tablet mode.
            TabletFacts {
                digitizer_present: false,
                ..field
            },
            TabletFacts {
                tablet_support: false,
                ..field
            },
        ] {
            assert!(!tablet_sends_mouse(ink), "{ink:?}");
        }
        assert!(is_pen_digitizer_collection(0x056A, 0xFF0D, 0x01));
        assert!(is_pen_digitizer_collection(0x045E, 0x0D, 0x02));
        // A touch screen is a digitizer page collection, not a pen.
        assert!(!is_pen_digitizer_collection(0x045E, 0x0D, 0x04));
        assert!(!is_pen_digitizer_collection(0x046D, 0x01, 0x02));
    }
    use super::*;
    use crate::pen_buttons::{HostPenButtons, PenButtonAction};
    use arcen_protocol::messages::{PEN_BUTTON_MIDDLE_CLICK, PEN_BUTTON_RIGHT_CLICK};

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
        assert_eq!(
            (event.tool, event.buttons),
            (PenTool::Tip, PEN_BUTTON_RIGHT_CLICK)
        );
        let leave = mapper.leave_at(0.0, 0.0, metadata()).expect("leave");
        assert_eq!(leave.tool, PenTool::Tip);
    }

    /// A fake `WM_POINTER` feed as a Wacom with Windows Ink on produces it:
    /// hover, the barrel (the side switch set to Right Click) pressed while
    /// hovering, then a tap with it held (`POINTER_FLAG_SECONDBUTTON`).
    #[test]
    fn the_windows_ink_barrel_is_a_right_click_on_every_host() {
        let mut barrel = sample(IN_RANGE, 0);
        barrel.pen_flags = PEN_FLAG_BARREL;
        let mut tap = sample(IN_CONTACT | POINTER_FLAG_SECONDBUTTON, 400);
        tap.pen_flags = PEN_FLAG_BARREL;
        let feed = [sample(IN_RANGE, 0), barrel, tap, sample(IN_RANGE, 0)];
        let run = |host| {
            let mut mapper = WindowsInkMapper::new();
            mapper.set_button_map(PenButtonMap {
                host,
                ..PenButtonMap::default()
            });
            feed.iter()
                .map(|raw| {
                    mapper
                        .on_sample(*raw, IMAGE, metadata())
                        .expect("placed")
                        .buttons
                })
                .collect::<Vec<_>>()
        };
        let right = PEN_BUTTON_RIGHT_CLICK;
        assert_eq!(run(HostPenButtons::Actions), [0, right, right, 0]);
        // A Linux Pier from before the meaning right-clicks bit 1.
        let legacy = PEN_BUTTON_MIDDLE_CLICK;
        assert_eq!(run(HostPenButtons::LinuxLegacy), [0, legacy, legacy, 0]);
    }

    #[test]
    fn the_barrel_follows_the_setting() {
        let mut mapper = WindowsInkMapper::new();
        let mut map = PenButtonMap::default();
        map.settings.barrel = PenButtonAction::MiddleClick;
        mapper.set_button_map(map);
        let mut raw = sample(IN_RANGE, 0);
        raw.pen_flags = PEN_FLAG_BARREL;
        let event = mapper.on_sample(raw, IMAGE, metadata()).expect("placed");
        assert_eq!(event.buttons, PEN_BUTTON_MIDDLE_CLICK);
        map.settings.barrel = PenButtonAction::Off;
        mapper.set_button_map(map);
        let event = mapper.on_sample(raw, IMAGE, metadata()).expect("placed");
        assert_eq!(event.buttons, 0);
    }

    /// A Wacom side switch set to Middle Click (or Right Click with Windows
    /// Ink's barrel left alone) arrives as `WM_MBUTTONDOWN`/`WM_RBUTTONDOWN`
    /// while the pen hovers: it is the pen's click, at the pen.
    #[test]
    fn a_click_the_driver_performs_rides_on_the_pen() {
        let mut mapper = WindowsInkMapper::new();
        assert_eq!(
            mapper.on_driver_click(DriverClick::Middle, true, metadata()),
            None,
            "no pen in range: the click is the mouse's"
        );
        let hover = mapper
            .on_sample(sample(IN_RANGE, 0), IMAGE, metadata())
            .expect("placed");
        let down = mapper
            .on_driver_click(DriverClick::Middle, true, metadata())
            .expect("pen click");
        assert_eq!(down.buttons, PEN_BUTTON_MIDDLE_CLICK);
        assert_eq!(
            (down.x, down.y, down.in_proximity),
            (hover.x, hover.y, true)
        );
        assert_eq!(
            mapper.on_driver_click(DriverClick::Middle, true, metadata()),
            None,
            "a repeated press changes nothing"
        );
        // Later samples keep it held until the driver releases it.
        let moved = mapper
            .on_sample(sample(IN_RANGE, 0), IMAGE, metadata())
            .expect("placed");
        assert_eq!(moved.buttons, PEN_BUTTON_MIDDLE_CLICK);
        let right = mapper
            .on_driver_click(DriverClick::Right, true, metadata())
            .expect("pen click");
        assert_eq!(
            right.buttons,
            PEN_BUTTON_MIDDLE_CLICK | PEN_BUTTON_RIGHT_CLICK
        );
        let up = mapper
            .on_driver_click(DriverClick::Middle, false, metadata())
            .expect("pen release");
        assert_eq!(up.buttons, PEN_BUTTON_RIGHT_CLICK);
        // Leaving range releases everything, and nothing stays held after.
        let leave = mapper.leave_at(0.5, 0.5, metadata()).expect("leave");
        assert_eq!(leave.buttons, 0);
        let back = mapper
            .on_sample(sample(IN_RANGE, 0), IMAGE, metadata())
            .expect("placed");
        assert_eq!(back.buttons, 0);
    }

    #[test]
    fn mouse_clicks_while_the_pen_hovers_are_the_drivers_pen_buttons() {
        let mut pen = WindowsInkMapper::new();
        pen.on_sample(sample(IN_RANGE, 0), IMAGE, metadata());
        assert_eq!(
            route_mouse_while_pen(WM_RBUTTONDOWN, &pen),
            PenMouseRoute::PenClick(DriverClick::Right, true)
        );
        assert_eq!(
            route_mouse_while_pen(WM_MBUTTONDBLCLK, &pen),
            PenMouseRoute::PenClick(DriverClick::Middle, true)
        );
        // A release the pen did not press is the mouse's (pressed before the
        // pen came into range).
        assert_eq!(
            route_mouse_while_pen(WM_RBUTTONUP, &pen),
            PenMouseRoute::Mouse
        );
        pen.on_driver_click(DriverClick::Right, true, metadata());
        assert_eq!(
            route_mouse_while_pen(WM_RBUTTONUP, &pen),
            PenMouseRoute::PenClick(DriverClick::Right, false)
        );
        // A side switch set to Left Click: the mouse's left button while
        // hovering; in contact the tip is the left button already.
        assert_eq!(
            route_mouse_while_pen(WM_LBUTTONDOWN, &pen),
            PenMouseRoute::Mouse
        );
        assert_eq!(
            route_mouse_while_pen(WM_LBUTTONUP, &pen),
            PenMouseRoute::Mouse
        );
        assert_eq!(
            route_mouse_while_pen(WM_MOUSEMOVE, &pen),
            PenMouseRoute::Drop
        );
        pen.on_sample(sample(IN_CONTACT, 300), IMAGE, metadata());
        assert_eq!(
            route_mouse_while_pen(WM_LBUTTONDOWN, &pen),
            PenMouseRoute::Drop
        );
        // The wheel (a Wacom Pan/Scroll) stays the mouse's.
        assert_eq!(route_mouse_while_pen(0x020A, &pen), PenMouseRoute::Mouse);
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
