// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-pier support-bundle`: the Linux Pier's sources for the shared
//! support-bundle collector (`arcen_observability::support_bundle`), which
//! owns the archive, its limits, the manifest and redaction.

use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arcen_observability::support_bundle::{
    self as bundle, BundleLimits, LogCandidate, OutputOption, SupportBundleError,
};
use arcen_telemetry::{
    redact_json_document_at, BundleComponent, BundleIdentityKind, BundlePath, BundlePseudonymKey,
    BundlePseudonymizer, BundleSource, NoticeCode, NoticeKind, RedactionDecision, RedactionReason,
    RedactionRecord, SupportBundleManifest, SupportBundleRedactionPolicy, REDACTED_VALUE,
};
use serde::Serialize;

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_COMMAND_BYTES: usize = 2 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// The live Wayland probe runs `gnome-shell --help` and similar, each
/// bounded on its own; this bounds the whole selection.
const DESKTOP_PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_COMMANDS: usize = 8;
const DEFAULT_OUTPUT_DIRECTORY: &str = "/var/lib/arcen/support";
const USAGE: &str = "USAGE:\n  arcen-pier support-bundle [--out <DIR>] [--include-crash-dumps]";

pub use bundle::WriteOptions as SupportBundleOptions;

pub fn parse_options(arguments: &[String]) -> Result<SupportBundleOptions, String> {
    match bundle::parse_output_option(arguments)? {
        OutputOption::Help => Err(USAGE.to_string()),
        OutputOption::Write(options) => Ok(options),
    }
}

#[derive(Debug)]
pub struct SupportBundleResult {
    pub path: PathBuf,
    pub omission_count: usize,
}

pub fn run(options: &SupportBundleOptions) -> Result<SupportBundleResult, String> {
    let using_default = options.output_directory.is_none();
    let output_directory = options
        .output_directory
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OUTPUT_DIRECTORY));
    std::fs::create_dir_all(&output_directory).map_err(|error| {
        format!(
            "create support-bundle output directory {}: {error}; use --out <DIR> to select a writable directory",
            output_directory.display()
        )
    })?;
    set_default_directory_permissions(&output_directory, using_default)?;
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

#[cfg(target_os = "linux")]
fn set_default_directory_permissions(path: &Path, using_default: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if using_default {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("set support directory mode 0700: {error}"))?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_default_directory_permissions(_path: &Path, _using_default: bool) -> Result<(), String> {
    Ok(())
}

/// Lists the managed logs for a bundle (`crate::logging::support_bundle_logs`).
type LogListing = fn() -> Result<
    (Vec<crate::logging::SupportBundleLog>, bool, bool),
    crate::logging::SupportBundleLogError,
>;

/// Where this Pier's bundle sources are. The host's own in production; a
/// golden test points them at fixtures and leaves out the live probes.
struct BundleSources {
    logs: LogListing,
    pier_config: PathBuf,
    xorg_config: PathBuf,
    /// The live host probes: system, service, effective configuration,
    /// runtime, desktop provider and the journal.
    host_probes: bool,
    /// Crash reports were asked for (`--include-crash-dumps`).
    include_crash_dumps: bool,
}

impl BundleSources {
    fn host(include_crash_dumps: bool) -> Self {
        Self {
            logs: crate::logging::support_bundle_logs,
            pier_config: PathBuf::from("/etc/arcen/pier.json"),
            xorg_config: PathBuf::from("/etc/arcen/xorg.conf"),
            host_probes: true,
            include_crash_dumps,
        }
    }
}

fn component() -> BundleComponent {
    BundleComponent {
        name: "arcen-pier-linux".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: "linux".to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

fn build_archive<W: Write + Seek>(
    writer: W,
    generated_at: u64,
    pseudonym_key: BundlePseudonymKey,
    sources: &BundleSources,
) -> Result<(W, SupportBundleManifest, usize), String> {
    let mut collector = Collector {
        inner: bundle::Collector::new(
            writer,
            component(),
            generated_at,
            BundlePseudonymizer::new(pseudonym_key),
            BundleLimits::PIER,
        ),
        command_count: 0,
    };
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
        "runtime/sessions",
        NoticeKind::Omitted,
        NoticeCode::SensitiveRuntimeExcluded,
    )?;
    // No crash-report source on this Pier yet (GAP-040).
    collector
        .inner
        .crash_report_choice(sources.include_crash_dumps, false)?;
    collector.notice(
        "logs/Xorg.log",
        NoticeKind::Omitted,
        NoticeCode::XorgLogExcluded,
    )?;
    collector.notice(
        "diagnostics/nv-control",
        NoticeKind::Unavailable,
        NoticeCode::DiagnosticUnavailable,
    )?;

    collect_logs(&mut collector, sources.logs)?;
    collect_json_config(&mut collector, &sources.pier_config, "config/pier.json")?;
    collect_regular_file(
        &mut collector,
        &sources.xorg_config,
        "config/xorg.conf",
        BundleSource::Configuration,
    )?;
    if sources.host_probes {
        collect_system_diagnostics(&mut collector)?;
        collect_service_diagnostics(&mut collector)?;
        collect_effective_config(&mut collector)?;
        collect_runtime_diagnostics(&mut collector)?;
        collect_desktop_provider(&mut collector)?;
        collect_lifecycle_events(&mut collector)?;
    }
    Ok(collector.inner.finish()?)
}

/// The shared collector plus this Pier's bound on diagnostic commands.
struct Collector<W: Write + Seek> {
    inner: bundle::Collector<W>,
    command_count: usize,
}

impl<W: Write + Seek> std::ops::Deref for Collector<W> {
    type Target = bundle::Collector<W>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<W: Write + Seek> std::ops::DerefMut for Collector<W> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<W: Write + Seek> Collector<W> {
    fn command(&mut self, program: &str, arguments: &[&str]) -> CommandOutcome {
        if self.command_count >= MAX_COMMANDS {
            return CommandOutcome::unavailable();
        }
        self.command_count += 1;
        run_bounded_command(program, arguments)
    }
}
fn collect_logs<W: Write + Seek>(
    collector: &mut Collector<W>,
    logs: LogListing,
) -> Result<(), String> {
    let (logs, candidates_truncated, legacy) = match logs() {
        Ok(result) => result,
        Err(crate::logging::SupportBundleLogError::PermissionDenied) => {
            collector.notice(
                "logs",
                NoticeKind::PermissionDenied,
                NoticeCode::SourcePermissionDenied,
            )?;
            return Ok(());
        }
        Err(crate::logging::SupportBundleLogError::Unavailable) => {
            collector.notice(
                "logs",
                NoticeKind::Unavailable,
                NoticeCode::SourceUnavailable,
            )?;
            return Ok(());
        }
    };
    if legacy {
        collector.notice("logs", NoticeKind::Advisory, NoticeCode::LegacyLogMode)?;
    }
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

fn collect_json_config<W: Write + Seek>(
    collector: &mut Collector<W>,
    source_path: &Path,
    archive_path: &str,
) -> Result<(), String> {
    let archive = BundlePath::new(archive_path).map_err(SupportBundleError::from)?;
    let Some(mut document) = bundle::read_json_document(collector, source_path, archive_path)?
    else {
        return Ok(());
    };
    let redactions = bundle::redact_pier_config(&archive, &mut document)?;
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize redacted Linux config: {error}"))?;
    collector.add_document(
        archive.as_str(),
        BundleSource::Configuration,
        None,
        &bytes,
        redactions,
    )?;
    Ok(())
}

fn collect_regular_file<W: Write + Seek>(
    collector: &mut Collector<W>,
    source_path: &Path,
    archive_path: &str,
    source: BundleSource,
) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(source_path) {
        Ok(metadata) => metadata,
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
    collector.add_file(
        source_path,
        archive_path,
        source,
        metadata.len(),
        metadata.len(),
        None,
    )?;
    Ok(())
}

fn collect_system_diagnostics<W: Write + Seek>(collector: &mut Collector<W>) -> Result<(), String> {
    let report = SystemReport {
        os: "linux",
        arch: std::env::consts::ARCH,
        kernel_release: read_small_trimmed(Path::new("/proc/sys/kernel/osrelease"), 4096),
        uptime_seconds: read_uptime(),
        os_release: read_os_release(),
    };
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| format!("serialize Linux system diagnostics: {error}"))?;
    collector.add_bytes("diagnostics/system.json", BundleSource::Diagnostics, &bytes)?;
    Ok(())
}

fn collect_service_diagnostics<W: Write + Seek>(
    collector: &mut Collector<W>,
) -> Result<(), String> {
    let outcome = collector.command(
        "systemctl",
        &[
            "show",
            "arcen-pier.service",
            "--property=LoadState,ActiveState,SubState,Result",
            "--no-pager",
        ],
    );
    if let Some(code) = command_notice_code(&outcome) {
        collector.notice("diagnostics/service", outcome.notice_kind(), code)?;
        return Ok(());
    }
    let Some(report) = sanitize_systemctl_properties(&outcome.stdout) else {
        collector.notice(
            "diagnostics/service",
            NoticeKind::Invalid,
            NoticeCode::SourceInvalid,
        )?;
        return Ok(());
    };
    if report
        .get("ActiveState")
        .is_some_and(|state| state == "inactive" || state == "failed")
    {
        collector.notice(
            "diagnostics/service",
            NoticeKind::Unavailable,
            NoticeCode::ServiceStopped,
        )?;
    }
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| format!("serialize service diagnostics: {error}"))?;
    collector.add_bytes(
        "diagnostics/service.json",
        BundleSource::Diagnostics,
        &bytes,
    )?;
    Ok(())
}

fn collect_effective_config<W: Write + Seek>(collector: &mut Collector<W>) -> Result<(), String> {
    let outcome = collector.command(
        "systemctl",
        &[
            "show",
            "arcen-pier.service",
            "--property=ExecStart",
            "--value",
            "--no-pager",
        ],
    );
    if let Some(code) = command_notice_code(&outcome) {
        collector.notice("diagnostics/effective-config", outcome.notice_kind(), code)?;
        return Ok(());
    }
    let Some((report, redactions)) = sanitize_exec_start(&outcome.stdout) else {
        collector.notice(
            "diagnostics/effective-config",
            NoticeKind::Invalid,
            NoticeCode::SourceInvalid,
        )?;
        return Ok(());
    };
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| format!("serialize effective config: {error}"))?;
    if collector.add_bytes(
        "diagnostics/effective-config.json",
        BundleSource::Diagnostics,
        &bytes,
    )? {
        let entry_path = BundlePath::new("diagnostics/effective-config.json")
            .map_err(SupportBundleError::from)?;
        for (key_path, reason) in redactions {
            collector.redaction(RedactionRecord {
                entry_path: entry_path.clone(),
                key_path,
                reason,
            })?;
        }
    }
    Ok(())
}

/// Is the multicall Pier binary present on this host?
///
/// `capenc`, `session-launcher` and `session-agent` are not separate files;
/// they are subcommands of the Pier itself, so their availability is exactly
/// the availability of that one binary.
///
/// Resolved from the running executable first, because that is the binary
/// actually serving this host however it was deployed, and a support bundle
/// that reports "capenc missing" because the operator installed somewhere
/// unusual sends whoever reads it after the wrong fault. The install locations
/// are only a fallback, and the pre-`/opt` path is still accepted so a bundle
/// taken on a host that has not been upgraded yet stays accurate.
fn pier_multicall_present() -> bool {
    if std::env::current_exe().is_ok_and(|path| path.is_file()) {
        return true;
    }
    [
        "/opt/arcen/bin/arcen-pier",
        "/usr/local/libexec/arcen/arcen-pier",
    ]
    .iter()
    .any(|path| Path::new(path).is_file())
}

fn collect_runtime_diagnostics<W: Write + Seek>(
    collector: &mut Collector<W>,
) -> Result<(), String> {
    let runtime_root = Path::new("/run/arcen/sessions");
    let metadata = std::fs::symlink_metadata(runtime_root).ok();
    let pier_multicall_present = pier_multicall_present();
    let report = serde_json::json!({
        "session_runtime_root_present": metadata.as_ref().is_some_and(std::fs::Metadata::is_dir),
        "session_runtime_contents_collected": false,
        "uinput_present": std::fs::symlink_metadata("/dev/uinput").is_ok(),
        "capenc_present": pier_multicall_present,
        "session_launcher_present": pier_multicall_present,
        "session_agent_present": pier_multicall_present,
        "pam_service": "login"
    });
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| format!("serialize runtime diagnostics: {error}"))?;
    collector.add_bytes(
        "diagnostics/runtime.json",
        BundleSource::Diagnostics,
        &bytes,
    )?;
    Ok(())
}

/// What the Pier would select as its desktop provider now, and the Wayland
/// qualification record it would read.
///
/// The selection runs the same start-time code as the service
/// (`select_configured_provider`): host facts only, never a session. The
/// record is the configured one (`capture.wayland_qualification_record`,
/// default production), included as diagnostics; it holds versions, the
/// Pier executable's hash and measured rates, and no secrets.
fn collect_desktop_provider<W: Write + Seek>(collector: &mut Collector<W>) -> Result<(), String> {
    const REPORT: &str = "diagnostics/desktop-provider.json";
    const RECORD: &str = "diagnostics/wayland-qualification.json";
    let config = match crate::cli::parse(&[]) {
        Ok(config) => config,
        Err(_) => {
            collector.notice(
                "diagnostics/desktop-provider",
                NoticeKind::Invalid,
                NoticeCode::SourceInvalid,
            )?;
            return Ok(());
        }
    };
    let record_path = PathBuf::from(config.wayland_qualification_target.record_path());
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            collector.notice(
                "diagnostics/desktop-provider",
                NoticeKind::Unavailable,
                NoticeCode::DiagnosticFailed,
            )?;
            return Ok(());
        }
    };
    // A prefix that may not be used refuses the selection, as at Pier start;
    // nothing is evaluated (or run) for `/usr` in its place.
    if let Err(prefix_error) = crate::session::wayland::apply_configured_compositor_prefix(
        config.wayland_compositor_prefix.as_deref(),
    ) {
        let report = desktop_provider_report(
            &config,
            &record_path,
            ProviderOutcome::PrefixRefused(&prefix_error),
        );
        let bytes = serde_json::to_vec_pretty(&report)
            .map_err(|error| format!("serialize desktop provider diagnostics: {error}"))?;
        collector.add_bytes(REPORT, BundleSource::Diagnostics, &bytes)?;
        return Ok(());
    }
    let selected = runtime.block_on(async {
        tokio::time::timeout(
            DESKTOP_PROVIDER_TIMEOUT,
            crate::session::wayland::select_configured_provider(
                config.provider_preference,
                &config.xorg_bin,
                &record_path,
            ),
        )
        .await
    });
    let report = match selected {
        Err(_) => {
            collector.notice(
                "diagnostics/desktop-provider",
                NoticeKind::TimedOut,
                NoticeCode::DiagnosticTimedOut,
            )?;
            None
        }
        Ok(selected) => Some(desktop_provider_report(
            &config,
            &record_path,
            match selected.as_ref() {
                Ok(configured) => ProviderOutcome::Selected(
                    &configured.selection,
                    configured.qualification.is_some(),
                ),
                Err(refusal) => ProviderOutcome::Refused(refusal),
            },
        )),
    };
    if let Some(report) = report {
        let bytes = serde_json::to_vec_pretty(&report)
            .map_err(|error| format!("serialize desktop provider diagnostics: {error}"))?;
        collector.add_bytes(REPORT, BundleSource::Diagnostics, &bytes)?;
    }
    let archive = BundlePath::new(RECORD).map_err(SupportBundleError::from)?;
    let Some(mut record) = bundle::read_json_document(collector, &record_path, RECORD)? else {
        return Ok(());
    };
    let redactions =
        redact_json_document_at(&archive, &mut record).map_err(SupportBundleError::from)?;
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|error| format!("serialize Wayland qualification record: {error}"))?;
    collector.add_document(
        archive.as_str(),
        BundleSource::Diagnostics,
        None,
        &bytes,
        redactions,
    )?;
    Ok(())
}

/// What the start-time provider selection would do.
enum ProviderOutcome<'a> {
    /// The selection, and whether the qualification record is valid.
    Selected(
        &'a arcen_session::desktop_provider::DesktopProviderSelection,
        bool,
    ),
    /// The selection's refusal.
    Refused(&'a arcen_session::desktop_provider::ProviderRefusal),
    /// The configured compositor prefix may not be used: the Pier refuses to
    /// start before any selection.
    PrefixRefused(&'a str),
}

fn desktop_provider_report(
    config: &crate::cli::Config,
    record_path: &Path,
    selected: ProviderOutcome<'_>,
) -> serde_json::Value {
    let mut report = serde_json::json!({
        "preference": config.provider_preference.token(),
        "wayland_adapter_built": crate::session::wayland::WAYLAND_ADAPTER_BUILT,
        "xorg_bin": config.xorg_bin.display().to_string(),
        "record_path": record_path.display().to_string(),
        "record_is_production": config.wayland_qualification_target.is_production(),
        "compositor_prefix": config
            .wayland_compositor_prefix
            .clone()
            .unwrap_or_else(|| arcen_session::compositor_prefix::SYSTEM_PREFIX.to_owned()),
        // The record is valid for one Pier executable; selection here is
        // evaluated for the binary taking the bundle.
        "evaluated_by": std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    });
    let outcome = match selected {
        ProviderOutcome::Selected(selection, record_valid) => serde_json::json!({
            "provider": selection.provider.token(),
            "reason": selection.reason.token(),
            "record_valid": record_valid,
        }),
        ProviderOutcome::Refused(refusal) => serde_json::json!({
            "provider": null,
            "refused": refusal.to_string(),
            "blockers": refusal.tokens(),
        }),
        ProviderOutcome::PrefixRefused(error) => serde_json::json!({
            "provider": null,
            "refused": error,
            "blockers": ["compositor_prefix"],
            "compositor_prefix_error": error,
        }),
    };
    if let (Some(report), serde_json::Value::Object(outcome)) = (report.as_object_mut(), outcome) {
        report.extend(outcome);
    }
    report
}

fn collect_lifecycle_events<W: Write + Seek>(collector: &mut Collector<W>) -> Result<(), String> {
    match crate::eventlog::query_recent_events(crate::eventlog::EventExcerptLimits::default()) {
        Ok(excerpt) => {
            let _ = (excerpt.record_count, excerpt.media_type);
            let (bytes, identity_redacted, invalid_omitted) = match excerpt.source {
                crate::eventlog::NativeEventQuerySource::Journal => {
                    pseudonymize_native_journal(&excerpt.bytes, collector.pseudonymizer())
                }
                crate::eventlog::NativeEventQuerySource::Syslog => (excerpt.bytes, false, false),
            };
            let added = collector.add_bytes(
                excerpt.suggested_name,
                BundleSource::LifecycleEvents,
                &bytes,
            )?;
            if invalid_omitted {
                collector.notice(
                    "events/linux-native",
                    NoticeKind::Invalid,
                    NoticeCode::SourceInvalid,
                )?;
            }
            if added && identity_redacted {
                collector.redaction(RedactionRecord {
                    entry_path: BundlePath::new(excerpt.suggested_name)
                        .map_err(SupportBundleError::from)?,
                    key_path: "/ARCEN_FIELD_SSID".to_string(),
                    reason: RedactionReason::IdentityPseudonymized,
                })?;
            }
            if excerpt.truncated {
                collector.notice(
                    "events/linux-native",
                    NoticeKind::Truncated,
                    NoticeCode::LifecycleQueryTruncated,
                )?;
            }
        }
        Err(crate::eventlog::NativeEventQueryError::PermissionDenied) => collector.notice(
            "events/linux-native",
            NoticeKind::PermissionDenied,
            NoticeCode::LifecycleQueryPermissionDenied,
        )?,
        Err(crate::eventlog::NativeEventQueryError::TimedOut) => collector.notice(
            "events/linux-native",
            NoticeKind::TimedOut,
            NoticeCode::LifecycleQueryTimedOut,
        )?,
        Err(_) => collector.notice(
            "events/linux-native",
            NoticeKind::Unavailable,
            NoticeCode::LifecycleQueryUnavailable,
        )?,
    }
    Ok(())
}

fn pseudonymize_native_journal(
    bytes: &[u8],
    pseudonymizer: &BundlePseudonymizer,
) -> (Vec<u8>, bool, bool) {
    let mut output = Vec::with_capacity(bytes.len());
    let mut identity_redacted = false;
    let mut invalid_omitted = false;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        if line.len() > arcen_telemetry::MAX_CANONICAL_JSON_LINE_BYTES {
            invalid_omitted = true;
            continue;
        }
        let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(line) else {
            invalid_omitted = true;
            continue;
        };
        let Some(object) = value.as_object_mut() else {
            invalid_omitted = true;
            continue;
        };
        if let Some(serde_json::Value::String(identity)) = object.get_mut("ARCEN_FIELD_SSID") {
            *identity = pseudonymizer.pseudonymize(BundleIdentityKind::NetworkIdentity, identity);
            identity_redacted = true;
        }
        let Ok(rendered) = serde_json::to_vec(&value) else {
            invalid_omitted = true;
            continue;
        };
        output.extend_from_slice(&rendered);
        output.push(b'\n');
    }
    (output, identity_redacted, invalid_omitted)
}

#[derive(Debug)]
struct CommandOutcome {
    stdout: Vec<u8>,
    success: bool,
    unavailable: bool,
    timed_out: bool,
    truncated: bool,
}

impl CommandOutcome {
    fn unavailable() -> Self {
        Self {
            stdout: Vec::new(),
            success: false,
            unavailable: true,
            timed_out: false,
            truncated: false,
        }
    }

    fn notice_kind(&self) -> NoticeKind {
        if self.timed_out {
            NoticeKind::TimedOut
        } else if self.truncated {
            NoticeKind::Truncated
        } else {
            NoticeKind::Unavailable
        }
    }
}

fn command_notice_code(outcome: &CommandOutcome) -> Option<NoticeCode> {
    if outcome.unavailable {
        Some(NoticeCode::DiagnosticUnavailable)
    } else if outcome.timed_out {
        Some(NoticeCode::DiagnosticTimedOut)
    } else if outcome.truncated {
        Some(NoticeCode::DiagnosticTruncated)
    } else if !outcome.success {
        Some(NoticeCode::DiagnosticFailed)
    } else {
        None
    }
}

fn run_bounded_command(program: &str, arguments: &[&str]) -> CommandOutcome {
    let mut child = match Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return CommandOutcome::unavailable(),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_thread =
        stdout.map(|stream| std::thread::spawn(move || drain_bounded(stream, MAX_COMMAND_BYTES)));
    let stderr_thread =
        stderr.map(|stream| std::thread::spawn(move || drain_bounded(stream, MAX_COMMAND_BYTES)));
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let (success, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.success(), false),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break (false, true);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break (false, false);
            }
        }
    };
    let (stdout, stdout_truncated) = stdout_thread
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    let (_, stderr_truncated) = stderr_thread
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default();
    CommandOutcome {
        stdout,
        success,
        unavailable: false,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    }
}

fn drain_bounded(mut stream: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut bytes = Vec::with_capacity(limit.min(COPY_BUFFER_BYTES));
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    let mut truncated = false;
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let remaining = limit.saturating_sub(bytes.len());
                let stored = count.min(remaining);
                bytes.extend_from_slice(&buffer[..stored]);
                truncated |= stored != count;
            }
        }
    }
    (bytes, truncated)
}

fn sanitize_systemctl_properties(
    bytes: &[u8],
) -> Option<std::collections::BTreeMap<String, String>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let allowed = ["LoadState", "ActiveState", "SubState", "Result"];
    let mut report = std::collections::BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line.split_once('=')?;
        if !allowed.contains(&key) || value.chars().any(char::is_control) {
            return None;
        }
        report.insert(key.to_string(), value.to_string());
    }
    (!report.is_empty()).then_some(report)
}

fn sanitize_exec_start(
    bytes: &[u8],
) -> Option<(serde_json::Value, Vec<(String, RedactionReason)>)> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.contains(['\0', '\n', '\r', '"', '\'', '\\']) {
        return None;
    }
    let argv = text.split_once("argv[]=")?.1.split_once(';')?.0.trim();
    let tokens = argv.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return None;
    }
    let mut options = serde_json::Map::new();
    let mut redactions = Vec::new();
    let mut index = 1;
    while index < tokens.len() {
        let option = tokens[index];
        if !option.starts_with("--")
            || !option[2..]
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return None;
        }
        let key = option.trim_start_matches("--");
        let has_value = tokens
            .get(index + 1)
            .is_some_and(|value| !value.starts_with("--"));
        let value = if has_value {
            index += 1;
            match SupportBundleRedactionPolicy::classify_key(key) {
                _ if key.eq_ignore_ascii_case("tls-cert")
                    || key.eq_ignore_ascii_case("tls-expected-san") =>
                {
                    redactions.push((format!("/options/{key}"), RedactionReason::SensitiveKey));
                    serde_json::Value::String(REDACTED_VALUE.to_string())
                }
                RedactionDecision::Keep => serde_json::Value::String(tokens[index].to_string()),
                RedactionDecision::Redact(mut reason) => {
                    if key.eq_ignore_ascii_case("tls-key") {
                        reason = RedactionReason::PrivateKeyPolicy;
                    }
                    redactions.push((format!("/options/{key}"), reason));
                    serde_json::Value::String(REDACTED_VALUE.to_string())
                }
            }
        } else {
            serde_json::Value::Bool(true)
        };
        options.insert(key.to_string(), value);
        index += 1;
    }
    Some((serde_json::json!({ "options": options }), redactions))
}

#[derive(Serialize)]
struct SystemReport {
    os: &'static str,
    arch: &'static str,
    kernel_release: Option<String>,
    uptime_seconds: Option<u64>,
    os_release: std::collections::BTreeMap<String, String>,
}

fn read_small_trimmed(path: &Path, limit: usize) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit as u64 {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .ok()?
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let value = String::from_utf8(bytes).ok()?.trim().to_string();
    (!value.is_empty() && !value.chars().any(char::is_control)).then_some(value)
}

fn read_uptime() -> Option<u64> {
    read_small_trimmed(Path::new("/proc/uptime"), 4096)?
        .split_whitespace()
        .next()?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn read_os_release() -> std::collections::BTreeMap<String, String> {
    let Some(contents) = read_bounded_multiline(Path::new("/etc/os-release"), 64 * 1024) else {
        return std::collections::BTreeMap::new();
    };
    let allowed = ["ID", "VERSION_ID", "PRETTY_NAME"];
    contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| allowed.contains(key))
        .map(|(key, value)| {
            (
                key.to_string(),
                value.trim_matches('"').chars().take(256).collect(),
            )
        })
        .collect()
}

fn read_bounded_multiline(path: &Path, limit: usize) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit as u64 {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .ok()?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > limit || bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_provider_report_states_the_selection_or_the_refusal() {
        use arcen_session::desktop_provider::{
            DesktopProvider, DesktopProviderSelection, ProviderRefusal, SelectionReason,
        };
        let config = crate::cli::Config::default();
        let record = Path::new("/var/lib/arcen/wayland-qualification.json");
        let selection = DesktopProviderSelection {
            provider: DesktopProvider::Wayland,
            reason: SelectionReason::WaylandQualified,
        };
        let selected =
            desktop_provider_report(&config, record, ProviderOutcome::Selected(&selection, true));
        assert_eq!(selected["provider"], "wayland");
        assert_eq!(selected["reason"], "wayland_qualified");
        assert_eq!(selected["record_valid"], true);
        assert_eq!(selected["record_is_production"], true);
        assert_eq!(selected["preference"], "auto");

        let refusal = ProviderRefusal::XorgMissing { requested: true };
        let refused = desktop_provider_report(&config, record, ProviderOutcome::Refused(&refusal));
        assert!(refused["provider"].is_null());
        assert_eq!(refused["blockers"], serde_json::json!(["xorg_server"]));
        assert!(refused["refused"]
            .as_str()
            .is_some_and(|text| text.contains("no Xorg server")));
        assert!(refused.get("compositor_prefix_error").is_none());
    }

    #[test]
    fn a_refused_compositor_prefix_refuses_the_selection_in_the_report() {
        let config = crate::cli::Config {
            wayland_compositor_prefix: Some("/opt/arcen-gnome/51".to_owned()),
            ..crate::cli::Config::default()
        };
        let record = Path::new("/var/lib/arcen/wayland-qualification.json");
        let error = "capture.wayland_compositor_prefix /opt/arcen-gnome/51: \
                     /opt/arcen-gnome/51/bin/gnome-shell is owned by uid 1000, not root";
        let report =
            desktop_provider_report(&config, record, ProviderOutcome::PrefixRefused(error));
        assert!(report["provider"].is_null());
        assert_eq!(report["blockers"], serde_json::json!(["compositor_prefix"]));
        assert_eq!(report["compositor_prefix_error"], error);
        assert_eq!(report["refused"], error);
        assert_eq!(report["compositor_prefix"], "/opt/arcen-gnome/51");
        assert!(report.get("reason").is_none());
    }

    #[test]
    fn parser_is_strict_and_service_independent() {
        assert_eq!(
            parse_options(&[]).expect("defaults"),
            SupportBundleOptions::default()
        );
        assert!(parse_options(&["--out".to_string()]).is_err());
        assert!(parse_options(&["--unknown".to_string()]).is_err());
    }

    #[test]
    fn effective_config_redacts_all_tls_identity_material_and_keeps_safe_posture() {
        let input = b"{ path=/usr/local/libexec/arcen/arcen-pier ; argv[]=/usr/local/libexec/arcen/arcen-pier --port 18444 --tls-cert /private/customer.crt --tls-key /private/customer.key --tls-expected-san pier.customer.example --tls-minimum-version TLS1.3 --tls-disabled-cipher-suite TLS13_AES_128_GCM_SHA256 --tls-expiry-warning-days 14 --xauthority /run/arcen/sessions/user/Xauthority --audio ; }";
        let (value, redactions) = sanitize_exec_start(input).expect("sanitized");
        let rendered = serde_json::to_string(&value).expect("rendered");
        assert!(rendered.contains(REDACTED_VALUE));
        assert!(!rendered.contains("/private/customer.key"));
        assert!(!rendered.contains("/private/customer.crt"));
        assert!(!rendered.contains("pier.customer.example"));
        assert!(!rendered.contains("/run/arcen/sessions"));
        assert!(rendered.contains("18444"));
        assert!(rendered.contains("TLS1.3"));
        assert!(rendered.contains("TLS13_AES_128_GCM_SHA256"));
        assert!(rendered.contains("14"));
        assert_eq!(
            redactions,
            vec![
                (
                    "/options/tls-cert".to_string(),
                    RedactionReason::SensitiveKey
                ),
                (
                    "/options/tls-key".to_string(),
                    RedactionReason::PrivateKeyPolicy
                ),
                (
                    "/options/tls-expected-san".to_string(),
                    RedactionReason::SensitiveKey
                ),
                (
                    "/options/xauthority".to_string(),
                    RedactionReason::SensitiveKey
                )
            ]
        );
    }

    const GOLDEN: &str = "tests/data/support-bundle/manifest.golden.json";

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/support-bundle")
            .join(name)
    }

    fn fixture_logs() -> Result<
        (Vec<crate::logging::SupportBundleLog>, bool, bool),
        crate::logging::SupportBundleLogError,
    > {
        let path = fixture("arcen-pier.jsonl");
        let size_bytes = std::fs::metadata(&path).expect("fixture log").len();
        Ok((
            vec![crate::logging::SupportBundleLog {
                path,
                archive_path: "logs/arcen-pier.log".to_string(),
                size_bytes,
                modified: UNIX_EPOCH,
            }],
            false,
            false,
        ))
    }

    /// The Linux Pier's bundle with the live host probes left out (they
    /// depend on the machine): its fixed notices, a canonical log and its
    /// configuration, at a fixed time and key. Re-bless after an intended
    /// change with `ARCEN_BLESS_SUPPORT_GOLDEN=1`.
    #[test]
    fn the_manifest_matches_the_golden_one_and_leaks_nothing() {
        let sources = BundleSources {
            logs: fixture_logs,
            pier_config: fixture("pier.json"),
            xorg_config: fixture("xorg.conf"),
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

    #[test]
    fn native_journal_network_identity_is_pseudonymized() {
        let input = br#"{"ARCEN_EVENT_ID":"1600","ARCEN_FIELD_SSID":"raw-network"}"#;
        let pseudonymizer = BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x77; 32]));
        let (first, redacted, invalid) = pseudonymize_native_journal(input, &pseudonymizer);
        let (second, _, _) = pseudonymize_native_journal(input, &pseudonymizer);
        assert!(redacted);
        assert!(!invalid);
        assert_eq!(first, second);
        let rendered = String::from_utf8(first).expect("UTF-8");
        assert!(!rendered.contains("raw-network"));
        assert!(rendered.contains("anon:"));
    }
}
