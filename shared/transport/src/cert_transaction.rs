//! OS-free certificate transaction recovery.
//!
//! Publishing certificate material touches several files at once: the
//! certificate, the key, the pin files and the ownership marker. A run that
//! dies partway through leaves the directory in a state nobody planned, and
//! the wrong recovery is worse than none — restoring a certificate over a key
//! that already moved on gives a host that serves material it cannot prove it
//! owns.
//!
//! The decision is recorded in a journal before anything is touched, and this
//! module turns that journal back into a per-file action. Deciding is pure and
//! lives here; moving and deleting files stays in the platform adapters, which
//! is what lets all three hosts recover an interrupted run the same way.
//!
//! The semantics match `packaging/linux/new-host-cert.sh`, which is the most
//! complete implementation Arcen has.

use std::fmt::{Display, Formatter};

/// Files a certificate transaction manages together.
///
/// They are published as a set. Recovering some but not others would leave a
/// certificate whose pins or ownership marker describe a different one.
pub const MANAGED_FILES: [&str; 5] = [
    "host.key",
    "host.crt",
    "host.cert-sha256",
    "host.spki-sha256",
    "host.generated-by-arcen",
];

/// How far a transaction had got before it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionPhase {
    /// Backups were taken and staging written, but nothing was published.
    Prepared,
    /// Every managed file was published.
    Committed,
}

impl TransactionPhase {
    /// Parses the phase as written in a journal.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "prepared" => Some(Self::Prepared),
            "committed" => Some(Self::Committed),
            _ => None,
        }
    }

    /// Returns the journal spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
        }
    }
}

/// What the directory looked like for one file before the transaction began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileBefore {
    /// Whether the file existed before the transaction started.
    pub existed: bool,
    /// Whether a backup of it is present now.
    pub backup_present: bool,
}

/// What a host must do with one managed file to finish recovering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRecovery {
    /// The transaction committed: the file must be present, and is kept.
    ///
    /// A committed transaction rolls forward. Undoing it would discard
    /// material clients may already have been served.
    RequirePublished,
    /// Restore the backup over the published file.
    RestoreBackup,
    /// Remove the file: it did not exist before the transaction.
    RemoveFile,
    /// Leave the file alone, but it must still be present.
    ///
    /// Nothing was published for it and no backup was taken, so the original
    /// is still in place. If it is missing, something outside the transaction
    /// removed it and recovery cannot continue silently.
    RequireUntouched,
}

/// Why a journal could not be acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalError {
    /// The journal did not name a transaction.
    MissingTransactionId,
    /// The journal did not record a phase.
    MissingPhase,
    /// The phase was not one this version understands.
    UnknownPhase,
    /// The journal did not record the prior state of every managed file.
    IncompleteFileRecord,
    /// The journal held a line this version cannot interpret.
    ///
    /// Guessing at an unknown journal risks deleting material a newer version
    /// deliberately kept, so it is refused instead.
    UnrecognizedEntry,
}

impl JournalError {
    /// Returns a stable operator-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingTransactionId => "missing_transaction_id",
            Self::MissingPhase => "missing_phase",
            Self::UnknownPhase => "unknown_phase",
            Self::IncompleteFileRecord => "incomplete_file_record",
            Self::UnrecognizedEntry => "unrecognized_journal_entry",
        }
    }
}

impl Display for JournalError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for JournalError {}

/// A parsed transaction journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionJournal {
    transaction_id: String,
    phase: TransactionPhase,
    existed: [bool; MANAGED_FILES.len()],
}

impl TransactionJournal {
    /// Builds a journal for a transaction that is about to begin.
    #[must_use]
    pub fn new(
        transaction_id: impl Into<String>,
        phase: TransactionPhase,
        existed: [bool; MANAGED_FILES.len()],
    ) -> Self {
        Self {
            transaction_id: transaction_id.into(),
            phase,
            existed,
        }
    }

    /// Returns the transaction identifier.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// Returns how far the transaction got.
    #[must_use]
    pub const fn phase(&self) -> TransactionPhase {
        self.phase
    }

    /// Renders the journal in the on-disk key/value form.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut text = String::new();
        let _ = writeln!(text, "transaction={}", self.transaction_id);
        let _ = writeln!(text, "phase={}", self.phase.as_str());
        for (index, name) in MANAGED_FILES.iter().enumerate() {
            let _ = writeln!(text, "existed.{name}={}", u8::from(self.existed[index]));
        }
        text
    }

    /// Parses a journal.
    ///
    /// # Errors
    ///
    /// Returns a [`JournalError`] for a journal that is incomplete or that this
    /// version does not fully understand, rather than acting on part of it.
    pub fn parse(text: &str) -> Result<Self, JournalError> {
        let mut transaction_id: Option<String> = None;
        let mut phase: Option<TransactionPhase> = None;
        let mut existed: [Option<bool>; MANAGED_FILES.len()] = [None; MANAGED_FILES.len()];

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(JournalError::UnrecognizedEntry);
            };
            match key {
                "transaction" => transaction_id = Some(value.to_owned()),
                "phase" => {
                    phase = Some(TransactionPhase::parse(value).ok_or(JournalError::UnknownPhase)?);
                }
                other => {
                    let name = other
                        .strip_prefix("existed.")
                        .ok_or(JournalError::UnrecognizedEntry)?;
                    let index = MANAGED_FILES
                        .iter()
                        .position(|managed| *managed == name)
                        .ok_or(JournalError::UnrecognizedEntry)?;
                    existed[index] = Some(match value {
                        "0" => false,
                        "1" => true,
                        _ => return Err(JournalError::UnrecognizedEntry),
                    });
                }
            }
        }

        let transaction_id = transaction_id.ok_or(JournalError::MissingTransactionId)?;
        if transaction_id.is_empty() {
            return Err(JournalError::MissingTransactionId);
        }
        let phase = phase.ok_or(JournalError::MissingPhase)?;
        let mut resolved = [false; MANAGED_FILES.len()];
        for (index, value) in existed.iter().enumerate() {
            resolved[index] = value.ok_or(JournalError::IncompleteFileRecord)?;
        }
        Ok(Self {
            transaction_id,
            phase,
            existed: resolved,
        })
    }

    /// Returns whether `name` existed before the transaction.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::UnrecognizedEntry`] for a file this transaction
    /// does not manage.
    pub fn existed_before(&self, name: &str) -> Result<bool, JournalError> {
        MANAGED_FILES
            .iter()
            .position(|managed| *managed == name)
            .map(|index| self.existed[index])
            .ok_or(JournalError::UnrecognizedEntry)
    }

    /// Decides what to do with one managed file.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::UnrecognizedEntry`] for a file this transaction
    /// does not manage.
    pub fn recover_file(&self, name: &str, now: FileBefore) -> Result<FileRecovery, JournalError> {
        let existed = self.existed_before(name)?;
        // A committed transaction rolls forward. Every file was published, so
        // undoing it would discard material clients may already have been
        // served.
        if self.phase == TransactionPhase::Committed {
            return Ok(FileRecovery::RequirePublished);
        }
        if now.backup_present {
            return Ok(FileRecovery::RestoreBackup);
        }
        if existed {
            Ok(FileRecovery::RequireUntouched)
        } else {
            Ok(FileRecovery::RemoveFile)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(phase: TransactionPhase, existed: bool) -> TransactionJournal {
        TransactionJournal::new("4242-1800000000", phase, [existed; MANAGED_FILES.len()])
    }

    #[test]
    fn a_journal_round_trips_through_its_on_disk_form() {
        let original = TransactionJournal::new(
            "17-99",
            TransactionPhase::Prepared,
            [true, false, true, false, true],
        );
        let parsed = TransactionJournal::parse(&original.render()).expect("round trip");
        assert_eq!(parsed, original);
        assert_eq!(parsed.transaction_id(), "17-99");
        assert_eq!(parsed.phase(), TransactionPhase::Prepared);
        assert!(parsed.existed_before("host.key").expect("known file"));
        assert!(!parsed.existed_before("host.crt").expect("known file"));
    }

    #[test]
    fn a_committed_transaction_rolls_forward() {
        // Undoing a committed publication would discard material clients may
        // already have been served.
        let journal = journal(TransactionPhase::Committed, false);
        for name in MANAGED_FILES {
            let recovery = journal
                .recover_file(
                    name,
                    FileBefore {
                        existed: false,
                        backup_present: true,
                    },
                )
                .expect("known file");
            assert_eq!(
                recovery,
                FileRecovery::RequirePublished,
                "{name} must roll forward even with a backup present"
            );
        }
    }

    #[test]
    fn an_interrupted_transaction_restores_from_backup() {
        let journal = journal(TransactionPhase::Prepared, true);
        let recovery = journal
            .recover_file(
                "host.key",
                FileBefore {
                    existed: true,
                    backup_present: true,
                },
            )
            .expect("known file");
        assert_eq!(recovery, FileRecovery::RestoreBackup);
    }

    #[test]
    fn an_interrupted_first_install_removes_what_it_created() {
        // Nothing existed before, so leaving a partially published file would
        // present a host identity that was never completed.
        let journal = journal(TransactionPhase::Prepared, false);
        let recovery = journal
            .recover_file(
                "host.crt",
                FileBefore {
                    existed: false,
                    backup_present: false,
                },
            )
            .expect("known file");
        assert_eq!(recovery, FileRecovery::RemoveFile);
    }

    #[test]
    fn an_untouched_file_is_required_to_still_be_there() {
        // It existed before and no backup was taken, so nothing moved it. If
        // it is gone, something outside the transaction interfered.
        let journal = journal(TransactionPhase::Prepared, true);
        let recovery = journal
            .recover_file(
                "host.crt",
                FileBefore {
                    existed: true,
                    backup_present: false,
                },
            )
            .expect("known file");
        assert_eq!(recovery, FileRecovery::RequireUntouched);
    }

    #[test]
    fn every_managed_file_is_covered_by_one_decision() {
        let journal = journal(TransactionPhase::Prepared, true);
        for name in MANAGED_FILES {
            assert!(
                journal
                    .recover_file(
                        name,
                        FileBefore {
                            existed: true,
                            backup_present: false,
                        }
                    )
                    .is_ok(),
                "{name} has no recovery decision"
            );
        }
    }

    #[test]
    fn files_outside_the_transaction_are_refused() {
        let journal = journal(TransactionPhase::Prepared, true);
        assert_eq!(
            journal.existed_before("host.unexpected"),
            Err(JournalError::UnrecognizedEntry)
        );
    }

    #[test]
    fn incomplete_journals_are_refused_rather_than_half_applied() {
        assert_eq!(
            TransactionJournal::parse("phase=prepared\n"),
            Err(JournalError::MissingTransactionId)
        );
        assert_eq!(
            TransactionJournal::parse("transaction=1-2\n"),
            Err(JournalError::MissingPhase)
        );
        assert_eq!(
            TransactionJournal::parse("transaction=1-2\nphase=prepared\nexisted.host.key=1\n"),
            Err(JournalError::IncompleteFileRecord)
        );
        assert_eq!(
            TransactionJournal::parse("transaction=\nphase=prepared\n"),
            Err(JournalError::MissingTransactionId)
        );
    }

    #[test]
    fn a_journal_this_version_does_not_understand_is_refused() {
        // Guessing could delete material a newer version deliberately kept.
        assert_eq!(
            TransactionJournal::parse("transaction=1-2\nphase=teleported\n"),
            Err(JournalError::UnknownPhase)
        );
        assert_eq!(
            TransactionJournal::parse("transaction=1-2\nphase=prepared\nsomething.else=1\n"),
            Err(JournalError::UnrecognizedEntry)
        );
        assert_eq!(
            TransactionJournal::parse("transaction=1-2\nphase=prepared\nexisted.host.key=maybe\n"),
            Err(JournalError::UnrecognizedEntry)
        );
        assert_eq!(
            TransactionJournal::parse("a line with no equals sign\n"),
            Err(JournalError::UnrecognizedEntry)
        );
    }

    #[test]
    fn a_journal_written_by_the_linux_helper_parses_here() {
        // Byte-for-byte in the order `packaging/linux/new-host-cert.sh`
        // writes it. If this drifts, the two implementations would disagree
        // about how to recover the same interrupted transaction.
        let on_disk = "transaction=4242-1800000000\n\
                       phase=prepared\n\
                       existed.host.key=1\n\
                       existed.host.crt=1\n\
                       existed.host.cert-sha256=0\n\
                       existed.host.spki-sha256=0\n\
                       existed.host.generated-by-arcen=0\n";
        let journal = TransactionJournal::parse(on_disk).expect("helper journal parses");
        assert_eq!(journal.transaction_id(), "4242-1800000000");
        assert_eq!(journal.phase(), TransactionPhase::Prepared);
        assert!(journal.existed_before("host.key").expect("known"));
        assert!(
            !journal
                .existed_before("host.generated-by-arcen")
                .expect("known")
        );
        // And we render the same shape back.
        assert_eq!(journal.render(), on_disk);
    }

    #[test]
    fn phase_spellings_match_the_on_disk_vocabulary() {
        assert_eq!(
            TransactionPhase::parse("prepared"),
            Some(TransactionPhase::Prepared)
        );
        assert_eq!(
            TransactionPhase::parse("committed"),
            Some(TransactionPhase::Committed)
        );
        assert_eq!(TransactionPhase::parse("Prepared"), None);
        assert_eq!(TransactionPhase::Prepared.as_str(), "prepared");
        assert_eq!(TransactionPhase::Committed.as_str(), "committed");
    }
}
