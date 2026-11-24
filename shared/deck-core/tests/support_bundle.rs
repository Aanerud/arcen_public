// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck support bundle end to end: a fixture Deck (logs, settings,
//! saved connections, pins, crash reports, facts) is bundled with a fixed
//! time and pseudonym key, and the archive is checked for leaks, for
//! hashes that match, and against a golden manifest.
//!
//! Re-bless the golden after an intended change with
//! `ARCEN_BLESS_SUPPORT_GOLDEN=1 cargo test -p arcen-deck-core --features support-bundle --test support_bundle`.

#![cfg(feature = "support-bundle")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value
)]

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use arcen_deck_core::decode::DecodeCapabilities;
use arcen_deck_core::decode_report::{DecodeRefusal, DecodeReport};
use arcen_deck_core::directories::DeckDirectories;
use arcen_deck_core::settings::LogLevel;
use arcen_deck_core::support_bundle::redact::LocalIdentity;
use arcen_deck_core::support_bundle::snapshot::SessionSnapshot;
use arcen_deck_core::support_bundle::{
    CrashReportKind, CrashReportSource, SupportBundleError, SupportBundleRequest, SupportDetails,
    SupportFacts, bundle_target, create, create_sealed, write_bundle,
};
use arcen_observability::support_bundle::{BundleSealer, fingerprint};
use arcen_protocol::messages::{BuildIdentityMsg, ClientMonitor};
use arcen_telemetry::{
    BundleComponent, BundlePseudonymKey, NoticeCode, NoticeKind, SupportBundleManifest,
};
use sha2::{Digest, Sha256};

const GOLDEN: &str = "tests/data/support-bundle/manifest.golden.json";
const EDID_HEX: &str = "00ffffffffffff0010ac5ad04c4a3130";
const SERIAL: u32 = 0x3031_4a4c;

fn canonical_line(fields: serde_json::Value, host: Option<&str>) -> String {
    serde_json::json!({
        "schema_version": 1,
        "timestamp": "2026-11-15T07:00:00.000000Z",
        "sequence": 1,
        "profile_level": 2,
        "profile_name": "info",
        "severity": "info",
        "role": "client",
        "component": "deck",
        "platform": "macos",
        "target": "arcen::session",
        "sid": null,
        "user": "alice",
        "host": host,
        "peer_addr": host.map(|host| format!("{host}:18444")),
        "health_state": null,
        "message": "Connecting to host",
        "fields": fields,
    })
    .to_string()
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("deck-support-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("alice-home");
        for dir in ["config", "logs", "crash"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        let home_text = home.display().to_string();
        let logs = home.join("logs");
        std::fs::write(
            logs.join("arcen-client.log.2026-11-14"),
            format!(
                "{}\nnot json\n",
                canonical_line(
                    serde_json::json!({
                        "endpoint": "203.0.113.82:18444",
                        "ssid": "Home Wi-Fi",
                        "bundle_path": format!("{home_text}/Applications/Arcen Deck.app"),
                    }),
                    Some("203.0.113.82"),
                )
            ),
        )
        .unwrap();
        std::fs::write(
            logs.join("arcen-client.log.2026-11-15"),
            format!(
                "{}\n",
                canonical_line(serde_json::json!({"frames": 42}), None)
            ),
        )
        .unwrap();
        std::fs::write(logs.join("unrelated.log"), "alice hunter2\n").unwrap();
        let config = home.join("config");
        std::fs::write(
            config.join("settings.json"),
            r#"{"log_level":2,"option_key_mode":"automatic","clipboard_enabled":true}"#,
        )
        .unwrap();
        std::fs::write(
            config.join("connections.json"),
            r#"{"version":2,"last_selected_connection_id":"a","connections":[
                {"id":"a","name":"Studio","host":"203.0.113.82","port":18444,
                 "settings":{"remembered_username":"alice.smith","keyboard_shortcuts":"automatic"},
                 "password":"hunter2"}]}"#,
        )
        .unwrap();
        std::fs::write(
            config.join("trusted_pins.json"),
            r#"{"version":1,"pins":{"203.0.113.82:18444":{"kind":"spki-sha256","digest":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","label":"Studio"}}}"#,
        )
        .unwrap();
        let crash = home.join("crash");
        std::fs::write(
            crash.join("arcen-deck-2026-11-15-070000.ips"),
            format!(
                "{}\n{}",
                serde_json::json!({"app_name": "arcen-deck", "incident_id": "X"}),
                serde_json::json!({
                    "procPath": format!("{home_text}/Applications/Arcen Deck.app/Contents/MacOS/arcen-deck"),
                    "crashReporterKey": "K-123",
                    "userName": "alice",
                })
            ),
        )
        .unwrap();
        // A MINIDUMP_HEADER with stack-only flags (MiniDumpNormal).
        let mut dump = b"MDMP\x93\xa7\0\0".to_vec();
        dump.resize(32, 0);
        dump.extend(
            "C:\\Users\\alice\\AppData"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        std::fs::write(crash.join("arcen-deck.exe.4242.dmp"), &dump).unwrap();
        let ips = std::fs::File::options()
            .write(true)
            .open(crash.join("arcen-deck-2026-11-15-070000.ips"))
            .unwrap();
        ips.set_modified(UNIX_EPOCH + Duration::from_secs(1_794_700_000))
            .unwrap();
        let minidump = std::fs::File::options()
            .write(true)
            .open(crash.join("arcen-deck.exe.4242.dmp"))
            .unwrap();
        minidump
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_794_600_000))
            .unwrap();
        Self { root, home }
    }

    fn request(&self) -> SupportBundleRequest {
        let crash = self.home.join("crash");
        SupportBundleRequest {
            output_dir: self.root.join("out"),
            component: BundleComponent {
                name: "arcen-deck-test".to_owned(),
                version: "0.0.0".to_owned(),
                os: "test".to_owned(),
                arch: "test".to_owned(),
            },
            directories: DeckDirectories {
                config: Some(self.home.join("config")),
                logs: Some(self.home.join("logs")),
            },
            log_level: LogLevel::Info,
            facts: SupportFacts {
                build: Some(BuildIdentityMsg::development("arcen-deck-test", "0.0.0")),
                platform_summary: "Test OS 1 test".to_owned(),
                displays: vec![ClientMonitor {
                    id: 1,
                    width_px: 3840,
                    height_px: 2160,
                    scale: 2.0,
                    refresh_hz: 60,
                    is_primary: true,
                    name: "Studio Display".to_owned(),
                    serial: SERIAL,
                    edid: EDID_HEX.to_owned(),
                    ..ClientMonitor::default()
                }],
                decode: Some(DecodeReport {
                    decoder: "NVIDIA GRID V100D-16Q".to_owned(),
                    capabilities: DecodeCapabilities {
                        h264: true,
                        h265: true,
                        main10: true,
                        ..DecodeCapabilities::default()
                    },
                    refusals: vec![DecodeRefusal {
                        profile: "hevc-main10-444".to_owned(),
                        reason: "hevc-main10-444 is not offered by NVIDIA GRID V100D-16Q"
                            .to_owned(),
                    }],
                }),
                network: None,
                outgoing: None,
                usb_component: Some(arcen_usb_bridge::NativeUsbComponentState::NotInstalled),
                details: SupportDetails {
                    platform: serde_json::json!({
                        "adapters": [{"name": "NVIDIA GRID V100D-16Q", "driver": "32.0.15.6094"}],
                        "user_profile": self.home.display().to_string(),
                        "api_key": "should-not-appear",
                    }),
                    crash_reports: vec![
                        CrashReportSource {
                            path: crash.join("arcen-deck.exe.4242.dmp"),
                            kind: CrashReportKind::WindowsMinidump,
                        },
                        CrashReportSource {
                            path: crash.join("arcen-deck-2026-11-15-070000.ips"),
                            kind: CrashReportKind::MacDiagnosticReport,
                        },
                    ],
                },
                session: Some(SessionSnapshot {
                    captured_unix_seconds: 1_794_726_000,
                    stream_statistics: vec!["59.8/60 fps · HEVC 4:2:0 8-bit · Auto".to_owned()],
                    negotiated_truth: vec!["Auto: served as requested".to_owned()],
                    health: Vec::new(),
                    decoder_refusal: None,
                    telemetry: vec!["frames_decoded=42".to_owned()],
                }),
            },
            identity: LocalIdentity {
                home: Some(self.home.display().to_string()),
                user: Some("alice".to_owned()),
                computer: Some("ALICE-PC".to_owned()),
            },
            generated_at: UNIX_EPOCH + Duration::from_secs(1_794_726_245),
            pseudonym_key: BundlePseudonymKey::from_bytes([42; 32]),
            // The fixture asks for them, so their scrubbing is checked too.
            include_crash_reports: true,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn entries(archive: Vec<u8>) -> Vec<(String, Vec<u8>)> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).unwrap();
    (0..zip.len())
        .map(|index| {
            let mut file = zip.by_index(index).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            (file.name().unwrap().into_owned(), bytes)
        })
        .collect()
}

#[test]
fn the_bundle_holds_no_secret_and_every_entry_matches_its_hash() {
    let fixture = Fixture::new("leaks");
    let (cursor, manifest) = write_bundle(Cursor::new(Vec::new()), fixture.request()).unwrap();
    let files = entries(cursor.into_inner());
    let home = fixture.home.display().to_string();
    for (name, bytes) in &files {
        let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
        let wide: String = String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
        .to_ascii_lowercase();
        for secret in [
            "alice",
            "hunter2",
            "home wi-fi",
            "k-123",
            "should-not-appear",
            "alice-pc",
            "tok-777",
            EDID_HEX,
            &home.to_ascii_lowercase(),
        ] {
            assert!(!text.contains(secret), "{secret} leaked in {name}");
            assert!(!wide.contains(secret), "{secret} leaked (UTF-16) in {name}");
        }
    }
    let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"logs/arcen-client.log.2026-11-14"));
    assert!(!names.iter().any(|name| name.contains("unrelated")));
    assert_eq!(names.last(), Some(&"manifest.json"));
    // Pier addresses leave every entry; the manifest lists where.
    assert!(
        files
            .iter()
            .all(|(_, bytes)| !String::from_utf8_lossy(bytes).contains("203.0.113"))
    );
    for entry in &manifest.entries {
        let (_, bytes) = files
            .iter()
            .find(|(name, _)| name == entry.path.as_str())
            .unwrap();
        assert_eq!(
            entry.sha256.to_string(),
            format!("{:x}", Sha256::digest(bytes)),
            "{}",
            entry.path
        );
        assert_eq!(entry.included_size_bytes, bytes.len() as u64);
    }
    assert_eq!(manifest.entries.len() + 1, files.len());
}

/// A stand-in cipher; the shared collector's own tests cover the files.
struct XorSealer;

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
        sealed.write_all(&bytes.iter().map(|byte| byte ^ 0xa5).collect::<Vec<_>>())?;
        Ok(())
    }
}

#[test]
fn the_public_summary_and_the_sealed_bundle_carry_no_identity() {
    let fixture = Fixture::new("public");
    let request = || {
        let mut request = fixture.request();
        // A private build revision and an artefact hash: only the latter
        // may be stated.
        request.facts.build = Some(BuildIdentityMsg {
            build_id: "ci-4242-private".to_owned(),
            source_revision: "deadbeefcafe".to_owned(),
            artifact_sha256: Some("1a45c17b".repeat(8)),
            ..BuildIdentityMsg::development("arcen-deck-test", "0.0.0")
        });
        request.facts.session.as_mut().unwrap().telemetry.extend(
            [
                "average_fps=59",
                "worst_health=Degraded",
                "frames_dropped=alice",
            ]
            .map(str::to_owned),
        );
        request
    };
    let sealed = create_sealed(request(), &XorSealer).unwrap();
    let published: Vec<_> = std::fs::read_dir(fixture.root.join("out"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(published, ["arcen-deck-support-20261115T070405Z.zip.xor"]);
    let plain: Vec<u8> = std::fs::read(&sealed.path)
        .unwrap()
        .iter()
        .map(|byte| byte ^ 0xa5)
        .collect();
    let markdown = sealed
        .summary
        .to_public_text(&["alice", "ALICE-PC"])
        .expect("passes the preflight");
    let archive = fingerprint(plain.as_slice()).unwrap();
    assert!(markdown.contains(&archive.sha256.to_string()));
    let home = fixture.home.display().to_string().to_ascii_lowercase();
    let lower = markdown.to_ascii_lowercase();
    for secret in [
        "alice",
        "hunter2",
        "home wi-fi",
        "203.0.113",
        "studio",
        "arcen-client.log",
        "ci-4242",
        "deadbeef",
        "2026-11-15",
        home.as_str(),
    ] {
        assert!(!lower.contains(secret), "{secret} leaked:\n{markdown}");
    }
    for expected in [
        &format!(
            "| Bundle | {} (not attached) |",
            sealed.manifest.bundle_id.clone().unwrap()
        ),
        "| Artifact SHA-256 | `1a45c17b1a45c17b…` |",
        "| OS | Test OS 1 test |",
        "| GPU | NVIDIA GRID V100D-16Q |",
        "| Decode | h264 yes, h265 yes, av1 no, main10 yes, yuv444 no |",
        "| Preset | Auto |",
        "| crash_reports_included | yes |",
        "| displays | 1 |",
        "| crash_reports_found | 2 |",
        "| frames_decoded | 42 |",
        "| average_fps | 59 |",
        "| worst_health | degraded |",
        "| decoder_refused_stream | no |",
        "| runtime | omitted | sensitive_runtime_excluded | 3 |",
    ] {
        assert!(
            markdown.contains(expected),
            "missing {expected}:\n{markdown}"
        );
    }
    // A value that is not a number is dropped, not printed.
    assert!(!markdown.contains("frames_dropped"));
    // The unsealed bundle's summary is the same, apart from its archive.
    let outcome = create(request()).unwrap();
    let unsealed = outcome.public_summary().unwrap().to_markdown();
    let strip = |text: &str| {
        text.lines()
            .filter(|line| !line.starts_with("| Bundle SHA-256"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&unsealed), strip(&markdown));
}

#[test]
fn crash_reports_stay_out_unless_asked_for_and_the_manifest_says_which() {
    let fixture = Fixture::new("crash-choice");
    let mut request = fixture.request();
    request.include_crash_reports = false;
    let (cursor, manifest) = write_bundle(Cursor::new(Vec::new()), request).unwrap();
    let files = entries(cursor.into_inner());
    assert!(!files.iter().any(|(name, _)| name.starts_with("crash/")));
    let crash_notices: Vec<_> = manifest
        .notices
        .iter()
        .filter(|notice| notice.source.as_str() == "crash")
        .map(|notice| (notice.kind, notice.code))
        .collect();
    assert_eq!(
        crash_notices,
        [(NoticeKind::Omitted, NoticeCode::CrashReportsNotRequested)]
    );
    let (_, asked) = write_bundle(Cursor::new(Vec::new()), fixture.request()).unwrap();
    assert!(
        asked
            .notices
            .iter()
            .any(|notice| notice.source.as_str() == "crash"
                && notice.code == NoticeCode::CrashReportsRequested)
    );
    assert!(
        asked
            .entries
            .iter()
            .any(|entry| entry.path.as_str().starts_with("crash/"))
    );
}

#[test]
fn every_pier_address_reads_as_one_pseudonym_with_its_scope_and_port() {
    let fixture = Fixture::new("addresses");
    let mut request = fixture.request();
    request.facts.network = Some(arcen_deck_core::support_bundle::NetworkFacts {
        target: "203.0.113.82:18444".to_owned(),
        probe: arcen_deck_core::network::NetworkProbeResult {
            snapshot: None,
            permission: arcen_deck_core::network::NetworkIdentityPermission::NotRequested,
        },
    });
    let (cursor, _) = write_bundle(Cursor::new(Vec::new()), request).unwrap();
    let files = entries(cursor.into_inner());
    let pseudonym =
        arcen_telemetry::BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([42; 32]))
            .pseudonymize(arcen_telemetry::BundleIdentityKind::Host, "203.0.113.82");
    for (name, bytes) in &files {
        let text = String::from_utf8_lossy(bytes);
        assert!(
            !text.contains("203.0.113"),
            "a Pier address stayed in {name}"
        );
    }
    for (name, expected) in [
        (
            "config/connections.json",
            format!("{pseudonym}[ipv4-public]"),
        ),
        (
            "config/trusted_pins.json",
            format!("{pseudonym}[ipv4-public]:18444"),
        ),
        (
            "diagnostics/network.json",
            format!("{pseudonym}[ipv4-public]:18444"),
        ),
        (
            "logs/arcen-client.log.2026-11-14",
            format!("{pseudonym}[ipv4-public]:18444"),
        ),
    ] {
        let (_, bytes) = files.iter().find(|(file, _)| file == name).unwrap();
        assert!(
            String::from_utf8_lossy(bytes).contains(&expected),
            "{name} lacks the shared pseudonym"
        );
    }
}

#[test]
fn the_network_target_is_the_last_session_else_the_selected_connection() {
    let fixture = Fixture::new("target");
    let config = fixture.home.join("config");
    assert_eq!(
        bundle_target(None, Some(&config)),
        Some(("203.0.113.82".to_owned(), 18444))
    );
    // A `--connect` session is not saved; the bundle follows it.
    assert_eq!(
        bundle_target(Some(("203.0.113.95".to_owned(), 18445)), Some(&config)),
        Some(("203.0.113.95".to_owned(), 18445))
    );
    assert_eq!(bundle_target(None, None), None);
}

#[test]
fn the_manifest_matches_the_golden_one() {
    let fixture = Fixture::new("golden");
    let (_, manifest) = write_bundle(Cursor::new(Vec::new()), fixture.request()).unwrap();
    let rendered = normalised(&manifest);
    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    if std::env::var_os("ARCEN_BLESS_SUPPORT_GOLDEN").is_some() {
        std::fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
        std::fs::write(&golden_path, &rendered).unwrap();
    }
    let golden = std::fs::read_to_string(&golden_path)
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(rendered, golden, "manifest differs from {GOLDEN}");
}

/// The manifest with the one host-dependent entry (`deck/about.json`
/// names this machine's folders) reduced to its presence.
fn normalised(manifest: &SupportBundleManifest) -> String {
    let mut value = serde_json::to_value(manifest).unwrap();
    for entry in value["entries"].as_array_mut().unwrap() {
        if entry["path"] == "deck/about.json" {
            entry["sha256"] = "<host-dependent>".into();
            entry["included_size_bytes"] = 0.into();
            entry["original_size_bytes"] = 0.into();
        }
        if entry["path"].as_str().is_some_and(|path| {
            path.starts_with("config/") || path.starts_with("logs/") || path.starts_with("crash/")
        }) {
            // Source sizes include this machine's fixture path.
            entry["original_size_bytes"] = 0.into();
        }
    }
    let mut text = serde_json::to_string_pretty(&value).unwrap();
    text.push('\n');
    text
}
