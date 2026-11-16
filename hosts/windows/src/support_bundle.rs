// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-pier support-bundle`: the Windows Pier's sources for the shared
//! support-bundle collector (`arcen_observability::support_bundle`), which
//! owns the archive, its limits, the manifest and redaction.

use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_observability::support_bundle::{
    self as bundle, BundleLimits, Collector, LogCandidate, OutputOption, SupportBundleError,
};
use arcen_telemetry::{
    BundleComponent, BundlePath, BundlePseudonymKey, BundlePseudonymizer, BundleSource, NoticeCode,
    NoticeKind, SupportBundleManifest,
};
use serde::Serialize;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::SystemInformation::{GetTickCount64, OSVERSIONINFOW};

const USAGE: &str = "USAGE:\n  arcen-pier support-bundle [--out <DIR>] [--include-crash-dumps]";

pub(crate) use bundle::WriteOptions as SupportBundleOptions;

pub(crate) fn parse_options(arguments: &[String]) -> Result<SupportBundleOptions, String> {
    match bundle::parse_output_option(arguments)? {
        OutputOption::Help => Err(USAGE.to_string()),
        OutputOption::Write(options) => Ok(options),
    }
}

#[derive(Debug)]
pub(crate) struct SupportBundleResult {
    pub(crate) path: PathBuf,
    pub(crate) omission_count: usize,
}

pub(crate) fn run(options: &SupportBundleOptions) -> Result<SupportBundleResult, String> {
    let output_directory = options
        .output_directory
        .clone()
        .unwrap_or_else(crate::paths::support_dir);
    std::fs::create_dir_all(&output_directory).map_err(|error| {
        format!(
            "create support-bundle output directory {}: {error}; use --out <DIR> to select a writable directory",
            output_directory.display()
        )
    })?;
    let generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock precedes the Unix epoch".to_string())?
        .as_secs();
    let mut pseudonym_key = BundlePseudonymKey::zeroed();
    getrandom::getrandom(pseudonym_key.entropy_buffer())
        .map_err(|_| "generate support-bundle pseudonymization key".to_string())?;
    let process_id = std::process::id();
    let sources = BundleSources::host(options.include_crash_dumps);
    let (path, omission_count) = bundle::write_archive(
        &output_directory,
        |suffix| format!("arcen-support-{generated_at}-{process_id}{suffix}.zip"),
        |file| {
            let (file, _, omissions) = build_archive(file, generated_at, pseudonym_key, &sources)
                .map_err(SupportBundleError::Archive)?;
            Ok((file, omissions))
        },
    )
    .map_err(bundle::command_error)?;
    Ok(SupportBundleResult {
        path,
        omission_count,
    })
}

type LogListing = fn() -> Result<
    (Vec<crate::log_maintenance::SupportBundleLog>, bool),
    crate::log_maintenance::SupportBundleLogError,
>;

/// Where this Pier's bundle sources are. The host's own in production; a
/// golden test points them at fixtures and leaves out the live probes.
struct BundleSources {
    logs: LogListing,
    pier_config: PathBuf,
    /// The live host probes: display and time-zone recovery journals,
    /// system, service and GPU diagnostics, and the Event Log.
    host_probes: bool,
    /// Crash reports were asked for (`--include-crash-dumps`).
    include_crash_dumps: bool,
}

impl BundleSources {
    fn host(include_crash_dumps: bool) -> Self {
        Self {
            logs: crate::log_maintenance::support_bundle_logs,
            pier_config: crate::paths::config_path(),
            host_probes: true,
            include_crash_dumps,
        }
    }
}

fn component() -> BundleComponent {
    BundleComponent {
        name: "arcen-pier-windows".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: "windows".to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

fn build_archive<W: Write + Seek>(
    writer: W,
    generated_at: u64,
    pseudonym_key: BundlePseudonymKey,
    sources: &BundleSources,
) -> Result<(W, SupportBundleManifest, usize), String> {
    let mut collector = Collector::new(
        writer,
        component(),
        generated_at,
        BundlePseudonymizer::new(pseudonym_key),
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
    collector.notice(
        "config/tls/private_key",
        NoticeKind::Omitted,
        NoticeCode::PrivateKeyExcluded,
    )?;
    collector.notice(
        "config/tls/certificate",
        NoticeKind::Omitted,
        NoticeCode::CertificateExcluded,
    )?;
    collector.notice(
        "diagnostics/nvidia-driver",
        NoticeKind::Unavailable,
        NoticeCode::DriverQueryUnavailable,
    )?;
    // No crash-report source on this Pier yet (GAP-040).
    collector.crash_report_choice(sources.include_crash_dumps, false)?;

    collect_logs(&mut collector, sources.logs)?;
    collect_config(&mut collector, &sources.pier_config)?;
    if sources.host_probes {
        collect_recovery(&mut collector)?;
        collect_timezone_recovery(&mut collector)?;
        collect_diagnostics(&mut collector)?;
        collect_lifecycle_events(&mut collector)?;
    }
    Ok(collector.finish()?)
}

fn collect_logs<W: Write + Seek>(
    collector: &mut Collector<W>,
    logs: LogListing,
) -> Result<(), String> {
    let (logs, candidates_truncated) = match logs() {
        Ok(result) => result,
        Err(crate::log_maintenance::SupportBundleLogError::PermissionDenied) => {
            collector.notice(
                "logs",
                NoticeKind::PermissionDenied,
                NoticeCode::SourcePermissionDenied,
            )?;
            return Ok(());
        }
        Err(crate::log_maintenance::SupportBundleLogError::Unavailable) => {
            collector.notice(
                "logs",
                NoticeKind::Unavailable,
                NoticeCode::SourceUnavailable,
            )?;
            return Ok(());
        }
    };
    if candidates_truncated {
        collector.notice("logs", NoticeKind::Truncated, NoticeCode::LogCandidateLimit)?;
    }
    let logs: Vec<LogCandidate> = logs
        .into_iter()
        .map(|log| LogCandidate {
            path: log.path,
            archive_path: log.archive_path,
            size_bytes: log.size_bytes,
        })
        .collect();
    bundle::collect_canonical_logs(collector, &logs)?;
    Ok(())
}

fn collect_config<W: Write + Seek>(
    collector: &mut Collector<W>,
    source_path: &Path,
) -> Result<(), String> {
    const ARCHIVE_PATH: &str = "config/pier.json";
    let archive_path = BundlePath::new(ARCHIVE_PATH).map_err(SupportBundleError::from)?;
    let Some(mut document) = bundle::read_json_document(collector, source_path, ARCHIVE_PATH)?
    else {
        return Ok(());
    };
    let redactions = bundle::redact_pier_config(&archive_path, &mut document)?;
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize redacted Pier config: {error}"))?;
    collector.add_document(
        ARCHIVE_PATH,
        BundleSource::Configuration,
        None,
        &bytes,
        redactions,
    )?;
    Ok(())
}

fn collect_recovery<W: Write + Seek>(collector: &mut Collector<W>) -> Result<(), String> {
    let source = crate::recovery::default_path();
    let approved = crate::paths::agent_runtime_dir().join("display-recovery.json");
    let archive_path = "runtime/display-recovery.json";
    if source != approved {
        collector.notice(archive_path, NoticeKind::Invalid, NoticeCode::SourceInvalid)?;
        return Ok(());
    }

    let metadata = match std::fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            collector.notice(
                archive_path,
                NoticeKind::Unavailable,
                NoticeCode::SourceNotFound,
            )?;
            return Ok(());
        }
        Err(error) => {
            collector.file_notice(archive_path, &error)?;
            return Ok(());
        }
    };
    if metadata.len() > collector.limits().max_document_bytes {
        collector.notice(
            archive_path,
            NoticeKind::Invalid,
            NoticeCode::SourceTooLarge,
        )?;
        return Ok(());
    }
    if bundle::is_unsafe_source(&metadata) {
        collector.notice(
            archive_path,
            NoticeKind::Invalid,
            NoticeCode::UnsafeFileType,
        )?;
        return Ok(());
    }
    let journal = match crate::recovery::read(&source) {
        Ok(journal) => journal,
        Err(_) => {
            collector.notice(archive_path, NoticeKind::Invalid, NoticeCode::SourceInvalid)?;
            return Ok(());
        }
    };
    let bytes = serde_json::to_vec_pretty(&journal)
        .map_err(|error| format!("serialize validated display recovery journal: {error}"))?;
    if collector.add_bytes(archive_path, BundleSource::RuntimeState, &bytes)? {
        collector.notice(
            archive_path,
            NoticeKind::Advisory,
            NoticeCode::PendingDisplayRestore,
        )?;
    }
    Ok(())
}

fn collect_timezone_recovery<W: Write + Seek>(collector: &mut Collector<W>) -> Result<(), String> {
    let source = crate::timezone::default_journal_path();
    let approved = crate::paths::recovery_dir().join("timezone-recovery.json");
    let archive_path = "recovery/timezone-recovery-metadata.json";
    if source != approved {
        collector.notice(archive_path, NoticeKind::Invalid, NoticeCode::SourceInvalid)?;
        return Ok(());
    }
    let metadata = match std::fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            collector.file_notice(archive_path, &error)?;
            return Ok(());
        }
    };
    if metadata.len() > 64 * 1024 {
        collector.notice(
            archive_path,
            NoticeKind::Invalid,
            NoticeCode::SourceTooLarge,
        )?;
        return Ok(());
    }
    if bundle::is_unsafe_source(&metadata) {
        collector.notice(
            archive_path,
            NoticeKind::Invalid,
            NoticeCode::UnsafeFileType,
        )?;
        return Ok(());
    }
    let journal = match crate::timezone::read_journal(&source) {
        Ok(journal) => journal,
        Err(_) => {
            collector.notice(archive_path, NoticeKind::Invalid, NoticeCode::SourceInvalid)?;
            return Ok(());
        }
    };
    let bytes = serde_json::to_vec_pretty(&journal.support_metadata())
        .map_err(|error| format!("serialize timezone recovery metadata: {error}"))?;
    collector.add_bytes(archive_path, BundleSource::RuntimeState, &bytes)?;
    Ok(())
}

fn collect_diagnostics<W: Write + Seek>(collector: &mut Collector<W>) -> Result<(), String> {
    collector.add_bytes(
        "diagnostics/system.json",
        BundleSource::Diagnostics,
        &serde_json::to_vec_pretty(&system_report())
            .map_err(|error| format!("serialize system diagnostics: {error}"))?,
    )?;
    let service_state = crate::service::query_installed_service_state();
    collector.add_bytes(
        "diagnostics/service.json",
        BundleSource::Diagnostics,
        &serde_json::to_vec_pretty(&serde_json::json!({
            "service": crate::service::SERVICE_NAME,
            "state": service_state,
        }))
        .map_err(|error| format!("serialize service diagnostics: {error}"))?,
    )?;
    match service_state {
        crate::service::InstalledServiceState::Stopped => collector.notice(
            "diagnostics/service",
            NoticeKind::Unavailable,
            NoticeCode::ServiceStopped,
        )?,
        crate::service::InstalledServiceState::NotInstalled => collector.notice(
            "diagnostics/service",
            NoticeKind::Unavailable,
            NoticeCode::ServiceNotInstalled,
        )?,
        _ => {}
    }
    match crate::gpu_probe::probe() {
        Ok(report) => {
            let bytes = serde_json::to_vec_pretty(&report)
                .map_err(|error| format!("serialize GPU diagnostics: {error}"))?;
            collector.add_bytes("diagnostics/gpu.json", BundleSource::Diagnostics, &bytes)?;
        }
        Err(_) => collector.notice(
            "diagnostics/gpu",
            NoticeKind::Unavailable,
            NoticeCode::DiagnosticUnavailable,
        )?,
    }
    Ok(())
}

fn collect_lifecycle_events<W: Write + Seek>(collector: &mut Collector<W>) -> Result<(), String> {
    match crate::eventlog::query_recent_events(crate::eventlog::EventExcerptLimits::default()) {
        Ok(excerpt) => {
            let _ = (excerpt.record_count, excerpt.source, excerpt.media_type);
            collector.add_bytes(
                excerpt.suggested_name,
                BundleSource::LifecycleEvents,
                &excerpt.bytes,
            )?;
            if excerpt.truncated {
                collector.notice(
                    "events/windows-event-log",
                    NoticeKind::Truncated,
                    NoticeCode::LifecycleQueryTruncated,
                )?;
            }
        }
        Err(crate::eventlog::NativeEventQueryError::PermissionDenied) => collector.notice(
            "events/windows-event-log",
            NoticeKind::PermissionDenied,
            NoticeCode::LifecycleQueryPermissionDenied,
        )?,
        Err(_) => collector.notice(
            "events/windows-event-log",
            NoticeKind::Unavailable,
            NoticeCode::LifecycleQueryUnavailable,
        )?,
    }
    Ok(())
}

#[derive(Serialize)]
struct SystemReport {
    os: &'static str,
    arch: &'static str,
    version_major: u32,
    version_minor: u32,
    build_number: u32,
    uptime_seconds: u64,
}

fn system_report() -> SystemReport {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    let status = unsafe { RtlGetVersion(&mut version) };
    if status.0 < 0 {
        version = OSVERSIONINFOW::default();
    }
    SystemReport {
        os: "windows",
        arch: std::env::consts::ARCH,
        version_major: version.dwMajorVersion,
        version_minor: version.dwMinorVersion,
        build_number: version.dwBuildNumber,
        uptime_seconds: unsafe { GetTickCount64() } / 1_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn support_bundle_parser_is_strict() {
        assert_eq!(
            parse_options(&[]).expect("defaults"),
            SupportBundleOptions::default()
        );
        assert_eq!(
            parse_options(&["--out".to_string(), "D:\\support".to_string()])
                .expect("explicit output")
                .output_directory,
            Some(PathBuf::from("D:\\support"))
        );
        assert!(parse_options(&["--out".to_string()]).is_err());
        assert!(parse_options(&["--unknown".to_string()]).is_err());
        assert!(parse_options(&["--help".to_string()]).is_err());
    }

    const GOLDEN: &str = "tests/data/support-bundle/manifest.golden.json";

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/support-bundle")
            .join(name)
    }

    fn fixture_logs() -> Result<
        (Vec<crate::log_maintenance::SupportBundleLog>, bool),
        crate::log_maintenance::SupportBundleLogError,
    > {
        let path = fixture("arcen-pier.jsonl");
        let size_bytes = std::fs::metadata(&path).expect("fixture log").len();
        Ok((
            vec![crate::log_maintenance::SupportBundleLog {
                path,
                archive_path: "logs/arcen-pier.log".to_string(),
                size_bytes,
                modified: UNIX_EPOCH,
            }],
            false,
        ))
    }

    /// The Windows Pier's bundle with the live host probes left out (they
    /// depend on the machine): its fixed notices, a canonical log and its
    /// configuration, at a fixed time and key. Re-bless after an intended
    /// change with `ARCEN_BLESS_SUPPORT_GOLDEN=1`.
    #[test]
    fn the_manifest_matches_the_golden_one_and_leaks_nothing() {
        let sources = BundleSources {
            logs: fixture_logs,
            pier_config: fixture("pier.json"),
            host_probes: false,
            include_crash_dumps: false,
        };
        let (writer, manifest, _) = build_archive(
            std::io::Cursor::new(Vec::new()),
            1_794_726_245,
            BundlePseudonymKey::from_bytes([0x11; 32]),
            &sources,
        )
        .expect("bundle");
        let mut value = serde_json::to_value(&manifest).expect("manifest JSON");
        value["component"]["version"] = "<version>".into();
        value["component"]["arch"] = "<arch>".into();
        for entry in value["entries"].as_array_mut().expect("entries") {
            // A checkout may turn the fixtures' line ends into CRLF.
            entry["original_size_bytes"] = 0.into();
        }
        let mut rendered = serde_json::to_string_pretty(&value).expect("render");
        rendered.push('\n');
        let golden_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
        if std::env::var_os("ARCEN_BLESS_SUPPORT_GOLDEN").is_some() {
            std::fs::write(&golden_path, &rendered).expect("bless golden");
        }
        let golden = std::fs::read_to_string(&golden_path)
            .expect("golden")
            .replace("\r\n", "\n");
        assert_eq!(rendered, golden, "manifest differs from {GOLDEN}");

        let mut archive = zip::ZipArchive::new(writer).expect("ZIP");
        let mut text = String::new();
        for index in 0..archive.len() {
            archive
                .by_index(index)
                .expect("entry")
                .read_to_string(&mut text)
                .expect("UTF-8 entry");
        }
        for secret in [
            "raw-artist",
            "raw-pier",
            "192.0.2.77",
            "raw-network",
            "raw-customer",
            "raw-secret",
        ] {
            assert!(!text.contains(secret), "{secret} leaked into the bundle");
        }
    }
}
