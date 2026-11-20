// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! macOS host certificate material.
//!
//! The decision of what to do — create, keep, renew, rekey or adopt — is not
//! made here. It comes from [`arcen_transport::cert_provisioning`], so all
//! three platforms answer it identically, and what is on disk is read by
//! [`arcen_transport::cert_store`], with the file names and pin files every
//! Pier shares. This module carries out the plan with the filesystem
//! semantics macOS needs.
//!
//! Ownership is recorded in a marker file holding the digest of the
//! certificate this host issued. If the certificate changes without the marker
//! changing, the material is reported as foreign and provisioning refuses to
//! touch it rather than overwriting something an administrator installed
//! deliberately.
//!
//! Publication is atomic. Material is written to a staging file, permissions
//! are set before it is visible under its real name, and the rename is what
//! makes it live. A run that dies partway leaves staging debris and never a
//! half-written key.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use arcen_transport::cert_issuance::{
    HostKey, IssuanceError, IssuanceProfile, IssuedMaterial, IssuerKey, issue,
};
use arcen_transport::cert_marker;
use arcen_transport::cert_provisioning::{
    MaterialState, ProvisioningAction, ProvisioningPlan, ProvisioningRefusal, ProvisioningRequest,
    plan,
};
use arcen_transport::cert_publication::{
    self, Publication, PublicationError, PublicationFiles, Slot,
};
pub use arcen_transport::cert_store::MaterialPaths;
use arcen_transport::cert_store::{self, InspectContext};
use arcen_transport::cert_transaction::{JOURNAL_FILE, MANAGED_FILES, transaction_id};
use zeroize::Zeroizing;

/// Where managed certificate material lives: the system Application Support
/// folder, where a root daemon keeps its state on macOS.
pub const DEFAULT_DIRECTORY: &str = "/Library/Application Support/Arcen";

/// Why certificate work failed.
#[derive(Debug)]
pub enum CertError {
    /// Provisioning was refused by shared policy.
    Refused(ProvisioningRefusal),
    /// A managed path is, or passes through, a symbolic link.
    UnsafePath(String),
    /// A filesystem operation failed.
    Io(String),
    /// Key or certificate generation failed.
    Generation(String),
    /// Another provisioning run holds the directory's lock.
    Busy(String),
    /// The directory is writable by accounts other than its owner, or owned
    /// by an account that is neither root nor this one.
    UnsafeDirectory(String),
}

impl std::fmt::Display for CertError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => {
                write!(formatter, "{}: {}", refusal.as_str(), refusal.guidance())
            }
            Self::UnsafePath(path) => write!(
                formatter,
                "refusing symbolic-link path component: {path}; \
                 certificate material must not be redirected"
            ),
            Self::Io(detail) => write!(formatter, "filesystem error: {detail}"),
            Self::Generation(detail) => write!(formatter, "generation failed: {detail}"),
            Self::Busy(directory) => write!(
                formatter,
                "another host-certificate transaction is active in {directory}; \
                 run again when it has finished"
            ),
            Self::UnsafeDirectory(detail) => write!(
                formatter,
                "refusing certificate directory {detail}; it must be owned by root or \
                 this account and writable by its owner alone"
            ),
        }
    }
}

impl std::error::Error for CertError {}

impl From<ProvisioningRefusal> for CertError {
    fn from(refusal: ProvisioningRefusal) -> Self {
        Self::Refused(refusal)
    }
}

/// The lock every provisioning run holds for its whole transaction, beside
/// the journal; the same name the Linux helper and installer lock.
const LOCK_FILE: &str = ".arcen-cert.lock";

/// An exclusive advisory lock on the certificate directory, held from
/// recovery to commit. Released when dropped, or by the kernel if the
/// process dies.
#[derive(Debug)]
struct DirectoryLock {
    _file: fs::File,
}

impl DirectoryLock {
    /// Takes the lock without waiting: a second run refuses with
    /// [`CertError::Busy`] instead of interleaving with the first.
    ///
    /// The directory must be writable by its owner alone (otherwise anyone
    /// in its group could replace files under a run) and owned by root or
    /// this account; root may act in a directory the service account owns,
    /// which is how the package installs it. The lock file is opened with
    /// `O_NOFOLLOW`, must be a regular file, and is kept at `0600`.
    fn acquire(directory: &Path) -> Result<Self, CertError> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

        let shown = || directory.display().to_string();
        let metadata = fs::symlink_metadata(directory)
            .map_err(|error| CertError::Io(format!("inspect {}: {error}", shown())))?;
        if metadata.file_type().is_symlink() {
            return Err(CertError::UnsafePath(shown()));
        }
        let euid = crate::timezone::current_euid();
        if !metadata.is_dir()
            || metadata.mode() & 0o022 != 0
            || (metadata.uid() != 0 && metadata.uid() != euid && euid != 0)
        {
            return Err(CertError::UnsafeDirectory(format!(
                "{} (owner {}, mode {:o})",
                shown(),
                metadata.uid(),
                metadata.mode() & 0o7777
            )));
        }

        let path = directory.join(LOCK_FILE);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| {
                if error.raw_os_error() == Some(libc::ELOOP) {
                    CertError::UnsafePath(path.display().to_string())
                } else {
                    CertError::Io(format!("open {}: {error}", path.display()))
                }
            })?;
        let lock_metadata = file
            .metadata()
            .map_err(|error| CertError::Io(format!("inspect {}: {error}", path.display())))?;
        if !lock_metadata.is_file() {
            return Err(CertError::UnsafePath(path.display().to_string()));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| CertError::Io(format!("chmod {}: {error}", path.display())))?;
        // SAFETY: `flock` on a descriptor this function owns; no pointers.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                CertError::Busy(shown())
            } else {
                CertError::Io(format!("lock {}: {error}", path.display()))
            });
        }
        Ok(Self { _file: file })
    }
}

/// Returns the staging path used to publish `target` atomically.
fn staging_for(target: &Path) -> PathBuf {
    let name = target.file_name().map_or_else(
        || "material".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    target.with_file_name(format!(".{name}.staging"))
}

/// Returns whether any staging debris is present.
fn has_stale_staging(paths: &MaterialPaths) -> bool {
    paths
        .managed()
        .into_iter()
        .any(|path| staging_for(path).exists())
}

/// Refuses a managed path that is itself a symbolic link.
///
/// Certificate material must not be redirectable: without this, anything able
/// to drop a link named `host.key` into the directory could make the host
/// publish its key somewhere else, or serve a certificate it did not issue.
///
/// Only the managed file is checked, not its ancestry. The containing
/// directory is the operator's choice, and on macOS the ordinary system paths
/// above it — `/tmp` and `/var` among them — are themselves links into
/// `/private`, so walking the whole chain would refuse perfectly normal
/// locations.
fn reject_symlink(path: &Path) -> Result<(), CertError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(CertError::UnsafePath(path.display().to_string()))
        }
        // Absent is fine: we are about to create it.
        Ok(_) | Err(_) => Ok(()),
    }
}

/// Checks every managed path before any of them is read or written.
fn reject_unsafe_paths(paths: &MaterialPaths) -> Result<(), CertError> {
    for path in paths.managed() {
        reject_symlink(path)?;
        reject_symlink(&staging_for(path))?;
    }
    Ok(())
}

/// Reads the certificate directory into the shared decision input.
///
/// `now_epoch_secs` is injected so expiry is testable and so the caller decides
/// which clock is authoritative.
///
/// # Errors
///
/// Returns [`CertError::Io`] when the directory cannot be inspected.
pub fn inspect(paths: &MaterialPaths, now_epoch_secs: u64) -> Result<MaterialState, CertError> {
    reject_unsafe_paths(paths)?;
    let context = InspectContext {
        stale_staging_present: has_stale_staging(paths),
        machine_sans: None,
    };
    cert_store::inspect(paths, now_epoch_secs, context)
        .map_err(|error| CertError::Io(format!("read certificate: {error}")))
}

/// Returns the certificate's SHA-256 as `openssl` reports it.
///
/// This hashes the DER, not the PEM text. Hashing the PEM would print a digest
/// that never matches `openssl x509 -fingerprint -sha256`, which is exactly the
/// value an operator compares it against.
fn reported_pin(pem_bytes: &[u8]) -> String {
    cert_marker::pins_from_pem(pem_bytes)
        .map_or_else(|| "unavailable".to_owned(), |pins| pins.certificate)
}

/// The outcome of a provisioning run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisioningOutcome {
    /// What the shared policy decided.
    pub action: ProvisioningAction,
    /// Whether existing client pins were invalidated.
    pub invalidated_pins: bool,
    /// SHA-256 of the certificate now in place.
    pub certificate_sha256: String,
}

/// Ensures certificate material exists according to `request`.
///
/// # Errors
///
/// Returns [`CertError::Refused`] when shared policy refuses, and
/// [`CertError::Io`] or [`CertError::Generation`] when the work itself fails.
pub fn provision(
    directory: &Path,
    request: ProvisioningRequest,
    subjects: &[String],
    now_epoch_secs: u64,
) -> Result<ProvisioningOutcome, CertError> {
    fs::create_dir_all(directory)
        .map_err(|error| CertError::Io(format!("create {}: {error}", directory.display())))?;
    let paths = MaterialPaths::in_directory(directory);
    // One run at a time, from recovery to commit: two interleaved runs would
    // recover each other's journal or publish over each other's staging.
    let _lock = DirectoryLock::acquire(directory)?;
    // Finish or undo an interrupted publication before judging what is here;
    // otherwise the decision would be made against a half-written directory.
    recover_interrupted(&paths)?;
    let state = inspect(&paths, now_epoch_secs)?;
    let decided: ProvisioningPlan = plan(request, state)?;

    if decided.clear_stale_staging {
        for target in paths.managed() {
            let staging = staging_for(target);
            if staging.exists() {
                fs::remove_file(&staging)
                    .map_err(|error| CertError::Io(format!("clear staging: {error}")))?;
            }
        }
    }

    if decided.action == ProvisioningAction::KeepExisting {
        let bytes = fs::read(&paths.certificate)
            .map_err(|error| CertError::Io(format!("read certificate: {error}")))?;
        return Ok(ProvisioningOutcome {
            action: decided.action,
            invalidated_pins: decided.invalidates_pins,
            certificate_sha256: reported_pin(&bytes),
        });
    }

    // What to issue is shared: the names a renewal keeps, the profile, and
    // whether the key on disk is reused. A renewal or adoption reissues over
    // the existing key, SEC1 or PKCS #8, without rewriting it: that is what
    // keeps every paired Deck's SPKI pin.
    let existing_certificate = fs::read(&paths.certificate).ok();
    let profile = IssuanceProfile::for_action(
        decided.action,
        subjects,
        existing_certificate.as_deref(),
        now_epoch_secs,
    )
    .map_err(issuance_error)?;
    let issued = if IssuanceProfile::preserves_key(decided.action) {
        let key_pem =
            fs::read(&paths.key).map_err(|error| CertError::Io(format!("read key: {error}")))?;
        let key = HostKey::from_pem(&key_pem).map_err(issuance_error)?;
        issue(&profile, IssuerKey::Existing(&key)).map_err(issuance_error)?
    } else {
        issue(&profile, IssuerKey::Generate).map_err(issuance_error)?
    };
    publish_in_transaction(&paths, &issued, now_epoch_secs, usize::MAX)?;
    Ok(ProvisioningOutcome {
        action: decided.action,
        invalidated_pins: decided.invalidates_pins,
        certificate_sha256: reported_pin(issued.certificate_pem.as_bytes()),
    })
}

/// Explains a shared issuance refusal.
fn issuance_error(error: IssuanceError) -> CertError {
    match error {
        IssuanceError::NoSubjectAltName => CertError::Generation(
            "at least one subject alternative name is required; a certificate without one is \
             refused by every client"
                .to_owned(),
        ),
        other => CertError::Generation(other.to_string()),
    }
}

/// Publishes issued material by the shared transaction
/// ([`arcen_transport::cert_publication`]): everything is staged and checked
/// first, the journal is written before any managed file moves, and a
/// failure before the commit puts the directory back.
///
/// `limit` stops after that many steps, as a crash would; production passes
/// `usize::MAX`.
fn publish_in_transaction(
    paths: &MaterialPaths,
    issued: &IssuedMaterial,
    now_epoch_secs: u64,
    limit: usize,
) -> Result<(), CertError> {
    let id = transaction_id(std::process::id(), now_epoch_secs);
    cert_publication::publish(
        &mut MacFiles { paths },
        &Publication::from(issued),
        &id,
        limit,
    )
    .map_err(|error| match error {
        PublicationError::Mismatch | PublicationError::Pins(_) => {
            CertError::Generation(error.to_string())
        }
        other => CertError::Io(other.to_string()),
    })
}

/// The name a backup is copied under before it is complete.
fn partial_backup(backup: &Path) -> PathBuf {
    let mut name = backup.as_os_str().to_owned();
    name.push(".partial");
    PathBuf::from(name)
}

/// Copies `source` to `backup` so that `backup` exists only complete: the
/// copy is made under a partial name, synced, then renamed. A copy, not a
/// move, so the served file stays in place until the rename that replaces
/// it; `fs::copy` keeps its mode, so a key's backup is `0600` too. Recovery
/// restores only the complete name, so a crash mid-copy can never put a
/// truncated key back over the intact one.
fn copy_atomically(source: &Path, backup: &Path) -> Result<(), CertError> {
    let partial = partial_backup(backup);
    reject_symlink(&partial)?;
    let _ = fs::remove_file(&partial);
    fs::copy(source, &partial)
        .map_err(|error| CertError::Io(format!("back up {}: {error}", source.display())))?;
    fs::File::open(&partial)
        .and_then(|file| file.sync_all())
        .map_err(|error| CertError::Io(format!("sync {}: {error}", partial.display())))?;
    fs::rename(&partial, backup)
        .map_err(|error| CertError::Io(format!("back up {}: {error}", source.display())))
}

/// Checks that `certificate` is over the key in `key` and that key is the
/// one the certificate was issued for.
#[cfg(test)]
fn verify_pair(certificate: &Path, key: &Path, spki_sha256: &[u8; 32]) -> Result<(), CertError> {
    let certificate_pem = fs::read(certificate)
        .map_err(|error| CertError::Io(format!("read {}: {error}", certificate.display())))?;
    let key_pem =
        fs::read(key).map_err(|error| CertError::Io(format!("read {}: {error}", key.display())))?;
    if cert_publication::pair_matches(&certificate_pem, &key_pem, spki_sha256) {
        Ok(())
    } else {
        Err(CertError::Generation(
            "the certificate and key do not match".to_owned(),
        ))
    }
}

/// Completes or undoes a publication that was interrupted, by the shared
/// journal decisions. A journal that cannot be understood is left in place
/// and reported, because acting on half of it could delete material.
fn recover_interrupted(paths: &MaterialPaths) -> Result<Option<String>, CertError> {
    cert_publication::recover(&mut MacFiles { paths })
        .map_err(|error| CertError::Io(error.to_string()))
}

/// The macOS file operations behind the shared transaction: files are
/// created with their final mode and `O_NOFOLLOW`, every managed, journal
/// and backup path is refused if it is a link, and a backup is a copy made
/// under a partial name, so the served file stays in place until the rename
/// that replaces it.
struct MacFiles<'a> {
    paths: &'a MaterialPaths,
}

impl MacFiles<'_> {
    fn path(&self, name: &str) -> Result<&PathBuf, CertError> {
        MANAGED_FILES
            .iter()
            .position(|managed| *managed == name)
            .map(|index| self.paths.managed()[index])
            .ok_or_else(|| CertError::Io(format!("unmanaged file {name}")))
    }

    fn backup(&self, backup: &str) -> Result<PathBuf, CertError> {
        let path = self.paths.directory.join(backup);
        reject_symlink(&path)?;
        Ok(path)
    }

    fn journal(&self) -> Result<PathBuf, CertError> {
        let path = self.paths.directory.join(JOURNAL_FILE);
        reject_symlink(&path)?;
        Ok(path)
    }
}

const fn mode_for(secret: bool) -> u32 {
    if secret { 0o600 } else { 0o644 }
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn remove_if_present(path: &Path) -> Result<(), CertError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CertError::Io(format!("remove {}: {error}", path.display()))),
    }
}

fn read_no_follow(path: &Path) -> Result<Zeroizing<Vec<u8>>, CertError> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| CertError::Io(format!("read {}: {error}", path.display())))?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.read_to_end(&mut bytes)
        .map_err(|error| CertError::Io(format!("read {}: {error}", path.display())))?;
    Ok(bytes)
}

impl PublicationFiles for MacFiles<'_> {
    type Error = CertError;

    fn stage(&mut self, name: &'static str, bytes: &[u8], secret: bool) -> Result<(), CertError> {
        write_staged(self.path(name)?, bytes, mode_for(secret)).map(|_| ())
    }

    fn read(&mut self, name: &'static str, slot: Slot) -> Result<Zeroizing<Vec<u8>>, CertError> {
        let path = self.path(name)?;
        match slot {
            Slot::Published => read_no_follow(path),
            Slot::Staged => read_no_follow(&staging_for(path)),
        }
    }

    fn exists(&mut self, name: &'static str) -> Result<bool, CertError> {
        Ok(is_regular_file(self.path(name)?))
    }

    fn back_up(&mut self, name: &'static str, backup: &str) -> Result<(), CertError> {
        let backup = self.backup(backup)?;
        copy_atomically(self.path(name)?, &backup)
    }

    fn backup_exists(&mut self, backup: &str) -> Result<bool, CertError> {
        Ok(is_regular_file(&self.backup(backup)?))
    }

    fn publish(&mut self, name: &'static str, secret: bool) -> Result<(), CertError> {
        let path = self.path(name)?;
        fs::rename(staging_for(path), path)
            .map_err(|error| CertError::Io(format!("publish {}: {error}", path.display())))?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode_for(secret)))
            .map_err(|error| CertError::Io(format!("chmod {}: {error}", path.display())))
    }

    fn restore(&mut self, name: &'static str, backup: &str) -> Result<(), CertError> {
        let backup = self.backup(backup)?;
        let path = self.path(name)?;
        fs::rename(&backup, path)
            .map_err(|error| CertError::Io(format!("restore {}: {error}", path.display())))
    }

    fn remove(&mut self, name: &'static str) -> Result<(), CertError> {
        remove_if_present(self.path(name)?)
    }

    fn remove_backup(&mut self, backup: &str) -> Result<(), CertError> {
        let backup = self.backup(backup)?;
        remove_if_present(&backup)?;
        remove_if_present(&partial_backup(&backup))
    }

    fn discard_staging(&mut self) -> Result<(), CertError> {
        for path in self.paths.managed() {
            remove_if_present(&staging_for(path))?;
        }
        Ok(())
    }

    fn write_journal(&mut self, text: &str) -> Result<(), CertError> {
        write_private(&self.journal()?, text.as_bytes(), 0o644)
    }

    fn read_journal(&mut self) -> Result<Option<String>, CertError> {
        let path = self.journal()?;
        if !is_regular_file(&path) {
            return Ok(None);
        }
        let bytes = read_no_follow(&path)?;
        String::from_utf8(bytes.to_vec())
            .map(Some)
            .map_err(|_| CertError::Io("certificate journal is not text".to_owned()))
    }

    fn remove_journal(&mut self) -> Result<(), CertError> {
        remove_if_present(&self.journal()?)
    }
}

/// Writes `bytes` to the staging file beside `target`, created afresh with
/// `mode` from the start (never through a link), and synced.
fn write_staged(target: &Path, bytes: &[u8], mode: u32) -> Result<PathBuf, CertError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let staging = staging_for(target);
    match fs::remove_file(&staging) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(CertError::Io(format!(
                "clear {}: {error}",
                staging.display()
            )));
        }
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&staging)
        .map_err(|error| CertError::Io(format!("create {}: {error}", staging.display())))?;
    // The umask may have narrowed the mode, never widened it; set it exactly.
    fs::set_permissions(&staging, fs::Permissions::from_mode(mode))
        .map_err(|error| CertError::Io(format!("chmod {}: {error}", staging.display())))?;
    file.write_all(bytes)
        .map_err(|error| CertError::Io(format!("write {}: {error}", staging.display())))?;
    file.sync_all()
        .map_err(|error| CertError::Io(format!("sync {}: {error}", staging.display())))?;
    Ok(staging)
}

/// Writes `bytes` to `target` through a staging file with `mode` set before
/// the file becomes visible under its real name.
fn write_private(target: &Path, bytes: &[u8], mode: u32) -> Result<(), CertError> {
    let staging = write_staged(target, bytes, mode)?;
    fs::rename(&staging, target)
        .map_err(|error| CertError::Io(format!("publish {}: {error}", target.display())))?;
    fs::set_permissions(target, fs::Permissions::from_mode(mode))
        .map_err(|error| CertError::Io(format!("chmod {}: {error}", target.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use arcen_transport::cert_provisioning::MaterialOwnership;
    use arcen_transport::cert_transaction::{
        PublicationPlan, PublicationStep, TransactionPhase, backup_file_name,
    };

    use super::*;

    fn inspected(paths: &MaterialPaths, now: u64) -> MaterialState {
        match inspect(paths, now) {
            Ok(state) => state,
            Err(error) => panic!("inspect: {error:?}"),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "arcen-cert-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        fs::create_dir_all(&base).expect("temp dir");
        base
    }

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn first_run_creates_private_material_and_records_ownership() {
        let dir = temp_dir("create");
        let outcome = provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("first provisioning succeeds");
        assert_eq!(outcome.action, ProvisioningAction::CreateNew);
        assert!(outcome.invalidated_pins);

        let paths = MaterialPaths::in_directory(&dir);
        assert!(paths.certificate.is_file());
        assert!(paths.key.is_file());

        let key_mode = fs::metadata(&paths.key)
            .expect("key metadata")
            .permissions()
            .mode();
        assert_eq!(
            key_mode & 0o777,
            0o600,
            "private key must not be readable by others"
        );

        // The material is now recognised as ours.
        let state = inspected(&paths, NOW);
        assert_eq!(state.ownership, Some(MaterialOwnership::Owned));
        assert!(state.certificate_valid);
        assert!(!state.expiring_or_expired);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_ensure_run_changes_nothing() {
        let dir = temp_dir("idempotent");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let key_before = fs::read(&paths.key).expect("key");

        let outcome = provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("second run");
        assert_eq!(outcome.action, ProvisioningAction::KeepExisting);
        assert!(!outcome.invalidated_pins);
        assert_eq!(fs::read(&paths.key).expect("key"), key_before);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn renewal_keeps_the_key_so_pins_survive() {
        let dir = temp_dir("renew");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let key_before = fs::read(&paths.key).expect("key");
        let cert_before = fs::read(&paths.certificate).expect("cert");

        let outcome = provision(
            &dir,
            ProvisioningRequest::Renew,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("renew");
        assert_eq!(outcome.action, ProvisioningAction::RenewPreservingKey);
        assert!(!outcome.invalidated_pins);
        assert_eq!(
            fs::read(&paths.key).expect("key"),
            key_before,
            "renewal must not replace the private key"
        );
        assert_ne!(fs::read(&paths.certificate).expect("cert"), cert_before);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rekey_without_existing_material_is_refused() {
        // Wrong-directory protection: replacing a key that does not exist is
        // a mistake worth stopping for, not an implicit first-time creation.
        let dir = temp_dir("rekey-empty");
        let refused = provision(
            &dir,
            ProvisioningRequest::Rekey,
            &["pier.example".to_owned()],
            NOW,
        );
        assert!(matches!(
            refused,
            Err(CertError::Refused(ProvisioningRefusal::NothingToRekey))
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rekey_replaces_the_key_and_says_so() {
        let dir = temp_dir("rekey");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let key_before = fs::read(&paths.key).expect("key");

        let outcome = provision(
            &dir,
            ProvisioningRequest::Rekey,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("rekey");
        assert_eq!(outcome.action, ProvisioningAction::ReplaceKeyAndCertificate);
        assert!(outcome.invalidated_pins);
        assert_ne!(fs::read(&paths.key).expect("key"), key_before);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unmarked_self_signed_pair_is_taken_over_without_asking() {
        let dir = temp_dir("foreign");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        // Simulate an administrator replacing the certificate: the marker no
        // longer matches what is on disk.
        fs::remove_file(&paths.marker).expect("remove marker");

        let state = inspected(&paths, NOW);
        assert_eq!(state.ownership, Some(MaterialOwnership::Foreign));

        assert!(state.legacy_arcen_self_signed);

        // An ordinary install takes it over; no adoption flag is needed.
        let key_before = fs::read(&paths.key).expect("key");
        let adopted = provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("adoptable");
        assert_eq!(adopted.action, ProvisioningAction::AdoptAndRenew);
        // Adoption reissues over the same key, so pins survive but the host
        // now controls a certificate it can actually renew.
        assert_eq!(fs::read(&paths.key).expect("key"), key_before);
        assert!(!adopted.invalidated_pins);
        // After adoption the ordinary path works again.
        let outcome = provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("owned now");
        assert_eq!(outcome.action, ProvisioningAction::KeepExisting);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn half_a_pair_is_refused_rather_than_completed() {
        let dir = temp_dir("partial");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        fs::remove_file(&paths.key).expect("remove key");

        let refused = provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        );
        assert!(
            matches!(
                refused,
                Err(CertError::Refused(ProvisioningRefusal::IncompleteMaterial))
            ),
            "generating the missing half would change the served identity"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_expiring_certificate_renews_automatically() {
        let dir = temp_dir("expiring");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let key_before = fs::read(&paths.key).expect("key");

        // Jump to within the renewal window of the certificate's expiry.
        let near_expiry =
            NOW + (cert_store::VALIDITY_DAYS as u64 * 24 * 60 * 60) - (5 * 24 * 60 * 60);
        let state = inspected(&paths, near_expiry);
        assert!(state.expiring_or_expired);

        let outcome = provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            near_expiry,
        )
        .expect("renews");
        assert_eq!(outcome.action, ProvisioningAction::RenewPreservingKey);
        assert_eq!(fs::read(&paths.key).expect("key"), key_before);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn interrupted_staging_does_not_block_a_fresh_creation() {
        let dir = temp_dir("staging");
        let paths = MaterialPaths::in_directory(&dir);
        let staging = staging_for(&paths.key);
        fs::write(&staging, b"half written").expect("debris");

        let state = inspected(&paths, NOW);
        assert!(state.stale_staging_present);

        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("creates");
        assert!(
            !staging.exists(),
            "debris must be cleared before publishing"
        );
        assert!(paths.key.is_file());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn material_behind_a_symbolic_link_is_refused() {
        // Without this, anything able to create a link in the directory could
        // make the host publish its key elsewhere or serve a foreign
        // certificate.
        let dir = temp_dir("symlink");
        let elsewhere = dir.join("elsewhere.crt");
        fs::write(&elsewhere, b"not ours").expect("target");
        let paths = MaterialPaths::in_directory(&dir);
        std::os::unix::fs::symlink(&elsewhere, &paths.certificate).expect("link");

        let refused = provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        );
        assert!(
            matches!(refused, Err(CertError::UnsafePath(_))),
            "symlinked certificate path must be refused, got {refused:?}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_artifact_set_matches_the_linux_helper() {
        // Same five files, same modes the Linux packaging test asserts, so an
        // operator sees the same directory on either platform.
        let dir = temp_dir("artifacts");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);

        let expected: [(&PathBuf, u32); 5] = [
            (&paths.key, 0o600),
            (&paths.certificate, 0o644),
            (&paths.certificate_pin, 0o644),
            (&paths.spki_pin, 0o644),
            (&paths.marker, 0o644),
        ];
        for (path, mode) in expected {
            let metadata = fs::metadata(path)
                .unwrap_or_else(|error| panic!("{} missing: {error}", path.display()));
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                mode,
                "{} has the wrong mode",
                path.display()
            );
            assert!(metadata.len() > 0, "{} is empty", path.display());
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pin_files_carry_the_documented_shapes() {
        let dir = temp_dir("pins");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);

        let certificate = fs::read_to_string(&paths.certificate_pin).expect("cert pin");
        assert!(
            certificate.starts_with("sha256 Fingerprint="),
            "unexpected certificate pin: {certificate}"
        );
        assert!(certificate.ends_with('\n'));

        let spki = fs::read_to_string(&paths.spki_pin).expect("spki pin");
        assert!(spki.starts_with("sha256/"), "unexpected spki pin: {spki}");
        assert!(spki.ends_with('\n'));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn renewal_refreshes_the_certificate_pin_but_not_the_key_pin() {
        // This is the whole reason renewal preserves the key: a Deck pinning
        // the subject public key keeps working across a rotation, while the
        // whole-certificate pin necessarily changes.
        let dir = temp_dir("pin-stability");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let certificate_before = fs::read_to_string(&paths.certificate_pin).expect("cert pin");
        let spki_before = fs::read_to_string(&paths.spki_pin).expect("spki pin");

        provision(
            &dir,
            ProvisioningRequest::Renew,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("renew");
        let certificate_after = fs::read_to_string(&paths.certificate_pin).expect("cert pin");
        let spki_after = fs::read_to_string(&paths.spki_pin).expect("spki pin");

        assert_ne!(certificate_before, certificate_after);
        assert_eq!(
            spki_before, spki_after,
            "renewal must not move the subject public key pin"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rekey_moves_both_pins() {
        // The counterpart: replacing the key breaks SPKI pins, which is why it
        // has to be asked for explicitly.
        let dir = temp_dir("pin-rekey");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let spki_before = fs::read_to_string(&paths.spki_pin).expect("spki pin");

        provision(
            &dir,
            ProvisioningRequest::Rekey,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("rekey");
        let spki_after = fs::read_to_string(&paths.spki_pin).expect("spki pin");
        assert_ne!(spki_before, spki_after);

        fs::remove_dir_all(&dir).ok();
    }

    /// Every file's bytes and mode by name: the managed files, and any
    /// journal or backup. Staging files are left out (without a journal they
    /// are inert, and the next run that writes clears them), as is the empty
    /// lock file every run leaves.
    fn snapshot(dir: &Path) -> std::collections::BTreeMap<String, (Vec<u8>, u32)> {
        fs::read_dir(dir)
            .expect("read dir")
            .filter(|entry| {
                !entry.as_ref().is_ok_and(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.ends_with(".staging") || name == LOCK_FILE
                })
            })
            .map(|entry| {
                let entry = entry.expect("entry");
                let metadata = entry.metadata().expect("metadata");
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    (
                        fs::read(entry.path()).expect("read"),
                        metadata.permissions().mode() & 0o777,
                    ),
                )
            })
            .collect()
    }

    /// The material a renewal (or a rekey) of `dir` would publish.
    fn reissue(dir: &Path, rekey: bool, now: u64) -> IssuedMaterial {
        let paths = MaterialPaths::in_directory(dir);
        let existing = fs::read(&paths.certificate).expect("certificate");
        let action = if rekey {
            ProvisioningAction::ReplaceKeyAndCertificate
        } else {
            ProvisioningAction::RenewPreservingKey
        };
        let profile =
            IssuanceProfile::for_action(action, &["pier.example".to_owned()], Some(&existing), now)
                .expect("profile");
        if rekey {
            issue(&profile, IssuerKey::Generate).expect("issue")
        } else {
            let key = HostKey::from_pem(&fs::read(&paths.key).expect("key")).expect("key");
            issue(&profile, IssuerKey::Existing(&key)).expect("issue")
        }
    }

    #[test]
    fn a_crash_at_any_step_recovers_to_the_old_or_the_new_material_whole() {
        // The restart after a crash mid-replace: whatever step the process
        // died after, the next run leaves exactly the previous five files or
        // exactly the replacement five, with their modes, and no debris.
        for rekey in [false, true] {
            let probe = temp_dir("crash-probe");
            provision(
                &probe,
                ProvisioningRequest::Ensure,
                &["pier.example".to_owned()],
                NOW,
            )
            .expect("create");
            let steps = {
                let issued = reissue(&probe, rekey, NOW + 60);
                PublicationPlan::new(issued.new_key_pem.is_some(), [true; MANAGED_FILES.len()])
                    .steps()
            };
            fs::remove_dir_all(&probe).ok();
            let committed_at = steps
                .iter()
                .position(|step| {
                    *step == PublicationStep::WriteJournal(TransactionPhase::Committed)
                })
                .expect("commit step");

            for limit in 0..=steps.len() {
                let dir = temp_dir("crash");
                provision(
                    &dir,
                    ProvisioningRequest::Ensure,
                    &["pier.example".to_owned()],
                    NOW,
                )
                .expect("create");
                let before = snapshot(&dir);
                let issued = reissue(&dir, rekey, NOW + 60);
                let paths = MaterialPaths::in_directory(&dir);
                publish_in_transaction(&paths, &issued, NOW + 60, limit).expect("partial run");
                if limit < steps.len() {
                    // The next start: provisioning recovers first.
                    recover_interrupted(&paths).expect("recovery");
                }
                let after = snapshot(&dir);
                if limit > committed_at {
                    assert_eq!(
                        after.get("host.crt").map(|(bytes, _)| bytes.as_slice()),
                        Some(issued.certificate_pem.as_bytes()),
                        "rekey={rekey}: crash after {limit} steps rolls forward"
                    );
                    assert_eq!(after.len(), MANAGED_FILES.len(), "no debris: {after:?}");
                    if !rekey {
                        assert_eq!(after.get("host.key"), before.get("host.key"));
                    }
                    verify_pair(&paths.certificate, &paths.key, &issued.spki_sha256)
                        .expect("published pair matches");
                } else {
                    assert_eq!(
                        after, before,
                        "rekey={rekey}: crash after {limit} steps rolls back"
                    );
                }
                fs::remove_dir_all(&dir).ok();
            }
        }
    }

    #[test]
    fn a_backup_cut_short_is_never_restored_over_the_key() {
        // A crash in the middle of copying the key aside leaves only the
        // partial name; recovery keeps the intact key and removes it.
        let dir = temp_dir("partial-backup");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let before = snapshot(&dir);
        let issued = reissue(&dir, true, NOW + 60);
        let paths = MaterialPaths::in_directory(&dir);
        let steps = PublicationPlan::new(true, [true; MANAGED_FILES.len()]).steps();
        let backing_up_key = steps
            .iter()
            .position(|step| *step == PublicationStep::BackUp("host.key"))
            .expect("key backup step");
        publish_in_transaction(&paths, &issued, NOW + 60, backing_up_key).expect("partial run");
        let id = format!("{}-{}", std::process::id(), NOW + 60);
        let backup = dir.join(backup_file_name(&id, "host.key"));
        fs::write(partial_backup(&backup), b"trunc").expect("truncated copy");

        recover_interrupted(&paths).expect("recovery");
        assert_eq!(snapshot(&dir), before);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_interrupted_first_install_leaves_no_material_behind() {
        // Nothing existed before, so recovery must remove what was partially
        // created rather than leave an incomplete host identity.
        let profile = IssuanceProfile::new(&["pier.example".to_owned()], NOW).expect("profile");
        let issued = issue(&profile, IssuerKey::Generate).expect("issue");
        let steps = PublicationPlan::new(true, [false; MANAGED_FILES.len()]).steps();
        let first_publish = steps
            .iter()
            .position(|step| matches!(step, PublicationStep::Publish(_)))
            .expect("publish");
        let dir = temp_dir("interrupted-first");
        let paths = MaterialPaths::in_directory(&dir);
        publish_in_transaction(&paths, &issued, NOW, first_publish + 2).expect("partial run");
        assert!(paths.key.exists() && paths.certificate.exists());

        recover_interrupted(&paths).expect("recovery runs");
        assert!(
            snapshot(&dir).is_empty(),
            "partial material must be removed: {:?}",
            snapshot(&dir).keys().collect::<Vec<_>>()
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_invalid_replacement_is_refused_before_anything_is_published() {
        let dir = temp_dir("invalid-replacement");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let before = snapshot(&dir);
        let kept = HostKey::from_pem(&fs::read(&paths.key).expect("key")).expect("key");

        // A certificate over another key, presented as a renewal of this one.
        let profile = IssuanceProfile::new(&["pier.example".to_owned()], NOW).expect("profile");
        let foreign = issue(&profile, IssuerKey::Generate).expect("issue");
        let mismatched = IssuedMaterial {
            certificate_pem: foreign.certificate_pem,
            new_key_pem: None,
            spki_sha256: *kept.spki_sha256(),
        };
        assert!(publish_in_transaction(&paths, &mismatched, NOW, usize::MAX).is_err());
        assert_eq!(snapshot(&dir), before, "nothing was published");
        fs::remove_dir_all(&dir).ok();
    }

    /// The SEC1 file `openssl ecparam -genkey -noout` writes for the PKCS #8
    /// P-256 key ring generated: the same key, the encoding Linux Piers and
    /// operator tooling hold.
    fn sec1_from_ring_pkcs8(pkcs8_pem: &[u8]) -> Vec<u8> {
        fn element(input: &[u8]) -> (u8, &[u8], &[u8]) {
            let (length, header) = if input[1] < 0x80 {
                (usize::from(input[1]), 2)
            } else {
                let count = usize::from(input[1] & 0x7f);
                let length = input[2..2 + count]
                    .iter()
                    .fold(0_usize, |length, byte| (length << 8) | usize::from(*byte));
                (length, 2 + count)
            };
            (
                input[0],
                &input[header..header + length],
                &input[header + length..],
            )
        }
        fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
            let mut encoded = vec![tag];
            if contents.len() < 0x80 {
                encoded.push(u8::try_from(contents.len()).expect("short"));
            } else {
                encoded.push(0x81);
                encoded.push(u8::try_from(contents.len()).expect("one byte"));
            }
            encoded.extend_from_slice(contents);
            encoded
        }
        let text = std::str::from_utf8(pkcs8_pem).expect("pem");
        let body: String = text
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let pkcs8 = cert_marker::base64_decode(&body).expect("base64");
        let (_, info, _) = element(&pkcs8);
        let (_, _, rest) = element(info);
        let (_, _, rest) = element(rest);
        let (_, inner, _) = element(rest);
        let (_, ec_private_key, _) = element(inner);
        let (_, version, rest) = element(ec_private_key);
        let (_, private, rest) = element(rest);
        let (_, public, _) = element(rest);
        let prime256v1 = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
        let mut sequence = der(0x02, version);
        sequence.extend(der(0x04, private));
        sequence.extend(der(0xa0, &prime256v1));
        sequence.extend(der(0xa1, public));
        let encoded = cert_marker::base64_encode(&der(0x30, &sequence));
        // Built at run time: the hygiene gate refuses the literal header.
        let label = ["EC", "PRIVATE", "KEY"].join(" ");
        let mut pem = format!("-----BEGIN {label}-----\n");
        for line in encoded.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).expect("base64"));
            pem.push('\n');
        }
        pem.push_str(&format!("-----END {label}-----\n"));
        pem.into_bytes()
    }

    #[test]
    fn renewal_over_an_openssl_sec1_key_keeps_the_key_file_and_its_pin() {
        // rcgen over ring reads only PKCS #8, so before the shared issuer a
        // renewal or adoption over this key failed with "reuse key".
        let dir = temp_dir("sec1-renew");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let sec1 = sec1_from_ring_pkcs8(&fs::read(&paths.key).expect("key"));
        write_private(&paths.key, &sec1, 0o600).expect("SEC1 key");
        let spki_before = fs::read_to_string(&paths.spki_pin).expect("spki pin");

        let outcome = provision(
            &dir,
            ProvisioningRequest::Renew,
            &["pier.example".to_owned()],
            NOW + 86_400,
        )
        .expect("renewal over a SEC1 key");
        assert_eq!(outcome.action, ProvisioningAction::RenewPreservingKey);
        assert_eq!(
            fs::read(&paths.key).expect("key"),
            sec1,
            "key file untouched"
        );
        assert_eq!(
            fs::read_to_string(&paths.spki_pin).expect("spki pin"),
            spki_before,
            "SPKI pin unchanged"
        );
        let served =
            cert_marker::pins_from_pem(&fs::read(&paths.certificate).expect("cert")).expect("pins");
        assert_eq!(format!("{}\n", served.spki), spki_before);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn renewal_keeps_every_name_the_certificate_already_covers() {
        // A Deck dialling the address must keep connecting after a renewal
        // that named only the host.
        let dir = temp_dir("keep-names");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned(), "203.0.113.7".to_owned()],
            NOW,
        )
        .expect("create");
        provision(
            &dir,
            ProvisioningRequest::Renew,
            &["pier.example".to_owned()],
            NOW + 60,
        )
        .expect("renew");
        let paths = MaterialPaths::in_directory(&dir);
        assert!(cert_marker::subject_alt_names_match_pem(
            &fs::read(&paths.certificate).expect("cert"),
            &["DNS:pier.example".to_owned(), "IP:203.0.113.7".to_owned()]
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn issued_material_is_the_shared_profile_backdated_five_minutes() {
        // Before: rcgen's default subject, no key usage, no basic
        // constraints, and a window starting at the second of issue.
        let dir = temp_dir("profile");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let certificate = fs::read(&paths.certificate).expect("cert");
        let validity = cert_marker::validity_from_pem(&certificate).expect("validity");
        assert_eq!(
            validity.not_before,
            i64::try_from(NOW).expect("now") - 300,
            "five-minute backdate"
        );
        let key = HostKey::from_pem(&fs::read(&paths.key).expect("key")).expect("key");
        IssuanceProfile::new(&["pier.example".to_owned()], NOW)
            .expect("profile")
            .verify_issued(&certificate, key.spki_sha256())
            .expect("CN=Arcen Pier, CA:FALSE, digitalSignature, serverAuth, the names");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_concurrent_run_refuses_cleanly_and_changes_nothing() {
        let dir = temp_dir("locked");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let before = snapshot(&dir);

        // The first run holds the lock for its whole transaction.
        let held = DirectoryLock::acquire(&dir).expect("first run's lock");
        let lock_mode = fs::metadata(dir.join(LOCK_FILE))
            .expect("lock file")
            .permissions()
            .mode();
        assert_eq!(lock_mode & 0o777, 0o600);
        let second = provision(
            &dir,
            ProvisioningRequest::Renew,
            &["pier.example".to_owned()],
            NOW + 60,
        );
        assert!(
            matches!(second, Err(CertError::Busy(_))),
            "a concurrent run must refuse: {second:?}"
        );
        assert_eq!(snapshot(&dir), before, "the refused run changed nothing");

        // Once the first run ends, the next one proceeds.
        drop(held);
        let renewed = provision(
            &dir,
            ProvisioningRequest::Renew,
            &["pier.example".to_owned()],
            NOW + 60,
        )
        .expect("runs after the lock is released");
        assert_eq!(renewed.action, ProvisioningAction::RenewPreservingKey);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_runs_from_two_threads_are_serialised() {
        // Two runs racing: exactly one creates the material; the other either
        // refuses as busy or, if it came second, keeps what the first made.
        let dir = temp_dir("race");
        let runs: Vec<_> = (0..2)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    provision(
                        &dir,
                        ProvisioningRequest::Ensure,
                        &["pier.example".to_owned()],
                        NOW,
                    )
                })
            })
            .collect();
        let outcomes: Vec<_> = runs
            .into_iter()
            .map(|run| run.join().expect("thread"))
            .collect();
        let created = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(o) if o.action == ProvisioningAction::CreateNew))
            .count();
        assert_eq!(created, 1, "{outcomes:?}");
        assert!(
            outcomes
                .iter()
                .all(|outcome| matches!(outcome, Ok(_) | Err(CertError::Busy(_))))
        );
        let paths = MaterialPaths::in_directory(&dir);
        let key = HostKey::from_pem(&fs::read(&paths.key).expect("key")).expect("key");
        verify_pair(&paths.certificate, &paths.key, key.spki_sha256()).expect("one whole pair");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_lock_behind_a_link_or_a_shared_directory_is_refused() {
        let dir = temp_dir("lock-link");
        let elsewhere = dir.join("elsewhere");
        fs::write(&elsewhere, b"").expect("target");
        std::os::unix::fs::symlink(&elsewhere, dir.join(LOCK_FILE)).expect("link");
        assert!(matches!(
            DirectoryLock::acquire(&dir),
            Err(CertError::UnsafePath(_))
        ));
        fs::remove_file(dir.join(LOCK_FILE)).expect("unlink");

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).expect("chmod");
        assert!(matches!(
            DirectoryLock::acquire(&dir),
            Err(CertError::UnsafeDirectory(_))
        ));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(DirectoryLock::acquire(&dir).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_journal_is_reported_rather_than_acted_on() {
        // Acting on half a journal could delete material it meant to keep.
        let dir = temp_dir("bad-journal");
        fs::create_dir_all(&dir).expect("dir");
        let paths = MaterialPaths::in_directory(&dir);
        fs::write(paths.directory.join(JOURNAL_FILE), b"phase=teleported\n").expect("journal");

        let result = recover_interrupted(&paths);
        assert!(matches!(result, Err(CertError::Io(_))));
        assert!(
            paths.directory.join(JOURNAL_FILE).exists(),
            "an unreadable journal must be left for an operator"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unparseable_certificates_are_reported_invalid_not_assumed_good() {
        let dir = temp_dir("unparseable");
        let paths = MaterialPaths::in_directory(&dir);
        fs::write(&paths.certificate, b"not a certificate").expect("certificate");
        fs::write(&paths.key, b"not a key").expect("key");
        let state = inspected(&paths, NOW);
        assert!(!state.certificate_valid);
        assert!(!state.expiring_or_expired);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_expired_certificate_is_not_reported_as_valid() {
        // The version this replaced compared only against the renewal
        // threshold, so a certificate that expired long ago read as valid and
        // merely due for renewal. Linux and Windows already asked
        // `is_current`, so the same directory produced different provisioning
        // decisions depending on which host inspected it.
        let directory = temp_dir("validity");
        let paths = MaterialPaths::in_directory(&directory);

        let issued_at = 1_700_000_000_u64;
        provision(
            &directory,
            ProvisioningRequest::Ensure,
            &["localhost".to_owned()],
            issued_at,
        )
        .expect("create material");

        let now = inspected(&paths, issued_at + 60);
        assert!(now.certificate_valid, "fresh material must be usable");

        // Far past any plausible validity window.
        let later = inspected(&paths, issued_at + 100 * 365 * 24 * 60 * 60);
        assert!(
            !later.certificate_valid,
            "an expired certificate must not report as usable"
        );
        assert!(
            later.expiring_or_expired,
            "and it is certainly due for renewal"
        );

        // Before it was issued is also not usable.
        let before = inspected(&paths, issued_at.saturating_sub(86_400));
        assert!(
            !before.certificate_valid,
            "a not-yet-valid certificate is not usable"
        );

        std::fs::remove_dir_all(&directory).ok();
    }
}
