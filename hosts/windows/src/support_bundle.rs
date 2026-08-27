use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_telemetry::{
    redact_json_document_at, transform_canonical_jsonl, BundleComponent, BundleEntry, BundleNotice,
    BundlePath, BundlePseudonymKey, BundlePseudonymizer, BundleSource, BundleTruncation,
    CanonicalJsonlTransformLimits, CanonicalJsonlTransformReport, NoticeCode, NoticeKind,
    RedactionReason, RedactionRecord, Sha256Digest, SupportBundleManifestBuilder, TruncationReason,
    MAX_REDACTION_RECORDS, REDACTED_VALUE,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_SHARE_READ};
use windows::Win32::System::SystemInformation::{GetTickCount64, OSVERSIONINFOW};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_LOG_BYTES: u64 = 200 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 1024 * 1024;
const COLLISION_LIMIT: u16 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupportBundleOptions {
    pub(crate) output_directory: Option<PathBuf>,
}

pub(crate) fn parse_options(arguments: &[String]) -> Result<SupportBundleOptions, String> {
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
        match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&partial_path)
        {
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

fn build_archive(file: File, generated_at: u64) -> Result<(File, usize), String> {
    let mut pseudonym_key = BundlePseudonymKey::zeroed();
    getrandom::getrandom(pseudonym_key.entropy_buffer())
        .map_err(|_| "generate support-bundle pseudonymization key".to_string())?;
    let component = BundleComponent {
        name: "arcen-pier-windows".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: "windows".to_string(),
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

    collect_logs(&mut collector)?;
    collect_config(&mut collector)?;
    collect_recovery(&mut collector)?;
    collect_timezone_recovery(&mut collector)?;
    collect_diagnostics(&mut collector)?;
    collect_lifecycle_events(&mut collector)?;
    collector.finish()
}

struct Collector {
    zip: ZipWriter<File>,
    manifest: SupportBundleManifestBuilder,
    pseudonymizer: BundlePseudonymizer,
    total_bytes: u64,
    omission_count: usize,
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
        let archive_path = BundlePath::new(archive_path).map_err(contract_error)?;
        if bytes.len() as u64 > MAX_TOTAL_BYTES.saturating_sub(self.total_bytes) {
            self.notice(
                archive_path.as_str(),
                NoticeKind::Omitted,
                NoticeCode::TotalPayloadLimit,
            )?;
            return Ok(false);
        }
        self.zip
            .start_file(archive_path.as_str(), zip_options())
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
                path: archive_path,
                source,
                original_size_bytes: size,
                included_size_bytes: size,
                sha256: Sha256Digest::from_bytes(digest.finalize().into()),
                truncation: None,
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
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
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
        if !opened.is_file() || opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
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
    let (logs, candidates_truncated) = match crate::log_maintenance::support_bundle_logs() {
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

fn collect_config(collector: &mut Collector) -> Result<(), String> {
    let archive_path = BundlePath::new("config/pier.json").map_err(contract_error)?;
    let Some(mut document) = read_json_document(
        &crate::paths::config_path(),
        MAX_DOCUMENT_BYTES,
        collector,
        &archive_path,
    )?
    else {
        return Ok(());
    };
    let mut redactions = redact_json_document_at(&archive_path, &mut document)
        .map_err(contract_error)?
        .into_iter()
        .map(|mut redaction| {
            if redaction.key_path.eq_ignore_ascii_case("/tls/key")
                || redaction.key_path.eq_ignore_ascii_case("/tls/private_key")
            {
                redaction.reason = RedactionReason::PrivateKeyPolicy;
            }
            redaction
        })
        .collect::<Vec<_>>();
    redact_tls_config_metadata(&archive_path, &mut document, &mut redactions)?;
    redactions.sort_by(|left, right| left.key_path.cmp(&right.key_path));
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serialize redacted Pier config: {error}"))?;
    if collector.add_bytes(archive_path.as_str(), BundleSource::Configuration, &bytes)? {
        for redaction in redactions {
            collector.redaction(redaction)?;
        }
    }
    Ok(())
}

fn redact_tls_config_metadata(
    archive_path: &BundlePath,
    document: &mut serde_json::Value,
    redactions: &mut Vec<RedactionRecord>,
) -> Result<(), String> {
    let Some(tls) = document
        .get_mut("tls")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(());
    };
    for key in ["cert", "certificate", "expected_sans"] {
        let Some((actual_key, value)) = tls
            .iter_mut()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        else {
            continue;
        };
        if redactions.len() >= MAX_REDACTION_RECORDS {
            return Err("Pier config contains too many redacted settings".to_string());
        }
        *value = serde_json::Value::String(REDACTED_VALUE.to_string());
        redactions.push(RedactionRecord {
            entry_path: archive_path.clone(),
            key_path: format!("/tls/{actual_key}"),
            reason: RedactionReason::SensitiveKey,
        });
    }
    Ok(())
}

fn collect_recovery(collector: &mut Collector) -> Result<(), String> {
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
    if metadata.len() > MAX_DOCUMENT_BYTES {
        collector.notice(
            archive_path,
            NoticeKind::Invalid,
            NoticeCode::SourceTooLarge,
        )?;
        return Ok(());
    }
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
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

fn collect_timezone_recovery(collector: &mut Collector) -> Result<(), String> {
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
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
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

fn collect_diagnostics(collector: &mut Collector) -> Result<(), String> {
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

fn collect_lifecycle_events(collector: &mut Collector) -> Result<(), String> {
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
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
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
    fn support_bundle_parser_is_strict() {
        assert_eq!(
            parse_options(&[]).expect("defaults"),
            SupportBundleOptions {
                output_directory: None
            }
        );
        assert_eq!(
            parse_options(&["--out".to_string(), "D:\\support".to_string()])
                .expect("explicit output")
                .output_directory,
            Some(PathBuf::from("D:\\support"))
        );
        assert!(parse_options(&["--out".to_string()]).is_err());
        assert!(parse_options(&["--unknown".to_string()]).is_err());
    }

    #[test]
    fn filenames_never_contain_hostname() {
        let root =
            std::env::temp_dir().join(format!("arcen-support-name-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("test root");
        let (final_path, partial_path, file) =
            create_output_file(&root, 1_700_000_000, 42).expect("output");
        drop(file);
        assert_eq!(
            final_path.file_name().and_then(|name| name.to_str()),
            Some("arcen-support-1700000000-42.zip")
        );
        let _ = std::fs::remove_file(partial_path);
        let _ = std::fs::remove_dir(root);
    }

    #[test]
    fn tls_key_aliases_are_redacted_without_opening_material_paths() {
        let archive_path = BundlePath::new("config/pier.json").expect("path");
        let mut document = serde_json::json!({
            "tls": {
                "certificate": "C:\\sensitive\\host.crt",
                "private_key": "C:\\sensitive\\host.key",
                "minimum_version": "TLS1.3",
                "expected_sans": ["host.example"]
            }
        });
        let mut redactions =
            redact_json_document_at(&archive_path, &mut document).expect("redactions");
        redact_tls_config_metadata(&archive_path, &mut document, &mut redactions)
            .expect("TLS redactions");
        assert_eq!(
            document["tls"]["private_key"],
            arcen_telemetry::REDACTED_VALUE
        );
        assert_eq!(
            document["tls"]["certificate"],
            arcen_telemetry::REDACTED_VALUE
        );
        assert_eq!(
            document["tls"]["expected_sans"],
            arcen_telemetry::REDACTED_VALUE
        );
        assert_eq!(document["tls"]["minimum_version"], "TLS1.3");
        assert_eq!(redactions.len(), 3);
        assert!(redactions
            .iter()
            .any(|redaction| redaction.key_path == "/tls/private_key"));
    }

    #[test]
    fn log_exports_hide_identity_and_correlate_across_archive_entries() {
        let root = std::env::current_dir()
            .expect("current directory")
            .join("target/arcen-windows-bundle-redaction-test");
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
            "platform": "windows",
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
            name: "arcen-pier-windows".to_string(),
            version: "test".to_string(),
            os: "windows".to_string(),
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
}
