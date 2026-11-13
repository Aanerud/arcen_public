// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Read-only I/O Registry checks that need no device and no privilege.
//!
//! They exercise the real `IOKit` lookups this crate captures with — on a Mac
//! with nothing attached as much as on one with a keyboard and a tablet —
//! without opening anything.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use arcen_usb_iousbhost::{
    CaptureMarker, CaptureStage, CaptureTarget, MarkerStore, RecoveryReport, boot_session_uuid,
    capture, hid_devices, recover, usb_devices,
};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

fn scratch_store(name: &str) -> MarkerStore {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("arcen-usb-iousbhost-registry")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let directory = std::fs::canonicalize(&directory).unwrap();
    let uid = std::fs::metadata(&directory).unwrap().uid();
    MarkerStore::new(directory.join("run"), uid)
}

#[test]
fn the_registry_lists_usb_devices_and_none_is_captured_by_arcen() {
    let devices = usb_devices().expect("the I/O Registry is readable unprivileged");
    for device in &devices {
        eprintln!(
            "usb device {:04x}:{:04x} registry={:#x} location={:?} speed={:?} owner={:?}",
            device.vendor_id,
            device.product_id,
            device.registry_id,
            device.location_id,
            device.speed,
            device.exclusive_owner
        );
        let owner = device
            .exclusive_owner
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            !owner.contains("arcen"),
            "a device is still captured by an Arcen process: {device:?}"
        );
    }
    let unique: std::collections::BTreeSet<u64> =
        devices.iter().map(|device| device.registry_id).collect();
    assert_eq!(unique.len(), devices.len(), "registry IDs are unique");
    hid_devices().expect("HID report descriptors are readable unprivileged");
}

#[test]
fn recovery_with_no_marker_does_nothing() {
    assert_eq!(recover(&scratch_store("absent")), RecoveryReport::Nothing);
}

#[test]
fn recovery_for_a_device_that_is_not_attached_discards_the_marker() {
    let store = scratch_store("unattached");
    store
        .write(&CaptureMarker {
            boot_session: boot_session_uuid().expect("kern.bootsessionuuid is readable"),
            registry_id: u64::MAX,
            location_id: Some(0xff00_0000),
            vendor_id: 0xffff,
            product_id: 0xffff,
            pid: 1,
        })
        .unwrap();
    assert!(matches!(
        recover(&store),
        RecoveryReport::Discarded { reason } if reason.contains("no longer attached")
    ));
    assert_eq!(store.read(), arcen_usb_iousbhost::MarkerRead::Absent);
}

#[test]
fn recovery_for_a_marker_from_another_boot_discards_it() {
    let store = scratch_store("other-boot");
    store
        .write(&CaptureMarker {
            boot_session: "00000000-0000-4000-8000-000000000000".to_owned(),
            registry_id: u64::MAX,
            location_id: None,
            vendor_id: 0xffff,
            product_id: 0xffff,
            pid: 1,
        })
        .unwrap();
    assert!(matches!(
        recover(&store),
        RecoveryReport::Discarded { reason } if reason.contains("another boot")
    ));
    assert_eq!(store.read(), arcen_usb_iousbhost::MarkerRead::Absent);
}

#[test]
fn capturing_a_device_that_is_not_attached_fails_before_anything_is_written() {
    let store = scratch_store("capture");
    let error = capture(
        &CaptureTarget {
            registry_id: u64::MAX,
            location_id: None,
            vendor_id: 0xffff,
            product_id: 0xffff,
        },
        Vec::new(),
        &store,
    )
    .unwrap_err();
    assert_eq!(error.stage, CaptureStage::Lookup);
    assert_eq!(store.read(), arcen_usb_iousbhost::MarkerRead::Absent);
}
