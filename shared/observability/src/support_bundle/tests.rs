// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use std::io::{Cursor, Read};

use arcen_telemetry::{BundlePseudonymKey, SUPPORT_BUNDLE_SCHEMA_VERSION};

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
