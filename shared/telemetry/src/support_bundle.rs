//! Pure support-bundle manifest and redaction contracts.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::io::{Read, Write};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// Current support-bundle manifest schema.
pub const SUPPORT_BUNDLE_SCHEMA_VERSION: u32 = 1;
/// Literal substituted for secret-bearing document values.
pub const REDACTED_VALUE: &str = "<redacted>";
/// Maximum payload entries in one bundle.
pub const MAX_BUNDLE_ENTRIES: usize = 2_048;
/// Maximum typed notices in one bundle.
pub const MAX_BUNDLE_NOTICES: usize = 4_096;
/// Maximum redaction records in one bundle.
pub const MAX_REDACTION_RECORDS: usize = 4_096;
/// Maximum UTF-8 length of a canonical archive-relative path.
pub const MAX_BUNDLE_PATH_BYTES: usize = 512;
/// Maximum UTF-8 length of a redacted JSON key path.
pub const MAX_REDACTION_KEY_PATH_BYTES: usize = 512;
/// Maximum bytes accepted for one canonical JSONL record before its newline.
pub const MAX_CANONICAL_JSON_LINE_BYTES: usize = 64 * 1024;

/// A validated canonical UTF-8 path relative to the ZIP root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BundlePath(String);

impl BundlePath {
    /// Validates a canonical ZIP-relative path.
    ///
    /// # Errors
    ///
    /// Rejects empty, absolute, drive/UNC, backslash, control-bearing,
    /// overlong, non-canonical, and traversal paths.
    pub fn new(value: impl Into<String>) -> Result<Self, SupportBundleContractError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_BUNDLE_PATH_BYTES
            || value.starts_with('/')
            || value.starts_with("\\\\")
            || value.as_bytes().get(1) == Some(&b':')
            || value.contains('\\')
            || value.contains("//")
            || value.chars().any(char::is_control)
            || value
                .split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            return Err(SupportBundleContractError::InvalidBundlePath);
        }
        Ok(Self(value))
    }

    /// Returns the canonical relative path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for BundlePath {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for BundlePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BundlePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// A strict lowercase hexadecimal SHA-256 digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// Constructs a digest from its raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses exactly 64 lowercase hexadecimal characters.
    ///
    /// # Errors
    ///
    /// Returns an error for the wrong length, uppercase, or non-hex input.
    pub fn parse(value: &str) -> Result<Self, SupportBundleContractError> {
        if value.len() != 64
            || value
                .bytes()
                .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
        {
            return Err(SupportBundleContractError::InvalidSha256Digest);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
        }
        Ok(Self(bytes))
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => 0,
    }
}

impl Display for Sha256Digest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Product identity included without machine or user identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleComponent {
    /// Arcen component name.
    pub name: String,
    /// Component version.
    pub version: String,
    /// Operating-system family.
    pub os: String,
    /// Process architecture.
    pub arch: String,
}

/// Allowlisted source category for an archive entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleSource {
    /// Managed Arcen logs.
    Log,
    /// Redacted configuration.
    Configuration,
    /// Bounded runtime/recovery state.
    RuntimeState,
    /// Native lifecycle records.
    LifecycleEvents,
    /// Synthetic or command-backed diagnostics.
    Diagnostics,
}

/// Why only a suffix or bounded portion of a source was included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    /// The source-specific cap was reached.
    PerSourceLimit,
    /// The total archive cap was reached.
    GlobalLimit,
    /// Source metadata changed while the file was streamed.
    ChangedDuringRead,
}

/// Truncation metadata for one payload entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleTruncation {
    /// Offset in the original source where included bytes begin.
    pub original_offset: u64,
    /// Stable reason vocabulary.
    pub reason: TruncationReason,
}

/// One hashed payload entry. `manifest.json` is never an entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEntry {
    /// Canonical archive path.
    pub path: BundlePath,
    /// Allowlisted source category.
    pub source: BundleSource,
    /// Original source size before truncation.
    pub original_size_bytes: u64,
    /// Transformed bytes written to the archive and hashed. Pseudonymization
    /// can make this larger than the source byte count.
    pub included_size_bytes: u64,
    /// SHA-256 of exactly the archived payload bytes.
    pub sha256: Sha256Digest,
    /// Optional bounded/truncation metadata.
    pub truncation: Option<BundleTruncation>,
}

/// Typed source outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// Informational state that does not indicate an omitted source.
    Advisory,
    /// Deliberately excluded by policy.
    Omitted,
    /// Source or platform capability was unavailable.
    Unavailable,
    /// Access was denied.
    PermissionDenied,
    /// Source was malformed or unsafe.
    Invalid,
    /// A bounded operation timed out.
    TimedOut,
    /// A bounded source was shortened.
    Truncated,
}

/// Closed, non-sensitive support-bundle reason vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeCode {
    /// Source does not exist.
    SourceNotFound,
    /// Source is not a regular non-link file.
    UnsafeFileType,
    /// Source exceeded its per-document cap.
    SourceTooLarge,
    /// Source could not be parsed.
    SourceInvalid,
    /// Reading the source failed without a more specific classification.
    SourceUnavailable,
    /// Access to the source was denied.
    SourcePermissionDenied,
    /// TLS private-key material is always excluded.
    PrivateKeyExcluded,
    /// TLS public certificate is unnecessary for schema v1.
    CertificateExcluded,
    /// Sensitive runtime/session trees are intentionally excluded.
    SensitiveRuntimeExcluded,
    /// Xorg logs are intentionally outside the managed-log allowlist.
    XorgLogExcluded,
    /// The managed-log candidate count was capped.
    LogCandidateLimit,
    /// The 200 MiB log payload cap was reached.
    LogPayloadLimit,
    /// The 256 MiB total payload cap was reached.
    TotalPayloadLimit,
    /// Source metadata changed while it was read.
    SourceChangedDuringRead,
    /// Native lifecycle query was unavailable.
    LifecycleQueryUnavailable,
    /// Native lifecycle query access was denied.
    LifecycleQueryPermissionDenied,
    /// Native lifecycle query exceeded its execution deadline.
    LifecycleQueryTimedOut,
    /// Native lifecycle query output was capped.
    LifecycleQueryTruncated,
    /// A diagnostic command was unavailable.
    DiagnosticUnavailable,
    /// A diagnostic command failed.
    DiagnosticFailed,
    /// A diagnostic command timed out.
    DiagnosticTimedOut,
    /// A diagnostic command output was capped.
    DiagnosticTruncated,
    /// The host service is not installed.
    ServiceNotInstalled,
    /// The host service is stopped; this is valid service-down collection.
    ServiceStopped,
    /// Display recovery state is present and may require restoration.
    PendingDisplayRestore,
    /// No supported read-only NVIDIA driver query exists.
    DriverQueryUnavailable,
    /// Legacy logging path selection was used.
    LegacyLogMode,
    /// A canonical log record was malformed, had the wrong schema, or had an unsafe shape.
    CanonicalLogRecordInvalid,
    /// A canonical log record exceeded the fixed per-line bound.
    CanonicalLogRecordTooLarge,
    /// A canonical log fragment lacked a complete line boundary.
    CanonicalLogRecordIncomplete,
}

/// One typed omission/degradation notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleNotice {
    /// Logical source name, never an absolute host path.
    pub source: BundlePath,
    /// Outcome category.
    pub kind: NoticeKind,
    /// Stable non-sensitive reason.
    pub code: NoticeCode,
}

/// Why a document value was replaced or omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionReason {
    /// The key name is secret-bearing.
    SensitiveKey,
    /// Private-key material is excluded without touching its path.
    PrivateKeyPolicy,
    /// An operational identity was replaced with a per-bundle pseudonym.
    IdentityPseudonymized,
}

/// Auditable redaction metadata without the original value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactionRecord {
    /// Archive entry containing the redacted document.
    pub entry_path: BundlePath,
    /// JSON pointer to the replaced key.
    pub key_path: String,
    /// Stable redaction policy reason.
    pub reason: RedactionReason,
}

/// Versioned deterministic archive manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportBundleManifest {
    /// Schema version, currently 1.
    pub schema_version: u32,
    /// Product component, without hostname.
    pub component: BundleComponent,
    /// Host-supplied UNIX timestamp.
    pub generated_at_unix_seconds: u64,
    /// Sorted hashed payload entries, excluding `manifest.json`.
    pub entries: Vec<BundleEntry>,
    /// Sorted typed source outcomes.
    pub notices: Vec<BundleNotice>,
    /// Sorted redaction audit records.
    pub redactions: Vec<RedactionRecord>,
}

/// Deterministic manifest builder enforcing schema-v1 bounds.
#[derive(Debug)]
pub struct SupportBundleManifestBuilder {
    component: BundleComponent,
    generated_at_unix_seconds: u64,
    entries: Vec<BundleEntry>,
    notices: Vec<BundleNotice>,
    redactions: Vec<RedactionRecord>,
}

impl SupportBundleManifestBuilder {
    /// Creates an empty schema-v1 manifest.
    #[must_use]
    pub const fn new(component: BundleComponent, generated_at_unix_seconds: u64) -> Self {
        Self {
            component,
            generated_at_unix_seconds,
            entries: Vec::new(),
            notices: Vec::new(),
            redactions: Vec::new(),
        }
    }

    /// Adds one unique payload entry.
    ///
    /// # Errors
    ///
    /// Rejects `manifest.json`, duplicate paths, and count overflow.
    pub fn add_entry(&mut self, entry: BundleEntry) -> Result<(), SupportBundleContractError> {
        if entry.path.as_str() == "manifest.json" {
            return Err(SupportBundleContractError::ManifestCannotIndexItself);
        }
        if self.entries.len() >= MAX_BUNDLE_ENTRIES {
            return Err(SupportBundleContractError::TooManyEntries);
        }
        if self
            .entries
            .iter()
            .any(|current| current.path == entry.path)
        {
            return Err(SupportBundleContractError::DuplicateEntryPath);
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Adds a typed source notice.
    ///
    /// # Errors
    ///
    /// Rejects notice count overflow.
    pub fn add_notice(&mut self, notice: BundleNotice) -> Result<(), SupportBundleContractError> {
        if self.notices.len() >= MAX_BUNDLE_NOTICES {
            return Err(SupportBundleContractError::TooManyNotices);
        }
        self.notices.push(notice);
        Ok(())
    }

    /// Adds one redaction audit record.
    ///
    /// # Errors
    ///
    /// Rejects overlong key paths and record count overflow.
    pub fn add_redaction(
        &mut self,
        redaction: RedactionRecord,
    ) -> Result<(), SupportBundleContractError> {
        if redaction.key_path.len() > MAX_REDACTION_KEY_PATH_BYTES
            || redaction.key_path.chars().any(char::is_control)
        {
            return Err(SupportBundleContractError::InvalidRedactionKeyPath);
        }
        if self.redactions.len() >= MAX_REDACTION_RECORDS {
            return Err(SupportBundleContractError::TooManyRedactions);
        }
        self.redactions.push(redaction);
        Ok(())
    }

    /// Sorts every collection into stable schema-v1 order.
    #[must_use]
    pub fn finish(mut self) -> SupportBundleManifest {
        self.entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        self.notices.sort_by(|left, right| {
            (&left.source, left.kind, left.code).cmp(&(&right.source, right.kind, right.code))
        });
        self.redactions.sort_by(|left, right| {
            (&left.entry_path, &left.key_path, left.reason).cmp(&(
                &right.entry_path,
                &right.key_path,
                right.reason,
            ))
        });
        SupportBundleManifest {
            schema_version: SUPPORT_BUNDLE_SCHEMA_VERSION,
            component: self.component,
            generated_at_unix_seconds: self.generated_at_unix_seconds,
            entries: self.entries,
            notices: self.notices,
            redactions: self.redactions,
        }
    }
}

/// Key classification result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionDecision {
    /// Preserve the value.
    Keep,
    /// Replace the value and record the reason.
    Redact(RedactionReason),
}

/// Shared deterministic document-redaction policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct SupportBundleRedactionPolicy;

impl SupportBundleRedactionPolicy {
    /// Classifies an object key or command-line option name.
    #[must_use]
    pub fn classify_key(key: &str) -> RedactionDecision {
        const SENSITIVE: &[&[u8]] = &[
            b"key",
            b"secret",
            b"token",
            b"password",
            b"credential",
            b"authorization",
            b"cookie",
            b"passphrase",
            b"private_key",
            b"xauthority",
        ];
        if SENSITIVE
            .iter()
            .any(|needle| contains_ascii_case_insensitive(key.as_bytes(), needle))
        {
            RedactionDecision::Redact(RedactionReason::SensitiveKey)
        } else {
            RedactionDecision::Keep
        }
    }
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

/// Redacts a JSON document using a stable generic entry path.
///
/// # Errors
///
/// Returns an error if a generated redaction key path exceeds schema bounds.
pub fn redact_json_document(
    document: &mut serde_json::Value,
) -> Result<Vec<RedactionRecord>, SupportBundleContractError> {
    let entry_path = BundlePath::new("document.json")?;
    redact_json_document_at(&entry_path, document)
}

/// Redacts a JSON document and attributes records to a specific archive entry.
///
/// # Errors
///
/// Returns an error if a generated redaction key path exceeds schema bounds.
pub fn redact_json_document_at(
    entry_path: &BundlePath,
    document: &mut serde_json::Value,
) -> Result<Vec<RedactionRecord>, SupportBundleContractError> {
    let mut records = Vec::new();
    redact_value(entry_path, document, "", &mut records)?;
    records.sort_by(|left, right| left.key_path.cmp(&right.key_path));
    Ok(records)
}

fn redact_value(
    entry_path: &BundlePath,
    value: &mut serde_json::Value,
    parent: &str,
    records: &mut Vec<RedactionRecord>,
) -> Result<(), SupportBundleContractError> {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                let key_path = format!("{parent}/{}", escape_json_pointer(key));
                if key_path.len() > MAX_REDACTION_KEY_PATH_BYTES {
                    return Err(SupportBundleContractError::InvalidRedactionKeyPath);
                }
                match SupportBundleRedactionPolicy::classify_key(key) {
                    RedactionDecision::Keep => {
                        redact_value(entry_path, child, &key_path, records)?;
                    }
                    RedactionDecision::Redact(reason) => {
                        *child = serde_json::Value::String(REDACTED_VALUE.to_string());
                        if records.len() >= MAX_REDACTION_RECORDS {
                            return Err(SupportBundleContractError::TooManyRedactions);
                        }
                        records.push(RedactionRecord {
                            entry_path: entry_path.clone(),
                            key_path,
                            reason,
                        });
                    }
                }
            }
        }
        serde_json::Value::Array(values) => {
            for (index, child) in values.iter_mut().enumerate() {
                let key_path = format!("{parent}/{index}");
                if key_path.len() > MAX_REDACTION_KEY_PATH_BYTES {
                    return Err(SupportBundleContractError::InvalidRedactionKeyPath);
                }
                redact_value(entry_path, child, &key_path, records)?;
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
    Ok(())
}

fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

/// Domain-separated identity classes supported by bundle pseudonymization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BundleIdentityKind {
    /// Canonical top-level user identity.
    User,
    /// Canonical top-level host identity.
    Host,
    /// Canonical top-level peer address.
    PeerAddress,
    /// Canonical network identity (`fields.ssid`).
    NetworkIdentity,
}

impl BundleIdentityKind {
    fn domain(self) -> &'static [u8] {
        match self {
            Self::User => b"user",
            Self::Host => b"host",
            Self::PeerAddress => b"peer_addr",
            Self::NetworkIdentity => b"network_identity",
        }
    }

    /// Returns the canonical JSON pointer represented by this identity class.
    #[must_use]
    pub const fn canonical_key_path(self) -> &'static str {
        match self {
            Self::User => "/user",
            Self::Host => "/host",
            Self::PeerAddress => "/peer_addr",
            Self::NetworkIdentity => "/fields/ssid",
        }
    }
}

/// Non-copyable storage filled once by a host entropy source.
pub struct BundlePseudonymKey([u8; 32]);

impl BundlePseudonymKey {
    /// Creates zeroed storage for a host to fill using its approved entropy source.
    #[must_use]
    pub const fn zeroed() -> Self {
        Self([0_u8; 32])
    }

    /// Creates deterministic key storage for tests and reproducible fixtures.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the storage for an approved entropy source to fill.
    pub const fn entropy_buffer(&mut self) -> &mut [u8; 32] {
        &mut self.0
    }
}

impl std::fmt::Debug for BundlePseudonymKey {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BundlePseudonymKey(<redacted>)")
    }
}

impl Drop for BundlePseudonymKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Per-bundle HMAC-SHA256 pseudonymizer.
///
/// The non-copyable key is moved into this value, is never exposed, and is
/// overwritten on drop. Pseudonyms deliberately preserve exact input bytes;
/// the canonical telemetry contract does not define identity normalization.
pub struct BundlePseudonymizer {
    key: BundlePseudonymKey,
}

impl BundlePseudonymizer {
    /// Takes ownership of one host-generated random 256-bit bundle key.
    #[must_use]
    pub const fn new(key: BundlePseudonymKey) -> Self {
        Self { key }
    }

    /// Returns a stable, domain-separated pseudonym for this bundle.
    #[must_use]
    pub fn pseudonymize(&self, kind: BundleIdentityKind, value: &str) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut inner_pad = [0x36_u8; 64];
        let mut outer_pad = [0x5c_u8; 64];
        for (index, byte) in self.key.0.iter().copied().enumerate() {
            inner_pad[index] ^= byte;
            outer_pad[index] ^= byte;
        }

        let mut inner = Sha256::new();
        inner.update(inner_pad.as_slice());
        inner.update(b"arcen-support-bundle-pseudonym-v1\0");
        inner.update(kind.domain());
        inner.update(b"\0");
        inner.update(value.as_bytes());
        let inner_digest = inner.finalize();
        inner_pad.fill(0);

        let mut outer = Sha256::new();
        outer.update(outer_pad.as_slice());
        outer.update(inner_digest);
        let digest = outer.finalize();
        outer_pad.fill(0);

        let mut pseudonym = String::with_capacity(69);
        pseudonym.push_str("anon:");
        for byte in digest {
            pseudonym.push(char::from(HEX[usize::from(byte >> 4)]));
            pseudonym.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        pseudonym
    }
}

impl std::fmt::Debug for BundlePseudonymizer {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BundlePseudonymizer(<redacted>)")
    }
}

/// Bounds and source-boundary state for one incremental JSONL transformation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalJsonlTransformLimits {
    /// Maximum source bytes to inspect.
    pub max_input_bytes: u64,
    /// Maximum transformed bytes to emit, always at complete-line boundaries.
    pub max_output_bytes: u64,
    /// Discard bytes through the first newline because the source starts mid-line.
    pub discard_initial_fragment: bool,
}

/// Bounded outcome of one canonical JSONL transformation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalJsonlTransformReport {
    /// Source bytes inspected.
    pub input_bytes: u64,
    /// Transformed bytes emitted.
    pub output_bytes: u64,
    /// Complete canonical records emitted.
    pub accepted_lines: u64,
    /// Complete malformed, legacy, non-v1, or noncanonical records omitted.
    pub invalid_lines: u64,
    /// Oversized records omitted.
    pub oversized_lines: u64,
    /// Initial or final incomplete fragments omitted.
    pub incomplete_lines: u64,
    /// Whether output stopped at the configured cap.
    pub output_limit_reached: bool,
    /// Identity kinds replaced at least once.
    pub redacted_kinds: Vec<BundleIdentityKind>,
}

impl CanonicalJsonlTransformReport {
    const fn new() -> Self {
        Self {
            input_bytes: 0,
            output_bytes: 0,
            accepted_lines: 0,
            invalid_lines: 0,
            oversized_lines: 0,
            incomplete_lines: 0,
            output_limit_reached: false,
            redacted_kinds: Vec::new(),
        }
    }

    fn record_kind(&mut self, kind: BundleIdentityKind) {
        if !self.redacted_kinds.contains(&kind) {
            self.redacted_kinds.push(kind);
            self.redacted_kinds.sort_unstable();
        }
    }
}

/// I/O or serialization failure during canonical JSONL transformation.
#[derive(Debug)]
pub enum CanonicalJsonlTransformError {
    /// Reading the protected local source failed.
    Read(std::io::Error),
    /// Writing transformed bytes failed.
    Write(std::io::Error),
    /// Deterministic JSON serialization failed.
    Serialize(serde_json::Error),
}

impl Display for CanonicalJsonlTransformError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(_) => formatter.write_str("read canonical JSONL source failed"),
            Self::Write(_) => formatter.write_str("write pseudonymized JSONL failed"),
            Self::Serialize(_) => formatter.write_str("serialize pseudonymized JSONL failed"),
        }
    }
}

impl Error for CanonicalJsonlTransformError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) | Self::Write(error) => Some(error),
            Self::Serialize(error) => Some(error),
        }
    }
}

/// Incrementally validates and pseudonymizes complete canonical schema-v1 JSONL records.
///
/// Malformed, legacy, oversized, and incomplete records are omitted rather
/// than copied. Memory is bounded to one fixed input buffer, one bounded line,
/// and one bounded serialized record.
///
/// # Errors
///
/// Returns a content-free I/O or serialization error. Rejected line content is
/// never included in the error.
pub fn transform_canonical_jsonl(
    mut reader: impl Read,
    mut writer: impl Write,
    pseudonymizer: &BundlePseudonymizer,
    limits: CanonicalJsonlTransformLimits,
) -> Result<CanonicalJsonlTransformReport, CanonicalJsonlTransformError> {
    let mut report = CanonicalJsonlTransformReport::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    let mut line = Vec::with_capacity(MAX_CANONICAL_JSON_LINE_BYTES);
    let mut remaining = limits.max_input_bytes;
    let mut discard = limits.discard_initial_fragment;
    let mut oversized = false;

    while remaining != 0 && !report.output_limit_reached {
        let read_size = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let count = reader
            .read(&mut buffer[..read_size])
            .map_err(CanonicalJsonlTransformError::Read)?;
        if count == 0 {
            break;
        }
        remaining -= count as u64;
        report.input_bytes += count as u64;

        for &byte in &buffer[..count] {
            if byte == b'\n' {
                if discard {
                    report.incomplete_lines += 1;
                    discard = false;
                } else if oversized {
                    report.oversized_lines += 1;
                } else {
                    emit_canonical_line(
                        &line,
                        &mut writer,
                        pseudonymizer,
                        limits.max_output_bytes,
                        &mut report,
                    )?;
                    if report.output_limit_reached {
                        break;
                    }
                }
                line.clear();
                oversized = false;
            } else if !discard && !oversized {
                if line.len() == MAX_CANONICAL_JSON_LINE_BYTES {
                    line.clear();
                    oversized = true;
                } else {
                    line.push(byte);
                }
            }
        }
    }

    if !report.output_limit_reached && (discard || oversized || !line.is_empty()) {
        if oversized {
            report.oversized_lines += 1;
        } else {
            report.incomplete_lines += 1;
        }
    }
    Ok(report)
}

fn emit_canonical_line(
    line: &[u8],
    writer: &mut impl Write,
    pseudonymizer: &BundlePseudonymizer,
    output_limit: u64,
    report: &mut CanonicalJsonlTransformReport,
) -> Result<(), CanonicalJsonlTransformError> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(line) else {
        report.invalid_lines += 1;
        return Ok(());
    };
    let Some(object) = value.as_object_mut() else {
        report.invalid_lines += 1;
        return Ok(());
    };
    if !is_canonical_record_shape(object) {
        report.invalid_lines += 1;
        return Ok(());
    }

    let mut redacted_kinds = Vec::with_capacity(4);
    for (field, kind) in [
        ("user", BundleIdentityKind::User),
        ("host", BundleIdentityKind::Host),
        ("peer_addr", BundleIdentityKind::PeerAddress),
    ] {
        if let Some(serde_json::Value::String(identity)) = object.get_mut(field) {
            *identity = pseudonymizer.pseudonymize(kind, identity);
            redacted_kinds.push(kind);
        }
    }
    let Some(fields) = object
        .get_mut("fields")
        .and_then(serde_json::Value::as_object_mut)
    else {
        report.invalid_lines += 1;
        return Ok(());
    };
    if let Some(serde_json::Value::String(identity)) = fields.get_mut("ssid") {
        *identity = pseudonymizer.pseudonymize(BundleIdentityKind::NetworkIdentity, identity);
        redacted_kinds.push(BundleIdentityKind::NetworkIdentity);
    }

    let mut rendered =
        serde_json::to_vec(&value).map_err(CanonicalJsonlTransformError::Serialize)?;
    rendered.push(b'\n');
    if rendered.len() as u64 > output_limit.saturating_sub(report.output_bytes) {
        report.output_limit_reached = true;
        return Ok(());
    }
    writer
        .write_all(&rendered)
        .map_err(CanonicalJsonlTransformError::Write)?;
    report.output_bytes += rendered.len() as u64;
    report.accepted_lines += 1;
    for kind in redacted_kinds {
        report.record_kind(kind);
    }
    Ok(())
}

fn is_canonical_record_shape(object: &serde_json::Map<String, serde_json::Value>) -> bool {
    const ALLOWED: &[&str] = &[
        "schema_version",
        "timestamp",
        "sequence",
        "profile_level",
        "profile_name",
        "severity",
        "role",
        "component",
        "platform",
        "target",
        "event_id",
        "event_name",
        "category",
        "outcome",
        "sid",
        "user",
        "host",
        "peer_addr",
        "health_state",
        "message",
        "fields",
    ];
    if object.len() > ALLOWED.len()
        || object.keys().any(|key| !ALLOWED.contains(&key.as_str()))
        || object
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(crate::CANONICAL_SCHEMA_VERSION))
        || ![
            "timestamp",
            "profile_name",
            "severity",
            "role",
            "component",
            "platform",
            "target",
            "message",
        ]
        .iter()
        .all(|key| object.get(*key).is_some_and(serde_json::Value::is_string))
        || !["sequence", "profile_level"].iter().all(|key| {
            object
                .get(*key)
                .and_then(serde_json::Value::as_u64)
                .is_some()
        })
        || !["sid", "user", "host", "peer_addr", "health_state"]
            .iter()
            .all(|key| object.get(*key).is_some_and(is_null_or_string))
        || !["event_name", "category", "outcome"]
            .iter()
            .all(|key| object.get(*key).is_none_or(serde_json::Value::is_string))
        || object
            .get("event_id")
            .is_some_and(|value| value.as_u64().is_none())
    {
        return false;
    }
    let Some(fields) = object.get("fields").and_then(serde_json::Value::as_object) else {
        return false;
    };
    fields.len() <= crate::MAX_STRUCTURED_FIELDS
        && fields.iter().all(|(key, value)| {
            !key.is_empty()
                && key.len() <= crate::MAX_FIELD_KEY_BYTES
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
                && (value.is_boolean()
                    || value.as_i64().is_some()
                    || value.as_str().is_some_and(|text| {
                        text.len() <= crate::MAX_FIELD_STRING_BYTES
                            && !text.chars().any(char::is_control)
                    }))
        })
        && fields.get("ssid").is_none_or(|value| {
            value.as_str().is_some_and(|text| {
                !text.is_empty()
                    && text.len() <= crate::MAX_NETWORK_IDENTITY_BYTES
                    && !text.chars().any(char::is_control)
            })
        })
        && ["user", "host", "peer_addr"].iter().all(|key| {
            object.get(*key).is_some_and(|value| {
                value.as_str().is_none_or(|text| {
                    !text.is_empty()
                        && text.len() <= crate::MAX_IDENTITY_BYTES
                        && !text.chars().any(char::is_control)
                })
            })
        })
}

fn is_null_or_string(value: &serde_json::Value) -> bool {
    value.is_null() || value.is_string()
}

/// Shared schema/redaction validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportBundleContractError {
    /// Archive path is unsafe or non-canonical.
    InvalidBundlePath,
    /// Digest text is not strict lowercase SHA-256 hex.
    InvalidSha256Digest,
    /// Two payloads use the same archive path.
    DuplicateEntryPath,
    /// `manifest.json` cannot hash or index itself.
    ManifestCannotIndexItself,
    /// Payload entry count exceeds the schema bound.
    TooManyEntries,
    /// Notice count exceeds the schema bound.
    TooManyNotices,
    /// Redaction count exceeds the schema bound.
    TooManyRedactions,
    /// Redaction key path is unsafe or overlong.
    InvalidRedactionKeyPath,
}

impl Display for SupportBundleContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidBundlePath => "support-bundle path is unsafe or non-canonical",
            Self::InvalidSha256Digest => "SHA-256 digest is not strict lowercase hexadecimal",
            Self::DuplicateEntryPath => "support-bundle payload path is duplicated",
            Self::ManifestCannotIndexItself => "manifest.json cannot index or hash itself",
            Self::TooManyEntries => "support-bundle payload entry count exceeds its bound",
            Self::TooManyNotices => "support-bundle notice count exceeds its bound",
            Self::TooManyRedactions => "support-bundle redaction count exceeds its bound",
            Self::InvalidRedactionKeyPath => {
                "support-bundle redaction key path is unsafe or overlong"
            }
        })
    }
}

impl Error for SupportBundleContractError {}
