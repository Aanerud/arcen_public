// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host-side reading of a bridged Intuos5-family pen: the inverse of
//! [`Intuos5PenReport::encode`](crate::Intuos5PenReport::encode).
//!
//! A host whose operating system has no driver that binds a bridged tablet
//! (a macOS Pier: Wacom's driver ignores `IOHIDUserDevice`s, and macOS gives a
//! generic digitizer no pressure — `hosts/macos/AGENTS.md`) can still give
//! applications pressure, tilt, barrel buttons and the eraser by reading the
//! tablet's own packets and posting tablet events itself, which is what the
//! tablet maker's driver does on that operating system.
//!
//! [`IntuosPenReader`] is sans-I/O: the platform hands it every interrupt IN
//! report of the pen interface, verbatim, and gets back a [`HostPenSample`]
//! in normalized units. It knows only the models in
//! [`HOST_DECODED_INTUOS_MODELS`]; anything else is left to the published
//! HID device, so a host never guesses a coordinate range.
//!
//! The tablet must be in Wacom mode first: [`INTUOS_MODE_SWITCH`] is the
//! feature report a host sends, as the vendor driver does at probe.

use crate::wacom_intuos5::{
    INTUOS5_MAX_PRESSURE, INTUOS5_MAX_X, INTUOS5_MAX_Y, INTUOS5_MODE_REPORT_ID,
    INTUOS5_PEN_REPORT_ID, INTUOS5_PEN_REPORT_LENGTH, INTUOS5_TOUCH_L_PRODUCT_ID,
    INTUOS5_WACOM_MODE, Intuos5PenState, WACOM_VENDOR_ID,
};

/// The coordinate and pressure ranges of one model whose pen packets a host
/// may read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntuosPenModel {
    pub product_id: u16,
    pub max_x: u32,
    pub max_y: u32,
    pub max_pressure: u16,
}

/// Every model a host reads itself. Only ranges established for Arcen's own
/// emulation of a tablet the project has in hand are listed; a model whose
/// range is not known stays with the published HID device.
pub const HOST_DECODED_INTUOS_MODELS: &[IntuosPenModel] = &[IntuosPenModel {
    product_id: INTUOS5_TOUCH_L_PRODUCT_ID,
    max_x: INTUOS5_MAX_X,
    max_y: INTUOS5_MAX_Y,
    max_pressure: INTUOS5_MAX_PRESSURE,
}];

/// The model `vendor_id:product_id` is, when a host may read its pen.
#[must_use]
pub fn host_decoded_intuos_model(vendor_id: u16, product_id: u16) -> Option<IntuosPenModel> {
    (vendor_id == WACOM_VENDOR_ID)
        .then(|| {
            HOST_DECODED_INTUOS_MODELS
                .iter()
                .find(|model| model.product_id == product_id)
                .copied()
        })
        .flatten()
}

/// `SET_REPORT(Feature)` that switches the pen interface out of HID mouse
/// mode: report id and bytes, id included.
pub const INTUOS_MODE_SWITCH: (u8, [u8; 2]) = (
    INTUOS5_MODE_REPORT_ID,
    [INTUOS5_MODE_REPORT_ID, INTUOS5_WACOM_MODE],
);

/// Raw pressure above which the tip counts as touching. The packet has no
/// tip switch; this is roughly 1 % of the Intuos5 range, chosen, not
/// measured on a pen.
pub const INTUOS_TOUCH_THRESHOLD: u16 = 20;

/// One pen packet, as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntuosPenPacket {
    /// A tool entered proximity.
    Enter { tool_id: u32, serial: u32 },
    /// A general pen packet.
    Pen(Intuos5PenState),
    /// The tool left proximity.
    Exit,
    /// A packet of the pen report this reader does not interpret (another
    /// tool kind, such as a puck or an airbrush wheel).
    Other,
}

/// Reads one interrupt IN report of the pen interface. `None` when it is not
/// a pen packet at all (another report id, or a short report).
#[must_use]
pub fn decode_intuos_pen_report(report: &[u8]) -> Option<IntuosPenPacket> {
    if report.len() < INTUOS5_PEN_REPORT_LENGTH || report[0] != INTUOS5_PEN_REPORT_ID {
        return None;
    }
    let r = |index: usize| u32::from(report[index]);
    let status = report[1];
    Some(if status & 0xfc == 0xc0 {
        let tool_id = (r(2) << 4) | (r(3) >> 4) | ((r(7) & 0x0f) << 16) | ((r(8) >> 4) << 12);
        let serial =
            ((r(3) & 0x0f) << 28) | (r(4) << 20) | (r(5) << 12) | (r(6) << 4) | (r(7) >> 4);
        IntuosPenPacket::Enter { tool_id, serial }
    } else if status & 0xfe == 0x80 {
        IntuosPenPacket::Exit
    } else if status & 0xb8 == 0xa0 {
        let tilt_x = ((r(7) & 0x3f) << 1) | (r(8) >> 7);
        let tilt_y = r(8) & 0x7f;
        let signed =
            |value: u32| i8::try_from(i32::try_from(value).unwrap_or(64) - 64).unwrap_or(0);
        IntuosPenPacket::Pen(Intuos5PenState {
            x: (((r(2) << 8) | r(3)) << 1) | ((r(9) >> 1) & 1),
            y: (((r(4) << 8) | r(5)) << 1) | (r(9) & 1),
            pressure: u16::try_from((r(6) << 3) | ((r(7) >> 6) << 1) | (r(1) & 1)).unwrap_or(0),
            distance: report[9] >> 2,
            tilt_x: signed(tilt_x),
            tilt_y: signed(tilt_y),
            stylus: status & 0x02 != 0,
            stylus2: status & 0x04 != 0,
        })
    } else {
        IntuosPenPacket::Other
    })
}

/// The 4D mouse: the one tool id with bit 3 set that is not an eraser.
const INTUOS_4D_MOUSE_TOOL_ID: u32 = 0x09c;

/// Whether `tool_id` is the eraser end of a pen, by the rule the Linux
/// `wacom` driver applies (`wacom_intuos_get_tool_type`): after its named
/// pencil, brush, mouse, lens and airbrush ids, any id with bit 3 set is an
/// eraser (`0x80a` Grip Pen, `0x80c` Art Pen, `0x1080c` Marker Pen, `0x82b`
/// and `0x91b` Intuos3), everything else a pen tip. Of the named ids only
/// the 4D mouse has bit 3 set.
#[must_use]
pub const fn intuos_tool_is_eraser(tool_id: u32) -> bool {
    tool_id & 0x0008 != 0 && tool_id != INTUOS_4D_MOUSE_TOOL_ID
}

/// One pen sample for a host's tablet events.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostPenSample {
    /// 0.0 (left) ..< 1.0 (right) of the tablet's active area, scaled as
    /// libinput and the X server scale an absolute axis
    /// (`device_x / (max_x + 1)`), so a host pointer position computed from
    /// the device units (`arcen_input::tablet_pointer`) lands on the same
    /// pixel.
    pub x: f64,
    /// 0.0 (top) ..< 1.0 (bottom), as `x`.
    pub y: f64,
    /// The column in device units, `0..=max_x` of the model.
    pub device_x: u32,
    /// The row in device units, `0..=max_y` of the model.
    pub device_y: u32,
    /// 0.0 ..= 1.0.
    pub pressure: f32,
    /// -64 ..= 63, about one degree per unit.
    pub tilt_x_degrees: f32,
    pub tilt_y_degrees: f32,
    pub eraser: bool,
    pub in_proximity: bool,
    pub touching: bool,
    /// Bit 0 the lower barrel button, bit 1 the upper.
    pub barrel_buttons: u16,
}

/// Turns one model's pen packets into samples, remembering the tool in
/// proximity and the last position (a proximity exit carries none).
#[derive(Debug, Clone)]
pub struct IntuosPenReader {
    model: IntuosPenModel,
    eraser: bool,
    last: Option<HostPenSample>,
    packets: u64,
    ignored: u64,
}

impl IntuosPenReader {
    #[must_use]
    pub const fn new(model: IntuosPenModel) -> Self {
        Self {
            model,
            eraser: false,
            last: None,
            packets: 0,
            ignored: 0,
        }
    }

    /// The model whose packets this reader reads.
    #[must_use]
    pub const fn model(&self) -> IntuosPenModel {
        self.model
    }

    /// Pen packets read so far, and reports or packets that produced no
    /// sample.
    #[must_use]
    pub const fn counts(&self) -> (u64, u64) {
        (self.packets, self.ignored)
    }

    /// One interrupt IN report of the pen interface. A sample when it moved
    /// the pen, entered or left proximity; `None` otherwise.
    pub fn read(&mut self, report: &[u8]) -> Option<HostPenSample> {
        let Some(packet) = decode_intuos_pen_report(report) else {
            self.ignored += 1;
            return None;
        };
        self.packets += 1;
        match packet {
            IntuosPenPacket::Enter { tool_id, .. } => {
                self.eraser = intuos_tool_is_eraser(tool_id);
                // Position arrives with the first general packet.
                None
            }
            IntuosPenPacket::Pen(state) => {
                let sample = self.sample(state);
                self.last = Some(sample);
                Some(sample)
            }
            IntuosPenPacket::Exit => {
                let last = self.last.take()?;
                self.eraser = false;
                Some(HostPenSample {
                    pressure: 0.0,
                    in_proximity: false,
                    touching: false,
                    barrel_buttons: 0,
                    ..last
                })
            }
            IntuosPenPacket::Other => {
                self.ignored += 1;
                None
            }
        }
    }

    fn sample(&self, state: Intuos5PenState) -> HostPenSample {
        let unit = |value: u32, max: u32| f64::from(value.min(max)) / (f64::from(max) + 1.0);
        let pressure = state.pressure.min(self.model.max_pressure);
        HostPenSample {
            x: unit(state.x, self.model.max_x),
            y: unit(state.y, self.model.max_y),
            device_x: state.x.min(self.model.max_x),
            device_y: state.y.min(self.model.max_y),
            pressure: f32::from(pressure) / f32::from(self.model.max_pressure.max(1)),
            tilt_x_degrees: f32::from(state.tilt_x),
            tilt_y_degrees: f32::from(state.tilt_y),
            eraser: self.eraser,
            in_proximity: true,
            touching: pressure > INTUOS_TOUCH_THRESHOLD,
            barrel_buttons: u16::from(state.stylus) | (u16::from(state.stylus2) << 1),
        }
    }
}

#[cfg(test)]
#[path = "wacom_intuos_pen_tests.rs"]
mod tests;
