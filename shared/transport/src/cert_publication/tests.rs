// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Tests for the shared publication and recovery, over an in-memory
//! directory that can fail or stop at any operation.

use std::collections::BTreeMap;

use rcgen::{CertificateParams, KeyPair};

use super::*;
use crate::test_keys::openssl_sec1_key;

/// What a directory holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Disk {
    published: BTreeMap<&'static str, Vec<u8>>,
    staged: BTreeMap<&'static str, Vec<u8>>,
    backups: BTreeMap<String, Vec<u8>>,
    journal: Option<String>,
}

/// When an injected failure strikes: before the operation has any effect,
/// or after it took effect (a rename that succeeded and a sync that failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strike {
    Before,
    After,
}

#[derive(Debug, Default)]
struct Fake {
    disk: Disk,
    operations: usize,
    failures: Vec<(usize, Strike)>,
    log: Vec<String>,
}

impl Fake {
    fn new(disk: Disk) -> Self {
        Self {
            disk,
            ..Self::default()
        }
    }

    /// Counts an operation and says whether, and when, it fails.
    fn strike(&mut self, operation: String) -> Option<Strike> {
        self.operations += 1;
        self.log.push(operation);
        let at = self.operations;
        let index = self.failures.iter().position(|(n, _)| *n == at)?;
        Some(self.failures.remove(index).1)
    }

    fn run<T>(
        &mut self,
        operation: String,
        effect: impl FnOnce(&mut Disk) -> Result<T, String>,
    ) -> Result<T, String> {
        match self.strike(operation) {
            Some(Strike::Before) => Err("injected failure".to_owned()),
            Some(Strike::After) => {
                effect(&mut self.disk)?;
                Err("injected failure after the effect".to_owned())
            }
            None => effect(&mut self.disk),
        }
    }
}

impl PublicationFiles for Fake {
    type Error = String;

    fn stage(&mut self, name: &'static str, bytes: &[u8], _secret: bool) -> Result<(), String> {
        let bytes = bytes.to_vec();
        self.run(format!("stage {name}"), |disk| {
            disk.staged.insert(name, bytes);
            Ok(())
        })
    }

    fn read(&mut self, name: &'static str, slot: Slot) -> Result<Zeroizing<Vec<u8>>, String> {
        self.run(format!("read {name} {slot:?}"), |disk| {
            let source = match slot {
                Slot::Published => &disk.published,
                Slot::Staged => &disk.staged,
            };
            source
                .get(name)
                .map(|bytes| Zeroizing::new(bytes.clone()))
                .ok_or_else(|| format!("{name} missing"))
        })
    }

    fn exists(&mut self, name: &'static str) -> Result<bool, String> {
        self.run(format!("exists {name}"), |disk| {
            Ok(disk.published.contains_key(name))
        })
    }

    fn back_up(&mut self, name: &'static str, backup: &str) -> Result<(), String> {
        let backup = backup.to_owned();
        self.run(format!("back up {name}"), |disk| {
            // A move, as the Linux and Windows hosts do: the target is absent
            // until the publish.
            let bytes = disk
                .published
                .remove(name)
                .ok_or_else(|| format!("{name} missing"))?;
            disk.backups.insert(backup, bytes);
            Ok(())
        })
    }

    fn backup_exists(&mut self, backup: &str) -> Result<bool, String> {
        let backup = backup.to_owned();
        self.run(format!("backup exists {backup}"), |disk| {
            Ok(disk.backups.contains_key(&backup))
        })
    }

    fn publish(&mut self, name: &'static str, _secret: bool) -> Result<(), String> {
        self.run(format!("publish {name}"), |disk| {
            let bytes = disk
                .staged
                .remove(name)
                .ok_or_else(|| format!("{name} not staged"))?;
            disk.published.insert(name, bytes);
            Ok(())
        })
    }

    fn restore(&mut self, name: &'static str, backup: &str) -> Result<(), String> {
        let backup = backup.to_owned();
        self.run(format!("restore {name}"), |disk| {
            let bytes = disk
                .backups
                .remove(&backup)
                .ok_or_else(|| format!("{backup} missing"))?;
            disk.published.insert(name, bytes);
            Ok(())
        })
    }

    fn remove(&mut self, name: &'static str) -> Result<(), String> {
        self.run(format!("remove {name}"), |disk| {
            disk.published.remove(name);
            Ok(())
        })
    }

    fn remove_backup(&mut self, backup: &str) -> Result<(), String> {
        let backup = backup.to_owned();
        self.run(format!("remove backup {backup}"), |disk| {
            disk.backups.remove(&backup);
            Ok(())
        })
    }

    fn discard_staging(&mut self) -> Result<(), String> {
        self.run("discard staging".to_owned(), |disk| {
            disk.staged.clear();
            Ok(())
        })
    }

    fn write_journal(&mut self, text: &str) -> Result<(), String> {
        let text = text.to_owned();
        self.run("write journal".to_owned(), |disk| {
            disk.journal = Some(text);
            Ok(())
        })
    }

    fn read_journal(&mut self) -> Result<Option<String>, String> {
        self.run("read journal".to_owned(), |disk| Ok(disk.journal.clone()))
    }

    fn remove_journal(&mut self) -> Result<(), String> {
        self.run("remove journal".to_owned(), |disk| {
            disk.journal = None;
            Ok(())
        })
    }
}

/// A self-signed certificate over `key`, issued on `day`.
fn certificate(key: &KeyPair, day: u8) -> Vec<u8> {
    let mut params =
        CertificateParams::new(vec!["pier.example.internal".to_owned()]).expect("params");
    params.not_before = rcgen::date_time_ymd(2026, 1, day);
    params.not_after = rcgen::date_time_ymd(2028, 1, day);
    params
        .self_signed(key)
        .expect("certificate")
        .pem()
        .into_bytes()
}

fn insert_pins(disk: &mut Disk, certificate: &[u8]) {
    let pins = PinFiles::for_certificate(certificate).expect("pins");
    disk.published.insert("host.crt", certificate.to_vec());
    disk.published
        .insert("host.cert-sha256", pins.certificate_pin.into_bytes());
    disk.published
        .insert("host.spki-sha256", pins.spki_pin.into_bytes());
    disk.published
        .insert("host.generated-by-arcen", pins.marker.into_bytes());
}

/// A directory holding a complete pair over `key_file`.
fn deployed(key: &KeyPair, key_file: &[u8]) -> Disk {
    let mut disk = Disk::default();
    disk.published.insert("host.key", key_file.to_vec());
    insert_pins(&mut disk, &certificate(key, 1));
    disk
}

/// One way material is replaced: what is there, and what replaces it.
struct Case {
    label: &'static str,
    before: Disk,
    certificate: Vec<u8>,
    new_key: Option<Vec<u8>>,
    spki: [u8; 32],
}

impl Case {
    fn publication(&self) -> Publication<'_> {
        Publication {
            certificate_pem: &self.certificate,
            new_key_pem: self.new_key.as_deref(),
            spki_sha256: self.spki,
        }
    }

    /// The directory a complete publication leaves.
    fn after(&self) -> Disk {
        let mut disk = Disk {
            published: self.before.published.clone(),
            ..Disk::default()
        };
        if let Some(key) = &self.new_key {
            disk.published.insert("host.key", key.clone());
        }
        insert_pins(&mut disk, &self.certificate);
        disk
    }
}

fn cases() -> Vec<Case> {
    let key = KeyPair::generate().expect("key");
    let sec1 = openssl_sec1_key(&key);
    let spki = *HostKey::from_pem(&sec1).expect("key").spki_sha256();
    let renewal = Case {
        label: "renewal over a SEC1 key",
        before: deployed(&key, &sec1),
        certificate: certificate(&key, 2),
        new_key: None,
        spki,
    };

    let old = KeyPair::generate().expect("key");
    let fresh = KeyPair::generate().expect("key");
    let fresh_pem = fresh.serialize_pem().into_bytes();
    let fresh_spki = *HostKey::from_pem(&fresh_pem).expect("key").spki_sha256();
    let rekey = Case {
        label: "rekey",
        before: deployed(&old, old.serialize_pem().as_bytes()),
        certificate: certificate(&fresh, 3),
        new_key: Some(fresh_pem.clone()),
        spki: fresh_spki,
    };
    let first = Case {
        label: "first issue",
        before: Disk::default(),
        certificate: certificate(&fresh, 4),
        new_key: Some(fresh_pem),
        spki: fresh_spki,
    };
    vec![renewal, rekey, first]
}

const ID: &str = "4242-1800000000";

fn step_count(case: &Case) -> usize {
    let existed = MANAGED_FILES.map(|name| case.before.published.contains_key(name));
    PublicationPlan::new(case.new_key.is_some(), existed)
        .steps()
        .len()
}

#[test]
fn a_crash_after_any_step_recovers_to_the_old_or_the_new_pair_whole() {
    for case in cases() {
        let steps = step_count(&case);
        let mut saw_old = false;
        let mut saw_new = false;
        for limit in 0..=steps {
            let mut files = Fake::new(case.before.clone());
            publish(&mut files, &case.publication(), ID, limit).expect("stopped run");
            // The next start.
            let recovered = recover(&mut files).expect("recovery");
            // Staging without a journal is inert; the next writing run
            // replaces it.
            files.disk.staged.clear();
            if files.disk == case.before {
                saw_old = true;
            } else {
                assert_eq!(
                    files.disk,
                    case.after(),
                    "{}: crash after {limit} of {steps} steps left a mix (recovered {recovered:?})",
                    case.label
                );
                saw_new = true;
            }
        }
        assert!(saw_old && saw_new, "{}", case.label);
    }
}

#[test]
fn a_failure_at_any_operation_leaves_a_whole_pair_and_no_debris() {
    for case in cases() {
        let total = {
            let mut files = Fake::new(case.before.clone());
            publish(&mut files, &case.publication(), ID, usize::MAX).expect("clean run");
            assert_eq!(files.disk, case.after(), "{}", case.label);
            files.operations
        };
        for at in 1..=total {
            for strike in [Strike::Before, Strike::After] {
                let mut files = Fake::new(case.before.clone());
                files.failures.push((at, strike));
                let result = publish(&mut files, &case.publication(), ID, usize::MAX);
                let operation = files.log.get(at - 1).cloned().unwrap_or_default();
                match result {
                    Ok(()) => assert_eq!(files.disk, case.after()),
                    Err(PublicationError::CommittedNotCleared(_)) => {
                        // Published; the next run clears what is left.
                        recover(&mut files).expect("recovery");
                        assert_eq!(files.disk, case.after(), "{}: {operation}", case.label);
                    }
                    Err(error) => {
                        assert!(
                            matches!(error, PublicationError::RolledBack(_)),
                            "{}: {operation} {strike:?}: {error}",
                            case.label
                        );
                        let whole = files.disk == case.before || files.disk == case.after();
                        assert!(
                            whole,
                            "{}: failing {operation} ({strike:?}) left {:?}",
                            case.label, files.disk
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn a_failed_rollback_keeps_the_journal_and_the_next_run_finishes_it() {
    for case in cases() {
        let publishing = {
            let mut probe = Fake::new(case.before.clone());
            publish(&mut probe, &case.publication(), ID, usize::MAX).expect("clean");
            probe
                .log
                .iter()
                .position(|op| op.starts_with("publish "))
                .expect("a publish")
                + 1
        };
        // Fail the first publish, then the first operation of the rollback.
        let mut files = Fake::new(case.before.clone());
        files.failures.push((publishing, Strike::Before));
        files.failures.push((publishing + 1, Strike::Before));
        let error = publish(&mut files, &case.publication(), ID, usize::MAX)
            .expect_err("injected failures");
        assert!(
            matches!(error, PublicationError::RollbackIncomplete { .. }),
            "{}: {error}",
            case.label
        );
        assert!(files.disk.journal.is_some(), "the journal is kept");

        recover(&mut files).expect("the next run recovers");
        assert_eq!(files.disk, case.before, "{}", case.label);
    }
}

#[test]
fn a_renewal_never_touches_the_key_file_or_its_pin() {
    let case = cases().remove(0);
    let mut files = Fake::new(case.before.clone());
    publish(&mut files, &case.publication(), ID, usize::MAX).expect("renewal");
    let touched: Vec<_> = files
        .log
        .iter()
        .filter(|op| {
            op.contains("host.key") && !op.starts_with("exists") && !op.starts_with("read")
        })
        .collect();
    assert!(touched.is_empty(), "{touched:?}");
    assert_eq!(
        files.disk.published.get("host.key"),
        case.before.published.get("host.key"),
        "key bytes unchanged"
    );
    assert_eq!(
        files.disk.published.get("host.spki-sha256"),
        case.before.published.get("host.spki-sha256"),
        "SPKI pin unchanged"
    );
}

#[test]
fn a_replacement_over_another_key_is_refused_before_anything_moves() {
    let case = cases().remove(0);
    let other = KeyPair::generate().expect("key");
    let foreign = certificate(&other, 5);
    let mut files = Fake::new(case.before.clone());
    let error = publish(
        &mut files,
        &Publication {
            certificate_pem: &foreign,
            new_key_pem: None,
            spki_sha256: case.spki,
        },
        ID,
        usize::MAX,
    )
    .expect_err("mismatch");
    assert!(matches!(error, PublicationError::Mismatch), "{error}");
    assert_eq!(files.disk, case.before);
    assert!(
        files
            .log
            .iter()
            .all(|op| !op.starts_with("back up") && !op.starts_with("publish")),
        "{:?}",
        files.log
    );
}

#[test]
fn a_journal_that_cannot_be_acted_on_is_left_for_an_operator() {
    let case = cases().remove(0);
    let mut disk = case.before.clone();
    disk.journal = Some("transaction=../../etc\nphase=prepared\n".to_owned());
    let mut files = Fake::new(disk.clone());
    assert!(matches!(
        recover(&mut files),
        Err(RecoveryError::Journal(JournalError::InvalidTransactionId))
    ));
    assert_eq!(files.disk, disk);
}

#[test]
fn a_file_missing_against_its_journal_is_reported_not_recreated() {
    let case = cases().remove(0);
    let mut disk = case.before.clone();
    disk.journal = Some(
        TransactionJournal::new(ID, TransactionPhase::Prepared, [true; MANAGED_FILES.len()])
            .render(),
    );
    disk.published.remove("host.key");
    let mut files = Fake::new(disk);
    assert!(matches!(
        recover(&mut files),
        Err(RecoveryError::Missing("host.key"))
    ));
    assert!(files.disk.journal.is_some());
}
