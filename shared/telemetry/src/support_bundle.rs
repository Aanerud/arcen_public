// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
    /// Crash reports were left out because the bundle was not asked to
    /// include them (they can hold stack fragments such as a password).
    CrashReportsNotRequested,
    /// Crash reports were included because the bundle was asked to.
    CrashReportsRequested,
    /// The bundle was asked for crash reports, but this product does not
    /// collect them yet.
    CrashReportsNotCollected,
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
    /// The bundle's random public identifier (`ARC-XXXX-XXXX`,
    /// [`BundlePseudonymizer::bundle_id`]): what a public report and the
    /// privately sent bundle share. Absent in bundles made before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
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
    bundle_id: Option<String>,
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
            bundle_id: None,
            entries: Vec::new(),
            notices: Vec::new(),
            redactions: Vec::new(),
        }
    }

    /// Records the bundle's public identifier.
    #[must_use]
    pub fn with_bundle_id(mut self, bundle_id: String) -> Self {
        self.bundle_id = Some(bundle_id);
        self
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
            bundle_id: self.bundle_id,
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

/// Keys of a canonical record's `fields` that carry an identity, and the
/// class each is pseudonymized as (host and peer-address values by
/// [`BundlePseudonymizer::pseudonymize_address`]). The top-level `user`, `host` and
/// `peer_addr` are reserved field keys; these are the names products log the
/// same identities under inside `fields`: a sign-in name, a host's own name
/// or address (its bind or local address, a TLS server name), a peer's
/// address, and a Wi-Fi name.
pub const BUNDLE_IDENTITY_FIELDS: &[(&str, BundleIdentityKind)] = &[
    ("addr", BundleIdentityKind::Host),
    ("endpoint", BundleIdentityKind::PeerAddress),
    ("hostname", BundleIdentityKind::Host),
    ("local_addr", BundleIdentityKind::Host),
    ("peer", BundleIdentityKind::PeerAddress),
    ("remote_addr", BundleIdentityKind::PeerAddress),
    ("resolved", BundleIdentityKind::Host),
    ("server", BundleIdentityKind::Host),
    ("server_name", BundleIdentityKind::Host),
    ("ssid", BundleIdentityKind::NetworkIdentity),
    ("username", BundleIdentityKind::User),
];

/// The identity class of a canonical record's `fields` key, when its name
/// says it holds one: an exact name in [`BUNDLE_IDENTITY_FIELDS`], or a name
/// shaped like one — `…_user`, `…_username`, `…_account` (a person);
/// `…_addr`, `…_address`, `…_addresses`, `…_host`, `…hostname`, `…_domain`,
/// `uri`, `url`, `…_uri`, `…_url`, `…_endpoint` (a machine). Only string
/// values are replaced, so a count or flag with such a name stays.
#[must_use]
pub fn bundle_identity_field(key: &str) -> Option<BundleIdentityKind> {
    if let Some((_, kind)) = BUNDLE_IDENTITY_FIELDS
        .iter()
        .find(|(field, _)| *field == key)
    {
        return Some(*kind);
    }
    let ends = |suffixes: &[&str]| suffixes.iter().any(|suffix| key.ends_with(suffix));
    if key == "account" || ends(&["_user", "_username", "_account"]) {
        Some(BundleIdentityKind::User)
    } else if matches!(key, "uri" | "url")
        || ends(&[
            "_addr",
            "_address",
            "_addresses",
            "_host",
            "hostname",
            "_domain",
            "_uri",
            "_url",
            "_endpoint",
        ])
    {
        Some(BundleIdentityKind::Host)
    } else {
        None
    }
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
        let digest = self.mac(kind.domain(), value.as_bytes());
        let mut pseudonym = String::with_capacity(69);
        pseudonym.push_str("anon:");
        push_hex(&mut pseudonym, &digest);
        pseudonym
    }

    /// The one rule for a machine's name or address, wherever a bundle
    /// carries one (a Pier's or a Deck's, in a log record, a saved
    /// connection, a trusted pin or a network probe): the host part becomes
    /// its [`BundleIdentityKind::Host`] pseudonym, so the same machine reads
    /// the same in every file of one bundle; an IP literal keeps only its
    /// family and scope, and a port stays.
    ///
    /// `203.0.113.82:18444` → `anon:<hex>[ipv4-public]:18444`;
    /// `[fd00::1]:18444` → `anon:<hex>[ipv6-ula]:18444`;
    /// `render07.example.internal` → `anon:<hex>[name]`.
    #[must_use]
    pub fn pseudonymize_address(&self, value: &str) -> String {
        let (host, port) = split_host_port(value);
        let scope = host
            .parse::<std::net::IpAddr>()
            .map_or("name", |address| address_scope(address.to_canonical()));
        let mut pseudonym = self.pseudonymize(BundleIdentityKind::Host, host);
        pseudonym.push('[');
        pseudonym.push_str(scope);
        pseudonym.push(']');
        if let Some(port) = port {
            pseudonym.push(':');
            pseudonym.push_str(port);
        }
        pseudonym
    }

    /// The pseudonym for an identity of `kind`: an address for
    /// [`BundleIdentityKind::Host`] and [`BundleIdentityKind::PeerAddress`],
    /// a plain pseudonym otherwise.
    #[must_use]
    pub fn pseudonymize_identity(&self, kind: BundleIdentityKind, value: &str) -> String {
        match kind {
            BundleIdentityKind::Host | BundleIdentityKind::PeerAddress => {
                self.pseudonymize_address(value)
            }
            BundleIdentityKind::User | BundleIdentityKind::NetworkIdentity => {
                self.pseudonymize(kind, value)
            }
        }
    }

    /// `text` with every IPv4 or IPv6 literal (with or without a port) in it
    /// replaced by its [`Self::pseudonymize_address`] pseudonym; `None` when
    /// it holds none. For free text a product does not control — a
    /// dependency's error message naming a destination — where no field name
    /// says an address is there.
    #[must_use]
    pub fn pseudonymize_ip_literals(&self, text: &str) -> Option<String> {
        let is_part = |c: char| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '[' | ']');
        let mut output = String::with_capacity(text.len());
        let mut replaced = false;
        let mut rest = text;
        while let Some(start) = rest.find(is_part) {
            let (before, from) = rest.split_at(start);
            output.push_str(before);
            let end = from.find(|c: char| !is_part(c)).unwrap_or(from.len());
            let run = &from[..end];
            let (lead, candidate, trail) = ip_candidate(run);
            let preceded_by_word = lead.is_empty()
                && before
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            if !preceded_by_word && is_ip_literal(candidate) {
                output.push_str(lead);
                output.push_str(&self.pseudonymize_address(candidate));
                output.push_str(trail);
                replaced = true;
            } else {
                output.push_str(run);
            }
            rest = &from[end..];
        }
        output.push_str(rest);
        replaced.then_some(output)
    }

    /// `text` with the account name of every home folder in it replaced by
    /// its [`BundleIdentityKind::User`] pseudonym (the one a record's `user`
    /// gets), so a path keeps its shape but not the person:
    /// `C:\Users\alice\AppData\Local\Arcen` →
    /// `C:\Users\anon:<hex>\AppData\Local\Arcen`. Windows homes in any
    /// case, with backslashes, forward slashes or JSON-escaped ones, and under
    /// `Documents and Settings`; macOS `/Users/<name>`; Linux `/home/<name>`.
    /// System folders (`Public`, `Default`, `All Users`, `Shared`) stay.
    /// `None` when `text` holds none.
    #[must_use]
    pub fn pseudonymize_home_paths(&self, text: &str) -> Option<String> {
        let mut output = String::with_capacity(text.len());
        let mut copied = 0;
        let mut search = 0;
        while let Some((root_end, name_end)) = next_home_account(text, search) {
            output.push_str(&text[copied..root_end]);
            output
                .push_str(&self.pseudonymize(BundleIdentityKind::User, &text[root_end..name_end]));
            copied = name_end;
            search = name_end;
        }
        (copied != 0).then(|| {
            output.push_str(&text[copied..]);
            output
        })
    }

    /// This bundle's public identifier, `ARC-XXXX-XXXX` (40 bits, Crockford
    /// base 32): derived from the bundle's random key, so it is random per
    /// bundle and says nothing about the key or the machine.
    #[must_use]
    pub fn bundle_id(&self) -> String {
        const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let digest = self.mac(b"bundle_id", b"");
        let bits = digest[..5]
            .iter()
            .fold(0_u64, |bits, byte| (bits << 8) | u64::from(*byte));
        let mut id = String::from("ARC-");
        for index in 0..8 {
            if index == 4 {
                id.push('-');
            }
            let symbol = usize::try_from((bits >> (35 - 5 * index)) & 0x1f).unwrap_or(0);
            id.push(char::from(CROCKFORD[symbol]));
        }
        id
    }

    fn mac(&self, domain: &[u8], value: &[u8]) -> [u8; 32] {
        let mut inner_pad = [0x36_u8; 64];
        let mut outer_pad = [0x5c_u8; 64];
        for (index, byte) in self.key.0.iter().copied().enumerate() {
            inner_pad[index] ^= byte;
            outer_pad[index] ^= byte;
        }

        let mut inner = Sha256::new();
        inner.update(inner_pad.as_slice());
        inner.update(b"arcen-support-bundle-pseudonym-v1\0");
        inner.update(domain);
        inner.update(b"\0");
        inner.update(value);
        let inner_digest = inner.finalize();
        inner_pad.fill(0);

        let mut outer = Sha256::new();
        outer.update(outer_pad.as_slice());
        outer.update(inner_digest);
        let digest = outer.finalize();
        outer_pad.fill(0);
        digest.into()
    }
}

fn push_hex(text: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}

/// Folders under a home root that belong to no person.
const HOME_SYSTEM_FOLDERS: &[&str] = &["public", "default", "default user", "all users", "shared"];

/// The next home folder's account name at or after `from`, as the byte
/// range `(start, end)` of the name: it follows `/Users/`, `/home/` or a
/// Windows `X:\Users\` (any case, `\`, `\\` or `/`, or `Documents and
/// Settings`) and runs to the next separator; a name with spaces counts
/// only up to a path separator (`C:\Users\alice smith\…`), otherwise
/// its first word. Placeholders (`<user>`, `%USERNAME%`, `$USER`, `~`), an
/// existing pseudonym and system folders are skipped.
fn next_home_account(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut index = from;
    while index < bytes.len() {
        let Some(start) = home_root_end(bytes, index) else {
            index += 1;
            continue;
        };
        index = start;
        let stop =
            |byte: u8| byte.is_ascii_whitespace() || b"\\/\"'`<>%{}$*:|?,;()[]".contains(&byte);
        let mut end = start;
        while end < bytes.len() && !stop(bytes[end]) {
            end += 1;
        }
        if end == start || text[start..].starts_with("anon:") {
            continue;
        }
        let first_word = end;
        // A name with single spaces in it, ended by a path separator.
        let mut extended = end;
        while bytes.get(extended) == Some(&b' ')
            && bytes.get(extended + 1).is_some_and(|byte| !stop(*byte))
        {
            extended += 1;
            while extended < bytes.len() && !stop(bytes[extended]) {
                extended += 1;
            }
        }
        if extended != end && matches!(bytes.get(extended), Some(b'\\' | b'/')) {
            end = extended;
        }
        let name = &text[start..end];
        let first = &text[start..first_word];
        if name.bytes().all(|byte| byte == b'.')
            || HOME_SYSTEM_FOLDERS.iter().any(|folder| {
                folder.eq_ignore_ascii_case(name) || folder.eq_ignore_ascii_case(first)
            })
        {
            index = end;
            continue;
        }
        return Some((start, end));
    }
    None
}

/// Where a home root that starts at `index` ends (the account name's first
/// byte), if one does.
fn home_root_end(bytes: &[u8], index: usize) -> Option<usize> {
    let rest = &bytes[index..];
    for root in [b"/Users/".as_slice(), b"/home/".as_slice()] {
        if rest.starts_with(root) {
            return Some(index + root.len());
        }
    }
    let separators = |at: usize| {
        let mut end = at;
        while matches!(bytes.get(end), Some(b'\\' | b'/')) {
            end += 1;
        }
        (end > at).then_some(end)
    };
    if !rest.first()?.is_ascii_alphabetic()
        || rest.get(1) != Some(&b':')
        || index
            .checked_sub(1)
            .is_some_and(|before| bytes[before].is_ascii_alphanumeric())
    {
        return None;
    }
    let folder = separators(index + 2)?;
    ["users", "documents and settings"]
        .into_iter()
        .find_map(|name| {
            let candidate = bytes.get(folder..folder + name.len())?;
            candidate
                .eq_ignore_ascii_case(name.as_bytes())
                .then(|| separators(folder + name.len()))
                .flatten()
        })
}

/// Splits a run of address characters into the text before an address,
/// the address and the text after it: a leading `:` (`peer:203.0.113.5`), a
/// `[` or `]` without its partner (`[a:p, b:p]` from a list), trailing `.`
/// or `:`, and brackets around a whole `host:port` (`[203.0.113.5:443]`).
fn ip_candidate(run: &str) -> (&str, &str, &str) {
    let mut start = 0;
    let mut end = run.len();
    loop {
        let core = &run[start..end];
        if core.starts_with(':') || (core.starts_with('[') && !core.contains(']')) {
            start += 1;
        } else if core.ends_with(['.', ':']) || (core.ends_with(']') && !core.contains('[')) {
            end -= 1;
        } else {
            break;
        }
        if start >= end {
            return (run, "", "");
        }
    }
    let core = &run[start..end];
    if core.len() > 2
        && core.starts_with('[')
        && core.ends_with(']')
        && core[1..core.len() - 1].contains(':')
        && split_host_port(core).1.is_none()
        && is_ip_literal(&core[1..core.len() - 1])
    {
        start += 1;
        end -= 1;
    }
    (&run[..start], &run[start..end], &run[end..])
}

/// An IPv4 or IPv6 literal, alone or with a port, that holds a digit (so a
/// Rust path's `::` is not one).
fn is_ip_literal(candidate: &str) -> bool {
    if !candidate.bytes().any(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let (host, _) = split_host_port(candidate);
    host.parse::<std::net::IpAddr>().is_ok()
}

/// `host:port`, `[v6]:port`, a bare IPv6 literal or a bare host.
fn split_host_port(value: &str) -> (&str, Option<&str>) {
    let is_port = |port: &str| !port.is_empty() && port.parse::<u16>().is_ok();
    if let Some(rest) = value.strip_prefix('[')
        && let Some((host, after)) = rest.split_once(']')
    {
        return match after.strip_prefix(':') {
            Some(port) if is_port(port) => (host, Some(port)),
            _ => (host, None),
        };
    }
    match value.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && is_port(port) => (host, Some(port)),
        _ => (value, None),
    }
}

/// The scope an IP literal keeps in a bundle.
fn address_scope(address: std::net::IpAddr) -> &'static str {
    match address {
        std::net::IpAddr::V4(v4) => {
            let [first, second, ..] = v4.octets();
            if v4.is_unspecified() {
                "ipv4-unspecified"
            } else if v4.is_loopback() {
                "ipv4-loopback"
            } else if v4.is_private() {
                "ipv4-private"
            } else if v4.is_link_local() {
                "ipv4-link-local"
            } else if first == 100 && (64..128).contains(&second) {
                "ipv4-shared"
            } else if v4.is_multicast() || v4.is_broadcast() {
                "ipv4-multicast"
            } else {
                "ipv4-public"
            }
        }
        std::net::IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            if v6.is_unspecified() {
                "ipv6-unspecified"
            } else if v6.is_loopback() {
                "ipv6-loopback"
            } else if first & 0xfe00 == 0xfc00 {
                "ipv6-ula"
            } else if first & 0xffc0 == 0xfe80 {
                "ipv6-link-local"
            } else if v6.is_multicast() {
                "ipv6-multicast"
            } else {
                "ipv6-public"
            }
        }
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
    /// JSON pointers replaced at least once (`/user`, `/fields/username`,
    /// …), sorted: the redaction records a bundle lists for the log.
    pub redacted_key_paths: Vec<String>,
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
            redacted_key_paths: Vec::new(),
        }
    }

    fn record(&mut self, kind: BundleIdentityKind, key_path: &str) {
        if !self.redacted_kinds.contains(&kind) {
            self.redacted_kinds.push(kind);
            self.redacted_kinds.sort_unstable();
        }
        if let Err(index) = self
            .redacted_key_paths
            .binary_search_by(|known| known.as_str().cmp(key_path))
        {
            self.redacted_key_paths.insert(index, key_path.to_owned());
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

    let mut redacted = Vec::with_capacity(4);
    for (field, kind) in [
        ("user", BundleIdentityKind::User),
        ("host", BundleIdentityKind::Host),
        ("peer_addr", BundleIdentityKind::PeerAddress),
    ] {
        if let Some(serde_json::Value::String(identity)) = object.get_mut(field) {
            *identity = pseudonymizer.pseudonymize_identity(kind, identity);
            redacted.push((kind, format!("/{field}")));
        }
    }
    let Some(fields) = object
        .get_mut("fields")
        .and_then(serde_json::Value::as_object_mut)
    else {
        report.invalid_lines += 1;
        return Ok(());
    };
    for (field, value) in fields.iter_mut() {
        let serde_json::Value::String(text) = value else {
            continue;
        };
        if let Some(kind) = bundle_identity_field(field) {
            *text = pseudonymizer.pseudonymize_identity(kind, text);
            redacted.push((kind, format!("/fields/{field}")));
            continue;
        }
        if let Some(scrubbed) = pseudonymizer.pseudonymize_ip_literals(text) {
            *text = scrubbed;
            redacted.push((BundleIdentityKind::Host, format!("/fields/{field}")));
        }
        if let Some(scrubbed) = pseudonymizer.pseudonymize_home_paths(text) {
            *text = scrubbed;
            redacted.push((BundleIdentityKind::User, format!("/fields/{field}")));
        }
    }
    if let Some(serde_json::Value::String(message)) = object.get_mut("message") {
        if let Some(scrubbed) = pseudonymizer.pseudonymize_ip_literals(message) {
            *message = scrubbed;
            redacted.push((BundleIdentityKind::Host, "/message".to_owned()));
        }
        if let Some(scrubbed) = pseudonymizer.pseudonymize_home_paths(message) {
            *message = scrubbed;
            redacted.push((BundleIdentityKind::User, "/message".to_owned()));
        }
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
    for (kind, key_path) in redacted {
        report.record(kind, &key_path);
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
