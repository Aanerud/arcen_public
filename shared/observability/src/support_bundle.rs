// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The support-bundle collector every Pier and every Deck writes with.
//!
//! One bounded, redacted `.zip` per request, following the schema-v1
//! contract in `arcen_telemetry::support_bundle` and
//! `docs/operations/support-bundles.md`: `manifest.json` hashes every entry
//! and lists every omission, truncation and redaction.
//!
//! What lives here is everything that is not a product's own list of
//! sources: the archive writer and its limits ([`Collector`]), safe opening of
//! a source file ([`open_source`]), canonical-log export with per-bundle
//! pseudonyms ([`Collector::add_canonical_log_file`],
//! [`collect_canonical_logs`]), redacted JSON documents
//! ([`read_json_document`], [`Collector::add_document`]), the private,
//! atomically published output file ([`write_archive`]) and the
//! `support-bundle [--out DIR]` grammar ([`parse_output_option`]). Each Pier
//! and Deck supplies only its sources: journald, the Event Log or the
//! unified log; its configuration; its diagnostics.
//!
//! For reporting a problem in public, two outputs sit beside the archive:
//! [`PublicSummary`], a Markdown summary made from the manifest's closed
//! vocabulary only (never a log line, value, path or name), and
//! [`write_sealed_archive`], which publishes the full archive only after a
//! [`BundleSealer`] has encrypted it to a recipient's key. Both carry the
//! plaintext archive's [`ArchiveFingerprint`], which ties a public issue to
//! the bundle sent privately.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use arcen_telemetry::{
    BundleComponent, BundleEntry, BundleNotice, BundlePath, BundlePseudonymizer, BundleSource,
    BundleTruncation, CanonicalJsonlTransformLimits, CanonicalJsonlTransformReport,
    MAX_REDACTION_RECORDS, NoticeCode, NoticeKind, REDACTED_VALUE, RedactionReason,
    RedactionRecord, Sha256Digest, SupportBundleContractError, SupportBundleManifest,
    SupportBundleManifestBuilder, TruncationReason, redact_json_document_at,
    transform_canonical_jsonl,
};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

mod native_identity;
pub mod seal;
mod source;
pub mod summary;

pub use seal::{BundleSealer, SealedArchive, write_sealed_archive};
pub use source::{
    ContainerFacts, FileIdentity, IdentityScheme, LinkFacts, OpenedSource, Owner, SourceFacts,
    SourceKind, VolumeFacts, check_source, link_is_trusted, open_source, verify_same_source,
};
pub use summary::{
    ArchiveFingerprint, PreflightRefusal, PublicSummary, SummaryDetails, SummaryFact, SummaryValue,
    fingerprint, fingerprint_file,
};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MIB: u64 = 1024 * 1024;
/// Archive name attempts within one second.
pub const COLLISION_LIMIT: u16 = 100;

/// How much a bundle may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleLimits {
    /// All exported logs together, after transformation.
    pub max_log_bytes: u64,
    /// The whole payload; a finished archive larger than this is refused.
    pub max_total_bytes: u64,
    /// One configuration document.
    pub max_document_bytes: u64,
    /// The tail of one log file that is read, when bounded separately from
    /// [`Self::max_log_bytes`].
    pub max_log_file_input_bytes: Option<u64>,
}

impl BundleLimits {
    /// Every Pier (Linux, Windows, macOS): 200 MiB of logs, 256 MiB in all,
    /// 1 MiB per document; a log file is read as far back as the remaining
    /// budget allows.
    pub const PIER: Self = Self {
        max_log_bytes: 200 * MIB,
        max_total_bytes: 256 * MIB,
        max_document_bytes: MIB,
        max_log_file_input_bytes: None,
    };

    /// Every Deck: the Piers' 256 MiB payload and 1 MiB documents, but
    /// 128 MiB of logs read 32 MiB per daily file. A Deck holds each log in
    /// memory to remove the computer's account name from it
    /// (`arcen_deck_core::support_bundle::redact`), which a Pier's canonical
    /// logs never carry, so its per-file read is bounded.
    pub const DECK: Self = Self {
        max_log_bytes: 128 * MIB,
        max_total_bytes: 256 * MIB,
        max_document_bytes: MIB,
        max_log_file_input_bytes: Some(32 * MIB),
    };
}

/// Why no bundle was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupportBundleError {
    /// The folder or file could not be created or published.
    Output(String),
    /// Writing the archive failed.
    Archive(String),
    /// A schema bound was exceeded.
    Contract(String),
}

impl std::fmt::Display for SupportBundleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Output(reason) | Self::Archive(reason) | Self::Contract(reason) => {
                formatter.write_str(reason)
            }
        }
    }
}

impl std::error::Error for SupportBundleError {}

impl From<SupportBundleContractError> for SupportBundleError {
    fn from(error: SupportBundleContractError) -> Self {
        Self::Contract(format!("support-bundle contract: {error}"))
    }
}

impl From<zip::result::ZipError> for SupportBundleError {
    fn from(error: zip::result::ZipError) -> Self {
        Self::Archive(format!("write support-bundle ZIP: {error}"))
    }
}

impl From<std::io::Error> for SupportBundleError {
    fn from(error: std::io::Error) -> Self {
        Self::Archive(format!("write support-bundle payload: {error}"))
    }
}

impl From<SupportBundleError> for String {
    fn from(error: SupportBundleError) -> Self {
        error.to_string()
    }
}

/// Why a source file was not opened.
#[derive(Debug)]
pub enum SourceOpenError {
    /// The file could not be inspected or opened.
    Io(std::io::Error),
    /// Not a regular file: a link, a directory, a FIFO, a socket, a device,
    /// or (Windows) any reparse point.
    UnsafeType,
    /// A folder on the way to the source was a link the collector does not
    /// trust ([`link_is_trusted`]) or a reparse point, or the opened file
    /// was not in the folder its path names.
    PathLinked,
    /// The source has more than one name (a hard link).
    HardLinked,
    /// The file that was opened is not the one that was inspected.
    Replaced,
    /// The file system reported no file number, so nothing proves the
    /// opened file is the inspected one ([`verify_same_source`]).
    IdentityUnproven,
    /// The source's volume cannot prove file identity: a network volume, or
    /// one without stable file IDs ([`VolumeFacts`]).
    IdentityUnstable,
}

impl SourceOpenError {
    /// The manifest notice this refusal is recorded as.
    #[must_use]
    pub fn notice(&self) -> (NoticeKind, NoticeCode) {
        match self {
            Self::Io(error) => io_notice(error),
            Self::UnsafeType => (NoticeKind::Invalid, NoticeCode::UnsafeFileType),
            Self::PathLinked => (NoticeKind::Invalid, NoticeCode::SourcePathLinked),
            Self::HardLinked => (NoticeKind::Invalid, NoticeCode::SourceHardLinked),
            Self::Replaced => (NoticeKind::Invalid, NoticeCode::SourceChangedDuringRead),
            Self::IdentityUnproven | Self::IdentityUnstable => (
                NoticeKind::Unavailable,
                NoticeCode::SourceIdentityUnverifiable,
            ),
        }
    }
}

/// The manifest notice for an I/O error reading a source.
#[must_use]
pub fn io_notice(error: &std::io::Error) -> (NoticeKind, NoticeCode) {
    match error.kind() {
        std::io::ErrorKind::NotFound => (NoticeKind::Unavailable, NoticeCode::SourceNotFound),
        std::io::ErrorKind::PermissionDenied => (
            NoticeKind::PermissionDenied,
            NoticeCode::SourcePermissionDenied,
        ),
        _ => (NoticeKind::Unavailable, NoticeCode::SourceUnavailable),
    }
}

/// Whether a source must not be read: anything but a regular file — a link
/// (never followed), and on Windows any reparse point (junctions, mount
/// points, cloud placeholders), which `is_symlink` does not cover.
#[must_use]
pub fn is_unsafe_source(metadata: &Metadata) -> bool {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// One log file a product offers for export, newest first in the list it
/// hands [`collect_canonical_logs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCandidate {
    /// Where it is.
    pub path: PathBuf,
    /// Its entry in the archive (`logs/<name>`).
    pub archive_path: String,
    /// Its size when it was listed.
    pub size_bytes: u64,
}

/// The ZIP writer, manifest and limits of one bundle.
#[derive(Debug)]
pub struct Collector<W: Write + Seek> {
    zip: ZipWriter<W>,
    manifest: SupportBundleManifestBuilder,
    pseudonymizer: BundlePseudonymizer,
    limits: BundleLimits,
    options: SimpleFileOptions,
    total_bytes: u64,
    omission_count: usize,
}

impl<W: Write + Seek> Collector<W> {
    /// Starts a bundle in `writer`. Every entry is deflated, private
    /// (`0600` when extracted on Unix) and stamped with `generated_at`, so
    /// a fixed time, key and content give a byte-identical archive.
    pub fn new(
        writer: W,
        component: BundleComponent,
        generated_at: u64,
        pseudonymizer: BundlePseudonymizer,
        limits: BundleLimits,
    ) -> Self {
        Self {
            zip: ZipWriter::new(writer),
            manifest: SupportBundleManifestBuilder::new(component, generated_at)
                .with_bundle_id(pseudonymizer.bundle_id()),
            pseudonymizer,
            limits,
            options: SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .unix_permissions(0o600)
                .last_modified_time(zip_time(generated_at)),
            total_bytes: 0,
            omission_count: 0,
        }
    }

    /// The per-bundle pseudonymizer every source uses, so one identity
    /// reads the same across entries.
    #[must_use]
    pub const fn pseudonymizer(&self) -> &BundlePseudonymizer {
        &self.pseudonymizer
    }

    /// The limits this bundle was started with.
    #[must_use]
    pub const fn limits(&self) -> BundleLimits {
        self.limits
    }

    /// Payload bytes written so far.
    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Records a notice. Omissions (everything but advisories and
    /// truncations) are counted for the caller's summary.
    ///
    /// # Errors
    ///
    /// The source path or the notice count breaks the schema's bounds.
    pub fn notice(
        &mut self,
        source: &str,
        kind: NoticeKind,
        code: NoticeCode,
    ) -> Result<(), SupportBundleError> {
        if !matches!(kind, NoticeKind::Advisory | NoticeKind::Truncated) {
            self.omission_count += 1;
        }
        self.manifest.add_notice(BundleNotice {
            source: BundlePath::new(source)?,
            kind,
            code,
        })?;
        Ok(())
    }

    /// Records a redaction.
    ///
    /// # Errors
    ///
    /// The redaction count breaks the schema's bounds.
    pub fn redaction(&mut self, record: RedactionRecord) -> Result<(), SupportBundleError> {
        self.manifest.add_redaction(record)?;
        Ok(())
    }

    /// Records whether crash reports were asked for, under `crash`. Crash
    /// reports are out of every bundle unless asked for (a Deck checkbox,
    /// `--include-crash-dumps`): their stacks can hold what the program was
    /// handling — a sign-in password, clipboard text — which no redaction
    /// can find. `collected` says whether this product collects them.
    ///
    /// # Errors
    ///
    /// As [`Self::notice`].
    pub fn crash_report_choice(
        &mut self,
        requested: bool,
        collected: bool,
    ) -> Result<(), SupportBundleError> {
        match (requested, collected) {
            (false, _) => self.notice(
                "crash",
                NoticeKind::Omitted,
                NoticeCode::CrashReportsNotRequested,
            ),
            (true, true) => self.notice(
                "crash",
                NoticeKind::Advisory,
                NoticeCode::CrashReportsRequested,
            ),
            (true, false) => self.notice(
                "crash",
                NoticeKind::Unavailable,
                NoticeCode::CrashReportsNotCollected,
            ),
        }
    }

    /// Records the notice for a source that could not be opened.
    ///
    /// # Errors
    ///
    /// As [`Self::notice`].
    pub fn source_notice(
        &mut self,
        source: &str,
        error: &SourceOpenError,
    ) -> Result<(), SupportBundleError> {
        let (kind, code) = error.notice();
        self.notice(source, kind, code)
    }

    /// Records the notice for an I/O error reading a source.
    ///
    /// # Errors
    ///
    /// As [`Self::notice`].
    pub fn file_notice(
        &mut self,
        source: &str,
        error: &std::io::Error,
    ) -> Result<(), SupportBundleError> {
        let (kind, code) = io_notice(error);
        self.notice(source, kind, code)
    }

    /// Adds `bytes` whole, as read from the source.
    ///
    /// # Errors
    ///
    /// Writing failed or a schema bound was exceeded.
    pub fn add_bytes(
        &mut self,
        archive_path: &str,
        source: BundleSource,
        bytes: &[u8],
    ) -> Result<bool, SupportBundleError> {
        self.add_entry(archive_path, source, bytes.len() as u64, bytes, None)
    }

    /// Adds one entry whose source was `original_size` bytes, or an
    /// omission notice when the payload limit would be passed. Returns
    /// whether it was added.
    ///
    /// # Errors
    ///
    /// Writing failed or a schema bound was exceeded.
    pub fn add_entry(
        &mut self,
        archive_path: &str,
        source: BundleSource,
        original_size: u64,
        bytes: &[u8],
        truncation: Option<BundleTruncation>,
    ) -> Result<bool, SupportBundleError> {
        let path = BundlePath::new(archive_path)?;
        if bytes.len() as u64 > self.limits.max_total_bytes.saturating_sub(self.total_bytes) {
            self.notice(
                path.as_str(),
                NoticeKind::Omitted,
                NoticeCode::TotalPayloadLimit,
            )?;
            return Ok(false);
        }
        self.zip.start_file(path.as_str(), self.options)?;
        let mut digest = Sha256::new();
        for chunk in bytes.chunks(COPY_BUFFER_BYTES) {
            self.zip.write_all(chunk)?;
            digest.update(chunk);
        }
        let size = bytes.len() as u64;
        self.total_bytes += size;
        self.manifest.add_entry(BundleEntry {
            path,
            source,
            original_size_bytes: original_size,
            included_size_bytes: size,
            sha256: Sha256Digest::from_bytes(digest.finalize().into()),
            truncation,
        })?;
        Ok(true)
    }

    /// Adds an already redacted document and, when it was added, the
    /// redactions made to it.
    ///
    /// # Errors
    ///
    /// Writing failed or a schema bound was exceeded.
    pub fn add_document(
        &mut self,
        archive_path: &str,
        source: BundleSource,
        original_size: Option<u64>,
        bytes: &[u8],
        redactions: Vec<RedactionRecord>,
    ) -> Result<bool, SupportBundleError> {
        let original = original_size.unwrap_or(bytes.len() as u64);
        let added = self.add_entry(archive_path, source, original, bytes, None)?;
        if added {
            for redaction in redactions {
                self.redaction(redaction)?;
            }
        }
        Ok(added)
    }

    /// Opens a source for `archive_path`, recording a notice instead when
    /// it cannot be read safely, and an advisory when it was opened but its
    /// identity was weaker than a 128-bit file ID
    /// ([`OpenedSource::advisory`]).
    ///
    /// # Errors
    ///
    /// As [`Self::notice`].
    pub fn open_source(
        &mut self,
        file_path: &Path,
        archive_path: &str,
    ) -> Result<Option<(File, Metadata)>, SupportBundleError> {
        match open_source(file_path) {
            Ok(opened) => {
                if let Some((kind, code)) = opened.advisory() {
                    self.notice(archive_path, kind, code)?;
                }
                Ok(Some((opened.file, opened.metadata)))
            }
            Err(error) => {
                self.source_notice(archive_path, &error)?;
                Ok(None)
            }
        }
    }

    /// Streams the last `included_size` bytes of a regular file into the
    /// archive. A file that changed while it was read is kept and marked.
    ///
    /// # Errors
    ///
    /// Writing failed or a schema bound was exceeded.
    pub fn add_file(
        &mut self,
        file_path: &Path,
        archive_path: &str,
        source: BundleSource,
        original_size: u64,
        included_size: u64,
        reason: Option<TruncationReason>,
    ) -> Result<bool, SupportBundleError> {
        if included_size == 0 {
            return Ok(false);
        }
        let path = BundlePath::new(archive_path)?;
        let Some((mut file, opened)) = self.open_source(file_path, path.as_str())? else {
            return Ok(false);
        };
        let offset = original_size.saturating_sub(included_size);
        file.seek(SeekFrom::Start(offset))?;
        self.zip.start_file(path.as_str(), self.options)?;
        let mut digest = Sha256::new();
        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
        let mut remaining = included_size;
        let mut written = 0_u64;
        while remaining != 0 {
            let want = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64))
                .unwrap_or(COPY_BUFFER_BYTES);
            let count = match file.read(&mut buffer[..want]) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) => {
                    self.zip.abort_file()?;
                    self.file_notice(path.as_str(), &error)?;
                    return Ok(false);
                }
            };
            self.zip.write_all(&buffer[..count])?;
            digest.update(&buffer[..count]);
            written += count as u64;
            remaining -= count as u64;
        }
        let after = file.metadata()?;
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
        self.manifest.add_entry(BundleEntry {
            path,
            source,
            original_size_bytes: original_size,
            included_size_bytes: written,
            sha256: Sha256Digest::from_bytes(digest.finalize().into()),
            truncation,
        })?;
        Ok(true)
    }

    /// Streams the tail of one canonical JSON Lines log through the shared
    /// transform: only canonical records, `user`/`host`/`peer_addr` and the
    /// identity `fields` (`arcen_telemetry::BUNDLE_IDENTITY_FIELDS`) replaced
    /// by this bundle's pseudonyms. At most
    /// `input_limit` bytes are read from the end of the file (a cut first
    /// line is dropped) and at most `output_limit` written. Returns the
    /// bytes written.
    ///
    /// # Errors
    ///
    /// Writing failed or a schema bound was exceeded.
    pub fn add_canonical_log_file(
        &mut self,
        file_path: &Path,
        archive_path: &str,
        original_size: u64,
        input_limit: u64,
        output_limit: u64,
        budget_reason: TruncationReason,
    ) -> Result<u64, SupportBundleError> {
        if input_limit == 0 || output_limit == 0 {
            return Ok(0);
        }
        let path = BundlePath::new(archive_path)?;
        let Some((mut file, opened)) = self.open_source(file_path, path.as_str())? else {
            return Ok(0);
        };
        let effective_input_limit = input_limit.min(opened.len());
        let offset = opened.len().saturating_sub(effective_input_limit);
        let discard_initial_fragment = starts_mid_line(&mut file, offset)?;
        file.seek(SeekFrom::Start(offset))?;
        self.zip.start_file(path.as_str(), self.options)?;
        let mut writer = DigestingWriter::new(&mut self.zip);
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
        .map_err(|error| {
            SupportBundleError::Archive(format!("transform canonical support-bundle log: {error}"))
        })?;
        let (written, digest) = writer.finish();
        if written == 0 {
            self.zip.abort_file()?;
        }
        let after = file.metadata()?;
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
        self.manifest.add_entry(BundleEntry {
            path: path.clone(),
            source: BundleSource::Log,
            original_size_bytes: original_size,
            included_size_bytes: written,
            sha256: Sha256Digest::from_bytes(digest),
            truncation,
        })?;
        for key_path in report.redacted_key_paths {
            self.redaction(RedactionRecord {
                entry_path: path.clone(),
                key_path,
                reason: RedactionReason::IdentityPseudonymized,
            })?;
        }
        Ok(written)
    }

    /// Records the canonical transform's refusals for `source`.
    ///
    /// # Errors
    ///
    /// As [`Self::notice`].
    pub fn record_log_transform(
        &mut self,
        source: &str,
        report: &CanonicalJsonlTransformReport,
    ) -> Result<(), SupportBundleError> {
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

    /// Writes `manifest.json` last and closes the archive. Returns the
    /// writer, the manifest and the number of omissions.
    ///
    /// # Errors
    ///
    /// Writing failed, or the archive passed the payload limit.
    pub fn finish(mut self) -> Result<(W, SupportBundleManifest, usize), SupportBundleError> {
        let manifest = self.manifest.finish();
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
            SupportBundleError::Archive(format!("serialize support-bundle manifest: {error}"))
        })?;
        self.zip.start_file("manifest.json", self.options)?;
        for chunk in bytes.chunks(COPY_BUFFER_BYTES) {
            self.zip.write_all(chunk)?;
        }
        let mut writer = self.zip.finish()?;
        let length = writer.seek(SeekFrom::End(0))?;
        if length > self.limits.max_total_bytes {
            return Err(SupportBundleError::Archive(format!(
                "support-bundle archive exceeded the {} MiB limit",
                self.limits.max_total_bytes / MIB
            )));
        }
        Ok((writer, manifest, self.omission_count))
    }
}

/// Exports `logs` (newest first) within the bundle's log budget: each file's
/// tail, as far back as the remaining log and payload budgets allow (and
/// [`BundleLimits::max_log_file_input_bytes`] when set).
///
/// # Errors
///
/// Writing failed or a schema bound was exceeded.
pub fn collect_canonical_logs<W: Write + Seek>(
    collector: &mut Collector<W>,
    logs: &[LogCandidate],
) -> Result<(), SupportBundleError> {
    let limits = collector.limits();
    let mut remaining_logs = limits.max_log_bytes;
    for log in logs {
        if remaining_logs == 0 {
            collector.notice("logs", NoticeKind::Truncated, NoticeCode::LogPayloadLimit)?;
            break;
        }
        let total_remaining = limits
            .max_total_bytes
            .saturating_sub(collector.total_bytes());
        if total_remaining == 0 {
            collector.notice("logs", NoticeKind::Truncated, NoticeCode::TotalPayloadLimit)?;
            break;
        }
        let output_limit = remaining_logs.min(total_remaining);
        let input_limit = limits
            .max_log_file_input_bytes
            .map_or(log.size_bytes, |limit| log.size_bytes.min(limit))
            .min(output_limit);
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

/// Reads one JSON document of at most the bundle's document limit, or
/// records why not (missing, unsafe, too large, not JSON) under
/// `archive_path`.
///
/// # Errors
///
/// Reading failed after opening, or a schema bound was exceeded.
pub fn read_json_document<W: Write + Seek>(
    collector: &mut Collector<W>,
    path: &Path,
    archive_path: &str,
) -> Result<Option<serde_json::Value>, SupportBundleError> {
    let limit = collector.limits().max_document_bytes;
    let Some((file, opened)) = collector.open_source(path, archive_path)? else {
        return Ok(None);
    };
    if opened.len() > limit {
        collector.notice(
            archive_path,
            NoticeKind::Invalid,
            NoticeCode::SourceTooLarge,
        )?;
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        collector.notice(
            archive_path,
            NoticeKind::Invalid,
            NoticeCode::SourceTooLarge,
        )?;
        return Ok(None);
    }
    if let Ok(document) = serde_json::from_slice(&bytes) {
        Ok(Some(document))
    } else {
        collector.notice(archive_path, NoticeKind::Invalid, NoticeCode::SourceInvalid)?;
        Ok(None)
    }
}

/// Redacts a Pier's `pier.json` for `config/pier.json`: the shared key
/// policy (`key`, `secret`, `password`, …), a private key recorded as the
/// private-key policy, and the TLS identity metadata the key policy does not
/// catch — the certificate path and the expected SANs, which name the
/// customer's host. Returns the redactions, sorted by key path.
///
/// # Errors
///
/// A key path or the redaction count breaks the schema's bounds.
pub fn redact_pier_config(
    archive_path: &BundlePath,
    document: &mut serde_json::Value,
) -> Result<Vec<RedactionRecord>, SupportBundleError> {
    let mut redactions = redact_json_document_at(archive_path, document)?
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
    if let Some(tls) = document
        .get_mut("tls")
        .and_then(serde_json::Value::as_object_mut)
    {
        for key in ["cert", "certificate", "expected_sans"] {
            let Some((actual_key, value)) = tls
                .iter_mut()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            else {
                continue;
            };
            if redactions.len() >= MAX_REDACTION_RECORDS {
                return Err(SupportBundleError::Contract(
                    "Pier config contains too many redacted settings".to_owned(),
                ));
            }
            *value = serde_json::Value::String(REDACTED_VALUE.to_owned());
            redactions.push(RedactionRecord {
                entry_path: archive_path.clone(),
                key_path: format!("/tls/{actual_key}"),
                reason: RedactionReason::SensitiveKey,
            });
        }
    }
    redactions.sort_by(|left, right| left.key_path.cmp(&right.key_path));
    Ok(redactions)
}

/// The shared `support-bundle` usage line's options.
pub const USAGE_OPTIONS: &str = "[--out DIR] [--include-crash-dumps]";

/// What `support-bundle [--out DIR] [--include-crash-dumps]` asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// This folder, or the product's default.
    pub output_directory: Option<PathBuf>,
    /// Crash reports go in only when asked for
    /// ([`Collector::crash_report_choice`]).
    pub include_crash_dumps: bool,
}

/// A failed bundle as a Pier's `support-bundle` command reports it: an
/// output failure names `--out <DIR>` as the way out.
#[must_use]
pub fn command_error(error: SupportBundleError) -> String {
    match error {
        SupportBundleError::Output(reason) => {
            format!("{reason}; use --out <DIR> to select a writable directory")
        }
        other => other.to_string(),
    }
}

/// What `support-bundle` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputOption {
    /// Write the bundle.
    Write(WriteOptions),
    /// `-h`/`--help`.
    Help,
}

/// Parses the arguments after `support-bundle`: `--out DIR` and
/// `--include-crash-dumps`, each at most once, or `-h`/`--help`.
///
/// # Errors
///
/// An unknown or repeated argument, or `--out` without a folder.
pub fn parse_output_option(arguments: &[String]) -> Result<OutputOption, String> {
    let mut output_directory = None;
    let mut include_crash_dumps = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--include-crash-dumps" => {
                if include_crash_dumps {
                    return Err("--include-crash-dumps may be supplied only once".to_owned());
                }
                include_crash_dumps = true;
            }
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
            "-h" | "--help" => return Ok(OutputOption::Help),
            other => return Err(format!("unknown support-bundle argument: {other}")),
        }
        index += 1;
    }
    Ok(OutputOption::Write(WriteOptions {
        output_directory,
        include_crash_dumps,
    }))
}

/// Creates `path`, which must not exist, for this process to write: mode
/// `0600` on Unix; on Windows (where the file inherits its folder's ACL)
/// other processes may read it but neither write nor delete it while it is
/// open.
///
/// # Errors
///
/// The file exists or could not be created.
pub fn create_private_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).read(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        options.share_mode(FILE_SHARE_READ);
    }
    options.open(path)
}

/// Writes a bundle into `directory` atomically: the first free name
/// `name(suffix)` (`suffix` empty, then `-1`, `-2`, …), written by `build`
/// through a private same-folder `.<name>.partial` file that is synced and
/// renamed only when complete, and removed on any failure. Returns the
/// published path and what `build` returned.
///
/// # Errors
///
/// No free name, the partial file could not be created, `build` failed, or
/// the archive could not be synced or published.
pub fn write_archive<T>(
    directory: &Path,
    name: impl Fn(&str) -> String,
    build: impl FnOnce(File) -> Result<(File, T), SupportBundleError>,
) -> Result<(PathBuf, T), SupportBundleError> {
    let (path, partial, file) = create_output_file(directory, name)?;
    match build(file) {
        Ok((file, value)) => {
            let published = file.sync_all().and_then(|()| {
                drop(file);
                std::fs::rename(&partial, &path)
            });
            if let Err(error) = published {
                let _ = std::fs::remove_file(&partial);
                return Err(SupportBundleError::Output(format!(
                    "publish support bundle {}: {error}",
                    path.display()
                )));
            }
            Ok((path, value))
        }
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            Err(error)
        }
    }
}

fn create_output_file(
    directory: &Path,
    name: impl Fn(&str) -> String,
) -> Result<(PathBuf, PathBuf, File), SupportBundleError> {
    for attempt in 0..COLLISION_LIMIT {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let name = name(&suffix);
        let path = directory.join(&name);
        if path.exists() {
            continue;
        }
        let partial = directory.join(format!(".{name}.partial"));
        match create_private_new(&partial) {
            Ok(file) => return Ok((path, partial, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(SupportBundleError::Output(format!(
                    "create support-bundle partial archive {}: {error}",
                    partial.display()
                )));
            }
        }
    }
    Err(SupportBundleError::Output(
        "support-bundle output collision limit reached".to_owned(),
    ))
}

/// `(year, month, day, hour, minute, second)` in UTC.
#[must_use]
pub fn utc_civil(unix_seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = i64::try_from(unix_seconds / 86_400).unwrap_or(0);
    let seconds = unix_seconds % 86_400;
    let (year, month, day) = crate::runtime::civil_from_days(days);
    let narrow = |value: i64| u32::try_from(value).unwrap_or(0);
    let narrow_seconds = |value: u64| u32::try_from(value).unwrap_or(0);
    (
        year,
        narrow(month),
        narrow(day),
        narrow_seconds(seconds / 3_600),
        narrow_seconds(seconds / 60 % 60),
        narrow_seconds(seconds % 60),
    )
}

/// `2026-11-15T07:04:05Z`.
#[must_use]
pub fn rfc3339(unix_seconds: u64) -> String {
    let (year, month, day, hour, minute, second) = utc_civil(unix_seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// `20261115T070405Z`, for archive names (never a hostname).
#[must_use]
pub fn utc_stamp(unix_seconds: u64) -> String {
    let (year, month, day, hour, minute, second) = utc_civil(unix_seconds);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

fn zip_time(unix_seconds: u64) -> zip::DateTime {
    let (year, month, day, hour, minute, second) = utc_civil(unix_seconds);
    let narrow = |value: u32| u8::try_from(value).unwrap_or(0);
    u16::try_from(year)
        .ok()
        .and_then(|year| {
            zip::DateTime::from_date_and_time(
                year,
                narrow(month),
                narrow(day),
                narrow(hour),
                narrow(minute),
                // ZIP stores even seconds only.
                narrow(second & !1),
            )
            .ok()
        })
        .unwrap_or_default()
}

fn starts_mid_line(file: &mut File, offset: u64) -> Result<bool, SupportBundleError> {
    if offset == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(offset - 1))?;
    let mut previous = [0_u8; 1];
    file.read_exact(&mut previous)?;
    Ok(previous[0] != b'\n')
}

struct DigestingWriter<'a, W: Write + Seek> {
    zip: &'a mut ZipWriter<W>,
    digest: Sha256,
    written: u64,
}

impl<'a, W: Write + Seek> DigestingWriter<'a, W> {
    fn new(zip: &'a mut ZipWriter<W>) -> Self {
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

impl<W: Write + Seek> Write for DigestingWriter<'_, W> {
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

#[cfg(test)]
mod tests;
