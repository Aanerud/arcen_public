// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Carrying out a [`PublicationPlan`] and recovering one that was
//! interrupted, over a host's own file operations.
//!
//! Every Pier, installer and helper that writes certificate material runs
//! the same transaction: stage and verify every replacement, record the
//! journal, back up what will be replaced, publish, verify the published
//! pair, commit, clean up. What is portable — the order, which files move,
//! the checks, the rollback after a failure and the recovery after a crash —
//! is here. What is the operating system's — how a file is created privately,
//! moved atomically, protected by a mode or an ACL, kept from following a
//! link — is a [`PublicationFiles`] the host implements.
//!
//! The caller holds the directory's lock for the whole run, from
//! [`recover`] to the end of [`publish`].

use std::fmt::{Display, Formatter};

use zeroize::Zeroizing;

use crate::cert_issuance::HostKey;
use crate::cert_store::{CERTIFICATE_FILE, KEY_FILE, PinFiles};
use crate::cert_transaction::{
    FileBefore, FileRecovery, JournalError, MANAGED_FILES, PublicationPlan, PublicationStep,
    TransactionJournal, TransactionPhase, backup_file_name,
};

/// Which copy of a managed file to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The file under its own name: what the host serves.
    Published,
    /// The replacement staged beside it.
    Staged,
}

/// The file operations a host provides, each on one managed file by its
/// [`MANAGED_FILES`] name. Only `host.key` is `secret`.
///
/// Every operation must be atomic in the sense recovery relies on: after
/// [`Self::back_up`] returns, a complete backup exists under the given name
/// (a copy cut short must never appear under it); after [`Self::publish`] or
/// [`Self::restore`], the target is either the old or the new file, never
/// part of one. Removing something already absent is not an error.
pub trait PublicationFiles {
    /// The host's error.
    type Error: Display;

    /// Writes the replacement for `name` beside it, created with the
    /// protection a `secret` or public file needs before any byte is written.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn stage(&mut self, name: &'static str, bytes: &[u8], secret: bool) -> Result<(), Self::Error>;

    /// Reads one copy of `name`.
    ///
    /// # Errors
    ///
    /// Returns the host's error, including when the copy is absent.
    fn read(&mut self, name: &'static str, slot: Slot) -> Result<Zeroizing<Vec<u8>>, Self::Error>;

    /// Whether `name` is published.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn exists(&mut self, name: &'static str) -> Result<bool, Self::Error>;

    /// Keeps a complete copy of published `name` as `backup`.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn back_up(&mut self, name: &'static str, backup: &str) -> Result<(), Self::Error>;

    /// Whether a complete backup named `backup` exists.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn backup_exists(&mut self, backup: &str) -> Result<bool, Self::Error>;

    /// Moves the staged replacement over `name` and protects it.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn publish(&mut self, name: &'static str, secret: bool) -> Result<(), Self::Error>;

    /// Moves `backup` back over `name`.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn restore(&mut self, name: &'static str, backup: &str) -> Result<(), Self::Error>;

    /// Removes published `name`.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn remove(&mut self, name: &'static str) -> Result<(), Self::Error>;

    /// Removes `backup`, and anything a backup cut short left behind.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn remove_backup(&mut self, backup: &str) -> Result<(), Self::Error>;

    /// Removes every staged replacement.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn discard_staging(&mut self) -> Result<(), Self::Error>;

    /// Replaces the journal atomically with `text`.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn write_journal(&mut self, text: &str) -> Result<(), Self::Error>;

    /// Reads the journal, `None` when there is none.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn read_journal(&mut self) -> Result<Option<String>, Self::Error>;

    /// Removes the journal.
    ///
    /// # Errors
    ///
    /// Returns the host's error.
    fn remove_journal(&mut self) -> Result<(), Self::Error>;
}

/// The material to publish.
#[derive(Clone, Copy)]
pub struct Publication<'a> {
    /// The certificate, PEM.
    pub certificate_pem: &'a [u8],
    /// The new key, PEM; `None` keeps the key on disk untouched.
    pub new_key_pem: Option<&'a [u8]>,
    /// The SPKI digest the certificate must carry, and the key must have.
    pub spki_sha256: [u8; 32],
}

impl std::fmt::Debug for Publication<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Publication")
            .field("new_key", &self.new_key_pem.is_some())
            .field(
                "spki_sha256",
                &crate::cert_marker::hex_lower(&self.spki_sha256),
            )
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "issuer")]
impl<'a> From<&'a crate::cert_issuance::IssuedMaterial> for Publication<'a> {
    fn from(issued: &'a crate::cert_issuance::IssuedMaterial) -> Self {
        Self {
            certificate_pem: issued.certificate_pem.as_bytes(),
            new_key_pem: issued.new_key_pem.as_ref().map(|key| key.as_bytes()),
            spki_sha256: issued.spki_sha256,
        }
    }
}

/// Why a publication did not complete.
#[derive(Debug)]
pub enum PublicationError<E> {
    /// The certificate could not be pinned.
    Pins(String),
    /// The staged or published certificate is not over the key it was
    /// issued for. Nothing that was served changed.
    Mismatch,
    /// A file operation failed before the commit was confirmed; the
    /// directory was recovered by its journal to a whole pair (the previous
    /// one, unless the commit had already been recorded).
    RolledBack(E),
    /// A file operation failed and putting the directory back failed too.
    /// The journal is kept, so the next run's [`recover`] finishes it.
    RollbackIncomplete {
        /// What failed first.
        cause: String,
        /// Why the rollback stopped.
        rollback: String,
    },
    /// The replacement is published and committed, but removing its backups
    /// or the journal failed; the next run's [`recover`] finishes it.
    CommittedNotCleared(E),
}

impl<E: Display> Display for PublicationError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pins(detail) => write!(formatter, "cannot pin the certificate: {detail}"),
            Self::Mismatch => formatter
                .write_str("the certificate and key to publish do not match; nothing was replaced"),
            Self::RolledBack(error) => write!(
                formatter,
                "{error}; the certificate directory was put back to a whole pair"
            ),
            Self::RollbackIncomplete { cause, rollback } => write!(
                formatter,
                "{cause}; putting the previous material back failed ({rollback}); the next \
                 run recovers it from the journal"
            ),
            Self::CommittedNotCleared(error) => write!(
                formatter,
                "the new material is published, but clearing its backups or journal failed \
                 ({error}); the next run clears them"
            ),
        }
    }
}

impl<E: std::fmt::Debug + Display> std::error::Error for PublicationError<E> {}

/// Why an interrupted publication could not be recovered.
#[derive(Debug)]
pub enum RecoveryError<E> {
    /// A file operation failed; the journal is kept.
    Files(E),
    /// The journal cannot be acted on; it is left for an operator.
    Journal(JournalError),
    /// A file the journal says must be present is missing.
    Missing(&'static str),
}

impl<E: Display> Display for RecoveryError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Files(error) => write!(formatter, "recovering certificate material: {error}"),
            Self::Journal(error) => write!(
                formatter,
                "unreadable certificate transaction journal ({error}); it was left in place"
            ),
            Self::Missing(name) => write!(
                formatter,
                "interrupted certificate transaction cannot restore {name}"
            ),
        }
    }
}

impl<E: std::fmt::Debug + Display> std::error::Error for RecoveryError<E> {}

fn is_secret(name: &str) -> bool {
    name == KEY_FILE
}

/// Whether `certificate_pem` is over the key in `key_pem`, and that key is
/// the one with `spki_sha256`.
#[must_use]
pub fn pair_matches(certificate_pem: &[u8], key_pem: &[u8], spki_sha256: &[u8; 32]) -> bool {
    let Ok(key) = HostKey::from_pem(key_pem) else {
        return false;
    };
    crate::cert_marker::pins_from_pem(certificate_pem)
        .is_some_and(|pins| key.spki_sha256() == spki_sha256 && pins.spki == key.spki_pin())
}

/// Publishes `publication` under the shared plan as transaction
/// `transaction_id` (made with [`crate::cert_transaction::transaction_id`]).
///
/// A failure before the commit puts the directory back as it was; after the
/// commit the new material stays. `limit` stops after that many steps, as a
/// crash would, for tests; pass `usize::MAX`.
///
/// # Errors
///
/// Returns a [`PublicationError`]; every variant but
/// [`PublicationError::CommittedNotCleared`] leaves the previous material
/// served, or recoverable by [`recover`].
pub fn publish<F: PublicationFiles>(
    files: &mut F,
    publication: &Publication<'_>,
    transaction_id: &str,
    limit: usize,
) -> Result<(), PublicationError<F::Error>> {
    let pins =
        PinFiles::for_certificate(publication.certificate_pem).map_err(PublicationError::Pins)?;
    let mut existed = [false; MANAGED_FILES.len()];
    for (index, name) in MANAGED_FILES.iter().enumerate() {
        existed[index] = files.exists(name).map_err(PublicationError::RolledBack)?;
    }
    let plan = PublicationPlan::new(publication.new_key_pem.is_some(), existed);

    let mut prepared = false;
    let mut committed = false;
    for step in plan.steps().into_iter().take(limit) {
        let result: Result<(), Failure<F::Error>> = match step {
            PublicationStep::StageAndVerify => stage_and_verify(files, plan, publication, &pins),
            PublicationStep::WriteJournal(phase) => {
                // Counted as written before the call: a failure part-way
                // through may still have replaced the journal, and rollback
                // then has to read and clear it.
                if phase == TransactionPhase::Prepared {
                    prepared = true;
                }
                files
                    .write_journal(&plan.journal(transaction_id, phase).render())
                    .map(|()| committed = phase == TransactionPhase::Committed)
                    .map_err(Failure::Files)
            }
            PublicationStep::BackUp(name) => files
                .back_up(name, &backup_file_name(transaction_id, name))
                .map_err(Failure::Files),
            PublicationStep::Publish(name) => {
                files.publish(name, is_secret(name)).map_err(Failure::Files)
            }
            PublicationStep::VerifyPublished => verify(files, Slot::Published, publication),
            PublicationStep::DiscardBackups => {
                // A backup left behind keeps the committed journal, so the
                // next recovery removes it; clearing the journal now would
                // orphan a copy of the old key.
                for name in MANAGED_FILES
                    .into_iter()
                    .filter(|name| plan.publishes(name))
                {
                    if let Err(error) = files.remove_backup(&backup_file_name(transaction_id, name))
                    {
                        return Err(PublicationError::CommittedNotCleared(error));
                    }
                }
                Ok(())
            }
            PublicationStep::ClearJournal => match files.remove_journal() {
                Ok(()) => Ok(()),
                Err(error) => return Err(PublicationError::CommittedNotCleared(error)),
            },
        };
        let Err(failure) = result else {
            continue;
        };
        if committed {
            // Only DiscardBackups and ClearJournal follow the commit, and
            // neither returns here with an error.
            return Err(failure.into_error());
        }
        return Err(roll_back(files, prepared, failure));
    }
    Ok(())
}

/// A failure inside [`publish`], before it is reported.
enum Failure<E> {
    Files(E),
    Mismatch,
}

impl<E: Display> Failure<E> {
    fn describe(&self) -> String {
        match self {
            Self::Files(error) => error.to_string(),
            Self::Mismatch => PublicationError::<E>::Mismatch.to_string(),
        }
    }

    fn into_error(self) -> PublicationError<E> {
        match self {
            Self::Files(error) => PublicationError::RolledBack(error),
            Self::Mismatch => PublicationError::Mismatch,
        }
    }
}

/// The bytes `name` is published with.
fn contents<'r>(name: &str, publication: &Publication<'r>, pins: &'r PinFiles) -> &'r [u8] {
    match name {
        KEY_FILE => publication.new_key_pem.unwrap_or_default(),
        CERTIFICATE_FILE => publication.certificate_pem,
        crate::cert_store::CERTIFICATE_PIN_FILE => pins.certificate_pin.as_bytes(),
        crate::cert_store::SPKI_PIN_FILE => pins.spki_pin.as_bytes(),
        _ => pins.marker.as_bytes(),
    }
}

fn stage_and_verify<F: PublicationFiles>(
    files: &mut F,
    plan: PublicationPlan,
    publication: &Publication<'_>,
    pins: &PinFiles,
) -> Result<(), Failure<F::Error>> {
    for name in MANAGED_FILES {
        if plan.publishes(name) {
            files
                .stage(name, contents(name, publication, pins), is_secret(name))
                .map_err(Failure::Files)?;
        }
    }
    let certificate = files
        .read(CERTIFICATE_FILE, Slot::Staged)
        .map_err(Failure::Files)?;
    let key_slot = if plan.publishes(KEY_FILE) {
        Slot::Staged
    } else {
        Slot::Published
    };
    let key = files.read(KEY_FILE, key_slot).map_err(Failure::Files)?;
    if pair_matches(&certificate, &key, &publication.spki_sha256) {
        Ok(())
    } else {
        Err(Failure::Mismatch)
    }
}

fn verify<F: PublicationFiles>(
    files: &mut F,
    slot: Slot,
    publication: &Publication<'_>,
) -> Result<(), Failure<F::Error>> {
    let certificate = files.read(CERTIFICATE_FILE, slot).map_err(Failure::Files)?;
    let key = files.read(KEY_FILE, slot).map_err(Failure::Files)?;
    if pair_matches(&certificate, &key, &publication.spki_sha256) {
        Ok(())
    } else {
        Err(Failure::Mismatch)
    }
}

/// Puts the directory back after `failure` before the commit. Once the
/// journal may exist this is exactly [`recover`]: the backups taken so far
/// are restored and the files that did not exist before are removed.
fn roll_back<F: PublicationFiles>(
    files: &mut F,
    prepared: bool,
    failure: Failure<F::Error>,
) -> PublicationError<F::Error> {
    let cause = failure.describe();
    let recovered = if prepared {
        recover(files)
            .map(|_| ())
            .map_err(|error| error.to_string())
    } else {
        Ok(())
    };
    let outcome =
        recovered.and_then(|()| files.discard_staging().map_err(|error| error.to_string()));
    match outcome {
        Ok(()) => failure.into_error(),
        Err(rollback) => PublicationError::RollbackIncomplete { cause, rollback },
    }
}

/// Finishes or undoes a publication a crash interrupted, by its journal.
/// Returns the transaction it recovered, or `None` when there was none.
///
/// A committed transaction rolls forward; a prepared one is undone. A
/// journal this version cannot fully understand is left in place and
/// reported, because acting on half of it could delete material.
///
/// # Errors
///
/// Returns a [`RecoveryError`]; the journal is kept for every one.
pub fn recover<F: PublicationFiles>(
    files: &mut F,
) -> Result<Option<String>, RecoveryError<F::Error>> {
    let Some(text) = files.read_journal().map_err(RecoveryError::Files)? else {
        return Ok(None);
    };
    let journal = TransactionJournal::parse(&text).map_err(RecoveryError::Journal)?;
    for name in MANAGED_FILES {
        let backup = backup_file_name(journal.transaction_id(), name);
        let before = FileBefore {
            existed: files.exists(name).map_err(RecoveryError::Files)?,
            backup_present: files.backup_exists(&backup).map_err(RecoveryError::Files)?,
        };
        match journal
            .recover_file(name, before)
            .map_err(RecoveryError::Journal)?
        {
            FileRecovery::RequirePublished | FileRecovery::RequireUntouched => {
                if !before.existed {
                    return Err(RecoveryError::Missing(name));
                }
            }
            FileRecovery::RestoreBackup => {
                files.restore(name, &backup).map_err(RecoveryError::Files)?;
            }
            FileRecovery::RemoveFile => files.remove(name).map_err(RecoveryError::Files)?,
        }
        files.remove_backup(&backup).map_err(RecoveryError::Files)?;
    }
    files.discard_staging().map_err(RecoveryError::Files)?;
    files.remove_journal().map_err(RecoveryError::Files)?;
    Ok(Some(journal.transaction_id().to_owned()))
}

#[cfg(test)]
mod tests;
