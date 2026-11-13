// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Hardware test: a real pen tablet captured, bridged through
//! the shared engine and given back. Ignored by default.
//!
//! Run it on a Mac with a USB pen tablet attached and nothing else of value
//! relying on that tablet, from Terminal, as root (capture needs root):
//!
//! ```sh
//! sudo -E cargo test -p arcen-usb-iousbhost --test hardware -- --ignored --nocapture
//! # pick one tablet when several are attached:
//! sudo -E ARCEN_IOUSBHOST_TABLET=056a:0027 cargo test -p arcen-usb-iousbhost \
//!     --test hardware -- --ignored --nocapture
//! ```
//!
//! While it runs the tablet stops moving the local pointer. When it prints
//! "move the pen", hover or draw on the tablet within 15 seconds. Afterwards
//! the tablet must move the local pointer again with no replug. If the test is
//! killed half-way, the marker it writes under the target directory lets
//! `recover` restore the device; unplugging the tablet always restores it.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use arcen_usb_bridge::{
    AttachmentGeneration, EndpointAddress, ReleaseOutcome, ReleaseReason, SetupPacket,
    TransferDirection, TransferKind, UrbId, UrbStatus, UrbSubmission, UsbExporter,
    authorize_hard_usb_tablet,
};
use arcen_usb_iousbhost::{
    CaptureTarget, MarkerRead, MarkerStore, RegistryUsbDevice, capture, report_descriptors_for,
    usb_devices,
};
use std::num::{NonZeroU32, NonZeroU64};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn wanted() -> Option<(u16, u16)> {
    let text = std::env::var("ARCEN_IOUSBHOST_TABLET").ok()?;
    let (vendor, product) = text.split_once(':')?;
    Some((
        u16::from_str_radix(vendor, 16).ok()?,
        u16::from_str_radix(product, 16).ok()?,
    ))
}

fn find_tablet() -> RegistryUsbDevice {
    let devices = usb_devices().unwrap();
    devices
        .into_iter()
        .find(|device| {
            if let Some((vendor, product)) = wanted() {
                return device.vendor_id == vendor && device.product_id == product;
            }
            report_descriptors_for(device).is_ok_and(|descriptors| {
                !descriptors.is_empty()
                    && authorize_hard_usb_tablet(device.device_class.unwrap_or(0), &descriptors)
                        .is_ok()
            })
        })
        .expect("attach a USB pen tablet (or set ARCEN_IOUSBHOST_TABLET=vvvv:pppp)")
}

fn urb(value: u32) -> UrbId {
    UrbId::new(NonZeroU32::new(value).unwrap())
}

fn generation() -> AttachmentGeneration {
    AttachmentGeneration::new(NonZeroU64::new(1).unwrap())
}

#[test]
#[ignore = "needs root and a physical USB pen tablet; see the module documentation"]
fn a_real_tablet_is_captured_bridged_and_given_back() {
    let tablet = find_tablet();
    eprintln!("tablet: {tablet:?}");
    let descriptors = report_descriptors_for(&tablet).unwrap();
    assert!(
        !descriptors.is_empty(),
        "macOS published no report descriptor"
    );

    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("arcen-usb-iousbhost-hw");
    std::fs::create_dir_all(&directory).unwrap();
    let directory = std::fs::canonicalize(&directory).unwrap();
    // The marker belongs to whoever runs the test (root): learn that uid
    // from a file this process creates, not from a directory a previous
    // unprivileged build may own.
    let probe = directory.join("owner");
    std::fs::write(&probe, b"").unwrap();
    let uid = std::fs::metadata(&probe).unwrap().uid();
    let markers = MarkerStore::new(directory.join("run"), uid);

    let device = match capture(&CaptureTarget::of(&tablet), descriptors, &markers) {
        Ok(device) => device,
        Err(error) if error.is_privilege() => panic!("run this test as root: {error}"),
        Err(error) => panic!("capture failed: {error}"),
    };
    assert!(matches!(markers.read(), MarkerRead::Present(_)));
    let captured = usb_devices()
        .unwrap()
        .into_iter()
        .find(|device| device.registry_id == tablet.registry_id)
        .expect("the captured device stays in the registry");
    eprintln!(
        "while captured, UsbExclusiveOwner = {:?}",
        captured.exclusive_owner
    );

    let link = device.end_link();
    let exporter = UsbExporter::new(Box::new(device))
        .unwrap_or_else(|(refusal, outcome)| panic!("refused: {refusal} ({outcome:?})"));
    link.connect(exporter.end_signal());

    device_descriptor_round_trip(&exporter, &tablet);
    one_pen_report(&exporter);

    let released = exporter.release(ReleaseReason::SessionEnded);
    assert_eq!(
        released,
        Some((ReleaseReason::SessionEnded, ReleaseOutcome::Restored))
    );
    drop(exporter);
    assert_eq!(markers.read(), MarkerRead::Absent);

    wait_until_rematched(&tablet);
}

fn device_descriptor_round_trip(exporter: &UsbExporter, tablet: &RegistryUsbDevice) {
    // GET_DESCRIPTOR(Device) through the real default pipe.
    exporter
        .submit(
            UrbSubmission {
                generation: generation(),
                urb_id: urb(1),
                endpoint: EndpointAddress(0x80),
                transfer_kind: TransferKind::Control,
                timeout_ms: 1_000,
                declared_length: 18,
                setup: Some(SetupPacket {
                    request_type: 0x80,
                    request: 6,
                    value: 0x0100,
                    index: 0,
                    length: 18,
                }),
            },
            &[],
        )
        .unwrap();
    let descriptor = exporter
        .next_completion_within(Duration::from_secs(2))
        .unwrap()
        .expect("GET_DESCRIPTOR completes");
    assert_eq!(descriptor.status, UrbStatus::Success);
    assert_eq!(descriptor.data.len(), 18);
    assert_eq!(
        u16::from_le_bytes([descriptor.data[8], descriptor.data[9]]),
        tablet.vendor_id
    );
}

fn one_pen_report(exporter: &UsbExporter) {
    // One interrupt IN report from the pen.
    let endpoint = exporter
        .facts()
        .snapshot
        .configuration
        .interfaces
        .iter()
        .flat_map(|interface| interface.endpoints.iter())
        .find(|endpoint| endpoint.address.direction() == TransferDirection::In)
        .copied()
        .expect("an interrupt IN endpoint");
    exporter
        .submit(
            UrbSubmission {
                generation: generation(),
                urb_id: urb(2),
                endpoint: endpoint.address,
                transfer_kind: TransferKind::Interrupt,
                timeout_ms: 0,
                declared_length: u32::from(endpoint.max_packet_size),
                setup: None,
            },
            &[],
        )
        .unwrap();
    eprintln!("move the pen over the tablet now (15 s)");
    let report = exporter
        .next_completion_within(Duration::from_secs(15))
        .unwrap()
        .expect("the pen reported within 15 s");
    assert_eq!(report.status, UrbStatus::Success);
    assert!(!report.data.is_empty());
    eprintln!("report: {:02x?}", report.data);
}

fn wait_until_rematched(tablet: &RegistryUsbDevice) {
    // The reset re-enumerates the device and its HID drivers match again.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let back = usb_devices().unwrap().into_iter().find(|device| {
            device.vendor_id == tablet.vendor_id
                && device.product_id == tablet.product_id
                && device.location_id == tablet.location_id
        });
        let rematched = back.as_ref().is_some_and(|device| {
            device.exclusive_owner.is_none()
                && report_descriptors_for(device).is_ok_and(|found| !found.is_empty())
        });
        if rematched {
            eprintln!("restored: {back:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the tablet's drivers did not re-match within 5 s: {back:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
