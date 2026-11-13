// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The `IOUSBHost` backend, linked behind the `iousbhost` feature
//! (ADR 0019).
//!
//! A helper built with the feature captures with `IOUSBHost` ([`capture`]):
//! the tablet is chosen from the I/O Registry alone, captured with
//! `DeviceCapture`, and bridged through the same shared engine the libusb
//! backend uses (`PhysicalDevice::bridge_iousbhost`). The feature also adds
//! crash recovery from the backend's captured marker at daemon start and a
//! read-only listing of what the backend sees. Default and release builds do
//! not contain this module and capture with libusb.

use crate::capture::{PhysicalDevice, NO_TABLET_ATTACHED};
use arcen_usb_bridge::{
    authorize_hard_usb_tablet, declares_any_digitizer, HardUsbTabletDenial,
    InterfaceReportDescriptor, PenTabletProfile, USB_CLASS_HID,
};
use arcen_usb_iousbhost::{
    hid_descriptors_for, CaptureTarget, MarkerRead, MarkerStore, RecoveryReport, RegistryHid,
    RegistryUsbDevice,
};

/// The attached pen tablet a capture is aimed at, as the registry shows it.
#[derive(Debug)]
pub struct ChosenTablet {
    pub device: RegistryUsbDevice,
    pub report_descriptors: Vec<InterfaceReportDescriptor>,
    pub profile: PenTabletProfile,
}

/// Picks the first attached device whose registry report descriptors
/// authorize it as a pen tablet, with the libusb path's rules: a device
/// that is not a pen tablet is skipped silently, a pen tablet that fails a
/// bridge rule is reported, and absence is [`NO_TABLET_ATTACHED`].
///
/// Nothing is opened. The shared engine checks the descriptors again
/// against what the device itself returns after capture.
///
/// # Errors
///
/// Returns [`NO_TABLET_ATTACHED`] or the reasons every attached pen tablet
/// was refused.
pub fn choose_tablet(
    devices: &[RegistryUsbDevice],
    hids: &[RegistryHid],
) -> Result<ChosenTablet, String> {
    let mut rejections: Vec<String> = Vec::new();
    for device in devices {
        let class = device.device_class.unwrap_or(0);
        if class != 0 && class != USB_CLASS_HID {
            continue;
        }
        let identity = format!("{:04x}:{:04x}", device.vendor_id, device.product_id);
        let report_descriptors = match hid_descriptors_for(hids, device) {
            Ok(descriptors) => descriptors,
            Err(reason) => {
                rejections.push(format!(
                    "{identity}: the I/O Registry cannot tell which descriptors belong to \
                     this device ({reason})"
                ));
                continue;
            }
        };
        if !report_descriptors
            .iter()
            .any(|entry| declares_any_digitizer(&entry.descriptor))
        {
            continue;
        }
        let profile = match authorize_hard_usb_tablet(class, &report_descriptors) {
            Ok(profile) => profile,
            Err(HardUsbTabletDenial::NotPenTablet(_)) => continue,
            Err(denial) => {
                rejections.push(format!("{identity}: {denial}"));
                continue;
            }
        };
        if !crate::frame::hello_fits(
            report_descriptors
                .iter()
                .map(|entry| entry.descriptor.len()),
        ) {
            rejections.push(format!(
                "pen tablet {identity}: its report descriptors do not fit the helper \
                 handshake; it cannot be bridged (use Tablet support)"
            ));
            continue;
        }
        return Ok(ChosenTablet {
            device: device.clone(),
            report_descriptors,
            profile,
        });
    }
    if rejections.is_empty() {
        Err(NO_TABLET_ATTACHED.to_owned())
    } else {
        Err(format!(
            "no attached pen tablet is admissible: {}",
            rejections.join("; ")
        ))
    }
}

/// Captures the attached pen tablet with `IOUSBHost` and bridges it.
///
/// # Errors
///
/// Returns [`NO_TABLET_ATTACHED`], the refusal reasons, or the capture step
/// that failed (the device has then been given back).
pub fn capture() -> Result<PhysicalDevice, String> {
    let devices = arcen_usb_iousbhost::usb_devices()
        .map_err(|error| format!("IOUSBHost: read USB devices from the I/O Registry: {error}"))?;
    let hids = arcen_usb_iousbhost::hid_devices()
        .map_err(|error| format!("IOUSBHost: read HID devices from the I/O Registry: {error}"))?;
    let ChosenTablet {
        device,
        report_descriptors,
        profile,
    } = choose_tablet(&devices, &hids)?;
    eprintln!(
        "arcen-usb-helper: pen tablet {:04x}:{:04x} admitted (IOUSBHost, registry={:#x} \
         location={}): evidence={:?} pressure_levels={:?} tilt={} twist={} eraser={} \
         barrel_buttons={}",
        device.vendor_id,
        device.product_id,
        device.registry_id,
        device
            .location_id
            .map_or_else(|| "-".to_owned(), |location| format!("{location:#010x}")),
        profile.evidence,
        profile.capabilities.pressure_levels(),
        profile.capabilities.has_tilt(),
        profile.capabilities.twist.is_some(),
        profile.capabilities.eraser,
        profile.capabilities.barrel_buttons,
    );
    let captured = arcen_usb_iousbhost::capture(
        &CaptureTarget::of(&device),
        report_descriptors,
        &MarkerStore::system(),
    )
    .map_err(|error| {
        format!(
            "IOUSBHost capture of {:04x}:{:04x}: {error}",
            device.vendor_id, device.product_id
        )
    })?;
    eprintln!(
        "arcen-usb-helper: IOUSBHost DeviceCapture holds {:04x}:{:04x}",
        device.vendor_id, device.product_id
    );
    PhysicalDevice::bridge_iousbhost(captured)
}

/// Creates and verifies the root-only marker directory, then restores a
/// device a previous helper left captured. Run once, before serving. Only
/// this backend writes the marker, so a helper that never captured with
/// `IOUSBHost` finds nothing to do.
///
/// A marker directory that is not root's own 0700 directory is refused: no
/// recovery runs, and every `IOUSBHost` capture fails at its marker step.
pub fn recover_at_start() {
    let markers = MarkerStore::system();
    if let Err(error) = markers.prepare() {
        eprintln!(
            "arcen-usb-helper: refusing the IOUSBHost capture marker directory {}: {error}",
            markers.directory().display()
        );
        return;
    }
    match arcen_usb_iousbhost::recover(&markers) {
        RecoveryReport::Nothing => {}
        report => eprintln!("arcen-usb-helper: IOUSBHost capture recovery: {report:?}"),
    }
}

/// What the `IOUSBHost` backend would see, without opening anything.
///
/// # Errors
///
/// Returns a message when the I/O Registry cannot be read.
pub fn probe() -> Result<Vec<String>, String> {
    let devices = arcen_usb_iousbhost::usb_devices().map_err(|error| error.to_string())?;
    let hids = arcen_usb_iousbhost::hid_devices().map_err(|error| error.to_string())?;
    Ok(probe_lines(&devices, &hids, &MarkerStore::system().read()))
}

/// One line per attached USB device with the shared classification of its
/// registry report descriptors, then the state of the captured marker.
pub fn probe_lines(
    devices: &[RegistryUsbDevice],
    hids: &[RegistryHid],
    marker: &MarkerRead,
) -> Vec<String> {
    let mut lines: Vec<String> = devices
        .iter()
        .map(|device| {
            let verdict = match hid_descriptors_for(hids, device) {
                Err(reason) => format!("not bridged: {reason}"),
                Ok(descriptors) if descriptors.is_empty() => {
                    "not bridged: no HID report descriptor in the I/O Registry".to_owned()
                }
                Ok(descriptors) => {
                    match authorize_hard_usb_tablet(device.device_class.unwrap_or(0), &descriptors)
                    {
                        Ok(_) => "pen tablet: admissible".to_owned(),
                        Err(denial) => format!("not bridged: {denial}"),
                    }
                }
            };
            format!(
                "usb {:04x}:{:04x} registry={:#x} location={} speed={:?} captured_by={}: {verdict}",
                device.vendor_id,
                device.product_id,
                device.registry_id,
                device
                    .location_id
                    .map_or_else(|| "-".to_owned(), |location| format!("{location:#010x}")),
                device.speed,
                device.exclusive_owner.as_deref().unwrap_or("-"),
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("no USB device is attached".to_owned());
    }
    lines.push(match marker {
        MarkerRead::Absent => "captured marker: none".to_owned(),
        MarkerRead::Present(marker) => format!(
            "captured marker: registry={:#x} {:04x}:{:04x} written by pid {}",
            marker.registry_id, marker.vendor_id, marker.product_id, marker.pid
        ),
        MarkerRead::Rejected(reason) => format!("captured marker: untrusted ({reason})"),
    });
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_usb_bridge::{
        SyntheticTabletDevice, UsbSpeed, ARCEN_LAB_PRODUCT_ID, ARCEN_LAB_VENDOR_ID,
    };
    use arcen_usb_iousbhost::CaptureMarker;

    fn lab_tablet() -> RegistryUsbDevice {
        RegistryUsbDevice {
            registry_id: 0x42,
            location_id: Some(0x0110_0000),
            vendor_id: ARCEN_LAB_VENDOR_ID,
            product_id: ARCEN_LAB_PRODUCT_ID,
            bcd_device: Some(0x0100),
            device_class: Some(0),
            speed: Some(UsbSpeed::Full),
            exclusive_owner: None,
        }
    }

    fn lab_hid(report_descriptor: &[u8]) -> RegistryHid {
        RegistryHid {
            vendor_id: ARCEN_LAB_VENDOR_ID,
            product_id: ARCEN_LAB_PRODUCT_ID,
            location_id: Some(0x0110_0000),
            interface: Some(0),
            report_descriptor: report_descriptor.to_vec(),
        }
    }

    #[test]
    fn a_tablet_is_listed_as_admissible_and_other_devices_say_why_not() {
        let tablet = lab_tablet();
        let keyboard = RegistryUsbDevice {
            registry_id: 0x43,
            location_id: Some(0x0120_0000),
            ..lab_tablet()
        };
        let lines = probe_lines(
            &[tablet, keyboard],
            &[lab_hid(SyntheticTabletDevice::report_descriptor())],
            &MarkerRead::Absent,
        );
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("registry=0x42"), "{}", lines[0]);
        assert!(lines[0].ends_with("pen tablet: admissible"), "{}", lines[0]);
        assert!(
            lines[1].ends_with("no HID report descriptor in the I/O Registry"),
            "{}",
            lines[1]
        );
        assert_eq!(lines[2], "captured marker: none");
    }

    #[test]
    fn an_empty_registry_and_a_left_over_marker_are_reported() {
        let marker = CaptureMarker {
            boot_session: "A4864961-7D0B-4B08-9FB8-156D97CB76A3".to_owned(),
            registry_id: 0x42,
            location_id: None,
            vendor_id: 0x056a,
            product_id: 0x0027,
            pid: 7,
        };
        assert_eq!(
            probe_lines(&[], &[], &MarkerRead::Present(marker)),
            vec![
                "no USB device is attached".to_owned(),
                "captured marker: registry=0x42 056a:0027 written by pid 7".to_owned(),
            ]
        );
        assert_eq!(
            probe_lines(
                &[],
                &[],
                &MarkerRead::Rejected("owned by uid 501".to_owned())
            )[1],
            "captured marker: untrusted (owned by uid 501)"
        );
    }

    #[test]
    fn capture_chooses_the_first_registry_pen_tablet_and_skips_the_rest() {
        let tablet = lab_tablet();
        let keyboard = RegistryUsbDevice {
            registry_id: 0x43,
            location_id: Some(0x0120_0000),
            vendor_id: 0x05ac,
            product_id: 0x0250,
            ..lab_tablet()
        };
        let hub = RegistryUsbDevice {
            registry_id: 0x44,
            device_class: Some(0x09),
            ..lab_tablet()
        };
        let chosen = choose_tablet(
            &[keyboard, hub, tablet],
            &[lab_hid(SyntheticTabletDevice::report_descriptor())],
        )
        .expect("the tablet is chosen");
        assert_eq!(chosen.device.registry_id, 0x42);
        assert_eq!(chosen.report_descriptors.len(), 1);
        assert_eq!(chosen.report_descriptors[0].interface, 0);
    }

    #[test]
    fn capture_reports_absence_as_no_tablet_attached() {
        assert_eq!(
            choose_tablet(&[], &[]).map(|chosen| chosen.device.registry_id),
            Err(NO_TABLET_ATTACHED.to_owned())
        );
        // A device whose registry entry has no digitizer is not a candidate.
        assert_eq!(
            choose_tablet(&[lab_tablet()], &[]).map(|chosen| chosen.device.registry_id),
            Err(NO_TABLET_ATTACHED.to_owned())
        );
    }

    #[test]
    fn a_pen_tablet_with_a_game_controller_is_refused_with_its_reason() {
        let mut descriptor = SyntheticTabletDevice::report_descriptor().to_vec();
        // A joystick application collection beside the pen.
        descriptor.extend_from_slice(&[
            0x05, 0x01, 0x09, 0x04, 0xa1, 0x01, 0x09, 0x30, 0x15, 0x00, 0x25, 0x7f, 0x75, 0x08,
            0x95, 0x01, 0x81, 0x02, 0xc0,
        ]);
        let refusal = choose_tablet(&[lab_tablet()], &[lab_hid(&descriptor)])
            .map(|chosen| chosen.device.registry_id)
            .expect_err("refused");
        assert!(
            refusal.starts_with("no attached pen tablet is admissible: "),
            "{refusal}"
        );
    }

    #[test]
    fn a_non_digitizer_report_descriptor_is_not_bridged() {
        // A boot mouse: buttons and X/Y, no digitizer collection.
        let mouse = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
            0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01,
            0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f,
            0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0xc0, 0xc0,
        ];
        let lines = probe_lines(&[lab_tablet()], &[lab_hid(&mouse)], &MarkerRead::Absent);
        assert!(lines[0].contains("not bridged: "), "{}", lines[0]);
        assert!(!lines[0].contains("admissible"), "{}", lines[0]);
    }
}
