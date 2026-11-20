// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use std::io::{Cursor, Read};

use arcen_telemetry::{BundlePseudonymKey, SUPPORT_BUNDLE_SCHEMA_VERSION};

use super::source::{Stage, open_source_with};
use super::*;

fn component() -> BundleComponent {
    BundleComponent {
        name: "arcen-test".to_owned(),
        version: "test".to_owned(),
        os: "test".to_owned(),
        arch: "test".to_owned(),
    }
}

fn collector(limits: BundleLimits) -> Collector<Cursor<Vec<u8>>> {
    Collector::new(
        Cursor::new(Vec::new()),
        component(),
        1_794_726_245,
        BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x11; 32])),
        limits,
    )
}

#[test]
fn every_pier_shares_one_set_of_limits() {
    assert_eq!(BundleLimits::PIER.max_log_bytes, 200 * MIB);
    assert_eq!(BundleLimits::PIER.max_total_bytes, 256 * MIB);
    assert_eq!(BundleLimits::PIER.max_document_bytes, MIB);
    assert_eq!(BundleLimits::PIER.max_log_file_input_bytes, None);
    assert_eq!(
        BundleLimits::DECK.max_total_bytes,
        BundleLimits::PIER.max_total_bytes
    );
}

#[test]
fn the_manifest_hashes_every_entry_and_is_not_its_own_entry() {
    let mut bundle = collector(BundleLimits::PIER);
    assert!(
        bundle
            .add_bytes(
                "diagnostics/fixture.txt",
                BundleSource::Diagnostics,
                b"fixture"
            )
            .expect("payload")
    );
    let (writer, manifest, omissions) = bundle.finish().expect("archive");
    assert_eq!(omissions, 0);
    assert_eq!(manifest.schema_version, SUPPORT_BUNDLE_SCHEMA_VERSION);
    assert_eq!(manifest.entries.len(), 1);
    assert_eq!(manifest.entries[0].path.as_str(), "diagnostics/fixture.txt");
    let mut archive = zip::ZipArchive::new(writer).expect("ZIP");
    let mut text = String::new();
    archive
        .by_name("diagnostics/fixture.txt")
        .expect("entry")
        .read_to_string(&mut text)
        .expect("read");
    assert_eq!(text, "fixture");
    let entry = archive.by_name("manifest.json").expect("manifest");
    assert_eq!(entry.unix_mode().map(|mode| mode & 0o777), Some(0o600));
    let stamp = entry.last_modified().expect("time");
    assert_eq!(
        (stamp.year(), stamp.month(), stamp.day(), stamp.hour()),
        (2026, 11, 15, 7)
    );
}

#[test]
fn a_fixed_time_key_and_content_give_the_same_bytes() {
    let build = || {
        let mut bundle = collector(BundleLimits::PIER);
        bundle
            .add_bytes("diagnostics/a.txt", BundleSource::Diagnostics, b"alpha")
            .expect("payload");
        bundle.finish().expect("archive").0.into_inner()
    };
    assert_eq!(build(), build());
}

#[test]
fn an_entry_past_the_payload_limit_is_omitted_and_counted() {
    let limits = BundleLimits {
        max_total_bytes: 4,
        ..BundleLimits::PIER
    };
    let mut bundle = collector(limits);
    assert!(
        !bundle
            .add_bytes("diagnostics/big.txt", BundleSource::Diagnostics, b"too big")
            .expect("notice")
    );
    assert_eq!(bundle.total_bytes(), 0);
    // The manifest itself is larger than four bytes: the finished archive
    // is refused rather than published over the limit.
    assert!(matches!(
        bundle.finish(),
        Err(SupportBundleError::Archive(reason)) if reason.contains("limit")
    ));

    let mut bundle = collector(BundleLimits::PIER);
    bundle
        .notice(
            "diagnostics/big.txt",
            NoticeKind::Omitted,
            NoticeCode::TotalPayloadLimit,
        )
        .expect("notice");
    bundle
        .notice("logs", NoticeKind::Truncated, NoticeCode::LogPayloadLimit)
        .expect("notice");
    bundle
        .notice("logs", NoticeKind::Advisory, NoticeCode::LegacyLogMode)
        .expect("notice");
    let (_, manifest, omissions) = bundle.finish().expect("archive");
    assert_eq!(omissions, 1, "only the omission counts");
    assert_eq!(manifest.notices.len(), 3);
}

#[test]
fn a_document_brings_its_redactions_only_when_it_is_added() {
    let path = BundlePath::new("config/pier.json").expect("path");
    let redaction = || RedactionRecord {
        entry_path: path.clone(),
        key_path: "/tls/key".to_owned(),
        reason: RedactionReason::PrivateKeyPolicy,
    };
    let mut bundle = collector(BundleLimits {
        max_total_bytes: 1 << 20,
        ..BundleLimits::PIER
    });
    assert!(
        bundle
            .add_document(
                "config/pier.json",
                BundleSource::Configuration,
                Some(99),
                b"{}",
                vec![redaction()],
            )
            .expect("document")
    );
    let (_, manifest, _) = bundle.finish().expect("archive");
    assert_eq!(manifest.redactions.len(), 1);
    assert_eq!(manifest.entries[0].original_size_bytes, 99);

    let mut full = collector(BundleLimits {
        max_total_bytes: 1,
        ..BundleLimits::PIER
    });
    assert!(
        !full
            .add_document(
                "config/pier.json",
                BundleSource::Configuration,
                None,
                b"{}",
                vec![redaction()],
            )
            .expect("notice")
    );
}

#[test]
fn the_output_option_grammar_is_strict() {
    let arguments = |values: &[&str]| -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    };
    let parse = |values: &[&str]| parse_output_option(&arguments(values));
    assert_eq!(parse(&[]), Ok(OutputOption::Write(WriteOptions::default())));
    assert_eq!(
        parse(&["--out", "D:\\support"]),
        Ok(OutputOption::Write(WriteOptions {
            output_directory: Some(PathBuf::from("D:\\support")),
            include_crash_dumps: false,
        }))
    );
    assert_eq!(
        parse(&["--include-crash-dumps", "--out", "/x"]),
        Ok(OutputOption::Write(WriteOptions {
            output_directory: Some(PathBuf::from("/x")),
            include_crash_dumps: true,
        }))
    );
    assert!(parse(&["--include-crash-dumps", "--include-crash-dumps"]).is_err());
    assert_eq!(parse(&["--help"]), Ok(OutputOption::Help));
    assert_eq!(parse(&["-h"]), Ok(OutputOption::Help));
    assert!(parse(&["--out"]).is_err());
    assert!(parse(&["--unknown"]).is_err());
    assert_eq!(
        parse(&["--out", "/a", "--out", "/b"]),
        Err("--out may be supplied only once".to_owned())
    );
}

#[test]
fn archive_names_and_times_are_utc() {
    assert_eq!(utc_stamp(1_794_726_245), "20261115T070405Z");
    assert_eq!(rfc3339(1_794_726_245), "2026-11-15T07:04:05Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
}

#[test]
fn read_errors_map_to_their_notices() {
    let notice = |kind| io_notice(&std::io::Error::from(kind));
    assert_eq!(
        notice(std::io::ErrorKind::NotFound),
        (NoticeKind::Unavailable, NoticeCode::SourceNotFound)
    );
    assert_eq!(
        notice(std::io::ErrorKind::PermissionDenied),
        (
            NoticeKind::PermissionDenied,
            NoticeCode::SourcePermissionDenied
        )
    );
    assert_eq!(
        notice(std::io::ErrorKind::Other),
        (NoticeKind::Unavailable, NoticeCode::SourceUnavailable)
    );
    assert_eq!(
        SourceOpenError::UnsafeType.notice(),
        (NoticeKind::Invalid, NoticeCode::UnsafeFileType)
    );
    assert_eq!(
        SourceOpenError::Replaced.notice(),
        (NoticeKind::Invalid, NoticeCode::SourceChangedDuringRead)
    );
    assert_eq!(
        SourceOpenError::PathLinked.notice(),
        (NoticeKind::Invalid, NoticeCode::SourcePathLinked)
    );
    assert_eq!(
        SourceOpenError::HardLinked.notice(),
        (NoticeKind::Invalid, NoticeCode::SourceHardLinked)
    );
    for unverifiable in [
        SourceOpenError::IdentityUnproven,
        SourceOpenError::IdentityUnstable,
    ] {
        assert_eq!(
            unverifiable.notice(),
            (
                NoticeKind::Unavailable,
                NoticeCode::SourceIdentityUnverifiable
            )
        );
    }
}

/// A regular file with one name, `file` on volume `0x1234_5678`.
fn facts(file: u128, scheme: IdentityScheme) -> SourceFacts {
    SourceFacts {
        kind: SourceKind::Regular,
        reparse_point: false,
        links: 1,
        identity: FileIdentity {
            volume: 0x1234_5678,
            file,
        },
        scheme,
    }
}

#[test]
fn only_the_same_numbered_file_on_the_same_volume_is_the_inspected_source() {
    let inspected = facts(0x0001_0000_0000_002a, IdentityScheme::FileId128);
    assert!(verify_same_source(&inspected, &inspected, VolumeFacts::LOCAL).is_ok());
    let mut other_file = inspected;
    other_file.identity.file = 0x0001_0000_0000_002b;
    let mut other_volume = inspected;
    other_volume.identity.volume = 0x1234_5679;
    // ReFS: two files whose 128-bit IDs share their low 64 bits (all a
    // 64-bit file index would show) are different files.
    let mut same_low_half = inspected;
    same_low_half.identity.file |= 1 << 64;
    for opened in [other_file, other_volume, same_low_half] {
        assert!(matches!(
            verify_same_source(&inspected, &opened, VolumeFacts::LOCAL),
            Err(SourceOpenError::Replaced)
        ));
    }
    // A volume without a serial number still numbers its files.
    let mut unnumbered_volume = inspected;
    unnumbered_volume.identity.volume = 0;
    assert!(verify_same_source(&unnumbered_volume, &unnumbered_volume, VolumeFacts::LOCAL).is_ok());
}

#[test]
fn a_file_system_without_file_numbers_proves_no_source() {
    let unnumbered = facts(0, IdentityScheme::FileId128);
    assert!(matches!(
        verify_same_source(&unnumbered, &unnumbered, VolumeFacts::LOCAL),
        Err(SourceOpenError::IdentityUnproven)
    ));
    let numbered = facts(7, IdentityScheme::FileId128);
    for (inspected, opened) in [(numbered, unnumbered), (unnumbered, numbered)] {
        assert!(matches!(
            verify_same_source(&inspected, &opened, VolumeFacts::LOCAL),
            Err(SourceOpenError::Replaced)
        ));
    }
}

#[test]
fn a_volume_that_cannot_prove_identity_is_unverifiable_not_replaced() {
    let inspected = facts(42, IdentityScheme::FileId128);
    let other = facts(43, IdentityScheme::FileId128);
    let network = VolumeFacts {
        remote: true,
        stable_file_ids: true,
    };
    let unstable = VolumeFacts {
        remote: false,
        stable_file_ids: false,
    };
    for volume in [network, unstable] {
        for opened in [inspected, other] {
            assert!(matches!(
                verify_same_source(&inspected, &opened, volume),
                Err(SourceOpenError::IdentityUnstable)
            ));
        }
    }
    let index_only = facts(42, IdentityScheme::FileIndex64);
    assert!(matches!(
        verify_same_source(&inspected, &index_only, VolumeFacts::LOCAL),
        Err(SourceOpenError::IdentityUnstable)
    ));
    assert!(verify_same_source(&index_only, &index_only, VolumeFacts::LOCAL).is_ok());
}

#[test]
fn only_a_regular_file_with_one_name_is_a_source() {
    let regular = facts(42, IdentityScheme::DeviceInode);
    assert!(check_source(&regular).is_ok());
    let deleted_since_open = SourceFacts {
        links: 0,
        ..regular
    };
    assert!(check_source(&deleted_since_open).is_ok());
    let hard_linked = SourceFacts {
        links: 2,
        ..regular
    };
    assert!(matches!(
        check_source(&hard_linked),
        Err(SourceOpenError::HardLinked)
    ));
    for kind in [SourceKind::Directory, SourceKind::Link, SourceKind::Other] {
        assert!(matches!(
            check_source(&SourceFacts { kind, ..regular }),
            Err(SourceOpenError::UnsafeType)
        ));
    }
    let reparse = SourceFacts {
        reparse_point: true,
        ..regular
    };
    assert!(matches!(
        check_source(&reparse),
        Err(SourceOpenError::UnsafeType)
    ));
}

#[test]
fn only_a_link_nobody_else_could_have_made_is_followed() {
    let private = ContainerFacts {
        owner: Owner::System,
        writable_by_others: false,
    };
    let system = LinkFacts {
        target: Some(std::path::PathBuf::from("private/var")),
        owner: Owner::System,
        container: private,
    };
    assert!(link_is_trusted(&system));
    let own = LinkFacts {
        owner: Owner::Process,
        container: ContainerFacts {
            owner: Owner::Process,
            ..private
        },
        ..system.clone()
    };
    assert!(link_is_trusted(&own));
    let refused = [
        LinkFacts {
            owner: Owner::Other,
            ..system.clone()
        },
        LinkFacts {
            container: ContainerFacts {
                owner: Owner::Other,
                ..private
            },
            ..system.clone()
        },
        LinkFacts {
            container: ContainerFacts {
                writable_by_others: true,
                ..private
            },
            ..system.clone()
        },
        // A Windows reparse point has no target to follow.
        LinkFacts {
            target: None,
            ..system
        },
    ];
    for link in &refused {
        assert!(!link_is_trusted(link), "{link:?}");
    }
}

#[test]
fn only_a_file_index_is_recorded_as_an_advisory() {
    let root = scratch("advisory");
    let source = root.join("arcen.jsonl");
    std::fs::write(&source, b"x\n").expect("source");
    let mut opened = open_source(&source).expect("source opens");
    assert_eq!(opened.advisory(), None);
    opened.scheme = IdentityScheme::FileIndex64;
    assert_eq!(
        opened.advisory(),
        Some((NoticeKind::Advisory, NoticeCode::SourceIdentityIndexOnly))
    );
    drop(opened);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_pier_config_loses_its_tls_identity_and_keeps_its_posture() {
    let archive_path = BundlePath::new("config/pier.json").expect("path");
    let mut document = serde_json::json!({
        "tls": {
            "certificate": "C:\\sensitive\\host.crt",
            "private_key": "C:\\sensitive\\host.key",
            "cert": "/etc/arcen/host.crt",
            "key": "/etc/arcen/host.key",
            "minimum_version": "TLS1.3",
            "expected_sans": ["host.example"]
        },
        "auth": {"password": "hunter2"}
    });
    let redactions = redact_pier_config(&archive_path, &mut document).expect("redactions");
    for key in ["certificate", "private_key", "cert", "key", "expected_sans"] {
        assert_eq!(document["tls"][key], REDACTED_VALUE, "{key}");
    }
    assert_eq!(document["auth"]["password"], REDACTED_VALUE);
    assert_eq!(document["tls"]["minimum_version"], "TLS1.3");
    let paths: Vec<(&str, RedactionReason)> = redactions
        .iter()
        .map(|redaction| (redaction.key_path.as_str(), redaction.reason))
        .collect();
    assert_eq!(
        paths,
        [
            ("/auth/password", RedactionReason::SensitiveKey),
            ("/tls/cert", RedactionReason::SensitiveKey),
            ("/tls/certificate", RedactionReason::SensitiveKey),
            ("/tls/expected_sans", RedactionReason::SensitiveKey),
            ("/tls/key", RedactionReason::PrivateKeyPolicy),
            ("/tls/private_key", RedactionReason::PrivateKeyPolicy),
        ]
    );
}

/// A fresh folder under the build's `target/tmp`, beside the test binary.
fn scratch(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let target = exe
        .ancestors()
        .nth(3)
        .expect("target folder above <profile>/deps");
    let root = target
        .join("tmp")
        .join(format!("observability-unit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("scratch folder");
    root
}

#[test]
fn a_source_replaced_between_inspection_and_open_is_refused() {
    let root = scratch("replaced");
    let source = root.join("arcen.jsonl");
    let replacement = root.join("replacement.jsonl");
    std::fs::write(&source, b"inspected\n").expect("source");
    std::fs::write(&replacement, b"replacement\n").expect("replacement");
    let result = open_source_with(&source, &mut |stage| {
        if stage == Stage::Inspected {
            std::fs::rename(&replacement, &source).expect("replace the inspected file");
        }
    });
    assert!(
        matches!(result, Err(SourceOpenError::Replaced)),
        "a file put in the inspected one's place must be refused: {result:?}"
    );
    assert_eq!(
        std::fs::read(&source).expect("replaced source"),
        b"replacement\n"
    );
    let mut file = open_source(&source)
        .expect("an untouched source opens")
        .file;
    let mut contents = String::new();
    file.read_to_string(&mut contents).expect("read");
    assert_eq!(contents, "replacement\n");
    drop(file);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_source_replaced_by_a_link_between_inspection_and_open_is_refused() {
    let root = scratch("relinked");
    let source = root.join("arcen.jsonl");
    let secret = root.join("secret.txt");
    std::fs::write(&source, b"inspected\n").expect("source");
    std::fs::write(&secret, b"secret\n").expect("link target");
    let result = open_source_with(&source, &mut |stage| {
        if stage != Stage::Inspected {
            return;
        }
        std::fs::remove_file(&source).expect("remove the inspected file");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &source).expect("symlink");
        // A file symbolic link needs an elevated or developer-mode account
        // on Windows; the lab's test account is one.
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&secret, &source).expect("symlink (elevated account)");
    });
    // Unix refuses to follow the link (O_NOFOLLOW); Windows opens the
    // reparse point itself and refuses its type. Neither reads the target.
    assert!(
        matches!(result, Err(SourceOpenError::UnsafeType)),
        "a link put in the inspected file's place must be refused: {result:?}"
    );
    std::fs::remove_dir_all(root).ok();
}

/// Runs `open` on another thread and fails unless it answers within five
/// seconds: opening a FIFO for reading blocks until a writer comes.
#[cfg(all(test, unix))]
fn answers_promptly<T: Send + 'static>(open: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(open());
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("opening a source must answer at once, not block")
}

#[cfg(all(test, unix))]
fn make_fifo(path: &std::path::Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo {}", path.display());
}

#[test]
#[cfg(all(test, unix))]
fn a_fifo_put_in_place_of_a_source_is_refused_without_blocking() {
    let root = scratch("fifo");
    let source = root.join("arcen.jsonl");
    std::fs::write(&source, b"inspected\n").expect("source");
    let path = source.clone();
    let result = answers_promptly(move || {
        open_source_with(&path, &mut |stage| {
            if stage == Stage::Inspected {
                std::fs::remove_file(&source).expect("remove the inspected file");
                make_fifo(&source);
            }
        })
        .map(|_| ())
    });
    assert!(
        matches!(result, Err(SourceOpenError::UnsafeType)),
        "a FIFO must be refused: {result:?}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
#[cfg(all(test, unix))]
fn a_source_behind_a_link_planted_in_a_shared_folder_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("planted");
    let secret = root.join("secret");
    std::fs::create_dir(&secret).expect("secret folder");
    std::fs::write(secret.join("arcen.jsonl"), b"secret\n").expect("secret file");
    let shared = root.join("shared");
    std::fs::create_dir(&shared).expect("shared folder");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777))
        .expect("anyone may write in the shared folder");
    std::os::unix::fs::symlink(&secret, shared.join("logs")).expect("planted link");
    let result = open_source(&shared.join("logs").join("arcen.jsonl")).map(|_| ());
    assert!(
        matches!(result, Err(SourceOpenError::PathLinked)),
        "a folder link anyone could have planted must not be followed: {result:?}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
#[cfg(all(test, windows))]
fn a_source_behind_a_junction_is_refused() {
    let root = scratch("junction");
    let secret = root.join("secret");
    std::fs::create_dir(&secret).expect("secret folder");
    std::fs::write(secret.join("arcen.jsonl"), b"secret\n").expect("secret file");
    let logs = root.join("logs");
    // A junction needs no privilege: any account that can write a folder
    // can plant one.
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&logs)
        .arg(&secret)
        .status()
        .expect("run mklink");
    assert!(status.success(), "mklink /J");
    let result = open_source(&logs.join("arcen.jsonl")).map(|_| ());
    assert!(
        matches!(result, Err(SourceOpenError::PathLinked)),
        "a folder reparse point must not be followed: {result:?}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_source_with_a_second_name_is_refused() {
    let root = scratch("hardlink");
    let secret = root.join("secret.txt");
    std::fs::write(&secret, b"secret\n").expect("secret file");
    let source = root.join("arcen.jsonl");
    std::fs::hard_link(&secret, &source).expect("hard link");
    let result = open_source(&source).map(|_| ());
    assert!(
        matches!(result, Err(SourceOpenError::HardLinked)),
        "a source with another name must be refused: {result:?}"
    );
    std::fs::remove_dir_all(root).ok();
}

/// `path` (`C:\…`) through this machine's administrative share
/// (`\\localhost\C$\…`): the same file, on a network (SMB) volume.
#[cfg(all(test, windows))]
fn through_admin_share(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let text = path.to_str()?;
    let (drive, rest) = text.split_once(":\\")?;
    Some(std::path::PathBuf::from(format!(
        r"\\localhost\{drive}$\{rest}"
    )))
}

#[test]
#[cfg(all(test, windows))]
fn a_source_on_a_network_volume_is_refused() {
    let root = scratch("remote");
    let source = root.join("arcen.jsonl");
    std::fs::write(&source, b"local\n").expect("source");
    let Some(remote) = through_admin_share(&source) else {
        eprintln!("skipped: {} has no drive letter", source.display());
        return;
    };
    if std::fs::metadata(&remote).is_err() {
        eprintln!("skipped: {} is not reachable", remote.display());
        return;
    }
    let result = open_source(&remote).map(|_| ());
    assert!(
        matches!(result, Err(SourceOpenError::IdentityUnstable)),
        "a network volume cannot prove which file was opened: {result:?}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
#[cfg(all(test, unix))]
fn a_fifo_at_a_source_path_is_refused_without_blocking() {
    let root = scratch("fifo-planted");
    let source = root.join("arcen.jsonl");
    make_fifo(&source);
    let path = source.clone();
    let result = answers_promptly(move || open_source(&path).map(|_| ()));
    assert!(
        matches!(result, Err(SourceOpenError::UnsafeType)),
        "a FIFO must be refused: {result:?}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
#[cfg(all(test, unix))]
fn a_link_in_a_folder_only_its_owner_can_write_is_followed() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("private-link");
    let real = root.join("real");
    std::fs::create_dir(&real).expect("real folder");
    std::fs::write(real.join("arcen.jsonl"), b"real\n").expect("source");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("private");
    std::os::unix::fs::symlink("real", root.join("logs")).expect("own link");
    let mut contents = String::new();
    open_source(&root.join("logs").join("arcen.jsonl"))
        .expect("the account's own link in its own folder is followed")
        .file
        .read_to_string(&mut contents)
        .expect("read");
    assert_eq!(contents, "real\n");
    std::fs::remove_dir_all(root).ok();
}

#[test]
#[cfg(all(test, unix))]
fn a_folder_swapped_after_the_walk_does_not_redirect_the_open() {
    let root = scratch("swapped-folder");
    let logs = root.join("logs");
    std::fs::create_dir(&logs).expect("logs");
    std::fs::write(logs.join("arcen.jsonl"), b"walked\n").expect("source");
    let secret = root.join("secret");
    std::fs::create_dir(&secret).expect("secret folder");
    std::fs::write(secret.join("arcen.jsonl"), b"secret\n").expect("secret file");
    let mut opened = open_source_with(&logs.join("arcen.jsonl"), &mut |stage| {
        if stage == Stage::Walked {
            std::fs::rename(&logs, root.join("moved")).expect("move the walked folder");
            std::os::unix::fs::symlink(&secret, &logs).expect("link in its place");
        }
    })
    .expect("the walked folder's own file opens");
    let mut contents = String::new();
    opened.file.read_to_string(&mut contents).expect("read");
    assert_eq!(
        contents, "walked\n",
        "the open must stay in the walked folder"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
#[cfg(all(test, windows))]
fn a_folder_swapped_for_a_junction_after_the_walk_is_refused() {
    let root = scratch("swapped-junction");
    let logs = root.join("logs");
    std::fs::create_dir(&logs).expect("logs");
    std::fs::write(logs.join("arcen.jsonl"), b"walked\n").expect("source");
    let secret = root.join("secret");
    std::fs::create_dir(&secret).expect("secret folder");
    std::fs::write(secret.join("arcen.jsonl"), b"secret\n").expect("secret file");
    let result = open_source_with(&logs.join("arcen.jsonl"), &mut |stage| {
        if stage == Stage::Walked {
            std::fs::rename(&logs, root.join("moved")).expect("move the walked folder");
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&logs)
                .arg(&secret)
                .status()
                .expect("run mklink");
            assert!(status.success(), "mklink /J");
        }
    })
    .map(|_| ());
    // The inspection and the open both pass through the junction to the
    // same file; only its real location shows it is not in the folder
    // the walk proved.
    assert!(
        matches!(result, Err(SourceOpenError::PathLinked)),
        "a file reached through a junction swapped in after the walk: {result:?}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
#[cfg(all(test, windows))]
fn an_ntfs_source_is_proven_by_its_128_bit_file_id() {
    let root = scratch("file-id");
    let source = root.join("arcen.jsonl");
    std::fs::write(&source, b"x\n").expect("source");
    let opened = open_source(&source).expect("source opens");
    assert_eq!(opened.scheme, IdentityScheme::FileId128);
    assert_eq!(opened.advisory(), None);
    drop(opened);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_relative_source_path_climbs_from_the_current_folder() {
    use std::path::Component;
    let root = scratch("relative");
    let source = root.join("arcen.jsonl");
    std::fs::write(&source, b"relative\n").expect("source");
    let cwd = std::env::current_dir().expect("current folder");
    // `../../…/<source without its root>`: up to the root, then down (the
    // test binary's folder and the crate's are on one drive).
    let mut relative = std::path::PathBuf::new();
    for _ in cwd
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
    {
        relative.push("..");
    }
    for component in source
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
    {
        relative.push(component);
    }
    let mut contents = String::new();
    open_source(&relative)
        .unwrap_or_else(|error| panic!("{}: {error:?}", relative.display()))
        .file
        .read_to_string(&mut contents)
        .expect("read");
    assert_eq!(contents, "relative\n");
    std::fs::remove_dir_all(root).ok();
}
