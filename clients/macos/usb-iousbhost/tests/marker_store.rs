// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The captured marker on a real file system: kept in an owner-only
//! directory, written atomically, readable by its owner only, and never
//! trusted — or waited on — when anything about it is off.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use arcen_usb_iousbhost::{CaptureMarker, MARKER_FILE_NAME, MarkerRead, MarkerStore};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A fresh parent directory for one test; the store's own directory is
/// `<parent>/run`.
fn scratch(name: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("arcen-usb-iousbhost-marker")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    // The target directory may be named with `..`, which a trusted path must
    // not contain.
    std::fs::canonicalize(&directory).unwrap()
}

fn my_uid(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().uid()
}

fn store_in(parent: &Path) -> MarkerStore {
    MarkerStore::new(parent.join("run"), my_uid(parent))
}

fn marker() -> CaptureMarker {
    CaptureMarker {
        boot_session: "A4864961-7D0B-4B08-9FB8-156D97CB76A3".to_owned(),
        registry_id: 0x1_0000_0a3c,
        location_id: Some(0x0110_0000),
        vendor_id: 0x056a,
        product_id: 0x0027,
        pid: 4242,
    }
}

/// Reads on another thread, so a read that blocks fails the test instead of
/// hanging it.
fn read_within(store: &MarkerStore, limit: Duration) -> MarkerRead {
    let store = store.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(store.read());
    });
    receiver
        .recv_timeout(limit)
        .expect("reading the marker blocked")
}

fn write_raw(path: &Path, bytes: &[u8], mode: u32) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn the_directory_is_created_owner_only_and_the_marker_round_trips() {
    let parent = scratch("roundtrip");
    let store = store_in(&parent);
    assert_eq!(store.read(), MarkerRead::Absent);
    store.clear().unwrap();
    store.prepare().unwrap();
    let directory = std::fs::symlink_metadata(store.directory()).unwrap();
    assert!(directory.file_type().is_dir());
    assert_eq!(directory.mode() & 0o777, 0o700);
    assert_eq!(store.read(), MarkerRead::Absent);

    store.write(&marker()).unwrap();
    let metadata = std::fs::symlink_metadata(store.path()).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(store.read(), MarkerRead::Present(marker()));
    // A second capture replaces the marker atomically; no staging file stays.
    let next = CaptureMarker {
        registry_id: 0x77,
        ..marker()
    };
    store.write(&next).unwrap();
    assert_eq!(store.read(), MarkerRead::Present(next.clone()));
    assert_eq!(
        std::fs::read_dir(store.directory()).unwrap().count(),
        1,
        "only the marker is left in the directory"
    );
    store.clear().unwrap();
    assert_eq!(store.read(), MarkerRead::Absent);
    store.clear().unwrap();
}

#[test]
fn a_stale_staging_link_does_not_redirect_a_write() {
    let parent = scratch("staging");
    let store = store_in(&parent);
    store.prepare().unwrap();
    let decoy = parent.join("decoy");
    std::fs::write(&decoy, b"keep").unwrap();
    std::os::unix::fs::symlink(&decoy, store.directory().join("usb-helper.captured.tmp")).unwrap();
    store.write(&marker()).unwrap();
    assert_eq!(std::fs::read(&decoy).unwrap(), b"keep");
    assert_eq!(store.read(), MarkerRead::Present(marker()));
}

#[test]
fn a_directory_with_a_wrong_mode_is_refused() {
    let parent = scratch("dir-mode");
    let store = store_in(&parent);
    std::fs::create_dir(store.directory()).unwrap();
    std::fs::set_permissions(store.directory(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(store.prepare().is_err());
    assert!(store.write(&marker()).is_err());
    assert!(store.clear().is_err());
    assert!(matches!(
        store.read(),
        MarkerRead::Rejected(reason) if reason.contains("mode 755, not 700")
    ));
}

#[test]
fn a_directory_owned_by_someone_else_is_refused() {
    let parent = scratch("dir-owner");
    let uid = my_uid(&parent);
    MarkerStore::new(parent.join("run"), uid)
        .write(&marker())
        .unwrap();
    let foreign = MarkerStore::new(parent.join("run"), uid.wrapping_add(1));
    assert!(foreign.prepare().is_err());
    assert!(foreign.write(&marker()).is_err());
    assert!(foreign.clear().is_err());
    assert!(matches!(
        foreign.read(),
        MarkerRead::Rejected(reason) if reason.contains("owned by uid")
    ));
}

#[test]
fn a_linked_directory_is_refused() {
    let parent = scratch("dir-link");
    let real = MarkerStore::new(parent.join("real"), my_uid(&parent));
    real.write(&marker()).unwrap();
    std::os::unix::fs::symlink(parent.join("real"), parent.join("run")).unwrap();
    let store = store_in(&parent);
    assert!(store.prepare().is_err());
    assert!(matches!(
        store.read(),
        MarkerRead::Rejected(reason) if reason.contains("symbolic link")
    ));
    assert!(store.clear().is_err());
    assert_eq!(real.read(), MarkerRead::Present(marker()));
}

#[test]
fn a_symbolic_link_at_the_marker_path_is_refused_and_clear_removes_only_the_link() {
    let parent = scratch("link");
    let store = store_in(&parent);
    store.prepare().unwrap();
    let target = parent.join("target");
    write_raw(&target, marker().encode().as_bytes(), 0o600);
    std::os::unix::fs::symlink(&target, store.path()).unwrap();
    assert!(matches!(
        store.read(),
        MarkerRead::Rejected(reason) if reason.contains("symbolic link")
    ));
    store.clear().unwrap();
    assert!(target.exists());
    assert_eq!(store.read(), MarkerRead::Absent);
}

#[test]
fn a_fifo_at_the_marker_path_is_refused_without_blocking() {
    let parent = scratch("fifo");
    let store = store_in(&parent);
    store.prepare().unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("600")
        .arg(store.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(matches!(
        read_within(&store, Duration::from_secs(5)),
        MarkerRead::Rejected(reason) if reason.contains("not a regular file")
    ));
}

#[test]
fn a_marker_open_to_others_is_refused() {
    let parent = scratch("file");
    let store = store_in(&parent);
    store.write(&marker()).unwrap();
    std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        store.read(),
        MarkerRead::Rejected(reason) if reason.contains("accessible")
    ));
}

#[test]
fn garbage_and_oversized_markers_are_refused() {
    let parent = scratch("garbage");
    let store = store_in(&parent);
    store.prepare().unwrap();
    let path = store.directory().join(MARKER_FILE_NAME);
    write_raw(&path, b"registry_id=0x1\n", 0o600);
    assert!(matches!(store.read(), MarkerRead::Rejected(_)));
    write_raw(&path, &[b'x'; 4096], 0o600);
    assert!(matches!(
        store.read(),
        MarkerRead::Rejected(reason) if reason.contains("too large")
    ));
}
