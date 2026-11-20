// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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

/// The journal recording an in-flight publication, beside the material.
pub const JOURNAL_FILE: &str = ".arcen-cert.transaction";

/// The longest run of digits on either side of a transaction id's `-`.
const MAX_TRANSACTION_ID_PART: usize = 20;

/// A transaction id for the process `process_id` starting at
/// `epoch_secs`: `<pid>-<seconds>`, the form `new-host-cert.sh` writes.
#[must_use]
pub fn transaction_id(process_id: u32, epoch_secs: u64) -> String {
    format!("{process_id}-{epoch_secs}")
}

/// Whether `value` is a transaction id: one to twenty ASCII digits, `-`, one
/// to twenty ASCII digits, and nothing else.
///
/// The id becomes part of a backup's file name, so anything that could name
/// another directory (`/`, `\`, `..`), end a string early (NUL) or grow
/// without bound is refused rather than joined into a path.
#[must_use]
pub fn is_transaction_id(value: &str) -> bool {
    let part = |digits: &str| {
        (1..=MAX_TRANSACTION_ID_PART).contains(&digits.len())
            && digits.bytes().all(|byte| byte.is_ascii_digit())
    };
    value
        .split_once('-')
        .is_some_and(|(process, seconds)| part(process) && part(seconds))
}

/// The name of the backup `transaction_id` takes of managed file `name`.
///
/// Every adapter, and `new-host-cert.sh`, uses this one spelling: a
/// recovery run by a different tool than the one that was interrupted must
/// find the backups. Only ids [`is_transaction_id`] accepts reach here: the
/// writers make them with [`transaction_id`], and [`TransactionJournal::parse`]
/// refuses any other.
#[must_use]
pub fn backup_file_name(transaction_id: &str, name: &str) -> String {
    format!(".arcen-cert.backup.{transaction_id}.{name}")
}

/// One step of publishing replacement material, in the order a host takes
/// them. The native adapter performs each step with its own file system
/// calls; the order and the set of files are decided here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationStep {
    /// Write every replacement to a staging file beside its target and check
    /// the staged certificate against the staged or kept key. Nothing the
    /// host serves has changed yet, so a malformed replacement stops here.
    StageAndVerify,
    /// Record the journal before any managed file moves, so an interrupted
    /// run always has a record of what to restore.
    WriteJournal(TransactionPhase),
    /// Move or copy one existing managed file aside.
    BackUp(&'static str),
    /// Rename one staged file over its target.
    Publish(&'static str),
    /// Check the published pair matches before committing; on failure the
    /// host rolls back from the backups.
    VerifyPublished,
    /// Remove the backups taken for this transaction.
    DiscardBackups,
    /// Remove the journal: the transaction is over.
    ClearJournal,
}

/// The publication of one issue: which managed files change, and the order
/// of the steps that change them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicationPlan {
    replace_key: bool,
    existed: [bool; MANAGED_FILES.len()],
}

impl PublicationPlan {
    /// A plan that replaces the key only when `replace_key`; `existed` is
    /// which [`MANAGED_FILES`] are present before it starts.
    ///
    /// A renewal keeps the key file exactly as it is: it is neither staged,
    /// backed up nor published, so its bytes, owner and mode never change.
    #[must_use]
    pub const fn new(replace_key: bool, existed: [bool; MANAGED_FILES.len()]) -> Self {
        Self {
            replace_key,
            existed,
        }
    }

    /// Whether managed file `name` is published by this plan.
    #[must_use]
    pub fn publishes(&self, name: &str) -> bool {
        MANAGED_FILES.contains(&name) && (self.replace_key || name != MANAGED_FILES[0])
    }

    /// Whether managed file `name` was present before the plan started.
    #[must_use]
    pub fn existed_before(&self, name: &str) -> bool {
        MANAGED_FILES
            .iter()
            .position(|managed| *managed == name)
            .is_some_and(|index| self.existed[index])
    }

    /// The journal this plan records in `phase`.
    #[must_use]
    pub fn journal(&self, transaction_id: &str, phase: TransactionPhase) -> TransactionJournal {
        TransactionJournal::new(transaction_id, phase, self.existed)
    }

    /// Every step, in order.
    #[must_use]
    pub fn steps(&self) -> Vec<PublicationStep> {
        let mut steps = vec![
            PublicationStep::StageAndVerify,
            PublicationStep::WriteJournal(TransactionPhase::Prepared),
        ];
        for (index, name) in MANAGED_FILES.iter().enumerate() {
            if self.existed[index] && self.publishes(name) {
                steps.push(PublicationStep::BackUp(name));
            }
        }
        for name in MANAGED_FILES {
            if self.publishes(name) {
                steps.push(PublicationStep::Publish(name));
            }
        }
        steps.extend([
            PublicationStep::VerifyPublished,
            PublicationStep::WriteJournal(TransactionPhase::Committed),
            PublicationStep::DiscardBackups,
            PublicationStep::ClearJournal,
        ]);
        steps
    }
}

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
    /// The transaction id is not `<digits>-<digits>`: it could name a path
    /// outside the certificate directory.
    InvalidTransactionId,
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
            Self::InvalidTransactionId => "invalid_transaction_id",
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
        if !is_transaction_id(&transaction_id) {
            return Err(JournalError::InvalidTransactionId);
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
    use std::collections::BTreeMap;

    use super::*;

    /// A directory in memory, driven through a [`PublicationPlan`] the way
    /// a host adapter drives its file system, with a crash after any step.
    struct Directory {
        files: BTreeMap<String, Vec<u8>>,
    }

    impl Directory {
        fn with(names: &[&str], tag: &str) -> Self {
            Self {
                files: names
                    .iter()
                    .map(|name| ((*name).to_owned(), format!("{tag}:{name}").into_bytes()))
                    .collect(),
            }
        }

        fn staging(name: &str) -> String {
            format!(".{name}.staging")
        }

        /// Runs the first `limit` steps of `plan`, then stops as a crash would.
        fn publish(&mut self, plan: PublicationPlan, id: &str, limit: usize) {
            for step in plan.steps().into_iter().take(limit) {
                match step {
                    PublicationStep::StageAndVerify => {
                        for name in MANAGED_FILES {
                            if plan.publishes(name) {
                                self.files.insert(
                                    Self::staging(name),
                                    format!("new:{name}").into_bytes(),
                                );
                            }
                        }
                    }
                    PublicationStep::WriteJournal(phase) => {
                        self.files.insert(
                            JOURNAL_FILE.to_owned(),
                            plan.journal(id, phase).render().into_bytes(),
                        );
                    }
                    PublicationStep::BackUp(name) => {
                        let bytes = self.files.remove(name).expect("backed-up file exists");
                        self.files.insert(backup_file_name(id, name), bytes);
                    }
                    PublicationStep::Publish(name) => {
                        let bytes = self
                            .files
                            .remove(&Self::staging(name))
                            .expect("staged file exists");
                        self.files.insert(name.to_owned(), bytes);
                    }
                    PublicationStep::VerifyPublished => {}
                    PublicationStep::DiscardBackups => {
                        self.files
                            .retain(|name, _| !name.starts_with(".arcen-cert.backup."));
                    }
                    PublicationStep::ClearJournal => {
                        self.files.remove(JOURNAL_FILE);
                    }
                }
            }
        }

        /// What every host does at its next start.
        fn recover(&mut self) {
            if let Some(text) = self.files.remove(JOURNAL_FILE) {
                let journal = TransactionJournal::parse(std::str::from_utf8(&text).expect("utf8"))
                    .expect("journal");
                for name in MANAGED_FILES {
                    let backup = backup_file_name(journal.transaction_id(), name);
                    let decision = journal
                        .recover_file(
                            name,
                            FileBefore {
                                existed: self.files.contains_key(name),
                                backup_present: self.files.contains_key(&backup),
                            },
                        )
                        .expect("decision");
                    match decision {
                        FileRecovery::RequirePublished | FileRecovery::RequireUntouched => {
                            assert!(self.files.contains_key(name), "{name} must be present");
                        }
                        FileRecovery::RestoreBackup => {
                            let bytes = self.files.remove(&backup).expect("backup");
                            self.files.insert(name.to_owned(), bytes);
                        }
                        FileRecovery::RemoveFile => {
                            self.files.remove(name);
                        }
                    }
                    self.files.remove(&backup);
                }
            }
            for name in MANAGED_FILES {
                self.files.remove(&Self::staging(name));
            }
        }
    }

    #[test]
    fn a_crash_after_any_step_recovers_to_the_old_or_the_new_material_whole() {
        for replace_key in [false, true] {
            for first_install in [false, true] {
                if first_install && !replace_key {
                    continue;
                }
                let existed = [!first_install; MANAGED_FILES.len()];
                let plan = PublicationPlan::new(replace_key, existed);
                let steps = plan.steps();
                let committed_at = steps
                    .iter()
                    .position(|step| {
                        *step == PublicationStep::WriteJournal(TransactionPhase::Committed)
                    })
                    .expect("commit step");
                let before = if first_install {
                    Directory::with(&[], "old")
                } else {
                    Directory::with(&MANAGED_FILES, "old")
                };
                let mut after = Directory::with(&MANAGED_FILES, "new");
                if !replace_key {
                    after
                        .files
                        .insert("host.key".to_owned(), b"old:host.key".to_vec());
                }
                for limit in 0..=steps.len() {
                    let mut directory = Directory {
                        files: before.files.clone(),
                    };
                    directory.publish(plan, "77-1800000000", limit);
                    directory.recover();
                    let expected = if limit > committed_at {
                        &after
                    } else {
                        &before
                    };
                    assert_eq!(
                        directory.files, expected.files,
                        "replace_key={replace_key} first_install={first_install}: \
                         crash after {limit} steps"
                    );
                }
            }
        }
    }

    #[test]
    fn a_renewal_never_touches_the_key_file() {
        let plan = PublicationPlan::new(false, [true; MANAGED_FILES.len()]);
        assert!(!plan.publishes("host.key"));
        assert!(plan.steps().iter().all(|step| !matches!(
            step,
            PublicationStep::BackUp("host.key") | PublicationStep::Publish("host.key")
        )));
        assert!(plan.publishes("host.crt"));
    }

    #[test]
    fn the_journal_is_written_before_anything_moves_and_the_key_moves_first() {
        let plan = PublicationPlan::new(true, [true; MANAGED_FILES.len()]);
        let steps = plan.steps();
        assert_eq!(steps[0], PublicationStep::StageAndVerify);
        assert_eq!(
            steps[1],
            PublicationStep::WriteJournal(TransactionPhase::Prepared)
        );
        assert_eq!(steps[2], PublicationStep::BackUp("host.key"));
        let first_publish = steps
            .iter()
            .position(|step| matches!(step, PublicationStep::Publish(_)))
            .expect("publish");
        assert_eq!(steps[first_publish], PublicationStep::Publish("host.key"));
        assert!(
            steps[..first_publish]
                .iter()
                .all(|step| !matches!(step, PublicationStep::Publish(_)))
        );
        assert_eq!(steps.last(), Some(&PublicationStep::ClearJournal));
    }

    #[test]
    fn a_transaction_id_that_could_name_a_path_is_refused() {
        use std::fmt::Write as _;

        let journal_with = |id: &str| {
            let mut text = format!("transaction={id}\nphase=prepared\n");
            for name in MANAGED_FILES {
                let _ = writeln!(text, "existed.{name}=1");
            }
            TransactionJournal::parse(&text)
        };
        let overlong = format!("{}-1", "9".repeat(MAX_TRANSACTION_ID_PART + 1));
        for hostile in [
            "1-2/../../etc",
            "../1-2",
            "1/2-3",
            "1-2\\..\\x",
            "..",
            "1-..",
            "1-2\0",
            "1\u{0}-2",
            "-1",
            "1-",
            "1-2-3",
            "a-1",
            "１-2",
            overlong.as_str(),
        ] {
            assert_eq!(
                journal_with(hostile),
                Err(JournalError::InvalidTransactionId),
                "{hostile:?}"
            );
        }
        let longest = format!(
            "{}-{}",
            "4".repeat(MAX_TRANSACTION_ID_PART),
            "2".repeat(MAX_TRANSACTION_ID_PART)
        );
        for valid in ["4242-1800000000", "1-0", longest.as_str()] {
            assert!(journal_with(valid).is_ok(), "{valid}");
        }
        assert!(is_transaction_id(&transaction_id(u32::MAX, u64::MAX)));
    }

    #[test]
    fn backup_names_match_the_linux_helper() {
        // `new-host-cert.sh` writes `$TLS_DIRECTORY/.arcen-cert.backup.$transaction.$name`.
        assert_eq!(
            backup_file_name("4242-1800000000", "host.key"),
            ".arcen-cert.backup.4242-1800000000.host.key"
        );
        assert_eq!(JOURNAL_FILE, ".arcen-cert.transaction");
    }

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
