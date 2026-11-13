// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Trusted directories on a real file system.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use arcen_secure_dir::{DirPolicy, TrustedDir};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("arcen-secure-dir")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    // The target directory may be named with `..`, which a trusted path
    // must not contain.
    std::fs::canonicalize(&directory).unwrap()
}

fn policy(parent: &Path, mode: u32) -> DirPolicy {
    DirPolicy {
        owner_uid: std::fs::metadata(parent).unwrap().uid(),
        mode,
    }
}

fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

#[test]
fn missing_components_are_created_with_exact_modes_and_reopened() {
    let parent = scratch("create");
    let target = parent.join("arcen").join("state");
    let policy = policy(&parent, 0o700);
    assert_eq!(
        TrustedDir::open(&target, policy).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    let created = TrustedDir::create(&target, policy, 0o755).unwrap();
    assert_eq!(created.path(), target);
    assert_eq!(mode_of(&parent.join("arcen")), 0o755);
    assert_eq!(mode_of(&target), 0o700);
    TrustedDir::open(&target, policy).unwrap();
    TrustedDir::create(&target, policy, 0o755).unwrap();
}

#[test]
fn a_directory_with_another_mode_or_owner_is_refused() {
    let parent = scratch("mode");
    let target = parent.join("state");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wanted = policy(&parent, 0o700);
    for result in [
        TrustedDir::open(&target, wanted),
        TrustedDir::create(&target, wanted, 0o755),
    ] {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("mode 755, not 700"), "{error}");
    }
    let foreign = DirPolicy {
        owner_uid: wanted.owner_uid.wrapping_add(1),
        mode: 0o755,
    };
    assert!(TrustedDir::open(&target, foreign).is_err());
}

#[test]
fn a_group_writable_ancestor_is_refused() {
    let parent = scratch("ancestor");
    let shared = parent.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o775)).unwrap();
    let error =
        TrustedDir::create(&shared.join("state"), policy(&parent, 0o700), 0o755).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("writable by group"), "{error}");
}

#[test]
fn a_symbolic_link_anywhere_on_the_path_is_refused() {
    let parent = scratch("link");
    let real = parent.join("real");
    TrustedDir::create(&real.join("state"), policy(&parent, 0o700), 0o755).unwrap();
    std::os::unix::fs::symlink(&real, parent.join("alias")).unwrap();
    std::os::unix::fs::symlink(real.join("state"), parent.join("state-link")).unwrap();
    for path in [
        parent.join("alias").join("state"),
        parent.join("state-link"),
    ] {
        let error = TrustedDir::open(&path, policy(&parent, 0o700)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{path:?}");
        assert!(error.to_string().contains("symbolic link"), "{error}");
        let error = TrustedDir::create(&path, policy(&parent, 0o700), 0o755).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{path:?}");
    }
}

#[test]
fn a_file_where_the_directory_should_be_is_refused() {
    let parent = scratch("file");
    std::fs::write(parent.join("state"), b"").unwrap();
    let error =
        TrustedDir::create(&parent.join("state"), policy(&parent, 0o700), 0o755).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
}

#[test]
fn relative_and_parent_paths_are_refused() {
    let parent = scratch("relative");
    let wanted = policy(&parent, 0o700);
    assert!(TrustedDir::open(Path::new("relative/state"), wanted).is_err());
    assert!(TrustedDir::open(&parent.join("..").join("state"), wanted).is_err());
}

/// A short scratch directory for sockets (`sun_path` holds 104 bytes, which
/// the target directory's path can exceed), removed when dropped.
struct SocketScratch(PathBuf);

impl Drop for SocketScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn socket_directory(name: &str) -> (SocketScratch, TrustedDir, u32) {
    let home = std::fs::canonicalize(std::env::var_os("HOME").unwrap()).unwrap();
    let parent = home.join(format!(".arcen-sd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wanted = policy(&parent, 0o755);
    let directory = TrustedDir::create(&parent.join("run"), wanted, 0o755).unwrap();
    (SocketScratch(parent), directory, wanted.owner_uid)
}

#[test]
fn a_socket_is_bound_owner_only_for_its_client_and_a_stale_one_is_replaced() {
    let (_scratch, directory, uid) = socket_directory("sock");
    let path = directory.path().join("usb-helper.sock");
    let first = directory.bind_socket("usb-helper.sock", uid).unwrap();
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    assert!(std::os::unix::fs::FileTypeExt::is_socket(
        &metadata.file_type()
    ));
    assert_eq!(metadata.uid(), uid);
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    std::os::unix::net::UnixStream::connect(&path).unwrap();
    drop(first);
    // The listener is gone but its socket file stays: a restart replaces it.
    let _second = directory.bind_socket("usb-helper.sock", uid).unwrap();
    std::os::unix::net::UnixStream::connect(&path).unwrap();
}

#[test]
fn anything_but_a_socket_at_the_name_is_refused_and_left_alone() {
    let (scratch, directory, uid) = socket_directory("nosock");
    let decoy = scratch.0.join("decoy");
    std::fs::write(&decoy, b"keep").unwrap();
    std::os::unix::fs::symlink(&decoy, directory.path().join("link.sock")).unwrap();
    std::fs::write(directory.path().join("file.sock"), b"keep").unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(directory.path().join("fifo.sock"))
            .status()
            .unwrap()
            .success()
    );
    for name in ["link.sock", "file.sock", "fifo.sock"] {
        let error = directory.bind_socket(name, uid).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{name}");
        assert!(error.to_string().contains("not a socket"), "{error}");
        assert!(std::fs::symlink_metadata(directory.path().join(name)).is_ok());
    }
    assert_eq!(std::fs::read(&decoy).unwrap(), b"keep");
    for bad in ["", "..", "../escape.sock", "nested/usb-helper.sock"] {
        assert!(directory.bind_socket(bad, uid).is_err(), "{bad:?}");
    }
}
