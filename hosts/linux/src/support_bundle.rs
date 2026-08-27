use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arcen_telemetry::{
    redact_json_document_at, transform_canonical_jsonl, BundleComponent, BundleEntry,
    BundleIdentityKind, BundleNotice, BundlePath, BundlePseudonymKey, BundlePseudonymizer,
    BundleSource, BundleTruncation, CanonicalJsonlTransformLimits, CanonicalJsonlTransformReport,
    NoticeCode, NoticeKind, RedactionDecision, RedactionReason, RedactionRecord, Sha256Digest,
    SupportBundleManifestBuilder, SupportBundleRedactionPolicy, TruncationReason, REDACTED_VALUE,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_LOG_BYTES: u64 = 200 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 2 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_COMMANDS: usize = 8;
const COLLISION_LIMIT: u16 = 100;
const DEFAULT_OUTPUT_DIRECTORY: &str = "/var/lib/arcen/support";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportBundleOptions {
    pub output_directory: Option<PathBuf>,
}

pub fn parse_options(arguments: &[String]) -> Result<SupportBundleOptions, String> {
    let mut output_directory = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--out" => {
                if output_directory.is_some() {
                    return Err("--out may be supplied only once".to_string());
                }
                index += 1;
                output_directory = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .ok_or_else(|| "--out requires a directory".to_string())?,
                ));
            }
            "-h" | "--help" => {
                return Err("USAGE:\n  arcen-pier support-bundle [--out <DIR>]".to_string());
            }
            other => return Err(format!("unknown support-bundle argument: {other}")),
        }
        index += 1;
    }
    Ok(SupportBundleOptions { output_directory })
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
    let (final_path, partial_path, file) =
        create_output_file(&output_directory, generated_at, std::process::id())?;
    let result = build_archive(file, generated_at);
    match result {
        Ok((file, omission_count)) => {
            if let Err(error) = file.sync_all() {
                drop(file);
                let _ = std::fs::remove_file(&partial_path);
                return Err(format!("sync support-bundle partial archive: {error}"));
            }
            drop(file);
            if let Err(error) = std::fs::rename(&partial_path, &final_path) {
                let _ = std::fs::remove_file(&partial_path);
                return Err(format!(
                    "publish support bundle {}: {error}",
                    final_path.display()
                ));
            }
            Ok(SupportBundleResult {
                path: final_path,
                omission_count,
            })
        }
        Err(error) => {
            let _ = std::fs::remove_file(&partial_path);
            Err(error)
        }
    }
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

fn create_output_file(
    directory: &Path,
    unix_seconds: u64,
    process_id: u32,
) -> Result<(PathBuf, PathBuf, File), String> {
    for suffix in 0..COLLISION_LIMIT {
        let suffix = if suffix == 0 {
            String::new()
        } else {
            format!("-{suffix}")
        };
        let name = format!("arcen-support-{unix_seconds}-{process_id}{suffix}.zip");
        let final_path = directory.join(&name);
        if final_path.exists() {
            continue;
        }
        let partial_path = directory.join(format!(".{name}.partial"));
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        set_private_file_mode(&mut options);
        match options.open(&partial_path) {
            Ok(file) => return Ok((final_path, partial_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "create support-bundle partial archive {}: {error}; use --out <DIR> to select a writable directory",
                    partial_path.display()
                ));
            }
        }
    }
    Err("support-bundle output collision limit reached".to_string())
}

#[cfg(target_os = "linux")]
fn set_private_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(target_os = "linux"))]
fn set_private_file_mode(_options: &mut OpenOptions) {}

fn build_archive(file: File, generated_at: u64) -> Result<(File, usize), String> {
    let mut pseudonym_key = BundlePseudonymKey::zeroed();
    getrandom::getrandom(pseudonym_key.entropy_buffer())
        .map_err(|_| "generate support-bundle pseudonymization key".to_string())?;
    let component = BundleComponent {
        name: "arcen-pier-linux".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: "linux".to_string(),
        arch: std::env::consts::ARCH.to_string(),
    };
    let mut collector = Collector::new(
        file,
        component,
        generated_at,
        BundlePseudonymizer::new(pseudonym_key),
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
        "runtime/sessions",
        NoticeKind::Omitted,
        NoticeCode::SensitiveRuntimeExcluded,
    )?;
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

    collect_logs(&mut collector)?;
    collect_json_config(
        &mut collector,
        Path::new("/etc/arcen/pier.json"),
        "config/pier.json",
    )?;
    collect_regular_file(
        &mut collector,
        Path::new("/etc/arcen/xorg.conf"),
        "config/xorg.conf",
        BundleSource::Configuration,
    )?;
    collect_system_diagnostics(&mut collector)?;
    collect_service_diagnostics(&mut collector)?;
    collect_effective_config(&mut collector)?;
    collect_runtime_diagnostics(&mut collector)?;
    collect_lifecycle_events(&mut collector)?;
    collector.finish()
}

struct Collector {
    zip: ZipWriter<File>,
    manifest: SupportBundleManifestBuilder,
    pseudonymizer: BundlePseudonymizer,
    total_bytes: u64,
    omission_count: usize,
    command_count: usize,
}

impl Collector {
    fn new(
        file: File,
        component: BundleComponent,
        generated_at: u64,
        pseudonymizer: BundlePseudonymizer,
    ) -> Self {
        Self {
            zip: ZipWriter::new(file),
            manifest: SupportBundleManifestBuilder::new(component, generated_at),
            pseudonymizer,
            total_bytes: 0,
            omission_count: 0,
            command_count: 0,
        }
    }

    fn notice(&mut self, source: &str, kind: NoticeKind, code: NoticeCode) -> Result<(), String> {
        if !matches!(kind, NoticeKind::Advisory | NoticeKind::Truncated) {
            self.omission_count += 1;
        }
        self.manifest
            .add_notice(BundleNotice {
                source: BundlePath::new(source).map_err(contract_error)?,
                kind,
                code,
            })
            .map_err(contract_error)
    }

    fn redaction(&mut self, record: RedactionRecord) -> Result<(), String> {
        self.manifest.add_redaction(record).map_err(contract_error)
    }

    fn add_bytes(
        &mut self,
        archive_path: &str,
        source: BundleSource,
        bytes: &[u8],
    ) -> Result<bool, String> {
        let path = BundlePath::new(archive_path).map_err(contract_error)?;
        if bytes.len() as u64 > MAX_TOTAL_BYTES.saturating_sub(self.total_bytes) {
            self.notice(
                path.as_str(),
                NoticeKind::Omitted,
                NoticeCode::TotalPayloadLimit,
            )?;
            return Ok(false);
        }
        self.zip
            .start_file(path.as_str(), zip_options())
            .map_err(zip_error)?;
        let mut digest = Sha256::new();
        for chunk in bytes.chunks(COPY_BUFFER_BYTES) {
            self.zip.write_all(chunk).map_err(io_error)?;
            digest.update(chunk);
        }
        let size = bytes.len() as u64;
        self.total_bytes += size;
        self.manifest
            .add_entry(BundleEntry {
                path,
                source,
                original_size_bytes: size,
                included_size_bytes: size,
                sha256: Sha256Digest::from_bytes(digest.finalize().into()),
                truncation: None,
            })
            .map_err(contract_error)?;
        Ok(true)
    }

    fn add_file(
        &mut self,
        file_path: &Path,
        archive_path: &str,
        source: BundleSource,
        original_size: u64,
        included_size: u64,
        reason: Option<TruncationReason>,
    ) -> Result<bool, String> {
        if included_size == 0 {
            return Ok(false);
        }
        let path = BundlePath::new(archive_path).map_err(contract_error)?;
        let metadata = match std::fs::symlink_metadata(file_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.file_notice(path.as_str(), &error)?;
                return Ok(false);
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            self.notice(
                path.as_str(),
                NoticeKind::Invalid,
                NoticeCode::UnsafeFileType,
            )?;
            return Ok(false);
        }
        let mut file = match File::open(file_path) {
            Ok(file) => file,
            Err(error) => {
                self.file_notice(path.as_str(), &error)?;
                return Ok(false);
            }
        };
        let opened = file.metadata().map_err(io_error)?;
        if !opened.is_file() {
            self.notice(
                path.as_str(),
                NoticeKind::Invalid,
                NoticeCode::UnsafeFileType,
            )?;
            return Ok(false);
        }
        let offset = original_size.saturating_sub(included_size);
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        self.zip
            .start_file(path.as_str(), zip_options())
            .map_err(zip_error)?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        let mut remaining = included_size;
        let mut written = 0_u64;
        while remaining != 0 {
            let count =
                match file.read(&mut buffer[..remaining.min(COPY_BUFFER_BYTES as u64) as usize]) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) => {
                        self.zip.abort_file().map_err(zip_error)?;
                        self.file_notice(path.as_str(), &error)?;
                        return Ok(false);
                    }
                };
            self.zip.write_all(&buffer[..count]).map_err(io_error)?;
            digest.update(&buffer[..count]);
            written += count as u64;
            remaining -= count as u64;
        }
        let after = file.metadata().map_err(io_error)?;
        let changed = after.len() != opened.len()
            || after.modified().ok() != opened.modified().ok()
            || written != included_size;
        let truncation = if changed {
            self.notice(
                path.as_str(),
                NoticeKind::Truncated,
                NoticeCode::SourceChangedDuringRead,
            )?;
            Some(BundleTruncation {
                original_offset: offset,
                reason: TruncationReason::ChangedDuringRead,
            })
        } else {
            reason.map(|reason| BundleTruncation {
                original_offset: offset,
                reason,
            })
        };
        self.total_bytes += written;
        self.manifest
            .add_entry(BundleEntry {
                path,
                source,
                original_size_bytes: original_size,
                included_size_bytes: written,
                sha256: Sha256Digest::from_bytes(digest.finalize().into()),
                truncation,
            })
            .map_err(contract_error)?;
        Ok(true)
    }

    fn add_canonical_log_file(
        &mut self,
        file_path: &Path,
        archive_path: &str,
        original_size: u64,
        input_limit: u64,
        output_limit: u64,
        budget_reason: TruncationReason,
    ) -> Result<u64, String> {
        if input_limit == 0 || output_limit == 0 {
            return Ok(0);
        }
        let path = BundlePath::new(archive_path).map_err(contract_error)?;
        let metadata = match std::fs::symlink_metadata(file_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.file_notice(path.as_str(), &error)?;
                return Ok(0);
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            self.notice(
                path.as_str(),
                NoticeKind::Invalid,
                NoticeCode::UnsafeFileType,
            )?;
            return Ok(0);
        }
        let mut file = match File::open(file_path) {
            Ok(file) => file,
            Err(error) => {
                self.file_notice(path.as_str(), &error)?;
                return Ok(0);
            }
        };
        let opened = file.metadata().map_err(io_error)?;
        if !opened.is_file() {
            self.notice(
                path.as_str(),
                NoticeKind::Invalid,
                NoticeCode::UnsafeFileType,
            )?;
            return Ok(0);
        }
        let effective_input_limit = input_limit.min(opened.len());
        let offset = opened.len().saturating_sub(effective_input_limit);
        let discard_initial_fragment = starts_mid_line(&mut file, offset)?;
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        self.zip
            .start_file(path.as_str(), zip_options())
            .map_err(zip_error)?;
        let mut writer = DigestingZipWriter::new(&mut self.zip);
        let report = transform_canonical_jsonl(
            Read::by_ref(&mut file),
            &mut writer,
            &self.pseudonymizer,
            CanonicalJsonlTransformLimits {
                max_input_bytes: effective_input_limit,
                max_output_bytes: output_limit,
                discard_initial_fragment,
            },
        )
        .map_err(|error| format!("transform canonical support-bundle log: {error}"))?;
        let (written, digest) = writer.finish();
        if written == 0 {
            self.zip.abort_file().map_err(zip_error)?;
        }

        let after = file.metadata().map_err(io_error)?;
        let changed = opened.len() != original_size
            || after.len() != opened.len()
            || after.modified().ok() != opened.modified().ok();
        self.record_log_transform(path.as_str(), &report)?;
        if report.output_limit_reached {
            self.notice(
                path.as_str(),
                NoticeKind::Truncated,
                match budget_reason {
                    TruncationReason::GlobalLimit => NoticeCode::TotalPayloadLimit,
                    TruncationReason::PerSourceLimit | TruncationReason::ChangedDuringRead => {
                        NoticeCode::LogPayloadLimit
                    }
                },
            )?;
        }
        let truncation = if changed {
            self.notice(
                path.as_str(),
                NoticeKind::Truncated,
                NoticeCode::SourceChangedDuringRead,
            )?;
            Some(BundleTruncation {
                original_offset: offset,
                reason: TruncationReason::ChangedDuringRead,
            })
        } else if offset != 0 || report.output_limit_reached {
            Some(BundleTruncation {
                original_offset: offset,
                reason: budget_reason,
            })
        } else {
            None
        };
        if written == 0 {
            return Ok(0);
        }
        self.total_bytes += written;
        self.manifest
            .add_entry(BundleEntry {
                path: path.clone(),
                source: BundleSource::Log,
                original_size_bytes: original_size,
                included_size_bytes: written,
                sha256: Sha256Digest::from_bytes(digest),
                truncation,
            })
            .map_err(contract_error)?;
        for kind in report.redacted_kinds {
            self.redaction(RedactionRecord {
                entry_path: path.clone(),
                key_path: kind.canonical_key_path().to_string(),
                reason: RedactionReason::IdentityPseudonymized,
            })?;
        }
        Ok(written)
    }

    fn record_log_transform(
        &mut self,
        source: &str,
        report: &CanonicalJsonlTransformReport,
    ) -> Result<(), String> {
        if report.invalid_lines != 0 {
            self.notice(
                source,
                NoticeKind::Invalid,
                NoticeCode::CanonicalLogRecordInvalid,
            )?;
        }
        if report.oversized_lines != 0 {
            self.notice(
                source,
                NoticeKind::Invalid,
                NoticeCode::CanonicalLogRecordTooLarge,
            )?;
        }
        if report.incomplete_lines != 0 {
            self.notice(
                source,
                NoticeKind::Invalid,
                NoticeCode::CanonicalLogRecordIncomplete,
            )?;
        }
        Ok(())
    }

    fn file_notice(&mut self, source: &str, error: &std::io::Error) -> Result<(), String> {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            self.notice(
                source,
                NoticeKind::PermissionDenied,
                NoticeCode::SourcePermissionDenied,
            )
        } else if error.kind() == std::io::ErrorKind::NotFound {
            self.notice(source, NoticeKind::Unavailable, NoticeCode::SourceNotFound)
        } else {
            self.notice(
                source,
                NoticeKind::Unavailable,
                NoticeCode::SourceUnavailable,
            )
        }
    }

    fn command(&mut self, program: &str, arguments: &[&str]) -> CommandOutcome {
        if self.command_count >= MAX_COMMANDS {
            return CommandOutcome::unavailable();
        }
        self.command_count += 1;
        run_bounded_command(program, arguments)
    }

    fn finish(mut self) -> Result<(File, usize), String> {
        let manifest = self.manifest.finish();
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("serialize support-bundle manifest: {error}"))?;
        self.zip
            .start_file("manifest.json", zip_options())
            .map_err(zip_error)?;
        for chunk in bytes.chunks(COPY_BUFFER_BYTES) {
            self.zip.write_all(chunk).map_err(io_error)?;
        }
        let file = self.zip.finish().map_err(zip_error)?;
        if file.metadata().map_err(io_error)?.len() > MAX_TOTAL_BYTES {
            return Err("support-bundle archive exceeded the 256 MiB limit".to_string());
        }
        Ok((file, self.omission_count))
    }
}

fn collect_logs(collector: &mut Collector) -> Result<(), String> {
    let (logs, candidates_truncated, legacy) = match crate::logging::support_bundle_logs() {
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
    let mut remaining_logs = MAX_LOG_BYTES;
    for log in logs {
        if remaining_logs == 0 {
            collector.notice("logs", NoticeKind::Truncated, NoticeCode::LogPayloadLimit)?;
            break;
        }
        let total_remaining = MAX_TOTAL_BYTES.saturating_sub(collector.total_bytes);
        if total_remaining == 0 {
            collector.notice("logs", NoticeKind::Truncated, NoticeCode::TotalPayloadLimit)?;
            break;
        }
        let output_limit = remaining_logs.min(total_remaining);
        let input_limit = log.size_bytes.min(output_limit);
        let budget_reason = if output_limit == total_remaining {
            TruncationReason::GlobalLimit
        } else {
            TruncationReason::PerSourceLimit
        };
        let written = collector.add_canonical_log_file(
            &log.path,
            &log.archive_path,
            log.size_bytes,
            input_limit,
            output_limit,
            budget_reason,
        )?;
        remaining_logs = remaining_logs.saturating_sub(written);
    }
    Ok(())
}

fn collect_json_config(
    collector: &mut Collector,
    source_path: &Path,
    archive_path: &str,
) -> Result<(), String> {
    let archive = BundlePath::new(archive_path).map_err(contract_error)?;
    let Some(mut document) =
        read_json_document(source_path, MAX_DOCUMENT_BYTES, collector, &archive)?
    else {
        return Ok(());
    };
    let redactions = redact_json_document_at(&archive, &mut document).map_err(contract_error)?;
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize redacted Linux config: {error}"))?;
    if collector.add_bytes(archive.as_str(), BundleSource::Configuration, &bytes)? {
        for redaction in redactions {
            collector.redaction(redaction)?;
        }
    }
    Ok(())
}

fn collect_regular_file(
    collector: &mut Collector,
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
    if metadata.len() > MAX_DOCUMENT_BYTES {
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

fn collect_system_diagnostics(collector: &mut Collector) -> Result<(), String> {
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

fn collect_service_diagnostics(collector: &mut Collector) -> Result<(), String> {
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

fn collect_effective_config(collector: &mut Collector) -> Result<(), String> {
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
        let entry_path =
            BundlePath::new("diagnostics/effective-config.json").map_err(contract_error)?;
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

fn collect_runtime_diagnostics(collector: &mut Collector) -> Result<(), String> {
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

fn collect_lifecycle_events(collector: &mut Collector) -> Result<(), String> {
    match crate::eventlog::query_recent_events(crate::eventlog::EventExcerptLimits::default()) {
        Ok(excerpt) => {
            let _ = (excerpt.record_count, excerpt.media_type);
            let (bytes, identity_redacted, invalid_omitted) = match excerpt.source {
                crate::eventlog::NativeEventQuerySource::Journal => {
                    pseudonymize_native_journal(&excerpt.bytes, &collector.pseudonymizer)
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
                    entry_path: BundlePath::new(excerpt.suggested_name).map_err(contract_error)?,
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

fn read_json_document(
    path: &Path,
    limit: u64,
    collector: &mut Collector,
    logical_path: &BundlePath,
) -> Result<Option<serde_json::Value>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            collector.file_notice(logical_path.as_str(), &error)?;
            return Ok(None);
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        collector.notice(
            logical_path.as_str(),
            NoticeKind::Invalid,
            NoticeCode::UnsafeFileType,
        )?;
        return Ok(None);
    }
    if metadata.len() > limit {
        collector.notice(
            logical_path.as_str(),
            NoticeKind::Invalid,
            NoticeCode::SourceTooLarge,
        )?;
        return Ok(None);
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            collector.file_notice(logical_path.as_str(), &error)?;
            return Ok(None);
        }
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let count = file.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > limit as usize {
            collector.notice(
                logical_path.as_str(),
                NoticeKind::Invalid,
                NoticeCode::SourceTooLarge,
            )?;
            return Ok(None);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    match serde_json::from_slice(&bytes) {
        Ok(document) => Ok(Some(document)),
        Err(_) => {
            collector.notice(
                logical_path.as_str(),
                NoticeKind::Invalid,
                NoticeCode::SourceInvalid,
            )?;
            Ok(None)
        }
    }
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

fn zip_options() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600)
}

fn starts_mid_line(file: &mut File, offset: u64) -> Result<bool, String> {
    if offset == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(offset - 1)).map_err(io_error)?;
    let mut previous = [0_u8; 1];
    file.read_exact(&mut previous).map_err(io_error)?;
    Ok(previous[0] != b'\n')
}

struct DigestingZipWriter<'a> {
    zip: &'a mut ZipWriter<File>,
    digest: Sha256,
    written: u64,
}

impl<'a> DigestingZipWriter<'a> {
    fn new(zip: &'a mut ZipWriter<File>) -> Self {
        Self {
            zip,
            digest: Sha256::new(),
            written: 0,
        }
    }

    fn finish(self) -> (u64, [u8; 32]) {
        (self.written, self.digest.finalize().into())
    }
}

impl Write for DigestingZipWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.zip.write(bytes)?;
        self.digest.update(&bytes[..count]);
        self.written += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.zip.flush()
    }
}

fn contract_error(error: impl std::fmt::Display) -> String {
    format!("support-bundle contract: {error}")
}

fn zip_error(error: zip::result::ZipError) -> String {
    format!("write support-bundle ZIP: {error}")
}

fn io_error(error: std::io::Error) -> String {
    format!("write support-bundle payload: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_is_strict_and_service_independent() {
        assert_eq!(
            parse_options(&[]).expect("defaults"),
            SupportBundleOptions {
                output_directory: None
            }
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

    #[test]
    fn fixture_zip_hashes_payload_and_excludes_manifest_from_index() {
        let root =
            std::env::temp_dir().join(format!("arcen-linux-bundle-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("test root");
        let path = root.join("fixture.zip");
        let file = File::create(&path).expect("fixture archive");
        let component = BundleComponent {
            name: "arcen-pier-linux".to_string(),
            version: "test".to_string(),
            os: "linux".to_string(),
            arch: "test".to_string(),
        };
        let mut collector = Collector::new(
            file,
            component,
            42,
            BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x11; 32])),
        );
        collector
            .add_bytes(
                "diagnostics/fixture.txt",
                BundleSource::Diagnostics,
                b"fixture",
            )
            .expect("payload");
        let (file, _) = collector.finish().expect("archive");
        drop(file);
        let mut archive = zip::ZipArchive::new(File::open(&path).expect("open")).expect("ZIP");
        assert!(archive.by_name("diagnostics/fixture.txt").is_ok());
        let manifest: serde_json::Value =
            serde_json::from_reader(archive.by_name("manifest.json").expect("manifest"))
                .expect("manifest JSON");
        assert_eq!(manifest["entries"].as_array().expect("entries").len(), 1);
        assert_eq!(manifest["entries"][0]["path"], "diagnostics/fixture.txt");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_zip_is_byte_for_byte_deterministic() {
        let root =
            std::env::temp_dir().join(format!("arcen-linux-bundle-stable-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("test root");
        let component = BundleComponent {
            name: "arcen-pier-linux".to_string(),
            version: "test".to_string(),
            os: "linux".to_string(),
            arch: "test".to_string(),
        };
        let mut outputs = Vec::new();
        for index in 0..2 {
            let path = root.join(format!("fixture-{index}.zip"));
            let file = File::create(&path).expect("fixture archive");
            let mut collector = Collector::new(
                file,
                component.clone(),
                1_700_000_000,
                BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x11; 32])),
            );
            collector
                .add_bytes("diagnostics/a.txt", BundleSource::Diagnostics, b"alpha")
                .expect("payload");
            let (file, _) = collector.finish().expect("archive");
            drop(file);
            outputs.push(std::fs::read(path).expect("archive bytes"));
        }
        assert_eq!(outputs[0], outputs[1]);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn log_exports_hide_identity_and_correlate_across_archive_entries() {
        let root = std::env::current_dir()
            .expect("current directory")
            .join("target/arcen-linux-bundle-redaction-test");
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).expect("test root");
        let raw = serde_json::json!({
            "schema_version": 1,
            "timestamp": "2026-07-24T16:00:00.000000Z",
            "sequence": 1,
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
        });
        let mut line = serde_json::to_vec(&raw).expect("fixture");
        line.push(b'\n');
        let first = root.join("first.jsonl");
        let second = root.join("second.jsonl");
        std::fs::write(&first, &line).expect("first log");
        std::fs::write(&second, &line).expect("second log");
        let archive_path = root.join("bundle.zip");
        let component = BundleComponent {
            name: "arcen-pier-linux".to_string(),
            version: "test".to_string(),
            os: "linux".to_string(),
            arch: "test".to_string(),
        };
        let mut collector = Collector::new(
            File::create(&archive_path).expect("archive"),
            component,
            42,
            BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x55; 32])),
        );
        for (source, destination) in [(&first, "logs/first.jsonl"), (&second, "logs/second.jsonl")]
        {
            assert_ne!(
                collector
                    .add_canonical_log_file(
                        source,
                        destination,
                        line.len() as u64,
                        line.len() as u64,
                        1024 * 1024,
                        TruncationReason::PerSourceLimit,
                    )
                    .expect("pseudonymized log"),
                0
            );
        }
        let (file, _) = collector.finish().expect("finish archive");
        drop(file);

        let mut archive =
            zip::ZipArchive::new(File::open(&archive_path).expect("open archive")).expect("ZIP");
        let mut first_export = String::new();
        archive
            .by_name("logs/first.jsonl")
            .expect("first export")
            .read_to_string(&mut first_export)
            .expect("read first");
        let mut second_export = String::new();
        archive
            .by_name("logs/second.jsonl")
            .expect("second export")
            .read_to_string(&mut second_export)
            .expect("read second");
        let mut manifest = String::new();
        archive
            .by_name("manifest.json")
            .expect("manifest")
            .read_to_string(&mut manifest)
            .expect("read manifest");
        let combined = format!("{first_export}{second_export}{manifest}");
        for identity in ["raw-artist", "raw-pier", "192.0.2.77:4444", "raw-network"] {
            assert!(!combined.contains(identity));
        }
        let first_value: serde_json::Value =
            serde_json::from_str(&first_export).expect("first JSON");
        let second_value: serde_json::Value =
            serde_json::from_str(&second_export).expect("second JSON");
        assert_eq!(first_value["user"], second_value["user"]);
        assert_eq!(
            first_value["fields"]["ssid"],
            second_value["fields"]["ssid"]
        );
        assert!(manifest.contains("identity_pseudonymized"));
        let manifest_value: serde_json::Value =
            serde_json::from_str(&manifest).expect("manifest JSON");
        let first_entry = manifest_value["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|entry| entry["path"] == "logs/first.jsonl")
            .expect("first manifest entry");
        assert_eq!(
            first_entry["included_size_bytes"],
            first_export.len() as u64
        );
        assert_eq!(
            first_entry["sha256"],
            format!("{:x}", Sha256::digest(first_export.as_bytes()))
        );
        std::fs::remove_dir_all(root).ok();
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
