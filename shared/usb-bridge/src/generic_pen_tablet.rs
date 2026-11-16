// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! [`GenericPenTablet`]: an emulated pen tablet that no host has a dedicated
//! reader or driver for, so lab runs prove the "any tablet" path (Q-16) end
//! to end: the Deck bridges it, a Linux Pier's `hid-generic` binds it, a
//! macOS Pier reads it with the generic reader.
//!
//! Its identity is `1209:000F`, the pid.codes *Test PID*, which "anyone may
//! assign to their device while they're testing in-house" and which must
//! never ship on a device (<https://pid.codes/1209/000F/>). It is not unique:
//! use it in lab builds only. Its one HID interface declares
//! [`crate::descriptor_fixtures::HUION_STYLE_PEN`], an authored standard
//! digitizer pen, and needs no mode switch.

use crate::descriptor_fixtures::HUION_STYLE_PEN;
use crate::{
    AlternateSetting, CapturedDeviceFacts, ControlResponse, DescriptorError, DeviceSnapshot,
    EmulatedUsbFunction, EndpointAddress, InterfaceNumber, InterfaceReportDescriptor, SetupPacket,
    UsbDeviceId, UsbSpeed, parse_configuration_descriptor,
};

/// pid.codes' vendor id.
pub const GENERIC_PEN_VENDOR_ID: u16 = 0x1209;
/// pid.codes' Test PID: in-house testing only, never on a shipped device.
pub const GENERIC_PEN_PRODUCT_ID: u16 = 0x000F;
/// The pen's interrupt IN endpoint.
pub const GENERIC_PEN_IN: EndpointAddress = EndpointAddress(0x81);
/// The pen report's id.
pub const GENERIC_PEN_REPORT_ID: u8 = 0x0A;
/// Largest X.
pub const GENERIC_PEN_MAX_X: u16 = 50_800;
/// Largest Y.
pub const GENERIC_PEN_MAX_Y: u16 = 31_836;
/// Largest pressure.
pub const GENERIC_PEN_MAX_PRESSURE: u16 = 8_191;

/// One pen state as the emulated tablet reports it.
// Independent switches of one report.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenericPenState {
    pub x: u16,
    pub y: u16,
    pub pressure: u16,
    /// Degrees, ±60.
    pub tilt_x: i8,
    pub tilt_y: i8,
    pub tip: bool,
    pub barrel: bool,
    pub secondary_barrel: bool,
    pub in_range: bool,
}

impl GenericPenState {
    /// The input report: `[10, switches, x, y, pressure, tilt x, tilt y]`,
    /// values clamped into the descriptor's ranges.
    #[must_use]
    pub fn encode(self) -> [u8; 10] {
        let switches = u8::from(self.tip)
            | (u8::from(self.barrel) << 1)
            | (u8::from(self.secondary_barrel) << 2)
            | (u8::from(self.in_range) << 5);
        let [x_low, x_high] = self.x.min(GENERIC_PEN_MAX_X).to_le_bytes();
        let [y_low, y_high] = self.y.min(GENERIC_PEN_MAX_Y).to_le_bytes();
        let [p_low, p_high] = self.pressure.min(GENERIC_PEN_MAX_PRESSURE).to_le_bytes();
        let [tilt_x] = self.tilt_x.clamp(-60, 60).to_le_bytes();
        let [tilt_y] = self.tilt_y.clamp(-60, 60).to_le_bytes();
        [
            GENERIC_PEN_REPORT_ID,
            switches,
            x_low,
            x_high,
            y_low,
            y_high,
            p_low,
            p_high,
            tilt_x,
            tilt_y,
        ]
    }
}

/// One step of a scripted stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenericScriptedReport {
    /// What the step shows, for logs.
    pub phase: &'static str,
    pub state: GenericPenState,
}

/// Where the scripted stroke touches down.
pub const GENERIC_SCRIPTED_START: (u16, u16) = (12_000, 9_000);
/// Where its horizontal leg ends.
pub const GENERIC_SCRIPTED_CORNER: (u16, u16) = (38_000, 9_000);
/// Where it lifts off.
pub const GENERIC_SCRIPTED_END: (u16, u16) = (38_000, 22_000);
/// Pressure while it moves (half the range).
pub const GENERIC_SCRIPTED_PRESSURE: u16 = 4_096;

/// A stroke for the emulated pen: hover in, land with a pressure ramp, an
/// L-shaped drag with the barrel button held on the second leg and the tilt
/// sweeping, lift, hover, leave proximity.
#[must_use]
pub fn generic_pen_scripted_stroke() -> Vec<GenericScriptedReport> {
    let (start_x, start_y) = GENERIC_SCRIPTED_START;
    let (corner_x, _) = GENERIC_SCRIPTED_CORNER;
    let (_, end_y) = GENERIC_SCRIPTED_END;
    let hover = |x, y| GenericPenState {
        x,
        y,
        in_range: true,
        ..GenericPenState::default()
    };
    let mut steps = Vec::new();
    for _ in 0..5 {
        steps.push(GenericScriptedReport {
            phase: "hover in",
            state: hover(start_x, start_y),
        });
    }
    for step in 1..=16_u16 {
        steps.push(GenericScriptedReport {
            phase: "touch down",
            state: GenericPenState {
                pressure: GENERIC_SCRIPTED_PRESSURE / 16 * step,
                tip: true,
                ..hover(start_x, start_y)
            },
        });
    }
    let legs = 40_u16;
    for step in 1..=legs {
        steps.push(GenericScriptedReport {
            phase: "horizontal",
            state: GenericPenState {
                pressure: GENERIC_SCRIPTED_PRESSURE,
                tip: true,
                ..hover(start_x + (corner_x - start_x) / legs * step, start_y)
            },
        });
    }
    for step in 1..=legs {
        let sweep = i8::try_from(i32::from(step) * 120 / i32::from(legs) - 60).unwrap_or(0);
        steps.push(GenericScriptedReport {
            phase: "vertical",
            state: GenericPenState {
                pressure: GENERIC_SCRIPTED_PRESSURE,
                tip: true,
                barrel: true,
                tilt_x: sweep,
                tilt_y: -sweep,
                ..hover(corner_x, start_y + (end_y - start_y) / legs * step)
            },
        });
    }
    for step in (0..8_u16).rev() {
        steps.push(GenericScriptedReport {
            phase: "lift",
            state: GenericPenState {
                pressure: GENERIC_SCRIPTED_PRESSURE / 8 * step,
                tip: step > 0,
                ..hover(corner_x, end_y)
            },
        });
    }
    for _ in 0..5 {
        steps.push(GenericScriptedReport {
            phase: "hover out",
            state: hover(corner_x, end_y),
        });
    }
    steps.push(GenericScriptedReport {
        phase: "leave",
        state: GenericPenState {
            x: corner_x,
            y: end_y,
            ..GenericPenState::default()
        },
    });
    steps
}

const REQUEST_GET_STATUS: u8 = 0x00;
const REQUEST_CLEAR_FEATURE: u8 = 0x01;
const REQUEST_SET_FEATURE: u8 = 0x03;
const REQUEST_SET_ADDRESS: u8 = 0x05;
const REQUEST_GET_DESCRIPTOR: u8 = 0x06;
const REQUEST_GET_CONFIGURATION: u8 = 0x08;
const REQUEST_SET_CONFIGURATION: u8 = 0x09;
const REQUEST_GET_INTERFACE: u8 = 0x0A;
const REQUEST_SET_INTERFACE: u8 = 0x0B;
const HID_GET_REPORT: u8 = 0x01;
const HID_GET_IDLE: u8 = 0x02;
const HID_GET_PROTOCOL: u8 = 0x03;
const HID_SET_IDLE: u8 = 0x0A;
const HID_SET_PROTOCOL: u8 = 0x0B;
const DESCRIPTOR_DEVICE: u8 = 0x01;
const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const DESCRIPTOR_STRING: u8 = 0x03;
const DESCRIPTOR_HID: u8 = 0x21;
const DESCRIPTOR_REPORT: u8 = 0x22;

/// The emulated generic pen tablet (see the module docs).
#[derive(Debug, Clone)]
pub struct GenericPenTablet {
    descriptor: &'static [u8],
    configuration: u8,
    idle_rate: u8,
    protocol: u8,
    last: GenericPenState,
}

impl Default for GenericPenTablet {
    fn default() -> Self {
        Self::with_descriptor(HUION_STYLE_PEN)
    }
}

impl GenericPenTablet {
    #[must_use]
    pub const fn identity() -> UsbDeviceId {
        UsbDeviceId {
            vendor_id: GENERIC_PEN_VENDOR_ID,
            product_id: GENERIC_PEN_PRODUCT_ID,
            bcd_device: 0x0100,
        }
    }

    #[must_use]
    pub fn device_descriptor() -> [u8; 18] {
        let [vid_lo, vid_hi] = GENERIC_PEN_VENDOR_ID.to_le_bytes();
        let [pid_lo, pid_hi] = GENERIC_PEN_PRODUCT_ID.to_le_bytes();
        [
            18, 1, 0x00, 0x02, 0, 0, 0, 64, vid_lo, vid_hi, pid_lo, pid_hi, 0x00, 0x01, 1, 2, 0, 1,
        ]
    }

    /// The same device declaring another report descriptor (a test can
    /// emulate any pen layout; [`GenericPenState::encode`] fits only the
    /// default one).
    #[must_use]
    pub const fn with_descriptor(descriptor: &'static [u8]) -> Self {
        Self {
            descriptor,
            configuration: 0,
            idle_rate: 0,
            protocol: 1,
            last: GenericPenState {
                x: 0,
                y: 0,
                pressure: 0,
                tilt_x: 0,
                tilt_y: 0,
                tip: false,
                barrel: false,
                secondary_barrel: false,
                in_range: false,
            },
        }
    }

    #[must_use]
    pub fn configuration_descriptor(&self) -> Vec<u8> {
        let [report_lo, report_hi] = u16::try_from(self.descriptor.len())
            .unwrap_or(u16::MAX)
            .to_le_bytes();
        vec![
            9, 2, 34, 0, 1, 1, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 1, 3, 0, 0, 0, // HID interface, no boot protocol
            9, 0x21, 0x11, 0x01, 0, 1, 0x22, report_lo, report_hi, // HID 1.11
            7, 5, 0x81, 3, 16, 0, 2, // interrupt IN 1, 16 bytes, 2 ms
        ]
    }

    /// Remembers the state a `GET_REPORT(Input)` answers with.
    pub fn set_state(&mut self, state: GenericPenState) {
        self.last = state;
    }

    fn descriptor(&self, setup: SetupPacket) -> ControlResponse {
        let data = match (setup.descriptor_type(), setup.descriptor_index()) {
            (DESCRIPTOR_DEVICE, 0) => Self::device_descriptor().to_vec(),
            (DESCRIPTOR_CONFIGURATION, 0) => self.configuration_descriptor(),
            (DESCRIPTOR_STRING, 0) => vec![4, 3, 0x09, 0x04],
            (DESCRIPTOR_STRING, 1) => string_descriptor("Arcen lab"),
            (DESCRIPTOR_STRING, 2) => string_descriptor("Generic pen (lab, pid.codes test PID)"),
            (DESCRIPTOR_HID, 0) => self.configuration_descriptor()[18..27].to_vec(),
            (DESCRIPTOR_REPORT, 0) => self.descriptor.to_vec(),
            _ => return ControlResponse::Stall,
        };
        truncate(data, setup.length)
    }
}

impl EmulatedUsbFunction for GenericPenTablet {
    fn captured_facts(&self) -> Result<CapturedDeviceFacts, DescriptorError> {
        Ok(CapturedDeviceFacts {
            snapshot: DeviceSnapshot {
                id: Self::identity(),
                device_class: 0,
                configuration: parse_configuration_descriptor(&self.configuration_descriptor())?,
            },
            speed: UsbSpeed::Full,
            report_descriptors: vec![InterfaceReportDescriptor {
                interface: 0,
                descriptor: self.descriptor.to_vec(),
            }],
        })
    }

    fn control(&mut self, setup: SetupPacket, _data: &[u8]) -> ControlResponse {
        let direction_in = setup.request_type & 0x80 != 0;
        let kind = (setup.request_type >> 5) & 0x03;
        let recipient = setup.request_type & 0x1F;
        match (kind, setup.request, direction_in, recipient) {
            (0, REQUEST_GET_STATUS, true, _) => truncate(vec![0, 0], setup.length),
            (0, REQUEST_CLEAR_FEATURE | REQUEST_SET_FEATURE, false, _)
            | (0, REQUEST_SET_ADDRESS, false, 0) => ControlResponse::Ack,
            (0, REQUEST_GET_DESCRIPTOR, true, _) => self.descriptor(setup),
            (0, REQUEST_GET_CONFIGURATION, true, 0) => {
                truncate(vec![self.configuration], setup.length)
            }
            (0, REQUEST_SET_CONFIGURATION, false, 0) if setup.value <= 1 => {
                self.configuration = u8::try_from(setup.value).unwrap_or_default();
                ControlResponse::Ack
            }
            (0, REQUEST_GET_INTERFACE, true, 1) => truncate(vec![0], setup.length),
            (0, REQUEST_SET_INTERFACE, false, 1) if setup.value == 0 => ControlResponse::Ack,
            (1, HID_GET_REPORT, true, 1) if setup.value == 0x010A => {
                truncate(self.last.encode().to_vec(), setup.length)
            }
            (1, HID_GET_IDLE, true, 1) => truncate(vec![self.idle_rate], setup.length),
            (1, HID_GET_PROTOCOL, true, 1) => truncate(vec![self.protocol], setup.length),
            (1, HID_SET_IDLE, false, 1) => {
                self.idle_rate = setup.value.to_be_bytes()[0];
                ControlResponse::Ack
            }
            (1, HID_SET_PROTOCOL, false, 1) if setup.value <= 1 => {
                self.protocol = u8::try_from(setup.value).unwrap_or_default();
                ControlResponse::Ack
            }
            _ => ControlResponse::Stall,
        }
    }

    fn primary_interrupt_in(&self) -> EndpointAddress {
        GENERIC_PEN_IN
    }

    fn has_interrupt_in(&self, endpoint: EndpointAddress) -> bool {
        endpoint == GENERIC_PEN_IN
    }

    fn has_alternate(&self, interface: InterfaceNumber, alternate: AlternateSetting) -> bool {
        interface.0 == 0 && alternate.0 == 0
    }
}

fn truncate(mut data: Vec<u8>, requested: u16) -> ControlResponse {
    data.truncate(usize::from(requested));
    ControlResponse::Data(data)
}

fn string_descriptor(value: &str) -> Vec<u8> {
    let utf16: Vec<u16> = value.encode_utf16().collect();
    let total = 2 + utf16.len() * 2;
    let mut bytes = Vec::with_capacity(total);
    bytes.push(u8::try_from(total).unwrap_or(u8::MAX));
    bytes.push(DESCRIPTOR_STRING);
    for unit in utf16 {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GenericPenReader, HostPenRoute, UsbExporter, generic_pen_layout, host_pen_route};

    #[test]
    fn the_emulated_generic_pen_is_admitted_and_routed_to_the_generic_reader() {
        let facts = GenericPenTablet::default().captured_facts().unwrap();
        let profile = crate::authorize_hard_usb_tablet(
            facts.snapshot.device_class,
            &facts.report_descriptors,
        )
        .unwrap();
        assert_eq!(profile.capabilities.pressure_levels(), Some(8192));
        let route =
            host_pen_route(GenericPenTablet::identity(), &facts.report_descriptors).unwrap();
        assert!(matches!(route, HostPenRoute::Generic { interface: 0, .. }));
        let (device, _handle) =
            crate::SyntheticDevice::emulate(GenericPenTablet::default()).unwrap();
        assert!(
            UsbExporter::new(Box::new(device)).is_ok(),
            "the Deck admits it"
        );
    }

    #[test]
    fn the_scripted_stroke_reads_back_through_the_generic_reader() {
        let mut reader = GenericPenReader::new(generic_pen_layout(HUION_STYLE_PEN).unwrap());
        let samples: Vec<_> = generic_pen_scripted_stroke()
            .iter()
            .filter_map(|step| reader.read(&step.state.encode()))
            .collect();
        assert!(
            samples
                .iter()
                .any(|s| s.touching && (s.pressure - 0.5).abs() < 0.01)
        );
        assert!(samples.iter().any(|s| s.barrel_buttons == 1));
        assert!(samples.iter().any(|s| s.tilt_x_degrees <= -50.0));
        assert!(samples.iter().any(|s| s.tilt_x_degrees >= 50.0));
        let last = samples.last().unwrap();
        assert!(!last.in_proximity);
        assert_eq!(
            (last.device_x, last.device_y),
            (
                u32::from(GENERIC_SCRIPTED_END.0),
                u32::from(GENERIC_SCRIPTED_END.1)
            )
        );
    }
}
