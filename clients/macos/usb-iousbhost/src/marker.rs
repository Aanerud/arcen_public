// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The root-owned "captured" marker and crash recovery (ADR 0019 §4).
//!
//! A crash cannot run `destroy`, and Apple documents what `destroy` does, not
//! what tearing down a dead client does. So before it captures, the helper
//! records which device it is about to take. `launchd` restarts the helper
//! after an abnormal exit; on start the helper reads the marker and, if the
//! very same registry entry is still attached in the same boot, captures it
//! again and destroys it at once — the documented reset-and-rematch — then
//! clears the marker.
//!
//! Where it lives, and why: `/var/run` on macOS is `root:daemon` 0775 without
//! the sticky bit, so anything in group `daemon` can rename, replace or delete
//! its entries — including a subdirectory — between a check and a use. The
//! marker therefore lives in `/private/var/db/arcen/state`: every ancestor is
//! `root:wheel` 0755, `arcen` is root's 0755, `state` is root's 0700. The
//! directory is verified from `/` down with `O_NOFOLLOW` on every component
//! ([`arcen_secure_dir::TrustedDir`]) and pinned by its descriptor; the marker
//! is written, read and removed only with `openat`, `renameat` and `unlinkat`
//! on that descriptor, and read with `O_NOFOLLOW | O_NONBLOCK`, checked on the
//! open descriptor. A marker that is not a regular file owned by the expected
//! user with no group or other access is never acted on.
//!
//! `/private/var/db` survives a restart, and registry IDs do not mean the same
//! device after one, so every marker records `kern.bootsessionuuid` and a
//! marker from another boot is discarded without touching any device.

use crate::matching::{CaptureTarget, LookupError, MatchedBy, RegistryUsbDevice, select_target};
use arcen_secure_dir::{DirPolicy, TrustedDir, mode};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

/// The root-owned 0700 directory that holds the marker.
pub const DEFAULT_MARKER_DIRECTORY: &str = "/private/var/db/arcen/state";
/// The marker's name in its directory.
pub const MARKER_FILE_NAME: &str = "usb-helper.captured";
const STAGING_FILE_NAME: &str = "usb-helper.captured.tmp";
/// Where the helper records a capture in progress.
pub const DEFAULT_MARKER_PATH: &str = "/private/var/db/arcen/state/usb-helper.captured";
/// Mode of the marker directory.
pub const MARKER_DIRECTORY_MODE: u32 = 0o700;
/// Mode of directories created above it (`/private/var/db/arcen`).
pub const PARENT_DIRECTORY_MODE: u32 = 0o755;
const MAGIC: &str = "arcen-usb-captured 2";
/// A marker is a few short lines; anything larger was not written by us.
const MAX_MARKER_BYTES: u64 = 512;
const MAX_BOOT_SESSION_BYTES: usize = 64;

/// The device a capture is about to take, written before the capture starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureMarker {
    /// `kern.bootsessionuuid` of the boot the capture happened in.
    pub boot_session: String,
    pub registry_id: u64,
    pub location_id: Option<u32>,
    pub vendor_id: u16,
    pub product_id: u16,
    /// The helper process that wrote it; for diagnostics only.
    pub pid: u32,
}

impl CaptureMarker {
    /// The marker for capturing `target` from process `pid` in boot
    /// `boot_session`.
    #[must_use]
    pub fn for_target(target: &CaptureTarget, pid: u32, boot_session: &str) -> Self {
        Self {
            boot_session: boot_session.to_owned(),
            registry_id: target.registry_id,
            location_id: target.location_id,
            vendor_id: target.vendor_id,
            product_id: target.product_id,
            pid,
        }
    }

    #[must_use]
    pub fn encode(&self) -> String {
        let location = self
            .location_id
            .map_or_else(|| "none".to_owned(), |location| format!("{location:#010x}"));
        format!(
            "{MAGIC}\nboot_session={}\nregistry_id={:#x}\nlocation_id={location}\n\
             vendor_id={:04x}\nproduct_id={:04x}\npid={}\n",
            self.boot_session, self.registry_id, self.vendor_id, self.product_id, self.pid
        )
    }

    /// Parses a marker strictly: the version line, then each key exactly once.
    ///
    /// # Errors
    ///
    /// Returns a reason for any other content.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next() != Some(MAGIC) {
            return Err("not an Arcen capture marker of a known version".to_owned());
        }
        let mut boot_session = None;
        let mut registry_id = None;
        let mut location_id = None;
        let mut vendor_id = None;
        let mut product_id = None;
        let mut pid = None;
        for line in lines {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("malformed marker line {line:?}"))?;
            let duplicate = match key {
                "boot_session" => boot_session.replace(parse_boot_session(value)?).is_some(),
                "registry_id" => registry_id.replace(parse_hex_u64(value)?).is_some(),
                "location_id" => location_id
                    .replace(if value == "none" {
                        None
                    } else {
                        Some(parse_hex_u32(value)?)
                    })
                    .is_some(),
                "vendor_id" => vendor_id.replace(parse_hex_u16(value)?).is_some(),
                "product_id" => product_id.replace(parse_hex_u16(value)?).is_some(),
                "pid" => pid
                    .replace(
                        value
                            .parse::<u32>()
                            .map_err(|_| format!("invalid pid {value:?}"))?,
                    )
                    .is_some(),
                other => return Err(format!("unknown marker key {other:?}")),
            };
            if duplicate {
                return Err(format!("marker key {key:?} appears twice"));
            }
        }
        let missing = |name: &str| format!("marker has no {name}");
        Ok(Self {
            boot_session: boot_session.ok_or_else(|| missing("boot_session"))?,
            registry_id: registry_id.ok_or_else(|| missing("registry_id"))?,
            location_id: location_id.ok_or_else(|| missing("location_id"))?,
            vendor_id: vendor_id.ok_or_else(|| missing("vendor_id"))?,
            product_id: product_id.ok_or_else(|| missing("product_id"))?,
            pid: pid.ok_or_else(|| missing("pid"))?,
        })
    }

    const fn target(&self) -> CaptureTarget {
        CaptureTarget {
            registry_id: self.registry_id,
            location_id: self.location_id,
            vendor_id: self.vendor_id,
            product_id: self.product_id,
        }
    }
}

/// A boot session identifier: a UUID's characters only, bounded.
fn parse_boot_session(value: &str) -> Result<String, String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_BOOT_SESSION_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
    if valid {
        Ok(value.to_owned())
    } else {
        Err(format!("invalid boot session {value:?}"))
    }
}

fn parse_hex_u64(value: &str) -> Result<u64, String> {
    let digits = value.strip_prefix("0x").unwrap_or(value);
    u64::from_str_radix(digits, 16).map_err(|_| format!("invalid hexadecimal {value:?}"))
}

fn parse_hex_u32(value: &str) -> Result<u32, String> {
    u32::try_from(parse_hex_u64(value)?).map_err(|_| format!("{value:?} does not fit 32 bits"))
}

fn parse_hex_u16(value: &str) -> Result<u16, String> {
    u16::try_from(parse_hex_u64(value)?).map_err(|_| format!("{value:?} does not fit 16 bits"))
}

/// What reading the marker found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerRead {
    /// No capture was in progress.
    Absent,
    /// A capture was in progress when the previous helper stopped.
    Present(CaptureMarker),
    /// Something is at the marker path that this process must not trust.
    Rejected(String),
}

/// The marker directory and file, with the ownership a trustworthy marker
/// must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerStore {
    directory: PathBuf,
    owner_uid: u32,
}

const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);
const CREATE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

impl MarkerStore {
    /// A store in the absolute, `..`-free `directory`, which must be owned by
    /// `owner_uid` with mode 0700 below ancestors only root or `owner_uid`
    /// own and nobody else can write.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>, owner_uid: u32) -> Self {
        Self {
            directory: directory.into(),
            owner_uid,
        }
    }

    /// The helper's store: [`DEFAULT_MARKER_DIRECTORY`], owned by root.
    #[must_use]
    pub fn system() -> Self {
        Self::new(DEFAULT_MARKER_DIRECTORY, 0)
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The marker file.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.directory.join(MARKER_FILE_NAME)
    }

    const fn policy(&self) -> DirPolicy {
        DirPolicy {
            owner_uid: self.owner_uid,
            mode: MARKER_DIRECTORY_MODE,
        }
    }

    /// The verified directory, created if missing. Run at daemon start;
    /// [`Self::write`] runs it too.
    ///
    /// # Errors
    ///
    /// Refuses a directory (or an ancestor) that a link, another owner or
    /// another mode makes untrustworthy, and returns the error of a failed
    /// creation.
    pub fn prepare(&self) -> std::io::Result<TrustedDir> {
        TrustedDir::create(&self.directory, self.policy(), PARENT_DIRECTORY_MODE)
    }

    /// Records `marker` atomically, readable by its owner only: a fresh
    /// staging file created with `O_EXCL | O_NOFOLLOW` in the pinned
    /// directory, flushed, then renamed into place within it.
    ///
    /// # Errors
    ///
    /// Returns the I/O error; the capture must not proceed without a marker.
    pub fn write(&self, marker: &CaptureMarker) -> std::io::Result<()> {
        let directory = self.prepare()?;
        match rustix::fs::unlinkat(directory.fd(), STAGING_FILE_NAME, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let staging =
            rustix::fs::openat(directory.fd(), STAGING_FILE_NAME, CREATE_FLAGS, mode(0o600))?;
        let mut file = std::fs::File::from(staging);
        file.write_all(marker.encode().as_bytes())?;
        file.sync_all()?;
        drop(file);
        rustix::fs::renameat(
            directory.fd(),
            STAGING_FILE_NAME,
            directory.fd(),
            MARKER_FILE_NAME,
        )?;
        rustix::fs::fsync(directory.fd())?;
        Ok(())
    }

    /// Reads the marker, refusing anything this process did not write.
    ///
    /// Never follows a link and never blocks: a FIFO opens at once with
    /// `O_NONBLOCK` and is refused by the checks on the descriptor.
    #[must_use]
    pub fn read(&self) -> MarkerRead {
        let directory = match TrustedDir::open(&self.directory, self.policy()) {
            Ok(directory) => directory,
            Err(error) if error.kind() == ErrorKind::NotFound => return MarkerRead::Absent,
            Err(error) => return MarkerRead::Rejected(error.to_string()),
        };
        let fd =
            match rustix::fs::openat(directory.fd(), MARKER_FILE_NAME, READ_FLAGS, Mode::empty()) {
                Ok(fd) => fd,
                Err(Errno::NOENT) => return MarkerRead::Absent,
                Err(Errno::LOOP) => {
                    return MarkerRead::Rejected("the marker is a symbolic link".to_owned());
                }
                Err(error) => {
                    return MarkerRead::Rejected(format!(
                        "cannot open marker: {}",
                        std::io::Error::from(error)
                    ));
                }
            };
        let stat = match rustix::fs::fstat(&fd) {
            Ok(stat) => stat,
            Err(error) => {
                return MarkerRead::Rejected(format!(
                    "cannot inspect marker: {}",
                    std::io::Error::from(error)
                ));
            }
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return MarkerRead::Rejected("the marker is not a regular file".to_owned());
        }
        if stat.st_uid != self.owner_uid {
            return MarkerRead::Rejected(format!(
                "the marker is owned by uid {}, not {}",
                stat.st_uid, self.owner_uid
            ));
        }
        let permissions = u32::from(stat.st_mode) & 0o7777;
        if permissions & 0o077 != 0 {
            return MarkerRead::Rejected(format!(
                "the marker is accessible to others (mode {permissions:o})"
            ));
        }
        if u64::try_from(stat.st_size).map_or(true, |size| size > MAX_MARKER_BYTES) {
            return MarkerRead::Rejected("the marker is too large".to_owned());
        }
        let mut text = String::new();
        if let Err(error) = std::fs::File::from(fd)
            .take(MAX_MARKER_BYTES)
            .read_to_string(&mut text)
        {
            return MarkerRead::Rejected(format!("cannot read marker: {error}"));
        }
        match CaptureMarker::parse(&text) {
            Ok(marker) => MarkerRead::Present(marker),
            Err(reason) => MarkerRead::Rejected(reason),
        }
    }

    /// Removes the marker (never what a link points to). Absent is success.
    ///
    /// # Errors
    ///
    /// Refuses an untrusted directory, and returns the I/O error of a marker
    /// that exists but cannot be removed.
    pub fn clear(&self) -> std::io::Result<()> {
        let directory = match TrustedDir::open(&self.directory, self.policy()) {
            Ok(directory) => directory,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        match rustix::fs::unlinkat(directory.fd(), MARKER_FILE_NAME, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// What a helper starting up does about a marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryPlan {
    /// No capture was in progress.
    Nothing,
    /// Remove the marker and touch no device.
    Discard { reason: String },
    /// Capture this registry entry again and destroy it at once, so it is
    /// reset and its drivers re-match; then remove the marker.
    ResetDevice { registry_id: u64 },
}

/// Decides recovery from the marker, the current boot and the devices attached
/// now.
///
/// Only the exact registry entry the marker names is ever reset, only in the
/// boot that wrote the marker, and only while the entry still has the
/// recorded identity and port. A marker from another boot (or when the
/// current boot cannot be read) is discarded: registry IDs are reassigned at
/// boot. A device that has re-enumerated since (a new registry ID at the same
/// port) was already reset and re-matched, and a device that is gone was
/// restored by unplugging it; neither is touched.
#[must_use]
pub fn plan_recovery(
    read: &MarkerRead,
    current_boot: Option<&str>,
    devices: &[RegistryUsbDevice],
) -> RecoveryPlan {
    let marker = match read {
        MarkerRead::Absent => return RecoveryPlan::Nothing,
        MarkerRead::Rejected(reason) => {
            return RecoveryPlan::Discard {
                reason: format!("untrusted marker: {reason}"),
            };
        }
        MarkerRead::Present(marker) => marker,
    };
    match current_boot {
        Some(boot) if boot == marker.boot_session => {}
        Some(_) => {
            return RecoveryPlan::Discard {
                reason: "the marker was written during another boot".to_owned(),
            };
        }
        None => {
            return RecoveryPlan::Discard {
                reason: "the current boot session cannot be read, so the marker cannot be \
                         matched to it"
                    .to_owned(),
            };
        }
    }
    match select_target(devices, &marker.target()) {
        Ok((device, MatchedBy::RegistryId)) => RecoveryPlan::ResetDevice {
            registry_id: device.registry_id,
        },
        Ok((_, MatchedBy::Location)) => RecoveryPlan::Discard {
            reason: "the device re-enumerated since the capture, so it was already reset"
                .to_owned(),
        },
        Err(LookupError::NotFound) => RecoveryPlan::Discard {
            reason: "the device is no longer attached; unplugging it restored it".to_owned(),
        },
        Err(error) => RecoveryPlan::Discard {
            reason: format!("not touching the device: {error}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_usb_bridge::UsbSpeed;

    const BOOT: &str = "A4864961-7D0B-4B08-9FB8-156D97CB76A3";

    fn marker() -> CaptureMarker {
        CaptureMarker {
            boot_session: BOOT.to_owned(),
            registry_id: 0x1_0000_0a3c,
            location_id: Some(0x0110_0000),
            vendor_id: 0x056a,
            product_id: 0x0027,
            pid: 4242,
        }
    }

    fn attached(registry_id: u64, location: Option<u32>, product: u16) -> RegistryUsbDevice {
        RegistryUsbDevice {
            registry_id,
            location_id: location,
            vendor_id: 0x056a,
            product_id: product,
            bcd_device: None,
            device_class: None,
            speed: Some(UsbSpeed::Full),
            exclusive_owner: None,
        }
    }

    #[test]
    fn the_default_marker_lives_in_a_directory_with_no_group_writable_parent() {
        assert_eq!(
            Path::new(DEFAULT_MARKER_DIRECTORY).join(MARKER_FILE_NAME),
            Path::new(DEFAULT_MARKER_PATH)
        );
        assert!(!DEFAULT_MARKER_PATH.starts_with("/var/"));
        assert!(!DEFAULT_MARKER_PATH.starts_with("/private/var/run"));
        assert!(arcen_secure_dir::components(Path::new(DEFAULT_MARKER_DIRECTORY)).is_ok());
    }

    #[test]
    fn a_marker_round_trips_with_and_without_a_location() {
        let with = marker();
        assert_eq!(
            with.encode(),
            "arcen-usb-captured 2\nboot_session=A4864961-7D0B-4B08-9FB8-156D97CB76A3\n\
             registry_id=0x100000a3c\nlocation_id=0x01100000\nvendor_id=056a\n\
             product_id=0027\npid=4242\n"
        );
        assert_eq!(CaptureMarker::parse(&with.encode()), Ok(with.clone()));
        let without = CaptureMarker {
            location_id: None,
            ..with
        };
        assert_eq!(CaptureMarker::parse(&without.encode()), Ok(without));
    }

    #[test]
    fn anything_but_a_well_formed_marker_is_refused() {
        let good = marker().encode();
        let cases = [
            String::new(),
            good.replace("arcen-usb-captured 2", "arcen-usb-captured 1"),
            good.replace("pid=4242\n", ""),
            good.replace(&format!("boot_session={BOOT}\n"), ""),
            good.replace(BOOT, ""),
            good.replace(BOOT, "../../etc"),
            good.replace(BOOT, &"A".repeat(65)),
            format!("{good}pid=1\n"),
            format!("{good}extra=1\n"),
            good.replace("vendor_id=056a", "vendor_id=1056a"),
            good.replace("location_id=0x01100000", "location_id=0x101100000"),
            good.replace("registry_id=0x100000a3c", "registry_id=zz"),
            good.replace("pid=4242", "pid=-1"),
            good.replace("pid=4242", "pid 4242"),
        ];
        for case in cases {
            assert!(CaptureMarker::parse(&case).is_err(), "{case:?}");
        }
    }

    #[test]
    fn no_marker_means_nothing_to_recover() {
        assert_eq!(
            plan_recovery(
                &MarkerRead::Absent,
                Some(BOOT),
                &[attached(1, None, 0x0027)]
            ),
            RecoveryPlan::Nothing
        );
    }

    #[test]
    fn the_exact_device_still_attached_in_the_same_boot_is_reset() {
        let devices = [
            attached(0x1_0000_0a3c, Some(0x0110_0000), 0x0027),
            attached(7, Some(0x0120_0000), 0x0027),
        ];
        assert_eq!(
            plan_recovery(&MarkerRead::Present(marker()), Some(BOOT), &devices),
            RecoveryPlan::ResetDevice {
                registry_id: 0x1_0000_0a3c
            }
        );
    }

    #[test]
    fn a_marker_from_another_boot_or_an_unknown_boot_touches_nothing() {
        let devices = [attached(0x1_0000_0a3c, Some(0x0110_0000), 0x0027)];
        assert!(matches!(
            plan_recovery(
                &MarkerRead::Present(marker()),
                Some("0B5C3B4E-0000-4000-8000-000000000000"),
                &devices
            ),
            RecoveryPlan::Discard { reason } if reason.contains("another boot")
        ));
        assert!(matches!(
            plan_recovery(&MarkerRead::Present(marker()), None, &devices),
            RecoveryPlan::Discard { reason } if reason.contains("cannot be read")
        ));
    }

    #[test]
    fn a_re_enumerated_or_unplugged_device_is_left_alone() {
        let re_enumerated = [attached(0x1_0000_0b00, Some(0x0110_0000), 0x0027)];
        assert!(matches!(
            plan_recovery(&MarkerRead::Present(marker()), Some(BOOT), &re_enumerated),
            RecoveryPlan::Discard { reason } if reason.contains("re-enumerated")
        ));
        assert!(matches!(
            plan_recovery(&MarkerRead::Present(marker()), Some(BOOT), &[]),
            RecoveryPlan::Discard { reason } if reason.contains("no longer attached")
        ));
    }

    #[test]
    fn a_registry_id_now_naming_another_device_is_not_touched() {
        let other = [attached(0x1_0000_0a3c, Some(0x0110_0000), 0x0300)];
        assert!(matches!(
            plan_recovery(&MarkerRead::Present(marker()), Some(BOOT), &other),
            RecoveryPlan::Discard { reason } if reason.contains("not touching")
        ));
        let moved = [attached(0x1_0000_0a3c, Some(0x0140_0000), 0x0027)];
        assert!(matches!(
            plan_recovery(&MarkerRead::Present(marker()), Some(BOOT), &moved),
            RecoveryPlan::Discard { .. }
        ));
    }

    #[test]
    fn an_untrusted_marker_is_discarded_without_touching_any_device() {
        let devices = [attached(0x1_0000_0a3c, Some(0x0110_0000), 0x0027)];
        assert!(matches!(
            plan_recovery(
                &MarkerRead::Rejected("owned by uid 501".to_owned()),
                Some(BOOT),
                &devices
            ),
            RecoveryPlan::Discard { reason } if reason.starts_with("untrusted marker")
        ));
    }
}
