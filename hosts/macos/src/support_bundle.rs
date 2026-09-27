use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_telemetry::{
    BundleComponent, BundleEntry, BundleNotice, BundlePath, BundlePseudonymKey,
    BundlePseudonymizer, BundleSource, CanonicalJsonlTransformLimits, NoticeCode, NoticeKind,
    Sha256Digest, SupportBundleManifestBuilder, TruncationReason, redact_json_document_at,
    transform_canonical_jsonl,
};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const DEFAULT_OUTPUT_DIRECTORY: &str = "/Library/Logs/Arcen/Pier/support";
const DEFAULT_LOG_DIRECTORY: &str = "/Library/Logs/Arcen/Pier";
const LOG_ENTRY_PATH: &str = "logs/arcen-pier-macos.jsonl";
const MAX_LOG_INPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LOG_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
const COLLISION_LIMIT: u16 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportBundleOptions {
    pub output_directory: Option<PathBuf>,
}

/// Parses support-bundle-specific arguments.
///
/// # Errors
///
/// Returns an error for unknown arguments, duplicate `--out`, or a missing
/// output directory.
pub fn parse_options(arguments: &[String]) -> Result<SupportBundleOptions, String> {
    let mut output_directory = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--out" => {
                if output_directory.is_some() {
                    return Err("--out may be supplied only once".to_owned());
                }
                index += 1;
                output_directory = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .ok_or_else(|| "--out requires a directory".to_owned())?,
                ));
            }
            "-h" | "--help" => {
                return Err("USAGE:\n  arcen-pier-macos support-bundle [--out <DIR>]".to_owned());
            }
            other => return Err(format!("unknown support-bundle argument: {other}")),
        }
        index += 1;
    }
    Ok(SupportBundleOptions { output_directory })
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
    set_private_directory_permissions(directory)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock precedes Unix epoch".to_owned())?
        .as_secs();
    let (path, partial_path, file) = create_output_file(directory, timestamp)?;
    let result = build(file, config, startup, timestamp);
    match result {
        Ok(file) => {
            file.sync_all().map_err(|error| {
                format!("sync support bundle {}: {error}", partial_path.display())
            })?;
            drop(file);
            std::fs::rename(&partial_path, &path).map_err(|error| {
                let _ = std::fs::remove_file(&partial_path);
                format!("publish support bundle {}: {error}", path.display())
            })?;
            Ok(path)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&partial_path);
            Err(error)
        }
    }
}

fn create_output_file(
    directory: &Path,
    timestamp: u64,
) -> Result<(PathBuf, PathBuf, File), String> {
    for suffix in 0..COLLISION_LIMIT {
        let suffix = if suffix == 0 {
            String::new()
        } else {
            format!("-{suffix}")
        };
        let path = directory.join(format!("arcen-pier-macos-support-{timestamp}{suffix}.zip"));
        if path.exists() {
            continue;
        }
        let partial_path = directory.join(format!(
            ".arcen-pier-macos-support-{timestamp}{suffix}.zip.partial"
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_private_file_permissions(&mut options);
        match options.open(&partial_path) {
            Ok(file) => return Ok((path, partial_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!("create support bundle {}: {error}", path.display()));
            }
        }
    }
    Err("support-bundle output collision limit reached".to_owned())
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

#[cfg(unix)]
fn set_private_file_permissions(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file_permissions(_options: &mut OpenOptions) {}

fn build(
    file: File,
    config: &crate::PierFileConfig,
    startup: &crate::StartupConfig,
    timestamp: u64,
) -> Result<File, String> {
    let component = BundleComponent {
        name: "arcen-pier-macos".to_owned(),
        version: crate::VERSION.to_owned(),
        os: "macos".to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    };
    let mut manifest = SupportBundleManifestBuilder::new(component, timestamp);
    let mut zip = ZipWriter::new(file);
    let mut add_json =
        |path: &str, source: BundleSource, value: serde_json::Value| -> Result<(), String> {
            let bundle_path = BundlePath::new(path).map_err(|error| error.to_string())?;
            let mut value = value;
            let redactions = redact_json_document_at(&bundle_path, &mut value)
                .map_err(|error| error.to_string())?;
            let bytes = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
            zip.start_file(
                bundle_path.as_str(),
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .map_err(|error| error.to_string())?;
            zip.write_all(&bytes).map_err(|error| error.to_string())?;
            let digest = Sha256::digest(&bytes);
            manifest
                .add_entry(BundleEntry {
                    path: bundle_path.clone(),
                    source,
                    original_size_bytes: bytes.len() as u64,
                    included_size_bytes: bytes.len() as u64,
                    sha256: Sha256Digest::from_bytes(digest.into()),
                    truncation: None,
                })
                .map_err(|error| error.to_string())?;
            for redaction in redactions {
                manifest
                    .add_redaction(redaction)
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        };
    let config_value = serde_json::json!({
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
    });
    add_json(
        "config/pier.json",
        BundleSource::Configuration,
        config_value,
    )?;
    let report = crate::DiagnosticsReport::collect(config, startup)?;
    add_json(
        "diagnostics/report.json",
        BundleSource::Diagnostics,
        serde_json::to_value(report).map_err(|error| error.to_string())?,
    )?;
    let log_directory = std::env::var_os("ARCEN_LOG_DIR")
        .map_or_else(|| PathBuf::from(DEFAULT_LOG_DIRECTORY), PathBuf::from);
    add_managed_log(&mut zip, &mut manifest, &log_directory)?;
    manifest
        .add_notice(BundleNotice {
            source: BundlePath::new("config/tls/key").map_err(|error| error.to_string())?,
            kind: NoticeKind::Omitted,
            code: NoticeCode::PrivateKeyExcluded,
        })
        .map_err(|error| error.to_string())?;
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest.finish()).map_err(|error| error.to_string())?;
    zip.start_file(
        "manifest.json",
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )
    .map_err(|error| error.to_string())?;
    zip.write_all(&manifest_bytes)
        .map_err(|error| error.to_string())?;
    zip.finish().map_err(|error| error.to_string())
}

#[allow(clippy::too_many_lines)]
fn add_managed_log(
    zip: &mut ZipWriter<impl Write + Seek>,
    manifest: &mut SupportBundleManifestBuilder,
    directory: &Path,
) -> Result<(), String> {
    let path = directory.join("arcen-pier-macos.jsonl");
    let source = BundlePath::new(LOG_ENTRY_PATH).map_err(|error| error.to_string())?;
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            manifest
                .add_notice(BundleNotice {
                    source,
                    kind: NoticeKind::Unavailable,
                    code: NoticeCode::SourceNotFound,
                })
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        Err(_) => {
            manifest
                .add_notice(BundleNotice {
                    source,
                    kind: NoticeKind::PermissionDenied,
                    code: NoticeCode::SourcePermissionDenied,
                })
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
    };
    let original_size = file.metadata().map_or(0, |metadata| metadata.len());
    let mut key = BundlePseudonymKey::zeroed();
    getrandom::getrandom(key.entropy_buffer())
        .map_err(|error| format!("generate support-bundle pseudonym key: {error}"))?;
    let pseudonymizer = BundlePseudonymizer::new(key);
    let mut transformed = Vec::new();
    let report = transform_canonical_jsonl(
        file.take(MAX_LOG_INPUT_BYTES),
        &mut transformed,
        &pseudonymizer,
        CanonicalJsonlTransformLimits {
            max_input_bytes: MAX_LOG_INPUT_BYTES,
            max_output_bytes: MAX_LOG_OUTPUT_BYTES,
            discard_initial_fragment: false,
        },
    )
    .map_err(|error| format!("transform managed log: {error}"))?;
    if report.output_bytes != 0 {
        zip.start_file(
            LOG_ENTRY_PATH,
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .map_err(|error| error.to_string())?;
        zip.write_all(&transformed)
            .map_err(|error| error.to_string())?;
        manifest
            .add_entry(BundleEntry {
                path: source.clone(),
                source: BundleSource::Log,
                original_size_bytes: original_size,
                included_size_bytes: transformed.len() as u64,
                sha256: Sha256Digest::from_bytes(Sha256::digest(&transformed).into()),
                truncation: (report.output_limit_reached || original_size > MAX_LOG_INPUT_BYTES)
                    .then_some(arcen_telemetry::BundleTruncation {
                        original_offset: 0,
                        reason: TruncationReason::PerSourceLimit,
                    }),
            })
            .map_err(|error| error.to_string())?;
    }
    if report.output_limit_reached || original_size > MAX_LOG_INPUT_BYTES {
        manifest
            .add_notice(BundleNotice {
                source: source.clone(),
                kind: NoticeKind::Truncated,
                code: NoticeCode::LogPayloadLimit,
            })
            .map_err(|error| error.to_string())?;
    }
    if report.invalid_lines != 0 || report.oversized_lines != 0 {
        manifest
            .add_notice(BundleNotice {
                source: source.clone(),
                kind: NoticeKind::Invalid,
                code: if report.oversized_lines != 0 {
                    NoticeCode::CanonicalLogRecordTooLarge
                } else {
                    NoticeCode::CanonicalLogRecordInvalid
                },
            })
            .map_err(|error| error.to_string())?;
    }
    if report.incomplete_lines != 0 {
        manifest
            .add_notice(BundleNotice {
                source,
                kind: NoticeKind::Invalid,
                code: NoticeCode::CanonicalLogRecordIncomplete,
            })
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    use arcen_telemetry::{BundleComponent, SupportBundleManifestBuilder};

    #[test]
    fn parses_bounded_output_option() {
        let options = parse_options(&["--out".to_owned(), "/tmp/arcen-support".to_owned()])
            .expect("valid support-bundle options");
        assert_eq!(
            options.output_directory,
            Some(PathBuf::from("/tmp/arcen-support"))
        );
    }

    #[test]
    fn rejects_duplicate_output_option() {
        assert!(
            parse_options(&[
                "--out".to_owned(),
                "/tmp/one".to_owned(),
                "--out".to_owned(),
                "/tmp/two".to_owned(),
            ])
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_bundle_file_and_directory() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "arcen-support-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).expect("create test directory");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_private_file_permissions(&mut options);
        let file = options
            .open(directory.join("bundle.zip"))
            .expect("create private file");
        assert_eq!(
            file.metadata().expect("file metadata").permissions().mode() & 0o777,
            0o600
        );
        set_private_directory_permissions(&directory).expect("set private directory");
        assert_eq!(
            std::fs::metadata(&directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(file);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn skips_existing_final_archive_name() {
        let directory = std::env::temp_dir().join(format!(
            "arcen-support-collision-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create test directory");
        let timestamp = 1_789_000_000;
        let first = directory.join(format!("arcen-pier-macos-support-{timestamp}.zip"));
        std::fs::write(&first, b"existing").expect("create existing archive");
        let (path, partial, file) = create_output_file(&directory, timestamp).expect("next name");
        assert_eq!(
            path.file_name().and_then(std::ffi::OsStr::to_str),
            Some("arcen-pier-macos-support-1789000000-1.zip")
        );
        assert!(
            partial
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.ends_with(".partial"))
        );
        drop(file);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn includes_redacted_managed_log_in_archive() {
        let directory =
            std::env::temp_dir().join(format!("arcen-support-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create log directory");
        let line = serde_json::json!({
            "schema_version": 1,
            "timestamp": "2026-07-24T16:00:00.000000Z",
            "sequence": 42,
            "profile_level": 0,
            "profile_name": "critical",
            "severity": "info",
            "role": "host",
            "component": "pier",
            "platform": "macos",
            "target": "arcen::session",
            "sid": "session-id",
            "user": "alice",
            "host": "macbook",
            "peer_addr": "203.0.113.7",
            "health_state": "ok",
            "message": "session started",
            "fields": {"ssid": "private-network"}
        });
        std::fs::write(
            directory.join("arcen-pier-macos.jsonl"),
            format!("{line}\n"),
        )
        .expect("write managed log");

        let component = BundleComponent {
            name: "arcen-pier-macos".to_owned(),
            version: crate::VERSION.to_owned(),
            os: "macos".to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        };
        let mut manifest = SupportBundleManifestBuilder::new(component, 1);
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        add_managed_log(&mut zip, &mut manifest, &directory).expect("add managed log");
        let archive = zip.finish().expect("finish archive").into_inner();
        let mut archive = zip::ZipArchive::new(Cursor::new(archive)).expect("read archive");
        let mut contents = String::new();
        archive
            .by_name(LOG_ENTRY_PATH)
            .expect("managed log entry")
            .read_to_string(&mut contents)
            .expect("read managed log entry");
        assert!(contents.contains("anon:"));
        assert!(!contents.contains("alice"));
        assert!(!contents.contains("private-network"));
        assert_eq!(manifest.finish().entries.len(), 1);
        std::fs::remove_dir_all(directory).expect("remove log directory");
    }

    #[test]
    fn records_missing_managed_log_notice() {
        let directory = std::env::temp_dir().join(format!(
            "arcen-support-missing-log-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create log directory");
        let component = BundleComponent {
            name: "arcen-pier-macos".to_owned(),
            version: crate::VERSION.to_owned(),
            os: "macos".to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        };
        let mut manifest = SupportBundleManifestBuilder::new(component, 1);
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        add_managed_log(&mut zip, &mut manifest, &directory).expect("record missing log");
        let _ = zip.finish().expect("finish archive");
        let manifest = manifest.finish();
        assert_eq!(manifest.entries.len(), 0);
        assert_eq!(manifest.notices.len(), 1);
        assert_eq!(manifest.notices[0].code, NoticeCode::SourceNotFound);
        std::fs::remove_dir_all(directory).expect("remove log directory");
    }
}
