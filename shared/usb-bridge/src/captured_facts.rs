// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Building [`CapturedDeviceFacts`] from descriptors a capture backend read
//! from the device itself, the same way on every platform (the macOS
//! `IOUSBHost` backend, the Windows `WinUSB` backend).

use crate::{
    CapturedDeviceFacts, DeviceSnapshot, InterfaceReportDescriptor,
    MAX_CONFIGURATION_DESCRIPTOR_BYTES, UsbDeviceId, UsbSpeed, parse_configuration_descriptor,
};

/// Bytes in a standard USB device descriptor.
pub const DEVICE_DESCRIPTOR_LENGTH: usize = 18;
const DESCRIPTOR_DEVICE: u8 = 0x01;
const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const CONFIGURATION_HEADER_LENGTH: usize = 9;

/// The total length a configuration descriptor header announces, checked
/// against the bridge bound before anything copies that many bytes.
///
/// # Errors
///
/// Returns a reason when the header is short, of the wrong type, or announces
/// an impossible or oversized length.
pub fn configuration_total_length(header: &[u8]) -> Result<usize, String> {
    if header.len() < 4 || header[1] != DESCRIPTOR_CONFIGURATION {
        return Err("the device returned no configuration descriptor header".to_owned());
    }
    let total = usize::from(u16::from_le_bytes([header[2], header[3]]));
    if total < CONFIGURATION_HEADER_LENGTH {
        return Err(format!(
            "configuration descriptor length {total} is too short"
        ));
    }
    if total > MAX_CONFIGURATION_DESCRIPTOR_BYTES {
        return Err(format!(
            "configuration descriptor length {total} exceeds the bridge limit \
             {MAX_CONFIGURATION_DESCRIPTOR_BYTES}"
        ));
    }
    Ok(total)
}

/// Builds the facts the shared engine verifies from descriptors read from the
/// captured device itself.
///
/// The device descriptor must still name `expected` (vendor, product): a
/// device that changed under the capture is refused here, before any
/// transfer. The shared engine re-runs the whole admission policy on the
/// result together with `report_descriptors`.
///
/// # Errors
///
/// Returns a reason when a descriptor is malformed, the identity changed, or
/// the speed is outside input bridge v1.
pub fn captured_facts_from_descriptors(
    device_descriptor: &[u8],
    configuration_descriptor: &[u8],
    speed: Option<UsbSpeed>,
    expected: (u16, u16),
    report_descriptors: Vec<InterfaceReportDescriptor>,
) -> Result<CapturedDeviceFacts, String> {
    if device_descriptor.len() < DEVICE_DESCRIPTOR_LENGTH
        || usize::from(device_descriptor[0]) < DEVICE_DESCRIPTOR_LENGTH
        || device_descriptor[1] != DESCRIPTOR_DEVICE
    {
        return Err("the captured device returned an invalid device descriptor".to_owned());
    }
    let id = UsbDeviceId {
        vendor_id: u16::from_le_bytes([device_descriptor[8], device_descriptor[9]]),
        product_id: u16::from_le_bytes([device_descriptor[10], device_descriptor[11]]),
        bcd_device: u16::from_le_bytes([device_descriptor[12], device_descriptor[13]]),
    };
    if (id.vendor_id, id.product_id) != expected {
        return Err(format!(
            "the captured device is {:04x}:{:04x}, not the classified {:04x}:{:04x}",
            id.vendor_id, id.product_id, expected.0, expected.1
        ));
    }
    let configuration = parse_configuration_descriptor(configuration_descriptor)
        .map_err(|error| format!("captured configuration descriptor: {error}"))?;
    let speed = speed.ok_or_else(|| "USB speed is outside input bridge v1".to_owned())?;
    Ok(CapturedDeviceFacts {
        snapshot: DeviceSnapshot {
            id,
            device_class: device_descriptor[4],
            configuration,
        },
        speed,
        report_descriptors,
    })
}

/// For every HID interface (alternate setting 0) of a raw configuration
/// descriptor, the interface number and the report descriptor length its HID
/// class descriptor announces — what a backend that reads descriptors with
/// control transfers must request.
#[must_use]
pub fn hid_report_lengths(configuration_descriptor: &[u8]) -> Vec<(u8, u16)> {
    const DESCRIPTOR_INTERFACE: u8 = 0x04;
    let mut out = Vec::new();
    let mut current: Option<u8> = None;
    let mut offset = 0_usize;
    while offset + 2 <= configuration_descriptor.len() {
        let length = usize::from(configuration_descriptor[offset]);
        if length < 2 || offset + length > configuration_descriptor.len() {
            break;
        }
        let descriptor = &configuration_descriptor[offset..offset + length];
        match descriptor[1] {
            DESCRIPTOR_INTERFACE if length >= 9 => {
                current = (descriptor[3] == 0 && descriptor[5] == crate::USB_CLASS_HID)
                    .then_some(descriptor[2]);
            }
            crate::DESCRIPTOR_HID => {
                if let (Some(interface), Ok(report_length)) = (
                    current.take(),
                    crate::parse_hid_class_descriptor(descriptor),
                ) {
                    out.push((interface, report_length));
                }
            }
            _ => {}
        }
        offset += length;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ARCEN_LAB_PRODUCT_ID, ARCEN_LAB_VENDOR_ID, SyntheticTabletDevice};

    const LAB: (u16, u16) = (ARCEN_LAB_VENDOR_ID, ARCEN_LAB_PRODUCT_ID);

    fn reports() -> Vec<InterfaceReportDescriptor> {
        vec![InterfaceReportDescriptor {
            interface: 0,
            descriptor: SyntheticTabletDevice::report_descriptor().to_vec(),
        }]
    }

    #[test]
    fn descriptors_read_back_become_the_facts_the_engine_admits() {
        let facts = captured_facts_from_descriptors(
            &SyntheticTabletDevice::device_descriptor(),
            &SyntheticTabletDevice::configuration_descriptor(),
            Some(UsbSpeed::Full),
            LAB,
            reports(),
        )
        .expect("facts");
        assert_eq!(
            facts,
            SyntheticTabletDevice::captured_facts().expect("fixture")
        );
        assert!(crate::verify_captured_tablet(&facts).is_ok());
    }

    #[test]
    fn hid_interfaces_announce_their_report_lengths() {
        let config = SyntheticTabletDevice::configuration_descriptor();
        let report = SyntheticTabletDevice::report_descriptor();
        assert_eq!(
            hid_report_lengths(&config),
            vec![(0, u16::try_from(report.len()).expect("fits"))]
        );
        assert_eq!(
            hid_report_lengths(crate::INTUOS5_CONFIGURATION_DESCRIPTOR)
                .iter()
                .map(|(interface, _)| *interface)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(hid_report_lengths(&config[..12]).is_empty());
        assert!(hid_report_lengths(&[]).is_empty());
    }

    #[test]
    fn a_changed_device_a_bad_descriptor_or_speed_is_refused() {
        let device = SyntheticTabletDevice::device_descriptor();
        let config = SyntheticTabletDevice::configuration_descriptor();
        let mut other = device;
        other[10] ^= 0xff;
        let refused = |device: &[u8], config: &[u8], speed| {
            captured_facts_from_descriptors(device, config, speed, LAB, reports()).is_err()
        };
        assert!(refused(&other, &config, Some(UsbSpeed::Full)));
        assert!(refused(&device[..17], &config, Some(UsbSpeed::Full)));
        assert!(refused(&device, &config[..20], Some(UsbSpeed::Full)));
        assert!(refused(&device, &config, None));
        assert_eq!(configuration_total_length(&config), Ok(config.len()));
        assert!(configuration_total_length(&[9, 2, 8, 0]).is_err());
        assert!(configuration_total_length(&[9, 2, 0x01, 0x10]).is_err());
        assert!(configuration_total_length(&[18, 1, 0, 2]).is_err());
    }
}
