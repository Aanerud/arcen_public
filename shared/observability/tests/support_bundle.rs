// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The shared support-bundle collector against real files: canonical log
//! export and pseudonyms, tails, unsafe sources, and the private,
//! atomically published archive every Pier and Deck writes.

#![cfg(feature = "support-bundle")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use arcen_observability::support_bundle::{
    BundleLimits, BundleSealer, Collector, LogCandidate, PreflightRefusal, PublicSummary,
    SummaryDetails, SummaryFact, SupportBundleError, collect_canonical_logs, fingerprint,
    read_json_document, write_archive, write_sealed_archive,
};
use arcen_telemetry::{
    BundleComponent, BundlePath, BundlePseudonymKey, BundlePseudonymizer, BundleSource, NoticeCode,
    NoticeKind, RedactionReason, RedactionRecord, TruncationReason,
};
use sha2::{Digest, Sha256};

fn scratch(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "observability-bundle-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("scratch folder");
    root
}

fn component() -> BundleComponent {
    BundleComponent {
        name: "arcen-test".to_owned(),
        version: "test".to_owned(),
        os: "test".to_owned(),
        arch: "test".to_owned(),
    }
}

fn collector(path: &Path, limits: BundleLimits) -> Collector<File> {
    Collector::new(
        File::create(path).expect("archive"),
        component(),
        42,
        BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x55; 32])),
        limits,
    )
}

fn canonical_line(sequence: u64) -> Vec<u8> {
    let mut line = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "timestamp": "2026-07-24T16:00:00.000000Z",
        "sequence": sequence,
        "profile_level": 0,
        "profile_name": "critical",
        "severity": "info",
        "role": "host",
        "component": "pier",
        "platform": "linux",
        "target": "arcen::session",
        "sid": null,
        "user": "raw-artist",
        "host": "raw-pier",
        "peer_addr": "192.0.2.77:4444",
        "health_state": null,
        "message": "fixture",
        "fields": {"ssid": "raw-network"}
    }))
    .expect("fixture");
    line.push(b'\n');
    line
}

fn read_entry(archive: &mut zip::ZipArchive<File>, name: &str) -> String {
    let mut text = String::new();
    archive
        .by_name(name)
        .expect(name)
        .read_to_string(&mut text)
        .expect("read entry");
    text
}

#[test]
fn log_exports_hide_identity_and_correlate_across_archive_entries() {
    let root = scratch("correlate");
    let line = canonical_line(1);
    let first = root.join("first.jsonl");
    let second = root.join("second.jsonl");
    std::fs::write(&first, &line).expect("first log");
    std::fs::write(&second, &line).expect("second log");
    let archive_path = root.join("bundle.zip");
    let mut bundle = collector(&archive_path, BundleLimits::PIER);
    for (source, destination) in [(&first, "logs/first.jsonl"), (&second, "logs/second.jsonl")] {
        let written = bundle
            .add_canonical_log_file(
                source,
                destination,
                line.len() as u64,
                line.len() as u64,
                1024 * 1024,
                TruncationReason::PerSourceLimit,
            )
            .expect("pseudonymized log");
        assert_ne!(written, 0);
    }
    let (file, manifest, _) = bundle.finish().expect("finish archive");
    drop(file);

    let mut archive = zip::ZipArchive::new(File::open(&archive_path).expect("open")).expect("ZIP");
    let first_export = read_entry(&mut archive, "logs/first.jsonl");
    let second_export = read_entry(&mut archive, "logs/second.jsonl");
    let manifest_text = read_entry(&mut archive, "manifest.json");
    let combined = format!("{first_export}{second_export}{manifest_text}");
    for identity in ["raw-artist", "raw-pier", "192.0.2.77:4444", "raw-network"] {
        assert!(!combined.contains(identity), "{identity} leaked");
    }
    let first_value: serde_json::Value = serde_json::from_str(&first_export).expect("JSON");
    let second_value: serde_json::Value = serde_json::from_str(&second_export).expect("JSON");
    assert_eq!(first_value["user"], second_value["user"]);
    assert_eq!(
        first_value["fields"]["ssid"],
        second_value["fields"]["ssid"]
    );
    assert!(manifest_text.contains("identity_pseudonymized"));
    let entry = manifest
        .entries
        .iter()
        .find(|entry| entry.path.as_str() == "logs/first.jsonl")
        .expect("first entry");
    assert_eq!(entry.included_size_bytes, first_export.len() as u64);
    let rendered = serde_json::to_value(entry).expect("entry JSON");
    assert_eq!(
        rendered["sha256"],
        format!("{:x}", Sha256::digest(first_export.as_bytes()))
    );
    std::fs::remove_dir_all(root).ok();
}

/// A Windows Pier or Deck log names home folders (`%LOCALAPPDATA%` expands
/// to `C:\Users\<name>\AppData\Local`): the account name leaves every
/// path, backslash or forward slash, in fields and in the message, and the
/// path keeps its shape with the record user's pseudonym.
#[test]
fn a_home_folder_in_a_log_keeps_its_path_and_loses_its_account() {
    let root = scratch("home-paths");
    let mut record: serde_json::Value =
        serde_json::from_slice(&canonical_line(1)).expect("fixture");
    record["user"] = "alice".into();
    record["platform"] = "windows".into();
    record["message"] = r"could not write C:\Users\alice\AppData\Local\Arcen\Deck\Logs".into();
    record["fields"]["config"] = "C:/Users/alice/AppData/Roaming/Arcen/Deck/settings.json".into();
    record["fields"]["mac"] = "/Users/alice/Library/Logs/Arcen".into();
    record["fields"]["linux"] = "/home/alice/.config/arcen".into();
    let mut line = serde_json::to_vec(&record).expect("line");
    line.push(b'\n');
    let source = root.join("arcen-client.log");
    std::fs::write(&source, &line).expect("log");
    let archive_path = root.join("bundle.zip");
    let mut bundle = collector(&archive_path, BundleLimits::PIER);
    bundle
        .add_canonical_log_file(
            &source,
            "logs/arcen-client.log",
            line.len() as u64,
            line.len() as u64,
            1024 * 1024,
            TruncationReason::PerSourceLimit,
        )
        .expect("pseudonymized log");
    let (file, _, _) = bundle.finish().expect("finish archive");
    drop(file);

    let mut archive = zip::ZipArchive::new(File::open(&archive_path).expect("open")).expect("ZIP");
    let export = read_entry(&mut archive, "logs/arcen-client.log");
    let manifest = read_entry(&mut archive, "manifest.json");
    assert!(!export.contains("alice"), "{export}");
    let value: serde_json::Value = serde_json::from_str(&export).expect("JSON");
    let user = value["user"].as_str().expect("user pseudonym");
    assert!(user.starts_with("anon:"));
    assert_eq!(
        value["message"],
        format!(r"could not write C:\Users\{user}\AppData\Local\Arcen\Deck\Logs")
    );
    assert_eq!(
        value["fields"]["config"],
        format!("C:/Users/{user}/AppData/Roaming/Arcen/Deck/settings.json")
    );
    assert_eq!(
        value["fields"]["mac"],
        format!("/Users/{user}/Library/Logs/Arcen")
    );
    assert_eq!(
        value["fields"]["linux"],
        format!("/home/{user}/.config/arcen")
    );
    for path in ["/message", "/fields/config", "/fields/mac", "/fields/linux"] {
        assert!(manifest.contains(&format!("\"{path}\"")), "{path}");
    }
    std::fs::remove_dir_all(root).ok();
}

/// The newest records matter most: a log longer than its budget is read
/// from its tail, never its head (the macOS Pier once read the head), and
/// the cut first line is dropped rather than exported half.
#[test]
fn a_long_log_is_exported_from_its_tail() {
    let root = scratch("tail");
    let mut log = Vec::new();
    for sequence in 1..=50 {
        log.extend(canonical_line(sequence));
    }
    let path = root.join("arcen-pier.log");
    std::fs::write(&path, &log).expect("log");
    let line_len = canonical_line(1).len() as u64;
    let limits = BundleLimits {
        max_log_file_input_bytes: Some(line_len * 3 + line_len / 2),
        ..BundleLimits::PIER
    };
    let archive_path = root.join("bundle.zip");
    let mut bundle = collector(&archive_path, limits);
    collect_canonical_logs(
        &mut bundle,
        &[LogCandidate {
            path: path.clone(),
            archive_path: "logs/arcen-pier.log".to_owned(),
            size_bytes: log.len() as u64,
        }],
    )
    .expect("logs");
    let (file, manifest, _) = bundle.finish().expect("archive");
    drop(file);
    let mut archive = zip::ZipArchive::new(File::open(&archive_path).expect("open")).expect("ZIP");
    let export = read_entry(&mut archive, "logs/arcen-pier.log");
    let sequences: Vec<u64> = export
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).expect("JSON")["sequence"]
                .as_u64()
                .expect("sequence")
        })
        .collect();
    assert!(!sequences.is_empty());
    assert_eq!(sequences.last(), Some(&50), "the newest record is kept");
    assert_eq!(sequences, [48, 49, 50]);
    let entry = &manifest.entries[0];
    assert!(entry.truncation.is_some());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_deck_reads_at_most_its_per_file_tail() {
    let root = scratch("deck-tail");
    let line = canonical_line(7);
    let path = root.join("arcen-client.log.2026-11-15");
    std::fs::write(&path, line.repeat(4)).expect("log");
    let limits = BundleLimits {
        max_log_file_input_bytes: Some(line.len() as u64),
        ..BundleLimits::DECK
    };
    let archive_path = root.join("bundle.zip");
    let mut bundle = collector(&archive_path, limits);
    collect_canonical_logs(
        &mut bundle,
        &[LogCandidate {
            path,
            archive_path: "logs/arcen-client.log.2026-11-15".to_owned(),
            size_bytes: line.len() as u64 * 4,
        }],
    )
    .expect("logs");
    let (_, manifest, _) = bundle.finish().expect("archive");
    let entry = &manifest.entries[0];
    assert_eq!(entry.original_size_bytes, line.len() as u64 * 4);
    assert_eq!(
        entry
            .truncation
            .as_ref()
            .map(|truncation| truncation.original_offset),
        Some(line.len() as u64 * 3)
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_linked_or_oversized_source_is_refused_with_a_notice() {
    let root = scratch("unsafe");
    let target = root.join("pier.json");
    std::fs::write(&target, b"{\"a\":1}").expect("target");
    let link = root.join("link.json");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    // A file symbolic link (a reparse point) needs an elevated or
    // developer-mode account on Windows; the lab's test account is one.
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &link).expect("symlink (elevated account)");
    let large = root.join("large.json");
    std::fs::write(&large, vec![b' '; 2 * 1024 * 1024]).expect("large");
    let bad = root.join("bad.json");
    std::fs::write(&bad, b"not json").expect("bad");
    let archive_path = root.join("bundle.zip");
    let mut bundle = collector(&archive_path, BundleLimits::PIER);
    assert!(
        read_json_document(&mut bundle, &link, "config/link.json")
            .expect("notice")
            .is_none()
    );
    assert!(
        read_json_document(&mut bundle, &large, "config/large.json")
            .expect("notice")
            .is_none()
    );
    assert!(
        read_json_document(&mut bundle, &bad, "config/bad.json")
            .expect("notice")
            .is_none()
    );
    assert!(
        read_json_document(
            &mut bundle,
            &root.join("missing.json"),
            "config/missing.json"
        )
        .expect("notice")
        .is_none()
    );
    assert_eq!(
        read_json_document(&mut bundle, &target, "config/pier.json").expect("document"),
        Some(serde_json::json!({"a": 1}))
    );
    let (_, manifest, omissions) = bundle.finish().expect("archive");
    let codes: Vec<(String, NoticeKind, NoticeCode)> = manifest
        .notices
        .iter()
        .map(|notice| (notice.source.as_str().to_owned(), notice.kind, notice.code))
        .collect();
    assert!(codes.contains(&(
        "config/link.json".to_owned(),
        NoticeKind::Invalid,
        NoticeCode::UnsafeFileType
    )));
    assert!(codes.contains(&(
        "config/large.json".to_owned(),
        NoticeKind::Invalid,
        NoticeCode::SourceTooLarge
    )));
    assert!(codes.contains(&(
        "config/bad.json".to_owned(),
        NoticeKind::Invalid,
        NoticeCode::SourceInvalid
    )));
    assert!(codes.contains(&(
        "config/missing.json".to_owned(),
        NoticeKind::Unavailable,
        NoticeCode::SourceNotFound
    )));
    assert_eq!(omissions, 4);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_archive_is_private_published_whole_and_never_overwrites() {
    let root = scratch("publish");
    let name = |suffix: &str| format!("arcen-support-1700000000-42{suffix}.zip");
    std::fs::write(root.join(name("")), b"existing").expect("existing");
    let (path, value) = write_archive(&root, name, |file| Ok((file, 7))).expect("published");
    assert_eq!(value, 7);
    assert_eq!(
        path.file_name().and_then(std::ffi::OsStr::to_str),
        Some("arcen-support-1700000000-42-1.zip")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // A failed build leaves nothing behind: no archive, no partial file.
    let failed = write_archive(
        &root,
        |suffix| format!("failed{suffix}.zip"),
        |_| Err::<(File, ()), _>(SupportBundleError::Archive("boom".to_owned())),
    );
    assert!(failed.is_err());
    let names: Vec<String> = std::fs::read_dir(&root)
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        names
            .iter()
            .all(|name| !name.starts_with("failed") && !name.contains(".partial")),
        "{names:?}"
    );
    std::fs::remove_dir_all(root).ok();
}

/// A stand-in cipher: the module under test owns the files, not the maths.
struct XorSealer(u8);

impl BundleSealer for XorSealer {
    fn extension(&self) -> &'static str {
        "xor"
    }

    fn seal(
        &self,
        archive: &mut dyn Read,
        sealed: &mut dyn std::io::Write,
    ) -> Result<(), SupportBundleError> {
        let mut bytes = Vec::new();
        archive.read_to_end(&mut bytes)?;
        for byte in &mut bytes {
            *byte ^= self.0;
        }
        sealed.write_all(&bytes)?;
        Ok(())
    }
}

struct FailingSealer;

impl BundleSealer for FailingSealer {
    fn extension(&self) -> &'static str {
        "age"
    }

    fn seal(
        &self,
        _archive: &mut dyn Read,
        sealed: &mut dyn std::io::Write,
    ) -> Result<(), SupportBundleError> {
        sealed.write_all(b"half")?;
        Err(SupportBundleError::Archive("no recipient".to_owned()))
    }
}

struct DottedSealer;

impl BundleSealer for DottedSealer {
    fn extension(&self) -> &'static str {
        "zip/../x"
    }

    fn seal(&self, _: &mut dyn Read, _: &mut dyn std::io::Write) -> Result<(), SupportBundleError> {
        Ok(())
    }
}

fn names_in(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn a_sealed_bundle_publishes_only_ciphertext_and_its_plaintext_fingerprint() {
    let root = scratch("sealed");
    let name = |suffix: &str| format!("arcen-deck-support-20261115T220000Z{suffix}.zip");
    let sealed = write_sealed_archive(&root, name, &XorSealer(0x5a), |file| {
        let mut bundle = Collector::new(
            file,
            component(),
            42,
            BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x55; 32])),
            BundleLimits::DECK,
        );
        bundle
            .add_bytes("diagnostics/fixture.json", BundleSource::Diagnostics, b"{}")
            .expect("entry");
        let (file, manifest, _) = bundle.finish()?;
        Ok((file, manifest))
    })
    .expect("sealed");
    assert_eq!(
        names_in(&root),
        ["arcen-deck-support-20261115T220000Z.zip.xor"],
        "only the sealed file is left"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&sealed.path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let mut plain = std::fs::read(&sealed.path).expect("sealed bytes");
    assert_ne!(&plain[..2], b"PK", "the published file is not a ZIP");
    for byte in &mut plain {
        *byte ^= 0x5a;
    }
    assert_eq!(
        fingerprint(plain.as_slice()).expect("fingerprint"),
        sealed.archive
    );
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(plain)).expect("ZIP");
    assert!(archive.by_name("manifest.json").is_ok());
    assert_eq!(sealed.value.entries.len(), 1);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_failed_seal_or_a_bad_extension_leaves_nothing_behind() {
    let root = scratch("sealed-failed");
    let name = |suffix: &str| format!("bundle{suffix}.zip");
    let failed = write_sealed_archive(&root, name, &FailingSealer, |mut file| {
        std::io::Write::write_all(&mut file, b"PK plaintext")?;
        Ok((file, ()))
    });
    assert!(failed.is_err());
    assert!(names_in(&root).is_empty(), "{:?}", names_in(&root));

    // A panicking build leaves nothing either.
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        write_sealed_archive(&root, name, &XorSealer(1), |_| -> Result<(File, ()), _> {
            panic!("build failed")
        })
    }));
    assert!(panicked.is_err());
    assert!(names_in(&root).is_empty(), "{:?}", names_in(&root));
    // While it is written, the plaintext archive has no name (Windows keeps
    // one until the delete-on-close handle closes).
    #[cfg(unix)]
    {
        let during = write_sealed_archive(&root, name, &XorSealer(1), |file| {
            Ok((file, names_in(&root)))
        })
        .expect("sealed");
        assert!(
            during.value.iter().all(|name| !name.contains("plain")),
            "{:?}",
            during.value
        );
        std::fs::remove_file(during.path).expect("remove");
    }
    let refused = write_sealed_archive(&root, name, &DottedSealer, |file| Ok((file, ())));
    assert!(matches!(refused, Err(SupportBundleError::Output(_))));
    assert!(names_in(&root).is_empty(), "{:?}", names_in(&root));
    std::fs::remove_dir_all(root).ok();
}

/// A written bundle with identities planted where a product might put them:
/// in archive names, a redacted key path, and a notice source.
fn planted_bundle(path: &Path) -> arcen_telemetry::SupportBundleManifest {
    let mut bundle = Collector::new(
        File::create(path).expect("archive"),
        BundleComponent {
            name: "arcen-deck-windows".to_owned(),
            version: "0.19.1".to_owned(),
            os: "windows".to_owned(),
            arch: "x86_64".to_owned(),
        },
        1_794_726_245,
        BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x55; 32])),
        BundleLimits::DECK,
    );
    bundle
        .add_bytes(
            "logs/arcen-client.log.alice-laptop",
            BundleSource::Log,
            b"{\"user\":\"alice\"}\n",
        )
        .expect("log");
    bundle
        .add_document(
            "config/connections.json",
            BundleSource::Configuration,
            None,
            b"{}",
            vec![RedactionRecord {
                entry_path: BundlePath::new("config/connections.json").expect("path"),
                key_path: "/build-server.corp.example/password".to_owned(),
                reason: RedactionReason::SensitiveKey,
            }],
        )
        .expect("config");
    bundle
        .notice(
            "crash/alice-laptop.dmp",
            NoticeKind::Omitted,
            NoticeCode::SensitiveRuntimeExcluded,
        )
        .expect("notice");
    bundle
        .notice("logs", NoticeKind::Truncated, NoticeCode::LogPayloadLimit)
        .expect("notice");
    bundle.finish().expect("finished").1
}

#[test]
fn the_public_summary_says_what_the_manifest_holds_and_nothing_identifying() {
    let root = scratch("summary");
    let path = root.join("bundle.zip");
    let manifest = planted_bundle(&path);
    let archive = arcen_observability::support_bundle::fingerprint_file(&path).expect("hash");
    // Planted where a product's details could carry them: a GPU label with
    // a path, an OS label with a host, a code with an address, a non-hex
    // artefact hash (a private revision), a fact word with an account.
    let details = SummaryDetails {
        artifact_sha256: Some("1a45c17b".repeat(8)),
        os: Some("Windows 11 26100 x64".to_owned()),
        gpus: vec![
            "NVIDIA GRID RTX6000-8Q".to_owned(),
            "C:/Users/alice/gpu".to_owned(),
        ],
        decoder: Some("D3d11Video on build-server.corp.example".to_owned()),
        decode_profiles: vec![("h265", true), ("av1", false)],
        preset: Some("Grading"),
        codes: vec![
            "diag.udp.handshake_failed".to_owned(),
            "LANE_REFUSED(2000)".to_owned(),
            "peer 192.0.2.10".to_owned(),
        ],
        facts: vec![
            SummaryFact::count("average_fps", 59),
            SummaryFact::token("worst_health", "degraded"),
            SummaryFact::flag("hdr", false),
            SummaryFact::token("host", "alice@build-server.corp.example"),
        ],
    };
    let summary = PublicSummary::new(&manifest, Some(archive), &details);
    let markdown = summary
        .to_public_text(&["ALICE-PC"])
        .unwrap_or_else(|refusal| panic!("{refusal}:\n{}", summary.to_markdown()));
    assert!(
        !markdown.contains("2026-11-15"),
        "no timestamp:\n{markdown}"
    );
    for planted in [
        "alice",
        "laptop",
        "build-server",
        "corp.example",
        "192.0.2",
        "password",
        "dmp",
    ] {
        assert!(!markdown.contains(planted), "{planted} leaked:\n{markdown}");
    }
    for expected in [
        "| Product | arcen-deck-windows 0.19.1 (windows, x86_64) |",
        &format!(
            "| Bundle | {} (not attached) |",
            manifest.bundle_id.clone().expect("id")
        ),
        "| Artifact SHA-256 | `1a45c17b1a45c17b…` |",
        "| OS | Windows 11 26100 x64 |",
        "| GPU | NVIDIA GRID RTX6000-8Q |",
        "| GPU | <withheld> |",
        "| Decoder | <withheld> |",
        "| Decode | h265 yes, av1 no |",
        "| Preset | Grading |",
        "| Codes | diag.udp.handshake_failed, LANE_REFUSED(2000), <withheld> |",
        &format!(
            "| Bundle SHA-256 | `{}` ({} bytes) |",
            archive.sha256, archive.size_bytes
        ),
        "| Omissions | 1 |",
        "| average_fps | 59 |",
        "| worst_health | degraded |",
        "| hdr | no |",
        "| host | <withheld> |",
        "| config | 1 | 2 | 0 |",
        "| logs | 1 | 17 | 0 |",
        "| crash | omitted | sensitive_runtime_excluded | 1 |",
        "| logs | truncated | log_payload_limit | 1 |",
        "| sensitive_key | 1 |",
    ] {
        assert!(
            markdown.contains(expected),
            "missing {expected}:\n{markdown}"
        );
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_component_string_that_is_not_a_plain_word_is_withheld() {
    let manifest = Collector::new(
        std::io::Cursor::new(Vec::new()),
        BundleComponent {
            name: "arcen-pier-linux".to_owned(),
            version: "0.19.1 built by alice on /home/alice".to_owned(),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
        },
        0,
        BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x55; 32])),
        BundleLimits::PIER,
    )
    .finish()
    .expect("empty bundle")
    .1;
    let markdown = PublicSummary::new(&manifest, None, &SummaryDetails::default()).to_markdown();
    assert!(markdown.contains("| Product | arcen-pier-linux <withheld> (linux, x86_64) |"));
    assert!(!markdown.contains("alice"));
    assert!(!markdown.contains("SHA-256"));
}

#[test]
fn the_preflight_refuses_text_that_still_names_a_machine_or_a_person() {
    use arcen_observability::support_bundle::summary::preflight;
    assert_eq!(
        preflight("| Codes | diag.udp.handshake_failed |", &[]),
        Ok(())
    );
    assert_eq!(
        preflight("| Product | arcen-pier-linux 0.19.1 |", &[]),
        Ok(())
    );
    for (text, refusal) in [
        ("peer 203.0.113.82:18444", PreflightRefusal::Address),
        ("peer [fd00::1]:18444", PreflightRefusal::Address),
        ("fe80::1 on en0", PreflightRefusal::Address),
        (
            "render07.client.example.internal",
            PreflightRefusal::HostName,
        ),
        (
            "log in /Users/someone/Library",
            PreflightRefusal::HomeFolder,
        ),
        ("C:\\Users\\someone", PreflightRefusal::HomeFolder),
        ("d:/users/someone/x", PreflightRefusal::HomeFolder),
        (
            "C:\\Documents and Settings\\someone",
            PreflightRefusal::HomeFolder,
        ),
        ("made on SOMEONE-PC", PreflightRefusal::Forbidden),
    ] {
        assert_eq!(preflight(text, &["someone-pc"]), Err(refusal), "{text}");
    }
}
