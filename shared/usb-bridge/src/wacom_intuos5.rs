// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! An emulated Wacom Intuos5 touch L (`056a:0317`; its USB product string —
//! the Linux `wacom` driver's table names the same id "Intuos Pro L"): the
//! device a Linux host's in-kernel `wacom` driver binds, driven by scripted pen strokes instead of a
//! pen in hand.
//!
//! [`WacomIntuos5`] answers the default control pipe from the descriptor
//! fixture in `fixtures/wacom-intuos5-touch-l/` and keeps the HID feature
//! reports a host sets — notably the mode switch the `wacom` driver sends at
//! probe (`SET_REPORT(Feature, id 2) = [2, 2]`, then `GET_REPORT` until the
//! device echoes it). [`Intuos5PenReport`] encodes the 10-byte Intuos pen
//! packets the driver parses (`wacom_intuos_inout` and
//! `wacom_intuos_general`), and [`intuos5_scripted_stroke`] is a known stroke
//! a host-side check can assert exactly.
//!
//! Transport-agnostic: everything here is what the real device produces —
//! descriptors, control answers, interrupt IN reports — and the HID-level
//! calls ([`WacomIntuos5::get_report`], [`WacomIntuos5::set_report`],
//! [`WacomIntuos5::report_descriptor`]) are what a host `uhid` presentation
//! needs, so the same emulation serves a virtual host controller and `uhid`.

use crate::{
    AlternateSetting, CapturedDeviceFacts, ControlResponse, DescriptorError, DeviceSnapshot,
    EmulatedUsbFunction, EndpointAddress, InterfaceNumber, InterfaceReportDescriptor, SetupPacket,
    UsbDeviceId, UsbSpeed, parse_configuration_descriptor,
};
use std::collections::BTreeMap;
use std::time::Duration;

/// Wacom's USB vendor id.
pub const WACOM_VENDOR_ID: u16 = 0x056a;
/// Intuos5 touch L by its USB product string; the Linux `wacom` driver's
/// table names `0x0317` "Wacom Intuos Pro L" (type `INTUOSPL`, the same
/// Intuos5 packet protocol), so its input devices are "Wacom Intuos Pro L
/// Pen/Pad/Finger".
pub const INTUOS5_TOUCH_L_PRODUCT_ID: u16 = 0x0317;

/// Device descriptor of the fixture.
pub const INTUOS5_DEVICE_DESCRIPTOR: &[u8] =
    include_bytes!("../fixtures/wacom-intuos5-touch-l/device.bin");
/// Full configuration descriptor of the fixture (three HID interfaces).
pub const INTUOS5_CONFIGURATION_DESCRIPTOR: &[u8] =
    include_bytes!("../fixtures/wacom-intuos5-touch-l/configuration.bin");
/// Interface 0: pen and pad, vendor Digitizers page `0xFF0D`.
pub const INTUOS5_PEN_REPORT_DESCRIPTOR: &[u8] =
    include_bytes!("../fixtures/wacom-intuos5-touch-l/report-interface0.bin");
/// Interface 1: touch, vendor page `0xFF00`, 64-byte reports.
pub const INTUOS5_TOUCH_REPORT_DESCRIPTOR: &[u8] =
    include_bytes!("../fixtures/wacom-intuos5-touch-l/report-interface1.bin");
/// Interface 2: boot mouse.
pub const INTUOS5_MOUSE_REPORT_DESCRIPTOR: &[u8] =
    include_bytes!("../fixtures/wacom-intuos5-touch-l/report-interface2.bin");

/// Pen and pad reports: interrupt IN 1 (interface 0).
pub const INTUOS5_PEN_IN: EndpointAddress = EndpointAddress(0x81);
/// Touch reports: interrupt IN 2 (interface 1).
pub const INTUOS5_TOUCH_IN: EndpointAddress = EndpointAddress(0x82);
/// Boot mouse reports: interrupt IN 3 (interface 2).
pub const INTUOS5_MOUSE_IN: EndpointAddress = EndpointAddress(0x83);

/// Report id of an Intuos pen packet (`WACOM_REPORT_PENABLED`).
pub const INTUOS5_PEN_REPORT_ID: u8 = 2;
/// Length of an Intuos pen packet, report id included (`WACOM_PKGLEN_INTUOS`).
pub const INTUOS5_PEN_REPORT_LENGTH: usize = 10;
/// Feature report the `wacom` driver writes to leave HID mouse mode.
pub const INTUOS5_MODE_REPORT_ID: u8 = 2;
/// Value of that feature report in Wacom (tablet) mode.
pub const INTUOS5_WACOM_MODE: u8 = 2;

/// Largest X the Linux driver accepts for this model.
pub const INTUOS5_MAX_X: u32 = 65_024;
/// Largest Y the Linux driver accepts for this model.
pub const INTUOS5_MAX_Y: u32 = 40_640;
/// Largest pressure (11 bits).
pub const INTUOS5_MAX_PRESSURE: u16 = 2_047;
/// Largest hover distance (6 bits).
pub const INTUOS5_MAX_DISTANCE: u8 = 63;
/// Tool id of the standard Intuos5 Grip Pen.
pub const INTUOS5_GRIP_PEN_TOOL_ID: u32 = 0x0802;
/// Tool id of the Intuos4/5/Pro Grip Pen (KP-501E) as the tablet reports
/// it (libwacom's stylus `0x100802`, the driver's `ABS_MISC` form): bit 16
/// is set, so it travels in the enter report's high nibbles too.
pub const INTUOS_PRO_GRIP_PEN_TOOL_ID: u32 = 0x1_0802;

const MANUFACTURER: &str = "Wacom Co.,Ltd.";
const PRODUCT: &str = "Intuos5 touch L";
const INTERFACES: u8 = 3;

const REQUEST_GET_STATUS: u8 = 0x00;
const REQUEST_CLEAR_FEATURE: u8 = 0x01;
const REQUEST_SET_FEATURE: u8 = 0x03;
const REQUEST_SET_ADDRESS: u8 = 0x05;
const REQUEST_GET_DESCRIPTOR: u8 = 0x06;
const REQUEST_GET_CONFIGURATION: u8 = 0x08;
const REQUEST_SET_CONFIGURATION: u8 = 0x09;
const REQUEST_GET_INTERFACE: u8 = 0x0a;
const HID_GET_REPORT: u8 = 0x01;
const HID_GET_IDLE: u8 = 0x02;
const HID_GET_PROTOCOL: u8 = 0x03;
const HID_SET_REPORT: u8 = 0x09;
const HID_SET_IDLE: u8 = 0x0a;
const HID_SET_PROTOCOL: u8 = 0x0b;
const DESCRIPTOR_DEVICE: u8 = 0x01;
const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const DESCRIPTOR_STRING: u8 = 0x03;
const DESCRIPTOR_HID: u8 = 0x21;
const DESCRIPTOR_REPORT: u8 = 0x22;
const RECIPIENT_DEVICE: u8 = 0;
const RECIPIENT_INTERFACE: u8 = 1;

/// The three HID report types (`wValue` high byte of `GET/SET_REPORT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HidReportKind {
    Input,
    Output,
    Feature,
}

impl HidReportKind {
    /// From the `GET/SET_REPORT` report type byte.
    #[must_use]
    pub const fn from_request(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Input),
            2 => Some(Self::Output),
            3 => Some(Self::Feature),
            _ => None,
        }
    }

    /// The `GET/SET_REPORT` report type byte.
    #[must_use]
    pub const fn request_value(self) -> u8 {
        match self {
            Self::Input => 1,
            Self::Output => 2,
            Self::Feature => 3,
        }
    }

    const fn main_item_tag(self) -> u8 {
        match self {
            Self::Input => 0x80,
            Self::Output => 0x90,
            Self::Feature => 0xb0,
        }
    }
}

/// Length in bytes, report id included, of every report of `kind` that a
/// HID report descriptor declares, by report id (0 when it uses none).
///
/// The Linux `wacom` driver sizes its packets the same way
/// (`hid_report_len`), so the longest input report of an interface is the
/// packet length the driver expects there.
#[must_use]
pub fn declared_report_lengths(descriptor: &[u8], kind: HidReportKind) -> BTreeMap<u8, usize> {
    let mut bits: BTreeMap<u8, usize> = BTreeMap::new();
    let (mut report_id, mut report_size, mut report_count) = (0_u8, 0_usize, 0_usize);
    let mut stack: Vec<(usize, usize, u8)> = Vec::new();
    let mut offset = 0;
    while offset < descriptor.len() {
        let prefix = descriptor[offset];
        if prefix == 0xfe {
            // Long item: skip it whole.
            let size = descriptor.get(offset + 1).copied().unwrap_or(0);
            offset = offset.saturating_add(3 + usize::from(size));
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            other => usize::from(other),
        };
        let data = descriptor
            .get(offset + 1..offset + 1 + size)
            .unwrap_or_default();
        let value = data
            .iter()
            .rev()
            .fold(0_usize, |value, byte| (value << 8) | usize::from(*byte));
        match prefix & 0xfc {
            0x74 => report_size = value,
            0x94 => report_count = value,
            0x84 => report_id = u8::try_from(value).unwrap_or(u8::MAX),
            0xa4 => stack.push((report_size, report_count, report_id)),
            0xb4 => {
                if let Some((size, count, id)) = stack.pop() {
                    (report_size, report_count, report_id) = (size, count, id);
                }
            }
            tag if tag == kind.main_item_tag() => {
                *bits.entry(report_id).or_default() += report_size.saturating_mul(report_count);
            }
            _ => {}
        }
        offset += 1 + size;
    }
    bits.into_iter()
        .map(|(id, bits)| (id, bits.div_ceil(8) + usize::from(id != 0)))
        .collect()
}

/// The emulated tablet's device side.
#[derive(Debug, Clone)]
pub struct WacomIntuos5 {
    configuration: u8,
    idle: [u8; INTERFACES as usize],
    protocol: [u8; INTERFACES as usize],
    reports: BTreeMap<(u8, HidReportKind, u8), Vec<u8>>,
    set_reports: usize,
}

impl Default for WacomIntuos5 {
    fn default() -> Self {
        Self::new()
    }
}

impl WacomIntuos5 {
    /// A tablet freshly plugged in: unconfigured, report protocol, HID mouse
    /// mode until a host switches it.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            configuration: 0,
            idle: [0; INTERFACES as usize],
            protocol: [1; INTERFACES as usize],
            reports: BTreeMap::new(),
            set_reports: 0,
        }
    }

    /// The identity the device descriptor carries.
    #[must_use]
    pub fn identity() -> UsbDeviceId {
        let field = |offset: usize| {
            u16::from_le_bytes([
                INTUOS5_DEVICE_DESCRIPTOR[offset],
                INTUOS5_DEVICE_DESCRIPTOR[offset + 1],
            ])
        };
        UsbDeviceId {
            vendor_id: field(8),
            product_id: field(10),
            bcd_device: field(12),
        }
    }

    /// The HID report descriptor of `interface`.
    #[must_use]
    pub const fn report_descriptor(interface: u8) -> Option<&'static [u8]> {
        match interface {
            0 => Some(INTUOS5_PEN_REPORT_DESCRIPTOR),
            1 => Some(INTUOS5_TOUCH_REPORT_DESCRIPTOR),
            2 => Some(INTUOS5_MOUSE_REPORT_DESCRIPTOR),
            _ => None,
        }
    }

    /// Whether a host has switched the pen interface into Wacom mode.
    #[must_use]
    pub fn wacom_mode(&self) -> bool {
        self.reports
            .get(&(0, HidReportKind::Feature, INTUOS5_MODE_REPORT_ID))
            .and_then(|report| report.get(1))
            == Some(&INTUOS5_WACOM_MODE)
    }

    /// How many `SET_REPORT` requests the device accepted.
    #[must_use]
    pub const fn set_reports(&self) -> usize {
        self.set_reports
    }

    /// HID `SET_REPORT`: stores the report, which starts with its id when the
    /// id is non-zero, as USB HID sends it. A report for an interface the
    /// device does not have, or whose first byte contradicts its id, is
    /// refused (the USB request stalls).
    pub fn set_report(
        &mut self,
        interface: u8,
        kind: HidReportKind,
        report_id: u8,
        data: &[u8],
    ) -> bool {
        if Self::report_descriptor(interface).is_none()
            || data.is_empty()
            || (report_id != 0 && data[0] != report_id)
        {
            return false;
        }
        self.reports
            .insert((interface, kind, report_id), data.to_vec());
        self.set_reports = self.set_reports.saturating_add(1);
        true
    }

    /// HID `GET_REPORT`: the last report set with this id, or a zeroed report
    /// of the declared length; `None` (a stall) for a report the interface
    /// neither declares nor was given.
    #[must_use]
    pub fn get_report(&self, interface: u8, kind: HidReportKind, report_id: u8) -> Option<Vec<u8>> {
        if let Some(report) = self.reports.get(&(interface, kind, report_id)) {
            return Some(report.clone());
        }
        let length =
            *declared_report_lengths(Self::report_descriptor(interface)?, kind).get(&report_id)?;
        let mut report = vec![0; length];
        if report_id != 0 {
            report[0] = report_id;
        }
        Some(report)
    }

    fn standard(
        &mut self,
        setup: SetupPacket,
        direction_in: bool,
        recipient: u8,
    ) -> ControlResponse {
        match (setup.request, direction_in, recipient) {
            (REQUEST_GET_STATUS, true, _) => data(vec![0, 0], setup.length),
            (REQUEST_CLEAR_FEATURE | REQUEST_SET_FEATURE | REQUEST_SET_ADDRESS, false, _) => {
                ControlResponse::Ack
            }
            (REQUEST_GET_DESCRIPTOR, true, _) => Self::descriptor(setup),
            (REQUEST_GET_CONFIGURATION, true, RECIPIENT_DEVICE) => {
                data(vec![self.configuration], setup.length)
            }
            (REQUEST_SET_CONFIGURATION, false, RECIPIENT_DEVICE) if setup.value <= 1 => {
                self.configuration = setup.value.to_le_bytes()[0];
                ControlResponse::Ack
            }
            (REQUEST_GET_INTERFACE, true, RECIPIENT_INTERFACE)
                if setup.index < u16::from(INTERFACES) =>
            {
                data(vec![0], setup.length)
            }
            _ => ControlResponse::Stall,
        }
    }

    fn descriptor(setup: SetupPacket) -> ControlResponse {
        let interface = setup.index.to_le_bytes()[0];
        let bytes = match (setup.descriptor_type(), setup.descriptor_index()) {
            (DESCRIPTOR_DEVICE, 0) => INTUOS5_DEVICE_DESCRIPTOR.to_vec(),
            (DESCRIPTOR_CONFIGURATION, 0) => INTUOS5_CONFIGURATION_DESCRIPTOR.to_vec(),
            (DESCRIPTOR_STRING, 0) => vec![4, DESCRIPTOR_STRING, 0x09, 0x04],
            (DESCRIPTOR_STRING, 1) => string_descriptor(MANUFACTURER),
            (DESCRIPTOR_STRING, 2) => string_descriptor(PRODUCT),
            (DESCRIPTOR_HID, 0) => match hid_class_descriptor(interface) {
                Some(bytes) => bytes,
                None => return ControlResponse::Stall,
            },
            (DESCRIPTOR_REPORT, 0) => match Self::report_descriptor(interface) {
                Some(bytes) => bytes.to_vec(),
                None => return ControlResponse::Stall,
            },
            // Device qualifier, other-speed configuration, BOS, other
            // strings: a full-speed USB 2.0 device stalls them.
            _ => return ControlResponse::Stall,
        };
        data(bytes, setup.length)
    }

    fn class(&mut self, setup: SetupPacket, payload: &[u8]) -> ControlResponse {
        let interface = setup.index.to_le_bytes()[0];
        if interface >= INTERFACES {
            return ControlResponse::Stall;
        }
        let slot = usize::from(interface);
        let [report_id, kind] = setup.value.to_le_bytes();
        match setup.request {
            HID_GET_REPORT => HidReportKind::from_request(kind)
                .and_then(|kind| self.get_report(interface, kind, report_id))
                .map_or(ControlResponse::Stall, |report| data(report, setup.length)),
            HID_SET_REPORT => match HidReportKind::from_request(kind) {
                Some(kind) if self.set_report(interface, kind, report_id, payload) => {
                    ControlResponse::Ack
                }
                _ => ControlResponse::Stall,
            },
            HID_GET_IDLE => data(vec![self.idle[slot]], setup.length),
            HID_SET_IDLE => {
                self.idle[slot] = setup.value.to_be_bytes()[0];
                ControlResponse::Ack
            }
            HID_GET_PROTOCOL => data(vec![self.protocol[slot]], setup.length),
            HID_SET_PROTOCOL if setup.value <= 1 => {
                self.protocol[slot] = setup.value.to_le_bytes()[0];
                ControlResponse::Ack
            }
            _ => ControlResponse::Stall,
        }
    }
}

impl EmulatedUsbFunction for WacomIntuos5 {
    fn captured_facts(&self) -> Result<CapturedDeviceFacts, DescriptorError> {
        Ok(CapturedDeviceFacts {
            snapshot: DeviceSnapshot {
                id: Self::identity(),
                device_class: INTUOS5_DEVICE_DESCRIPTOR[4],
                configuration: parse_configuration_descriptor(INTUOS5_CONFIGURATION_DESCRIPTOR)?,
            },
            speed: UsbSpeed::Full,
            report_descriptors: (0..INTERFACES)
                .filter_map(|interface| {
                    Self::report_descriptor(interface).map(|descriptor| InterfaceReportDescriptor {
                        interface,
                        descriptor: descriptor.to_vec(),
                    })
                })
                .collect(),
        })
    }

    fn control(&mut self, setup: SetupPacket, payload: &[u8]) -> ControlResponse {
        let direction_in = setup.request_type & 0x80 != 0;
        let recipient = setup.request_type & 0x1f;
        match (setup.request_type >> 5) & 0x03 {
            0 => self.standard(setup, direction_in, recipient),
            1 if recipient == RECIPIENT_INTERFACE => self.class(setup, payload),
            _ => ControlResponse::Stall,
        }
    }

    fn primary_interrupt_in(&self) -> EndpointAddress {
        INTUOS5_PEN_IN
    }

    fn has_interrupt_in(&self, endpoint: EndpointAddress) -> bool {
        [INTUOS5_PEN_IN, INTUOS5_TOUCH_IN, INTUOS5_MOUSE_IN].contains(&endpoint)
    }

    fn has_alternate(&self, interface: InterfaceNumber, alternate: AlternateSetting) -> bool {
        interface.0 < INTERFACES && alternate.0 == 0
    }
}

/// The nine-byte HID class descriptor of `interface`, cut from the
/// configuration descriptor.
fn hid_class_descriptor(interface: u8) -> Option<Vec<u8>> {
    let bytes = INTUOS5_CONFIGURATION_DESCRIPTOR;
    let mut offset = 0;
    let mut current = None;
    while offset + 1 < bytes.len() {
        let length = usize::from(bytes[offset]);
        if length < 2 {
            return None;
        }
        match bytes[offset + 1] {
            0x04 => current = bytes.get(offset + 2).copied(),
            DESCRIPTOR_HID if current == Some(interface) => {
                return bytes.get(offset..offset + length).map(<[u8]>::to_vec);
            }
            _ => {}
        }
        offset += length;
    }
    None
}

fn data(mut bytes: Vec<u8>, requested: u16) -> ControlResponse {
    bytes.truncate(usize::from(requested));
    ControlResponse::Data(bytes)
}

fn string_descriptor(value: &str) -> Vec<u8> {
    let mut bytes = vec![0, DESCRIPTOR_STRING];
    for unit in value.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes[0] = u8::try_from(bytes.len()).unwrap_or(u8::MAX);
    bytes
}

/// One Intuos pen packet (report id 2, ten bytes), as the Linux `wacom`
/// driver parses it for the Intuos5 family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intuos5PenReport {
    /// A tool enters proximity: its tool id and serial number.
    Enter { tool_id: u32, serial: u32 },
    /// A tool is within range but not (or no longer) close enough to report
    /// a position: what a real pen sends before it enters proximity and
    /// after it leaves. The Linux driver reads it as "in range"
    /// (`data[1] & 0xfe == 0x20`) and, while it was reporting, releases the
    /// tip (touch and pressure zero, distance at its maximum).
    InRange,
    /// A general pen packet. Values are clamped to the model's ranges.
    Pen(Intuos5PenState),
    /// The tool leaves proximity.
    Exit,
}

/// Position, pressure, hover distance, tilt and barrel buttons of one pen
/// packet, in device units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Intuos5PenState {
    pub x: u32,
    pub y: u32,
    pub pressure: u16,
    pub distance: u8,
    /// -64..=63, as the driver reports `ABS_TILT_X`.
    pub tilt_x: i8,
    /// -64..=63, as the driver reports `ABS_TILT_Y`.
    pub tilt_y: i8,
    /// Lower barrel button (`BTN_STYLUS`).
    pub stylus: bool,
    /// Upper barrel button (`BTN_STYLUS2`).
    pub stylus2: bool,
}

impl Intuos5PenReport {
    /// The ten report bytes, report id first.
    #[must_use]
    pub fn encode(self) -> [u8; INTUOS5_PEN_REPORT_LENGTH] {
        let mut report = [0_u8; INTUOS5_PEN_REPORT_LENGTH];
        report[0] = INTUOS5_PEN_REPORT_ID;
        match self {
            Self::Enter { tool_id, serial } => {
                let id = tool_id & 0x000f_ffff;
                report[1] = 0xc2;
                report[2] = byte(id >> 4);
                report[3] = byte(((id & 0x0f) << 4) | (serial >> 28));
                report[4] = byte(serial >> 20);
                report[5] = byte(serial >> 12);
                report[6] = byte(serial >> 4);
                report[7] = byte(((serial & 0x0f) << 4) | ((id >> 16) & 0x0f));
                report[8] = byte(((id >> 12) & 0x0f) << 4);
            }
            Self::Pen(state) => {
                let x = state.x.min(INTUOS5_MAX_X);
                let y = state.y.min(INTUOS5_MAX_Y);
                let pressure = u32::from(state.pressure.min(INTUOS5_MAX_PRESSURE));
                let tilt_x = u32::from((state.tilt_x.clamp(-64, 63) + 64).unsigned_abs());
                let tilt_y = u32::from((state.tilt_y.clamp(-64, 63) + 64).unsigned_abs());
                let distance = u32::from(state.distance.min(INTUOS5_MAX_DISTANCE));
                report[1] = 0xe0
                    | (u8::from(state.stylus2) << 2)
                    | (u8::from(state.stylus) << 1)
                    | byte(pressure & 1);
                report[2..4].copy_from_slice(&u16::try_from(x >> 1).unwrap_or(0).to_be_bytes());
                report[4..6].copy_from_slice(&u16::try_from(y >> 1).unwrap_or(0).to_be_bytes());
                report[6] = byte(pressure >> 3);
                report[7] = byte((((pressure >> 1) & 0x03) << 6) | (tilt_x >> 1));
                report[8] = byte(((tilt_x & 1) << 7) | tilt_y);
                report[9] = byte((distance << 2) | ((x & 1) << 1) | (y & 1));
            }
            Self::InRange => report[1] = 0x20,
            Self::Exit => report[1] = 0x80,
        }
        report
    }
}

const fn byte(value: u32) -> u8 {
    value.to_le_bytes()[0]
}

/// One step of a scripted stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptedReport {
    /// What the step shows, for logs.
    pub phase: &'static str,
    pub report: Intuos5PenReport,
}

/// Serial number of the scripted pen.
pub const SCRIPTED_PEN_SERIAL: u32 = 0x0a2c_e017;
/// Where the scripted stroke touches down.
pub const SCRIPTED_START: (u32, u32) = (16_000, 12_000);
/// Where the horizontal leg of the scripted stroke ends.
pub const SCRIPTED_CORNER: (u32, u32) = (48_000, 12_000);
/// Where the scripted stroke lifts off.
pub const SCRIPTED_END: (u32, u32) = (48_000, 28_000);
/// Pressure while the scripted stroke moves.
pub const SCRIPTED_MOVE_PRESSURE: u16 = 1_024;
/// Hover distance before and after the stroke.
pub const SCRIPTED_HOVER_DISTANCE: u8 = 20;
/// Pressure steps of the touch-down ramp.
pub const SCRIPTED_RAMP_STEPS: u16 = 16;
/// Steps of the horizontal and vertical legs.
pub const SCRIPTED_LEG_STEPS: (u32, u32) = (16, 8);
/// Pause the scripted stroke expects between reports at least.
pub const SCRIPTED_MINIMUM_INTERVAL: Duration = Duration::from_millis(5);

/// The known stroke: the Grip Pen enters proximity hovering over
/// [`SCRIPTED_START`], presses and releases the lower barrel button while
/// hovering, touches down with pressure ramping to the maximum, moves right to
/// [`SCRIPTED_CORNER`] and down to [`SCRIPTED_END`] at
/// [`SCRIPTED_MOVE_PRESSURE`], lifts with pressure ramping to zero, hovers,
/// and leaves proximity.
#[must_use]
pub fn intuos5_scripted_stroke() -> Vec<ScriptedReport> {
    let hover = |(x, y): (u32, u32), stylus: bool| Intuos5PenState {
        x,
        y,
        distance: SCRIPTED_HOVER_DISTANCE,
        stylus,
        ..Intuos5PenState::default()
    };
    let touch = |(x, y): (u32, u32), pressure: u16| Intuos5PenState {
        x,
        y,
        pressure,
        ..Intuos5PenState::default()
    };
    let step = |phase, state| ScriptedReport {
        phase,
        report: Intuos5PenReport::Pen(state),
    };
    let mut script = vec![ScriptedReport {
        phase: "proximity-in",
        report: Intuos5PenReport::Enter {
            tool_id: INTUOS5_GRIP_PEN_TOOL_ID,
            serial: SCRIPTED_PEN_SERIAL,
        },
    }];
    script.extend((0..3).map(|_| step("hover", hover(SCRIPTED_START, false))));
    script.extend((0..3).map(|_| step("barrel-down", hover(SCRIPTED_START, true))));
    script.extend((0..2).map(|_| step("barrel-up", hover(SCRIPTED_START, false))));
    script.extend((1..=SCRIPTED_RAMP_STEPS).map(|index| {
        let pressure =
            u32::from(INTUOS5_MAX_PRESSURE) * u32::from(index) / u32::from(SCRIPTED_RAMP_STEPS);
        step(
            "pressure-ramp",
            touch(SCRIPTED_START, u16::try_from(pressure).unwrap_or(0)),
        )
    }));
    let (right, down) = SCRIPTED_LEG_STEPS;
    script.extend((1..=right).map(|index| {
        let x = SCRIPTED_START.0 + (SCRIPTED_CORNER.0 - SCRIPTED_START.0) * index / right;
        step(
            "move-right",
            touch((x, SCRIPTED_START.1), SCRIPTED_MOVE_PRESSURE),
        )
    }));
    script.extend((1..=down).map(|index| {
        let y = SCRIPTED_CORNER.1 + (SCRIPTED_END.1 - SCRIPTED_CORNER.1) * index / down;
        step(
            "move-down",
            touch((SCRIPTED_CORNER.0, y), SCRIPTED_MOVE_PRESSURE),
        )
    }));
    script.extend((1..=4_u16).rev().map(|quarter| {
        step(
            "pen-up",
            touch(SCRIPTED_END, SCRIPTED_MOVE_PRESSURE * (quarter - 1) / 4),
        )
    }));
    script.extend((0..2).map(|_| step("hover", hover(SCRIPTED_END, false))));
    script.push(ScriptedReport {
        phase: "proximity-out",
        report: Intuos5PenReport::Exit,
    });
    script
}

/// A stroke whose first pen packet is already pressing: the Grip Pen enters
/// proximity and its first packet touches [`SCRIPTED_START`] at
/// [`SCRIPTED_MOVE_PRESSURE`], with no hover before it. This is what a host
/// sees when a pen lands between two polls of a bridged tablet (one poll per
/// network round trip, ADR 0021), and what defeats a driver that takes the
/// first in-proximity pressure as the pen's zero. It then moves along the
/// same legs as [`intuos5_scripted_stroke`], lifts in one packet, hovers
/// and leaves proximity.
#[must_use]
pub fn intuos5_landing_stroke() -> Vec<ScriptedReport> {
    let touch = |(x, y): (u32, u32)| Intuos5PenState {
        x,
        y,
        pressure: SCRIPTED_MOVE_PRESSURE,
        ..Intuos5PenState::default()
    };
    let step = |phase, state| ScriptedReport {
        phase,
        report: Intuos5PenReport::Pen(state),
    };
    let mut script = vec![ScriptedReport {
        phase: "proximity-in",
        report: Intuos5PenReport::Enter {
            tool_id: INTUOS5_GRIP_PEN_TOOL_ID,
            serial: SCRIPTED_PEN_SERIAL,
        },
    }];
    script.push(step("landing", touch(SCRIPTED_START)));
    let (right, down) = SCRIPTED_LEG_STEPS;
    script.extend((1..=right).map(|index| {
        let x = SCRIPTED_START.0 + (SCRIPTED_CORNER.0 - SCRIPTED_START.0) * index / right;
        step("move-right", touch((x, SCRIPTED_START.1)))
    }));
    script.extend((1..=down).map(|index| {
        let y = SCRIPTED_CORNER.1 + (SCRIPTED_END.1 - SCRIPTED_CORNER.1) * index / down;
        step("move-down", touch((SCRIPTED_CORNER.0, y)))
    }));
    script.push(step(
        "hover",
        Intuos5PenState {
            x: SCRIPTED_END.0,
            y: SCRIPTED_END.1,
            distance: SCRIPTED_HOVER_DISTANCE,
            ..Intuos5PenState::default()
        },
    ));
    script.push(ScriptedReport {
        phase: "proximity-out",
        report: Intuos5PenReport::Exit,
    });
    script
}

/// A stroke shaped like a real hand on a bridged tablet, read once per
/// network round trip: the pen comes into range (in-range packets), enters
/// proximity as the KP-501E Grip Pen ([`INTUOS_PRO_GRIP_PEN_TOOL_ID`]),
/// descends while hovering, lands already pressing (between two polls),
/// draws with varying pressure and a held tilt, lifts into a hover, goes
/// out of range and leaves proximity. Every pen packet moves, as a hand
/// does. Unlike [`intuos5_scripted_stroke`] and [`intuos5_landing_stroke`]
/// it exercises the in-range packets, a tool id with its high bits and a
/// non-zero tilt.
#[must_use]
pub fn intuos5_realistic_stroke() -> Vec<ScriptedReport> {
    const TILT: (i8, i8) = (25, -11);
    let pen = |phase, x: u32, y: u32, pressure: u16, distance: u8| ScriptedReport {
        phase,
        report: Intuos5PenReport::Pen(Intuos5PenState {
            x,
            y,
            pressure,
            distance,
            tilt_x: TILT.0,
            tilt_y: TILT.1,
            ..Intuos5PenState::default()
        }),
    };
    let in_range = || ScriptedReport {
        phase: "in-range",
        report: Intuos5PenReport::InRange,
    };
    let (x0, y0) = SCRIPTED_START;
    let mut script = vec![in_range(), in_range()];
    script.push(ScriptedReport {
        phase: "proximity-in",
        report: Intuos5PenReport::Enter {
            tool_id: INTUOS_PRO_GRIP_PEN_TOOL_ID,
            serial: SCRIPTED_PEN_SERIAL,
        },
    });
    for (step, distance) in [40_u8, 28, 14].into_iter().enumerate() {
        let shift = 300 * u32::try_from(3 - step).unwrap_or(0);
        script.push(pen("descend", x0 - shift, y0 - shift, 0, distance));
    }
    let (right, down) = SCRIPTED_LEG_STEPS;
    let max = u32::from(INTUOS5_MAX_PRESSURE);
    for index in 0..=right {
        let x = x0 + (SCRIPTED_CORNER.0 - x0) * index / right;
        // 0.40 at the landing, rising to 0.78, as a pressing hand does.
        let pressure = max * (40 + 38 * index / right) / 100;
        let phase = if index == 0 { "landing" } else { "draw-right" };
        script.push(pen(
            phase,
            x,
            y0 + 40 * (index % 3),
            u16::try_from(pressure).unwrap_or(0),
            0,
        ));
    }
    for index in 1..=down {
        let y = SCRIPTED_CORNER.1 + (SCRIPTED_END.1 - SCRIPTED_CORNER.1) * index / down;
        let pressure = max * (78 - 20 * index / down) / 100;
        script.push(pen(
            "draw-down",
            SCRIPTED_CORNER.0 + 30 * (index % 2),
            y,
            u16::try_from(pressure).unwrap_or(0),
            0,
        ));
    }
    let (x1, y1) = SCRIPTED_END;
    for (step, distance) in [8_u8, 22, 38].into_iter().enumerate() {
        let shift = 250 * u32::try_from(step + 1).unwrap_or(0);
        script.push(pen("lift", x1 + shift, y1 + shift, 0, distance));
    }
    script.push(in_range());
    script.push(ScriptedReport {
        phase: "proximity-out",
        report: Intuos5PenReport::Exit,
    });
    script
}

/// `script` with the upper barrel button (`BTN_STYLUS2`) held in every pen
/// packet, as a real Intuos5 reported it for a whole session when its
/// user's finger rested on the side switch (owner capture, 2026-11-12
/// 12:39Z: bit 2 set in 900+ of 920 pen packets).
#[must_use]
pub fn with_upper_switch_held(script: Vec<ScriptedReport>) -> Vec<ScriptedReport> {
    script
        .into_iter()
        .map(|step| match step.report {
            Intuos5PenReport::Pen(state) => ScriptedReport {
                report: Intuos5PenReport::Pen(Intuos5PenState {
                    stylus2: true,
                    ..state
                }),
                ..step
            },
            _ => step,
        })
        .collect()
}

/// How long a [`intuos5_resting_stroke`] leaves its last packet standing
/// before the playback reports itself done (and a CLI smoke disconnects):
/// time for a lab to cut the session while the pen rests.
pub const RESTING_HOLD: Duration = Duration::from_secs(8);

/// A stroke that never ends: the Grip Pen enters proximity, hovers, lands
/// pressing and draws right; then, when `pressing`, it stays down at
/// [`SCRIPTED_CORNER`], otherwise it lifts into a hover there. No lift and
/// no exit report follow, so the session ends (or is cut) with the pen still
/// down or still in proximity, as when a Deck disconnects while its user's
/// pen rests on the tablet.
#[must_use]
pub fn intuos5_resting_stroke(pressing: bool) -> Vec<ScriptedReport> {
    let pen = |phase, (x, y): (u32, u32), pressure: u16, distance: u8| ScriptedReport {
        phase,
        report: Intuos5PenReport::Pen(Intuos5PenState {
            x,
            y,
            pressure,
            distance,
            ..Intuos5PenState::default()
        }),
    };
    let mut script = vec![ScriptedReport {
        phase: "proximity-in",
        report: Intuos5PenReport::Enter {
            tool_id: INTUOS_PRO_GRIP_PEN_TOOL_ID,
            serial: SCRIPTED_PEN_SERIAL,
        },
    }];
    script.extend((0..2).map(|_| pen("hover", SCRIPTED_START, 0, SCRIPTED_HOVER_DISTANCE)));
    let (right, _) = SCRIPTED_LEG_STEPS;
    script.extend((0..=right).map(|index| {
        let x = SCRIPTED_START.0 + (SCRIPTED_CORNER.0 - SCRIPTED_START.0) * index / right;
        pen(
            "draw-right",
            (x, SCRIPTED_START.1),
            SCRIPTED_MOVE_PRESSURE,
            0,
        )
    }));
    if !pressing {
        script.push(pen(
            "rest-hover",
            SCRIPTED_CORNER,
            0,
            SCRIPTED_HOVER_DISTANCE,
        ));
    }
    script
}

#[cfg(test)]
#[path = "wacom_intuos5_tests.rs"]
mod tests;
