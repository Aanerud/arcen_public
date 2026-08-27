use crate::{SetupPacket, UsbDeviceId};

/// Reserved invalid VID used only by the lab fixture; never a production USB
/// identity or USB-IF assignment claim.
pub const ARCEN_LAB_VENDOR_ID: u16 = 0xffff;
/// Lab-only product identifier.
pub const ARCEN_LAB_PRODUCT_ID: u16 = 0xa2ce;

const REQUEST_GET_STATUS: u8 = 0x00;
const REQUEST_CLEAR_FEATURE: u8 = 0x01;
const REQUEST_SET_FEATURE: u8 = 0x03;
const REQUEST_SET_ADDRESS: u8 = 0x05;
const REQUEST_GET_DESCRIPTOR: u8 = 0x06;
const REQUEST_GET_CONFIGURATION: u8 = 0x08;
const REQUEST_SET_CONFIGURATION: u8 = 0x09;
const HID_REQUEST_GET_IDLE: u8 = 0x02;
const HID_REQUEST_GET_PROTOCOL: u8 = 0x03;
const HID_REQUEST_SET_IDLE: u8 = 0x0a;
const HID_REQUEST_SET_PROTOCOL: u8 = 0x0b;

const DESCRIPTOR_DEVICE: u8 = 0x01;
const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const DESCRIPTOR_STRING: u8 = 0x03;
const DESCRIPTOR_HID: u8 = 0x21;
const DESCRIPTOR_REPORT: u8 = 0x22;

const REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x0d, // Usage Page (Digitizers)
    0x09, 0x02, // Usage (Pen)
    0xa1, 0x01, // Collection (Application)
    0x85, 0x01, // Report ID 1
    0x09, 0x20, // Usage (Stylus)
    0xa1, 0x00, // Collection (Physical)
    0x09, 0x42, // Tip Switch
    0x09, 0x32, // In Range
    0x09, 0x45, // Eraser
    0x09, 0x44, // Barrel Switch
    0x09, 0x5a, // Secondary Barrel Switch
    0x15, 0x00, // Logical Minimum 0
    0x25, 0x01, // Logical Maximum 1
    0x75, 0x01, // Report Size 1
    0x95, 0x05, // Report Count 5
    0x81, 0x02, // Input (Data, Variable, Absolute)
    0x75, 0x03, // Report Size 3
    0x95, 0x01, // Report Count 1
    0x81, 0x03, // Input (Constant)
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x30, // Usage X
    0x09, 0x31, // Usage Y
    0x16, 0x00, 0x00, // Logical Minimum 0
    0x26, 0xff, 0xff, // Logical Maximum 65535
    0x75, 0x10, // Report Size 16
    0x95, 0x02, // Report Count 2
    0x81, 0x02, // Input (Data, Variable, Absolute)
    0x05, 0x0d, // Usage Page (Digitizers)
    0x09, 0x30, // Tip Pressure
    0x16, 0x00, 0x00, // Logical Minimum 0
    0x26, 0xff, 0x03, // Logical Maximum 1023
    0x75, 0x10, // Report Size 16
    0x95, 0x01, // Report Count 1
    0x81, 0x02, // Input (Data, Variable, Absolute)
    0x09, 0x3d, // X Tilt
    0x09, 0x3e, // Y Tilt
    0x15, 0xa6, // Logical Minimum -90
    0x25, 0x5a, // Logical Maximum 90
    0x75, 0x08, // Report Size 8
    0x95, 0x02, // Report Count 2
    0x81, 0x02, // Input (Data, Variable, Absolute)
    0xc0, // End Collection
    0xc0, // End Collection
];

/// One normalized pen state converted into the synthetic tablet report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenSample {
    pub x: f64,
    pub y: f64,
    pub pressure: f32,
    pub tilt_x_degrees: f32,
    pub tilt_y_degrees: f32,
    pub switches: PenSwitches,
}

/// One boolean switch encoded in the synthetic report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenSwitch {
    Touching,
    InRange,
    Eraser,
    Barrel,
    SecondaryBarrel,
}

/// Compact pen switch state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PenSwitches(u8);

impl PenSwitches {
    /// Returns a copy with one switch set or cleared.
    #[must_use]
    pub const fn with(mut self, switch: PenSwitch, enabled: bool) -> Self {
        let mask = match switch {
            PenSwitch::Touching => 1 << 0,
            PenSwitch::InRange => 1 << 1,
            PenSwitch::Eraser => 1 << 2,
            PenSwitch::Barrel => 1 << 3,
            PenSwitch::SecondaryBarrel => 1 << 4,
        };
        if enabled {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
        self
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl PenSample {
    /// Encodes one full-state report. Coordinates and pressure are clamped;
    /// non-finite values become zero rather than entering the USB boundary.
    #[must_use]
    pub fn encode_report(self) -> [u8; 10] {
        let flags = self.switches.bits();
        let x = unit_to_u16(self.x);
        let y = unit_to_u16(self.y);
        let pressure = unit_to_pressure(self.pressure);
        let tilt_x = tilt_to_i8(self.tilt_x_degrees);
        let tilt_y = tilt_to_i8(self.tilt_y_degrees);
        let [x_low, x_high] = x.to_le_bytes();
        let [y_low, y_high] = y.to_le_bytes();
        let [pressure_low, pressure_high] = pressure.to_le_bytes();
        let [horizontal_tilt_byte] = tilt_x.to_le_bytes();
        let [vertical_tilt_byte] = tilt_y.to_le_bytes();
        [
            1,
            flags,
            x_low,
            x_high,
            y_low,
            y_high,
            pressure_low,
            pressure_high,
            horizontal_tilt_byte,
            vertical_tilt_byte,
        ]
    }
}

/// Standard control response from the synthetic device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlResponse {
    Ack,
    Data(Vec<u8>),
    Stall,
}

/// Independently authored USB HID tablet used only by the lab bridge.
#[derive(Debug, Clone)]
pub struct SyntheticTabletDevice {
    address: u8,
    configuration: u8,
    idle_rate: u8,
    protocol: u8,
}

impl Default for SyntheticTabletDevice {
    fn default() -> Self {
        Self {
            address: 0,
            configuration: 0,
            idle_rate: 0,
            protocol: 1,
        }
    }
}

impl SyntheticTabletDevice {
    #[must_use]
    pub const fn identity() -> UsbDeviceId {
        UsbDeviceId {
            vendor_id: ARCEN_LAB_VENDOR_ID,
            product_id: ARCEN_LAB_PRODUCT_ID,
            bcd_device: 0x0100,
        }
    }

    #[must_use]
    pub const fn address(&self) -> u8 {
        self.address
    }

    #[must_use]
    pub const fn configuration(&self) -> u8 {
        self.configuration
    }

    #[must_use]
    pub fn device_descriptor() -> [u8; 18] {
        let [vid_lo, vid_hi] = ARCEN_LAB_VENDOR_ID.to_le_bytes();
        let [pid_lo, pid_hi] = ARCEN_LAB_PRODUCT_ID.to_le_bytes();
        [
            18, 1, 0x00, 0x02, 0, 0, 0, 64, vid_lo, vid_hi, pid_lo, pid_hi, 0x00, 0x01, 1, 2, 0, 1,
        ]
    }

    #[must_use]
    pub fn configuration_descriptor() -> Vec<u8> {
        let report_length = u16::try_from(REPORT_DESCRIPTOR.len()).unwrap_or(u16::MAX);
        let [report_lo, report_hi] = report_length.to_le_bytes();
        vec![
            9, 2, 34, 0, 1, 1, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 1, 3, 0, 0, 0, // HID interface
            9, 0x21, 0x11, 0x01, 0, 1, 0x22, report_lo, report_hi, // HID
            7, 5, 0x81, 3, 16, 0, 1, // interrupt IN
        ]
    }

    #[must_use]
    pub const fn report_descriptor() -> &'static [u8] {
        REPORT_DESCRIPTOR
    }

    /// Handles the bounded standard/class control requests needed by the lab
    /// HID tablet. Unsupported requests stall explicitly.
    pub fn handle_control(&mut self, setup: SetupPacket) -> ControlResponse {
        let direction_in = setup.request_type & 0x80 != 0;
        let request_kind = (setup.request_type >> 5) & 0x03;
        let recipient = setup.request_type & 0x1f;
        if request_kind == 0 {
            return self.handle_standard(setup, direction_in, recipient);
        }
        if request_kind == 1 && recipient == 1 {
            return self.handle_hid_class(setup, direction_in);
        }
        ControlResponse::Stall
    }

    fn handle_standard(
        &mut self,
        setup: SetupPacket,
        direction_in: bool,
        recipient: u8,
    ) -> ControlResponse {
        match (setup.request, direction_in, recipient) {
            (REQUEST_GET_STATUS, true, _) => truncate(vec![0, 0], setup.length),
            (REQUEST_CLEAR_FEATURE | REQUEST_SET_FEATURE, false, _) => ControlResponse::Ack,
            (REQUEST_SET_ADDRESS, false, 0) if setup.value <= 127 => {
                self.address = u8::try_from(setup.value).unwrap_or_default();
                ControlResponse::Ack
            }
            (REQUEST_GET_DESCRIPTOR, true, _) => Self::descriptor(setup),
            (REQUEST_GET_CONFIGURATION, true, 0) => {
                truncate(vec![self.configuration], setup.length)
            }
            (REQUEST_SET_CONFIGURATION, false, 0) if setup.value <= 1 => {
                self.configuration = u8::try_from(setup.value).unwrap_or_default();
                ControlResponse::Ack
            }
            _ => ControlResponse::Stall,
        }
    }

    fn handle_hid_class(&mut self, setup: SetupPacket, direction_in: bool) -> ControlResponse {
        match (setup.request, direction_in) {
            (HID_REQUEST_GET_IDLE, true) => truncate(vec![self.idle_rate], setup.length),
            (HID_REQUEST_GET_PROTOCOL, true) => truncate(vec![self.protocol], setup.length),
            (HID_REQUEST_SET_IDLE, false) => {
                self.idle_rate = setup.value.to_be_bytes()[0];
                ControlResponse::Ack
            }
            (HID_REQUEST_SET_PROTOCOL, false) if setup.value <= 1 => {
                self.protocol = u8::try_from(setup.value).unwrap_or_default();
                ControlResponse::Ack
            }
            _ => ControlResponse::Stall,
        }
    }

    fn descriptor(setup: SetupPacket) -> ControlResponse {
        let data = match (setup.descriptor_type(), setup.descriptor_index()) {
            (DESCRIPTOR_DEVICE, 0) => Self::device_descriptor().to_vec(),
            (DESCRIPTOR_CONFIGURATION, 0) => Self::configuration_descriptor(),
            (DESCRIPTOR_STRING, 0) => vec![4, 3, 0x09, 0x04],
            (DESCRIPTOR_STRING, 1) => string_descriptor("Arcen"),
            (DESCRIPTOR_STRING, 2) => string_descriptor("USB Bridge Lab Tablet"),
            (DESCRIPTOR_HID, 0) => Self::configuration_descriptor()[18..27].to_vec(),
            (DESCRIPTOR_REPORT, 0) => REPORT_DESCRIPTOR.to_vec(),
            _ => return ControlResponse::Stall,
        };
        truncate(data, setup.length)
    }
}

fn truncate(mut data: Vec<u8>, requested: u16) -> ControlResponse {
    data.truncate(usize::from(requested));
    ControlResponse::Data(data)
}

fn string_descriptor(value: &str) -> Vec<u8> {
    let utf16: Vec<_> = value.encode_utf16().collect();
    let total = 2_usize.saturating_add(utf16.len().saturating_mul(2));
    let mut bytes = Vec::with_capacity(total);
    bytes.push(u8::try_from(total).unwrap_or(u8::MAX));
    bytes.push(DESCRIPTOR_STRING);
    for unit in utf16 {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn unit_to_u16(value: f64) -> u16 {
    if !value.is_finite() {
        return 0;
    }
    (value.clamp(0.0, 1.0) * f64::from(u16::MAX)).round() as u16
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn unit_to_pressure(value: f32) -> u16 {
    if !value.is_finite() {
        return 0;
    }
    (value.clamp(0.0, 1.0) * 1023.0).round() as u16
}

#[allow(clippy::cast_possible_truncation)]
fn tilt_to_i8(value: f32) -> i8 {
    if !value.is_finite() {
        return 0;
    }
    value.clamp(-90.0, 90.0).round() as i8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_configuration_descriptor;

    #[test]
    fn fixture_configuration_passes_the_strict_parser() {
        let parsed =
            parse_configuration_descriptor(&SyntheticTabletDevice::configuration_descriptor())
                .unwrap();
        assert_eq!(parsed.interfaces.len(), 1);
        assert_eq!(parsed.interfaces[0].class, 3);
        assert_eq!(parsed.interfaces[0].endpoints.len(), 1);
    }

    #[test]
    fn fixture_answers_device_and_report_descriptors() {
        let mut device = SyntheticTabletDevice::default();
        let device_descriptor = device.handle_control(SetupPacket {
            request_type: 0x80,
            request: REQUEST_GET_DESCRIPTOR,
            value: u16::from(DESCRIPTOR_DEVICE) << 8,
            index: 0,
            length: 64,
        });
        assert_eq!(
            device_descriptor,
            ControlResponse::Data(SyntheticTabletDevice::device_descriptor().to_vec())
        );
        let report_descriptor = device.handle_control(SetupPacket {
            request_type: 0x81,
            request: REQUEST_GET_DESCRIPTOR,
            value: u16::from(DESCRIPTOR_REPORT) << 8,
            index: 0,
            length: u16::MAX,
        });
        assert_eq!(
            report_descriptor,
            ControlResponse::Data(REPORT_DESCRIPTOR.to_vec())
        );
    }

    #[test]
    fn pen_report_clamps_and_preserves_edges() {
        let report = PenSample {
            x: 1.5,
            y: -1.0,
            pressure: 0.5,
            tilt_x_degrees: 100.0,
            tilt_y_degrees: -100.0,
            switches: PenSwitches::default()
                .with(PenSwitch::InRange, true)
                .with(PenSwitch::Touching, true)
                .with(PenSwitch::Barrel, true),
        }
        .encode_report();
        assert_eq!(report[0], 1);
        assert_eq!(report[1] & 0b1011, 0b1011);
        assert_eq!(u16::from_le_bytes([report[2], report[3]]), u16::MAX);
        assert_eq!(u16::from_le_bytes([report[4], report[5]]), 0);
        assert_eq!(i8::from_le_bytes([report[8]]), 90);
        assert_eq!(i8::from_le_bytes([report[9]]), -90);
    }
}
