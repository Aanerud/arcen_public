//! macOS host certificate material.
//!
//! The decision of what to do — create, keep, renew, rekey or adopt — is not
//! made here. It comes from [`arcen_transport::cert_provisioning`], so all
//! three platforms answer it identically. This module only reports what is on
//! disk and carries out the plan with the filesystem semantics macOS needs.
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

use arcen_transport::cert_marker::{self, OwnershipMarker};
use arcen_transport::cert_provisioning::{
    MaterialOwnership, MaterialState, ProvisioningAction, ProvisioningPlan, ProvisioningRefusal,
    ProvisioningRequest, plan,
};
use arcen_transport::cert_transaction::{
    FileBefore, FileRecovery, MANAGED_FILES, TransactionJournal, TransactionPhase,
};

/// Where managed certificate material lives.
pub const DEFAULT_DIRECTORY: &str = "/Library/Application Support/Arcen";
/// Certificate file name.
const CERTIFICATE_FILE: &str = "host.crt";
/// Private key file name.
const KEY_FILE: &str = "host.key";
/// Ownership marker file name.
/// Ownership marker file name. Matches the Linux helper so the two
/// implementations read each other's material rather than each treating the
/// other's as foreign.
const MARKER_FILE: &str = "host.generated-by-arcen";
/// Whole-certificate pin, for operators to compare against `openssl`.
const CERT_PIN_FILE: &str = "host.cert-sha256";
/// Subject public key pin, the value a pinning client compares.
const SPKI_PIN_FILE: &str = "host.spki-sha256";
/// Journal recording an in-flight publication so an interrupted run can be
/// undone or completed rather than left half-applied.
const JOURNAL_FILE: &str = ".arcen-cert.transaction";
/// Certificate lifetime. Short enough that a stale host is noticed, long
/// enough that renewal is not a constant chore.
const VALIDITY_DAYS: i64 = 825;
/// How close to expiry counts as needing renewal.
const RENEW_WITHIN_DAYS: i64 = 30;

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
        }
    }
}

impl std::error::Error for CertError {}

impl From<ProvisioningRefusal> for CertError {
    fn from(refusal: ProvisioningRefusal) -> Self {
        Self::Refused(refusal)
    }
}

/// Paths to the managed material in one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialPaths {
    /// Certificate path.
    pub certificate: PathBuf,
    /// Private key path.
    pub key: PathBuf,
    /// Ownership marker path.
    pub marker: PathBuf,
    /// Whole-certificate pin path.
    pub certificate_pin: PathBuf,
    /// Subject public key pin path.
    pub spki_pin: PathBuf,
    /// The directory holding all of it.
    pub directory: PathBuf,
}

impl MaterialPaths {
    /// Returns the managed paths inside `directory`.
    #[must_use]
    pub fn in_directory(directory: &Path) -> Self {
        Self {
            certificate: directory.join(CERTIFICATE_FILE),
            key: directory.join(KEY_FILE),
            marker: directory.join(MARKER_FILE),
            certificate_pin: directory.join(CERT_PIN_FILE),
            spki_pin: directory.join(SPKI_PIN_FILE),
            directory: directory.to_path_buf(),
        }
    }

    /// Returns every file the transaction manages, in the shared order.
    fn managed(&self) -> [&PathBuf; 5] {
        [
            &self.key,
            &self.certificate,
            &self.certificate_pin,
            &self.spki_pin,
            &self.marker,
        ]
    }

    /// Returns the staging path used to publish `target` atomically.
    #[must_use]
    fn staging_for(target: &Path) -> PathBuf {
        let name = target.file_name().map_or_else(
            || "material".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        target.with_file_name(format!(".{name}.staging"))
    }

    /// Returns whether any staging debris is present.
    #[must_use]
    fn has_stale_staging(&self) -> bool {
        self.managed()
            .into_iter()
            .any(|path| Self::staging_for(path).exists())
    }
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
        reject_symlink(&MaterialPaths::staging_for(path))?;
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
    let certificate_present = paths.certificate.is_file();
    let key_present = paths.key.is_file();
    if !certificate_present && !key_present {
        return Ok(MaterialState {
            stale_staging_present: paths.has_stale_staging(),
            ..MaterialState::absent()
        });
    }

    let certificate_bytes = if certificate_present {
        Some(
            fs::read(&paths.certificate)
                .map_err(|error| CertError::Io(format!("read certificate: {error}")))?,
        )
    } else {
        None
    };

    let ownership = certificate_bytes.as_ref().map(|bytes| {
        if !paths.marker.exists() {
            MaterialOwnership::Foreign
        } else if marker_matches(paths, bytes) {
            MaterialOwnership::Owned
        } else {
            MaterialOwnership::Ambiguous
        }
    });

    let (certificate_valid, expiring_or_expired) =
        certificate_bytes.as_ref().map_or((false, false), |bytes| {
            inspect_validity(bytes, now_epoch_secs)
        });

    Ok(MaterialState {
        certificate_present,
        key_present,
        ownership,
        certificate_valid,
        expiring_or_expired,
        stale_staging_present: paths.has_stale_staging(),
        legacy_arcen_self_signed: certificate_bytes.as_ref().is_some_and(|bytes| {
            arcen_transport::cert_marker::is_legacy_arcen_self_signed_pem(
                bytes,
                legacy_arcen_evidence(paths, bytes),
            )
        }),
    })
}

/// Returns whether the ownership marker describes the certificate on disk.
///
/// Both the certificate and subject-public-key pins must match. A marker left
/// in place while the certificate underneath it changed does not count as
/// ownership.
fn marker_matches(paths: &MaterialPaths, certificate_bytes: &[u8]) -> bool {
    let Ok(recorded) = fs::read_to_string(&paths.marker) else {
        return false;
    };
    let Ok(marker) = OwnershipMarker::parse(&recorded) else {
        return false;
    };
    let Some(pins) = cert_marker::pins_from_pem(certificate_bytes) else {
        return false;
    };
    marker.matches(&pins.certificate, &pins.spki)
}

fn legacy_arcen_evidence(
    paths: &MaterialPaths,
    certificate_bytes: &[u8],
) -> arcen_transport::cert_marker::LegacyArcenEvidence {
    let companion_pins_match = fs::read_to_string(&paths.certificate_pin)
        .ok()
        .zip(fs::read_to_string(&paths.spki_pin).ok())
        .is_some_and(|(certificate_pin, spki_pin)| {
            arcen_transport::cert_marker::companion_pins_match_pem(
                certificate_bytes,
                &certificate_pin,
                &spki_pin,
            )
        });
    arcen_transport::cert_marker::LegacyArcenEvidence {
        arcen_tls_directory: true,
        companion_pins_match,
        machine_sans_match: false,
    }
}

/// Reads a certificate's validity through the shared contract.
///
/// Returns `(usable_now, renewal_due)`.
///
/// This delegates rather than parsing again. The version that lived here had
/// drifted: it compared only against the renewal threshold, so a certificate
/// that expired last year reported as valid. Linux and Windows already asked
/// `ValidityWindow::is_current`, which is the kind of divergence that makes
/// the same directory produce different provisioning decisions on different
/// hosts.
fn inspect_validity(pem_bytes: &[u8], now_epoch_secs: u64) -> (bool, bool) {
    let Some(window) = cert_marker::validity_from_pem(pem_bytes) else {
        // Material this host cannot inspect is not material it will keep
        // serving.
        return (false, false);
    };
    let now = i64::try_from(now_epoch_secs).unwrap_or(i64::MAX);
    let renew_threshold = RENEW_WITHIN_DAYS * 24 * 60 * 60;
    (
        window.is_current(now),
        window.is_due_for_renewal(now, renew_threshold),
    )
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
    // Finish or undo an interrupted publication before judging what is here;
    // otherwise the decision would be made against a half-written directory.
    recover_interrupted(&paths)?;
    let state = inspect(&paths, now_epoch_secs)?;
    let decided: ProvisioningPlan = plan(request, state)?;

    if decided.clear_stale_staging {
        for target in paths.managed() {
            let staging = MaterialPaths::staging_for(target);
            if staging.exists() {
                fs::remove_file(&staging)
                    .map_err(|error| CertError::Io(format!("clear staging: {error}")))?;
            }
        }
    }

    match decided.action {
        ProvisioningAction::KeepExisting => {
            let bytes = fs::read(&paths.certificate)
                .map_err(|error| CertError::Io(format!("read certificate: {error}")))?;
            Ok(ProvisioningOutcome {
                action: decided.action,
                invalidated_pins: decided.invalidates_pins,
                certificate_sha256: reported_pin(&bytes),
            })
        }
        ProvisioningAction::AdoptAndRenew => {
            // Adoption reissues over the existing key. Only writing a marker
            // would claim ownership of a certificate this host cannot renew.
            let existing_key = fs::read_to_string(&paths.key)
                .map_err(|error| CertError::Io(format!("read key: {error}")))?;
            let (certificate_pem, _) = generate(subjects, Some(&existing_key), now_epoch_secs)?;
            publish_in_transaction(&paths, &certificate_pem, None, now_epoch_secs)?;
            Ok(ProvisioningOutcome {
                action: decided.action,
                invalidated_pins: decided.invalidates_pins,
                certificate_sha256: reported_pin(certificate_pem.as_bytes()),
            })
        }
        ProvisioningAction::CreateNew | ProvisioningAction::ReplaceKeyAndCertificate => {
            let (certificate_pem, key_pem) = generate(subjects, None, now_epoch_secs)?;
            publish_in_transaction(&paths, &certificate_pem, Some(&key_pem), now_epoch_secs)?;
            Ok(ProvisioningOutcome {
                action: decided.action,
                invalidated_pins: decided.invalidates_pins,
                certificate_sha256: reported_pin(certificate_pem.as_bytes()),
            })
        }
        ProvisioningAction::RenewPreservingKey => {
            let existing_key = fs::read_to_string(&paths.key)
                .map_err(|error| CertError::Io(format!("read key: {error}")))?;
            let (certificate_pem, _) = generate(subjects, Some(&existing_key), now_epoch_secs)?;
            // The key is deliberately not rewritten: reusing it is what keeps
            // existing client pins valid across a renewal.
            publish_in_transaction(&paths, &certificate_pem, None, now_epoch_secs)?;
            Ok(ProvisioningOutcome {
                action: decided.action,
                invalidated_pins: decided.invalidates_pins,
                certificate_sha256: reported_pin(certificate_pem.as_bytes()),
            })
        }
    }
}

/// Generates a certificate, reusing `existing_key_pem` when renewing.
///
/// The validity window is set explicitly rather than left to a default, since
/// the renewal decision is made by comparing `not_after` against the clock.
fn generate(
    subjects: &[String],
    existing_key_pem: Option<&str>,
    now_epoch_secs: u64,
) -> Result<(String, String), CertError> {
    use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair};

    if subjects.is_empty() {
        return Err(CertError::Generation(
            "at least one subject alternative name is required; a certificate without one is \
             refused by every client"
                .to_owned(),
        ));
    }
    // `rcgen` classifies each name itself, so an address becomes an IP SAN and
    // a hostname becomes a DNS SAN. That distinction is not cosmetic: a Deck
    // dialling an address validates against the IP entry, and a certificate
    // carrying only a hostname is refused with "certificate not valid for
    // name", which reads like a trust problem rather than a missing name.
    let mut params = CertificateParams::new(subjects.to_vec())
        .map_err(|error| CertError::Generation(format!("certificate parameters: {error}")))?;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let issued_at = i64::try_from(now_epoch_secs)
        .map_err(|_| CertError::Generation("clock is out of range".to_owned()))?;
    let not_before = time::OffsetDateTime::from_unix_timestamp(issued_at)
        .map_err(|error| CertError::Generation(format!("issue time: {error}")))?;
    let not_after = not_before
        .checked_add(time::Duration::days(VALIDITY_DAYS))
        .ok_or_else(|| CertError::Generation("validity window overflows".to_owned()))?;
    params.not_before = not_before;
    params.not_after = not_after;

    let key = match existing_key_pem {
        Some(pem) => KeyPair::from_pem(pem)
            .map_err(|error| CertError::Generation(format!("reuse key: {error}")))?,
        None => KeyPair::generate()
            .map_err(|error| CertError::Generation(format!("generate key: {error}")))?,
    };
    let certificate = params
        .self_signed(&key)
        .map_err(|error| CertError::Generation(format!("self-sign: {error}")))?;
    Ok((certificate.pem(), key.serialize_pem()))
}

/// A publication in progress.
///
/// Existing material is copied aside before anything is overwritten, and the
/// journal records what the directory looked like first. If the process dies
/// between those two facts and the commit, the next run can put things back.
struct Transaction {
    id: String,
    journal_path: PathBuf,
    backups: Vec<(PathBuf, PathBuf)>,
}

impl Transaction {
    /// Records the current state and takes backups.
    fn begin(paths: &MaterialPaths, now_epoch_secs: u64) -> Result<Self, CertError> {
        let id = format!("{}-{now_epoch_secs}", std::process::id());
        let journal_path = paths.directory.join(JOURNAL_FILE);
        reject_symlink(&journal_path)?;

        let mut existed = [false; MANAGED_FILES.len()];
        let mut backups = Vec::new();
        for (index, path) in paths.managed().into_iter().enumerate() {
            let present = path.is_file();
            existed[index] = present;
            if present {
                let backup = paths
                    .directory
                    .join(format!(".arcen-cert.backup.{id}.{}", MANAGED_FILES[index]));
                reject_symlink(&backup)?;
                fs::copy(path, &backup).map_err(|error| {
                    CertError::Io(format!("back up {}: {error}", path.display()))
                })?;
                backups.push((path.clone(), backup));
            }
        }

        // The journal is written before anything is overwritten, so recovery
        // always has a record of what to restore.
        let journal = TransactionJournal::new(id.clone(), TransactionPhase::Prepared, existed);
        write_private(&journal_path, journal.render().as_bytes(), 0o644)?;

        Ok(Self {
            id,
            journal_path,
            backups,
        })
    }

    /// Marks the publication complete and removes the working files.
    fn commit(self, paths: &MaterialPaths) -> Result<(), CertError> {
        let mut existed = [true; MANAGED_FILES.len()];
        for (index, path) in paths.managed().into_iter().enumerate() {
            existed[index] = path.is_file();
        }
        let journal = TransactionJournal::new(self.id, TransactionPhase::Committed, existed);
        write_private(&self.journal_path, journal.render().as_bytes(), 0o644)?;

        for (_, backup) in &self.backups {
            let _ = fs::remove_file(backup);
        }
        fs::remove_file(&self.journal_path)
            .map_err(|error| CertError::Io(format!("clear journal: {error}")))
    }

    /// Puts the directory back after a failure part-way through publishing.
    fn roll_back(self, paths: &MaterialPaths) {
        for (final_path, backup) in &self.backups {
            let _ = fs::remove_file(final_path);
            let _ = fs::rename(backup, final_path);
        }
        // Files that had no backup did not exist beforehand.
        let backed_up: Vec<&PathBuf> = self.backups.iter().map(|(path, _)| path).collect();
        for path in paths.managed() {
            if !backed_up.contains(&path) {
                let _ = fs::remove_file(path);
            }
        }
        let _ = fs::remove_file(&self.journal_path);
    }
}

/// Completes or undoes a publication that was interrupted.
///
/// The decision for each file comes from the shared journal contract; only the
/// file operations are native. A journal that cannot be understood is left in
/// place and reported, because acting on half of it could delete material.
fn recover_interrupted(paths: &MaterialPaths) -> Result<Option<String>, CertError> {
    let journal_path = paths.directory.join(JOURNAL_FILE);
    if !journal_path.is_file() {
        return Ok(None);
    }
    reject_symlink(&journal_path)?;
    let text = fs::read_to_string(&journal_path)
        .map_err(|error| CertError::Io(format!("read journal: {error}")))?;
    let journal = TransactionJournal::parse(&text)
        .map_err(|error| CertError::Io(format!("unreadable certificate journal: {error}")))?;

    for (index, path) in paths.managed().into_iter().enumerate() {
        let name = MANAGED_FILES[index];
        let backup = paths.directory.join(format!(
            ".arcen-cert.backup.{}.{name}",
            journal.transaction_id()
        ));
        let decision = journal
            .recover_file(
                name,
                FileBefore {
                    existed: path.is_file(),
                    backup_present: backup.is_file(),
                },
            )
            .map_err(|error| CertError::Io(format!("journal decision for {name}: {error}")))?;
        match decision {
            FileRecovery::RequirePublished | FileRecovery::RequireUntouched => {
                if !path.is_file() {
                    return Err(CertError::Io(format!(
                        "interrupted certificate transaction cannot restore {name}"
                    )));
                }
            }
            FileRecovery::RestoreBackup => {
                let _ = fs::remove_file(path);
                fs::rename(&backup, path).map_err(|error| {
                    CertError::Io(format!("restore {}: {error}", path.display()))
                })?;
            }
            FileRecovery::RemoveFile => {
                let _ = fs::remove_file(path);
            }
        }
        let _ = fs::remove_file(&backup);
    }

    for path in paths.managed() {
        let _ = fs::remove_file(MaterialPaths::staging_for(path));
    }
    let transaction = journal.transaction_id().to_owned();
    fs::remove_file(&journal_path)
        .map_err(|error| CertError::Io(format!("clear journal: {error}")))?;
    Ok(Some(transaction))
}

/// Publishes material inside a journalled transaction.
///
/// Every file is written as one set. If any write fails, the directory is put
/// back as it was rather than left serving a certificate whose pins or marker
/// describe a different one.
fn publish_in_transaction(
    paths: &MaterialPaths,
    certificate_pem: &str,
    key_pem: Option<&str>,
    now_epoch_secs: u64,
) -> Result<(), CertError> {
    let transaction = Transaction::begin(paths, now_epoch_secs)?;
    match publish(paths, certificate_pem, key_pem) {
        Ok(()) => transaction.commit(paths),
        Err(error) => {
            transaction.roll_back(paths);
            Err(error)
        }
    }
}

/// Writes material atomically with private permissions.
fn publish(
    paths: &MaterialPaths,
    certificate_pem: &str,
    key_pem: Option<&str>,
) -> Result<(), CertError> {
    if let Some(key_pem) = key_pem {
        write_private(&paths.key, key_pem.as_bytes(), 0o600)?;
    }
    write_private(&paths.certificate, certificate_pem.as_bytes(), 0o644)?;
    write_pins(paths, certificate_pem.as_bytes())?;
    write_marker(paths, certificate_pem.as_bytes())
}

/// Writes the operator-facing pin files.
///
/// The shapes match what `packaging/linux/README.md` documents, so the two
/// platforms produce files an operator can compare side by side: the
/// whole-certificate fingerprint as `openssl` prints it, and one
/// `sha256/<base64>` subject public key line.
fn write_pins(paths: &MaterialPaths, certificate_bytes: &[u8]) -> Result<(), CertError> {
    let pins = cert_marker::pins_from_pem(certificate_bytes)
        .ok_or_else(|| CertError::Generation("cannot pin the generated certificate".to_owned()))?;
    write_private(
        &paths.certificate_pin,
        format!(
            "sha256 Fingerprint={}\n",
            cert_marker::colon_hex(&pins.certificate)
        )
        .as_bytes(),
        0o644,
    )?;
    write_private(
        &paths.spki_pin,
        format!("{}\n", pins.spki).as_bytes(),
        0o644,
    )
}

fn write_marker(paths: &MaterialPaths, certificate_bytes: &[u8]) -> Result<(), CertError> {
    let pins = cert_marker::pins_from_pem(certificate_bytes)
        .ok_or_else(|| CertError::Generation("cannot pin the generated certificate".to_owned()))?;
    let marker = OwnershipMarker::new(&pins.certificate, &pins.spki)
        .map_err(|error| CertError::Generation(format!("ownership marker: {error}")))?;
    // The marker is not secret and operators read it, so it matches the
    // certificate's mode rather than the key's.
    write_private(&paths.marker, marker.render().as_bytes(), 0o644)
}

/// Writes `bytes` to `target` through a staging file with `mode` set before
/// the file becomes visible under its real name.
fn write_private(target: &Path, bytes: &[u8], mode: u32) -> Result<(), CertError> {
    let staging = MaterialPaths::staging_for(target);
    {
        let mut file = fs::File::create(&staging)
            .map_err(|error| CertError::Io(format!("create {}: {error}", staging.display())))?;
        // Permissions are applied before any content is written, so the key is
        // never briefly readable by others.
        fs::set_permissions(&staging, fs::Permissions::from_mode(mode))
            .map_err(|error| CertError::Io(format!("chmod {}: {error}", staging.display())))?;
        file.write_all(bytes)
            .map_err(|error| CertError::Io(format!("write {}: {error}", staging.display())))?;
        file.sync_all()
            .map_err(|error| CertError::Io(format!("sync {}: {error}", staging.display())))?;
    }
    fs::rename(&staging, target)
        .map_err(|error| CertError::Io(format!("publish {}: {error}", target.display())))?;
    fs::set_permissions(target, fs::Permissions::from_mode(mode))
        .map_err(|error| CertError::Io(format!("chmod {}: {error}", target.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let state = inspect(&paths, NOW).expect("inspect");
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

        let state = inspect(&paths, NOW).expect("inspect");
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
        let near_expiry = NOW + (VALIDITY_DAYS as u64 * 24 * 60 * 60) - (5 * 24 * 60 * 60);
        let state = inspect(&paths, near_expiry).expect("inspect");
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
        let staging = MaterialPaths::staging_for(&paths.key);
        fs::write(&staging, b"half written").expect("debris");

        let state = inspect(&paths, NOW).expect("inspect");
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

    #[test]
    fn an_interrupted_publication_is_rolled_back_on_the_next_run() {
        // Simulate a process that died after taking backups and writing the
        // journal, leaving a half-published directory behind.
        let dir = temp_dir("interrupted");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let good_certificate = fs::read(&paths.certificate).expect("cert");
        let good_marker = fs::read(&paths.marker).expect("marker");

        let transaction = Transaction::begin(&paths, NOW).expect("begin");
        // Corrupt the published material the way a partial write would.
        fs::write(&paths.certificate, b"half written").expect("corrupt");
        fs::remove_file(&paths.marker).expect("lose marker");
        drop(transaction); // die without committing

        let recovered = recover_interrupted(&paths).expect("recovery runs");
        assert!(
            recovered.is_some(),
            "an interrupted journal should be found"
        );
        assert_eq!(
            fs::read(&paths.certificate).expect("cert"),
            good_certificate
        );
        assert_eq!(fs::read(&paths.marker).expect("marker"), good_marker);
        assert!(!paths.directory.join(JOURNAL_FILE).exists());

        // And the directory is usable again.
        let outcome = provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("usable after recovery");
        assert_eq!(outcome.action, ProvisioningAction::KeepExisting);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_interrupted_first_install_leaves_no_material_behind() {
        // Nothing existed before, so recovery must remove what was partially
        // created rather than leave an incomplete host identity.
        let dir = temp_dir("interrupted-first");
        fs::create_dir_all(&dir).expect("dir");
        let paths = MaterialPaths::in_directory(&dir);

        let transaction = Transaction::begin(&paths, NOW).expect("begin");
        fs::write(&paths.certificate, b"partial").expect("partial write");
        drop(transaction);

        recover_interrupted(&paths).expect("recovery runs");
        assert!(
            !paths.certificate.exists(),
            "partial material must be removed"
        );
        assert!(!paths.directory.join(JOURNAL_FILE).exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_publication_restores_the_previous_certificate() {
        let dir = temp_dir("rollback");
        provision(
            &dir,
            ProvisioningRequest::Ensure,
            &["pier.example".to_owned()],
            NOW,
        )
        .expect("create");
        let paths = MaterialPaths::in_directory(&dir);
        let before = fs::read(&paths.certificate).expect("cert");

        let transaction = Transaction::begin(&paths, NOW).expect("begin");
        fs::write(&paths.certificate, b"doomed").expect("write");
        transaction.roll_back(&paths);

        assert_eq!(fs::read(&paths.certificate).expect("cert"), before);
        assert!(!paths.directory.join(JOURNAL_FILE).exists());

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
        let (valid, expiring) = inspect_validity(b"not a certificate", NOW);
        assert!(!valid);
        assert!(!expiring);
    }
    #[test]
    fn an_expired_certificate_is_not_reported_as_valid() {
        // The version this replaced compared only against the renewal
        // threshold, so a certificate that expired long ago read as valid and
        // merely due for renewal. Linux and Windows already asked
        // `is_current`, so the same directory produced different provisioning
        // decisions depending on which host inspected it.
        let directory = std::env::temp_dir().join(format!(
            "arcen-validity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let paths = MaterialPaths::in_directory(&directory);

        let issued_at = 1_700_000_000_u64;
        provision(
            &directory,
            ProvisioningRequest::Ensure,
            &["localhost".to_owned()],
            issued_at,
        )
        .expect("create material");
        let pem = std::fs::read(&paths.certificate).expect("read certificate");

        let (valid_now, _) = inspect_validity(&pem, issued_at + 60);
        assert!(valid_now, "fresh material must be usable");

        // Far past any plausible validity window.
        let (valid_later, renew_later) =
            inspect_validity(&pem, issued_at + 100 * 365 * 24 * 60 * 60);
        assert!(
            !valid_later,
            "an expired certificate must not report as usable"
        );
        assert!(renew_later, "and it is certainly due for renewal");

        // Before it was issued is also not usable.
        let (valid_before, _) = inspect_validity(&pem, issued_at.saturating_sub(86_400));
        assert!(!valid_before, "a not-yet-valid certificate is not usable");

        std::fs::remove_dir_all(&directory).ok();
    }
}
