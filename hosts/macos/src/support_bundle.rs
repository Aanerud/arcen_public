// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-pier-macos support-bundle`: the macOS Pier's sources for the
//! shared support-bundle collector (`arcen_observability::support_bundle`),
//! which owns the archive, its limits (the same as every Pier's), the
//! manifest and redaction.

use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_observability::support_bundle::{
    self as bundle, BundleLimits, Collector, LogCandidate, OutputOption,
};
use arcen_telemetry::{
    BundleComponent, BundlePath, BundlePseudonymKey, BundlePseudonymizer, BundleSource, NoticeCode,
    NoticeKind, SupportBundleManifest,
};

const DEFAULT_OUTPUT_DIRECTORY: &str = "/Library/Logs/Arcen/Pier/support";
const DEFAULT_LOG_DIRECTORY: &str = "/Library/Logs/Arcen/Pier";
const LOG_FILE_NAME: &str = "arcen-pier-macos.jsonl";
const LOG_ENTRY_PATH: &str = "logs/arcen-pier-macos.jsonl";
const USAGE: &str =
    "USAGE:\n  arcen-pier-macos support-bundle [--out <DIR>] [--include-crash-dumps]";

pub use bundle::WriteOptions as SupportBundleOptions;

/// Parses support-bundle-specific arguments.
///
/// # Errors
///
/// Returns an error for unknown arguments, duplicate `--out`, a missing
/// output directory, or `--help` (the usage).
pub fn parse_options(arguments: &[String]) -> Result<SupportBundleOptions, String> {
    match bundle::parse_output_option(arguments)? {
        OutputOption::Help => Err(USAGE.to_owned()),
        OutputOption::Write(options) => Ok(options),
    }
}

/// Creates a redacted support bundle in the selected output directory.
///
/// # Errors
///
/// Returns an error when the output directory, archive, native diagnostics,
/// or shared support-bundle contract cannot be written.
pub fn run(
    options: &SupportBundleOptions,
    config: &crate::PierFileConfig,
    startup: &crate::StartupConfig,
) -> Result<PathBuf, String> {
    let directory = options
        .output_directory
        .as_deref()
        .unwrap_or_else(|| Path::new(DEFAULT_OUTPUT_DIRECTORY));
    std::fs::create_dir_all(directory).map_err(|error| {
        format!(
            "create support-bundle directory {}: {error}",
            directory.display()
        )
    })?;
    // Only the folder this Pier owns is made private; a folder the operator
    // chose with --out keeps its permissions, as on the Linux Pier.
    if options.output_directory.is_none() {
        set_private_directory_permissions(directory)?;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock precedes Unix epoch".to_owned())?
        .as_secs();
    let mut key = BundlePseudonymKey::zeroed();
    getrandom::getrandom(key.entropy_buffer())
        .map_err(|error| format!("generate support-bundle pseudonym key: {error}"))?;
    let sources = BundleSources::host(config, startup);
    let (path, ()) = bundle::write_archive(
        directory,
        |suffix| format!("arcen-pier-macos-support-{timestamp}{suffix}.zip"),
        |file| {
            let (file, _, _) = build(file, timestamp, key, &sources, options.include_crash_dumps)
                .map_err(bundle::SupportBundleError::Archive)?;
            Ok((file, ()))
        },
    )?;
    Ok(path)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("set support-bundle directory mode 0700: {error}"))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Where this Pier's bundle sources are. The host's own in production; a
/// golden test points the log at a fixture and leaves out the live probe.
struct BundleSources<'a> {
    config: &'a crate::PierFileConfig,
    startup: &'a crate::StartupConfig,
    log_directory: PathBuf,
    /// The live host probe (`DiagnosticsReport::collect`).
    host_probes: bool,
}

impl<'a> BundleSources<'a> {
    fn host(config: &'a crate::PierFileConfig, startup: &'a crate::StartupConfig) -> Self {
        Self {
            config,
            startup,
            log_directory: std::env::var_os("ARCEN_LOG_DIR")
                .map_or_else(|| PathBuf::from(DEFAULT_LOG_DIRECTORY), PathBuf::from),
            host_probes: true,
        }
    }
}

fn component() -> BundleComponent {
    BundleComponent {
        name: "arcen-pier-macos".to_owned(),
        version: crate::VERSION.to_owned(),
        os: "macos".to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    }
}

fn build<W: Write + Seek>(
    writer: W,
    timestamp: u64,
    key: BundlePseudonymKey,
    sources: &BundleSources<'_>,
    include_crash_dumps: bool,
) -> Result<(W, SupportBundleManifest, usize), String> {
    let mut collector = Collector::new(
        writer,
        component(),
        timestamp,
        BundlePseudonymizer::new(key),
        BundleLimits::PIER,
    );
    collector.notice(
        "config/tls/key",
        NoticeKind::Omitted,
        NoticeCode::PrivateKeyExcluded,
    )?;
    collector.notice(
        "config/tls/cert",
        NoticeKind::Omitted,
        NoticeCode::CertificateExcluded,
    )?;
    // No crash-report source on this Pier yet (GAP-040).
    collector.crash_report_choice(include_crash_dumps, false)?;
    let config = sources.config;
    let startup = sources.startup;
    add_json(
        &mut collector,
        "config/pier.json",
        BundleSource::Configuration,
        serde_json::json!({
            "logging": {
                "profile": startup.profile.as_str(),
                "source": format!("{:?}", startup.profile_source),
                "retention_days": config.logging.retention_days,
            },
            "platform": {
                "native_login_enabled": config.platform.native_login_enabled,
                "privileged_broker_enabled": config.platform.privileged_broker_enabled,
                "virtual_display_enabled": config.platform.virtual_display_enabled,
            }
        }),
    )?;
    if sources.host_probes {
        let report = crate::DiagnosticsReport::collect(config, startup)?;
        add_json(
            &mut collector,
            "diagnostics/report.json",
            BundleSource::Diagnostics,
            serde_json::to_value(report).map_err(|error| error.to_string())?,
        )?;
    }
    add_managed_log(&mut collector, &sources.log_directory)?;
    Ok(collector.finish()?)
}

fn add_json<W: Write + Seek>(
    collector: &mut Collector<W>,
    path: &str,
    source: BundleSource,
    mut value: serde_json::Value,
) -> Result<(), String> {
    let bundle_path = BundlePath::new(path).map_err(|error| error.to_string())?;
    let redactions = bundle::redact_pier_config(&bundle_path, &mut value)?;
    let bytes = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
    collector.add_document(path, source, None, &bytes, redactions)?;
    Ok(())
}

/// The managed log's newest records: the same tail, budget and pseudonyms
/// as every Pier's logs.
fn add_managed_log<W: Write + Seek>(
    collector: &mut Collector<W>,
    directory: &Path,
) -> Result<(), String> {
    let path = directory.join(LOG_FILE_NAME);
    let size_bytes = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            collector.file_notice(LOG_ENTRY_PATH, &error)?;
            return Ok(());
        }
    };
    bundle::collect_canonical_logs(
        collector,
        &[LogCandidate {
            path,
            archive_path: LOG_ENTRY_PATH.to_owned(),
            size_bytes,
        }],
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    #[test]
    fn parses_bounded_output_option() {
        let options = parse_options(&["--out".to_owned(), "/Users/Shared/arcen".to_owned()])
            .expect("valid support-bundle options");
        assert_eq!(
            options.output_directory,
            Some(PathBuf::from("/Users/Shared/arcen"))
        );
        assert!(
            parse_options(&[
                "--out".to_owned(),
                "/one".to_owned(),
                "--out".to_owned(),
                "/two".to_owned(),
            ])
            .is_err()
        );
    }

    const GOLDEN: &str = "tests/data/support-bundle/manifest.golden.json";

    fn fixture_directory() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/support-bundle")
    }

    fn golden_bundle(log_directory: PathBuf) -> (Vec<u8>, SupportBundleManifest) {
        let config = crate::parse_config(
            br#"{"audio":{"enabled":false,"compressed":true},"microphone_input":{"enabled":false},"logging":{"level":0},"platform":{}}"#,
            Path::new("pier.json"),
        )
        .expect("fixture config");
        let resolved = config.logging.resolved_profile().expect("profile");
        let startup = crate::StartupConfig {
            path: PathBuf::from("pier.json"),
            profile: resolved.profile,
            profile_source: resolved.source,
        };
        let sources = BundleSources {
            config: &config,
            startup: &startup,
            log_directory,
            host_probes: false,
        };
        let (writer, manifest, _) = build(
            Cursor::new(Vec::new()),
            1_794_726_245,
            BundlePseudonymKey::from_bytes([0x11; 32]),
            &sources,
            false,
        )
        .expect("bundle");
        (writer.into_inner(), manifest)
    }

    /// The macOS Pier's bundle with the live host probe left out (it
    /// depends on the machine): its fixed notices, its configuration and the
    /// managed log, at a fixed time and key. Re-bless after an intended
    /// change with `ARCEN_BLESS_SUPPORT_GOLDEN=1`.
    #[test]
    fn the_manifest_matches_the_golden_one_and_leaks_nothing() {
        let (archive, manifest) = golden_bundle(fixture_directory());
        let mut value = serde_json::to_value(&manifest).expect("manifest JSON");
        value["component"]["version"] = "<version>".into();
        value["component"]["arch"] = "<arch>".into();
        let mut rendered = serde_json::to_string_pretty(&value).expect("render");
        rendered.push('\n');
        let golden_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
        if std::env::var_os("ARCEN_BLESS_SUPPORT_GOLDEN").is_some() {
            std::fs::write(&golden_path, &rendered).expect("bless golden");
        }
        let golden = std::fs::read_to_string(&golden_path).expect("golden");
        assert_eq!(rendered, golden, "manifest differs from {GOLDEN}");

        let mut archive = zip::ZipArchive::new(Cursor::new(archive)).expect("ZIP");
        let mut text = String::new();
        for index in 0..archive.len() {
            archive
                .by_index(index)
                .expect("entry")
                .read_to_string(&mut text)
                .expect("UTF-8 entry");
        }
        assert!(text.contains("anon:"));
        for secret in ["raw-artist", "raw-pier", "192.0.2.77", "raw-network"] {
            assert!(!text.contains(secret), "{secret} leaked into the bundle");
        }
    }

    #[test]
    fn records_missing_managed_log_notice() {
        let (_, manifest) = golden_bundle(fixture_directory().join("missing"));
        assert!(
            manifest
                .entries
                .iter()
                .all(|entry| entry.path.as_str() != LOG_ENTRY_PATH)
        );
        assert!(manifest.notices.iter().any(|notice| {
            notice.source.as_str() == LOG_ENTRY_PATH
                && notice.code == NoticeCode::SourceNotFound
                && notice.kind == NoticeKind::Unavailable
        }));
    }
}
