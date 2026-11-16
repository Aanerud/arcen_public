// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What a bundle may say in public: a summary built from fixed, typed
//! fields, never from a payload, and never by redacting log text.
//!
//! A bundle is sensitive operational data (`docs/operations/support-bundles.md`);
//! its summary is not. [`PublicSummary`] reads the manifest's bundle ID,
//! counts, sizes, notice and redaction codes and the first segment of each
//! archive path (a product constant such as `logs` or `config`), and the
//! product's [`SummaryDetails`]: the version with the release artefact's
//! SHA-256 (never the private build ID or source revision), the OS, GPUs,
//! decoder, preset, error codes and typed facts. Words must pass
//! [`is_public_token`], labels [`is_public_label`], codes [`is_public_code`];
//! anything else prints as `<withheld>`. [`PublicSummary::to_public_text`]
//! then refuses text that still looks like an address, a host name, a home
//! folder or a name the caller forbids.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;

use arcen_telemetry::{NoticeKind, Sha256Digest, SupportBundleManifest};
use sha2::{Digest, Sha256};

/// The longest word a summary prints.
pub const MAX_PUBLIC_TOKEN_BYTES: usize = 64;
/// The most product facts one summary carries.
pub const MAX_SUMMARY_FACTS: usize = 32;
const WITHHELD: &str = "<withheld>";

/// Whether `value` may appear in a public summary: 1 to
/// [`MAX_PUBLIC_TOKEN_BYTES`] ASCII letters, digits, `.`, `_`, `+` or `-`.
/// That admits product names, versions, OS families, architectures and the
/// manifest's own vocabulary, and refuses anything with a separator a path,
/// address, account or sentence needs (`/`, `\`, `:`, `@`, space).
#[must_use]
pub fn is_public_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PUBLIC_TOKEN_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

/// Whether `value` may appear as a label (an OS, GPU or decoder name, as
/// the platform reports it): 1 to 80 ASCII letters, digits, spaces and
/// `.`, `_`, `+`, `-`, `(`, `)`, `,`; no `/`, `\`, `:` or `@`; and nothing
/// [`preflight`] refuses (an address or a dotted host name).
#[must_use]
pub fn is_public_label(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 80
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b' ' | b'.' | b'_' | b'+' | b'-' | b'(' | b')' | b',')
        })
        && preflight(value, &[]).is_ok()
}

/// Whether `value` may appear as an error code from the error-code
/// reference (`diag.udp.handshake_failed`, `LANE_REFUSED(2000)`): a token
/// with an optional parenthesised number.
#[must_use]
pub fn is_public_code(value: &str) -> bool {
    match value.split_once('(') {
        Some((code, number)) => {
            is_public_token(code)
                && number.strip_suffix(')').is_some_and(|number| {
                    !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                })
        }
        None => is_public_token(value),
    }
}

fn public(value: &str) -> &str {
    if is_public_token(value) {
        value
    } else {
        WITHHELD
    }
}

fn public_label(value: &str) -> &str {
    if is_public_label(value) {
        value.trim()
    } else {
        WITHHELD
    }
}

/// What a product states about itself in a public summary, all typed or
/// checked: nothing here comes from a log or a configuration value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SummaryDetails {
    /// The release artefact's SHA-256 (`BuildIdentityMsg::artifact_sha256`),
    /// which matches a published asset. The private build ID and source
    /// revision are never stated.
    pub artifact_sha256: Option<String>,
    /// The OS, as the platform names it (`macOS 26.1 arm64`).
    pub os: Option<String>,
    /// The GPUs, as the platform names them.
    pub gpus: Vec<String>,
    /// The decoder that answered the probe.
    pub decoder: Option<String>,
    /// What the decoder proved, by profile word (`h265`, `main10`, …).
    pub decode_profiles: Vec<(&'static str, bool)>,
    /// The streaming preset of the last session (`Auto`, `Speed`, …).
    pub preset: Option<&'static str>,
    /// Error codes from the error-code reference.
    pub codes: Vec<String>,
    /// Further typed facts.
    pub facts: Vec<SummaryFact>,
}

/// A value a product may state in public. There is no free-text variant on
/// purpose: a word must be compiled into the product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryValue {
    /// A count, duration or rate.
    Count(u64),
    /// Yes or no.
    Flag(bool),
    /// A word from the product's fixed vocabulary (a health state, a codec,
    /// a contract name).
    Token(&'static str),
}

/// One product fact for a public summary, such as the last session's
/// average frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryFact {
    /// The fact's name, from the product's fixed vocabulary.
    pub key: &'static str,
    /// Its value.
    pub value: SummaryValue,
}

impl SummaryFact {
    /// A count.
    #[must_use]
    pub const fn count(key: &'static str, value: u64) -> Self {
        Self {
            key,
            value: SummaryValue::Count(value),
        }
    }

    /// A flag.
    #[must_use]
    pub const fn flag(key: &'static str, value: bool) -> Self {
        Self {
            key,
            value: SummaryValue::Flag(value),
        }
    }

    /// A word.
    #[must_use]
    pub const fn token(key: &'static str, value: &'static str) -> Self {
        Self {
            key,
            value: SummaryValue::Token(value),
        }
    }
}

/// The SHA-256 and size of a whole archive: it ties a public summary to the
/// bundle that was sent privately, without revealing anything inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveFingerprint {
    /// SHA-256 of the archive's bytes (the plaintext `.zip`).
    pub sha256: Sha256Digest,
    /// Its length.
    pub size_bytes: u64,
}

/// Fingerprints an archive read from `reader` to its end.
///
/// # Errors
///
/// Reading failed.
pub fn fingerprint<R: Read>(mut reader: R) -> std::io::Result<ArchiveFingerprint> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; super::COPY_BUFFER_BYTES];
    let mut size_bytes = 0_u64;
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        digest.update(&buffer[..count]);
        size_bytes += count as u64;
    }
    Ok(ArchiveFingerprint {
        sha256: Sha256Digest::from_bytes(digest.finalize().into()),
        size_bytes,
    })
}

/// Fingerprints a written archive.
///
/// # Errors
///
/// The file could not be read.
pub fn fingerprint_file(path: &Path) -> std::io::Result<ArchiveFingerprint> {
    fingerprint(std::fs::File::open(path)?)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Section {
    entries: usize,
    included_bytes: u64,
    truncated: usize,
}

/// A bundle's public summary: what a user may paste into a public issue.
/// Build it with [`Self::new`], print it with [`Self::to_markdown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicSummary {
    product: String,
    version: String,
    os_family: String,
    arch: String,
    artifact: Option<String>,
    os: Option<String>,
    gpus: Vec<String>,
    decoder: Option<String>,
    decode_profiles: Vec<(String, bool)>,
    preset: Option<String>,
    codes: Vec<String>,
    schema_version: u32,
    bundle_id: Option<String>,
    archive: Option<ArchiveFingerprint>,
    sections: BTreeMap<String, Section>,
    notices: BTreeMap<(String, String, String), usize>,
    redactions: BTreeMap<String, usize>,
    facts: Vec<SummaryFact>,
}

impl PublicSummary {
    /// Summarises `manifest`, the archive's fingerprint when it was written
    /// to a file, and the product's `details` (at most
    /// [`MAX_SUMMARY_FACTS`] facts, 8 GPUs, 16 profiles and 16 codes).
    #[must_use]
    pub fn new(
        manifest: &SupportBundleManifest,
        archive: Option<ArchiveFingerprint>,
        details: &SummaryDetails,
    ) -> Self {
        let mut sections = BTreeMap::<String, Section>::new();
        for entry in &manifest.entries {
            let section = sections.entry(section_of(entry.path.as_str())).or_default();
            section.entries += 1;
            section.included_bytes += entry.included_size_bytes;
            if entry.truncation.is_some() {
                section.truncated += 1;
            }
        }
        let mut notices = BTreeMap::new();
        for notice in &manifest.notices {
            *notices
                .entry((
                    section_of(notice.source.as_str()),
                    vocabulary(serde_json::to_value(notice.kind)),
                    vocabulary(serde_json::to_value(notice.code)),
                ))
                .or_insert(0) += 1;
        }
        let mut redactions = BTreeMap::new();
        for redaction in &manifest.redactions {
            *redactions
                .entry(vocabulary(serde_json::to_value(redaction.reason)))
                .or_insert(0) += 1;
        }
        Self {
            product: public(&manifest.component.name).to_owned(),
            version: public(&manifest.component.version).to_owned(),
            os_family: public(&manifest.component.os).to_owned(),
            arch: public(&manifest.component.arch).to_owned(),
            artifact: details.artifact_sha256.as_deref().map(|hash| {
                if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    hash[..16].to_ascii_lowercase()
                } else {
                    WITHHELD.to_owned()
                }
            }),
            os: details.os.as_deref().map(|os| public_label(os).to_owned()),
            gpus: details
                .gpus
                .iter()
                .take(8)
                .map(|gpu| public_label(gpu).to_owned())
                .collect(),
            decoder: details
                .decoder
                .as_deref()
                .map(|decoder| public_label(decoder).to_owned()),
            decode_profiles: details
                .decode_profiles
                .iter()
                .take(16)
                .map(|(profile, proved)| (public(profile).to_owned(), *proved))
                .collect(),
            preset: details.preset.map(|preset| public(preset).to_owned()),
            codes: details
                .codes
                .iter()
                .take(16)
                .map(|code| {
                    if is_public_code(code) {
                        code.clone()
                    } else {
                        WITHHELD.to_owned()
                    }
                })
                .collect(),
            schema_version: manifest.schema_version,
            bundle_id: manifest
                .bundle_id
                .as_deref()
                .map(|id| public(id).to_owned()),
            archive,
            sections,
            notices,
            redactions,
            facts: details
                .facts
                .iter()
                .take(MAX_SUMMARY_FACTS)
                .copied()
                .collect(),
        }
    }

    /// Omissions: notices other than advisories and truncations.
    #[must_use]
    pub fn omissions(&self) -> usize {
        self.notices
            .iter()
            .filter(|((_, kind, _), _)| {
                kind != &vocabulary(serde_json::to_value(NoticeKind::Advisory))
                    && kind != &vocabulary(serde_json::to_value(NoticeKind::Truncated))
            })
            .map(|(_, count)| count)
            .sum()
    }

    /// The summary as GitHub-flavoured Markdown.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "### Arcen support bundle summary\n");
        let _ = writeln!(text, "| | |\n| --- | --- |");
        if let Some(bundle_id) = &self.bundle_id {
            let _ = writeln!(text, "| Bundle | {bundle_id} (not attached) |");
        }
        if let Some(archive) = self.archive {
            let _ = writeln!(
                text,
                "| Bundle SHA-256 | `{}` ({} bytes) |",
                archive.sha256, archive.size_bytes
            );
        }
        let _ = writeln!(
            text,
            "| Product | {} {} ({}, {}) |",
            self.product, self.version, self.os_family, self.arch
        );
        if let Some(artifact) = &self.artifact {
            let _ = writeln!(text, "| Artifact SHA-256 | `{artifact}…` |");
        }
        if let Some(os) = &self.os {
            let _ = writeln!(text, "| OS | {os} |");
        }
        for gpu in &self.gpus {
            let _ = writeln!(text, "| GPU | {gpu} |");
        }
        if let Some(decoder) = &self.decoder {
            let _ = writeln!(text, "| Decoder | {decoder} |");
        }
        if !self.decode_profiles.is_empty() {
            let profiles = self
                .decode_profiles
                .iter()
                .map(|(profile, proved)| {
                    format!("{profile} {}", if *proved { "yes" } else { "no" })
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(text, "| Decode | {profiles} |");
        }
        if let Some(preset) = &self.preset {
            let _ = writeln!(text, "| Preset | {preset} |");
        }
        if !self.codes.is_empty() {
            let _ = writeln!(text, "| Codes | {} |", self.codes.join(", "));
        }
        let _ = writeln!(text, "| Manifest schema | v{} |", self.schema_version);
        let _ = writeln!(text, "| Omissions | {} |", self.omissions());
        if !self.facts.is_empty() {
            let _ = writeln!(text, "\n| Fact | Value |\n| --- | --- |");
            for fact in &self.facts {
                let value = match fact.value {
                    SummaryValue::Count(value) => value.to_string(),
                    SummaryValue::Flag(value) => if value { "yes" } else { "no" }.to_owned(),
                    SummaryValue::Token(value) => public(value).to_owned(),
                };
                let _ = writeln!(text, "| {} | {value} |", public(fact.key));
            }
        }
        let _ = writeln!(
            text,
            "\n| Section | Entries | Bytes | Truncated |\n| --- | --- | --- | --- |"
        );
        for (name, section) in &self.sections {
            let _ = writeln!(
                text,
                "| {name} | {} | {} | {} |",
                section.entries, section.included_bytes, section.truncated
            );
        }
        if !self.notices.is_empty() {
            let _ = writeln!(
                text,
                "\n| Notice section | Kind | Code | Count |\n| --- | --- | --- | --- |"
            );
            for ((section, kind, code), count) in &self.notices {
                let _ = writeln!(text, "| {section} | {kind} | {code} | {count} |");
            }
        }
        if !self.redactions.is_empty() {
            let _ = writeln!(text, "\n| Redaction | Count |\n| --- | --- |");
            for (reason, count) in &self.redactions {
                let _ = writeln!(text, "| {reason} | {count} |");
            }
        }
        let _ = writeln!(
            text,
            "\nMade from fixed fields only (the manifest, version, OS, GPU, decoder, \
             preset, codes): no log lines, settings, paths, names or addresses. The \
             bundle itself stays private; send it only when a maintainer asks, the \
             way they ask."
        );
        text
    }
}

impl PublicSummary {
    /// [`Self::to_markdown`], checked once more before it may leave the
    /// computer: refused when it holds anything that reads as an IPv4 or
    /// IPv6 literal, a dotted host name, a home folder (`/Users/`, `/home/`,
    /// `C:\Users`), or one of `forbidden` (the account and computer name),
    /// case-insensitively.
    ///
    /// # Errors
    ///
    /// What was found, by kind (never the value).
    pub fn to_public_text(&self, forbidden: &[&str]) -> Result<String, PreflightRefusal> {
        let text = self.to_markdown();
        preflight(&text, forbidden)?;
        Ok(text)
    }
}

/// Why [`PublicSummary::to_public_text`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightRefusal {
    /// An IPv4 or IPv6 literal.
    Address,
    /// Something shaped like a dotted host name.
    HostName,
    /// A home-folder path.
    HomeFolder,
    /// A name the caller forbade.
    Forbidden,
}

impl std::fmt::Display for PreflightRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Address => "the summary holds an address",
            Self::HostName => "the summary holds a host name",
            Self::HomeFolder => "the summary holds a home folder",
            Self::Forbidden => "the summary holds this computer's account or name",
        })
    }
}

impl std::error::Error for PreflightRefusal {}

/// The checks of [`PublicSummary::to_public_text`] on any text.
///
/// # Errors
///
/// What was found, by kind.
pub fn preflight(text: &str, forbidden: &[&str]) -> Result<(), PreflightRefusal> {
    let lower = text.to_ascii_lowercase();
    for name in forbidden {
        let name = name.trim().to_ascii_lowercase();
        if !name.is_empty() && lower.contains(&name) {
            return Err(PreflightRefusal::Forbidden);
        }
    }
    if [
        "/users/",
        "/home/",
        "c:\\users",
        "\\users\\",
        "\\documents and settings\\",
        "/documents and settings/",
    ]
    .iter()
    .any(|home| lower.contains(home))
    {
        return Err(PreflightRefusal::HomeFolder);
    }
    for word in text.split(|c: char| {
        !(c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_' | '[' | ']'))
    }) {
        let word = word.trim_matches(|c: char| matches!(c, '.' | ':' | '[' | ']' | '-'));
        if word.is_empty() {
            continue;
        }
        let bare = word
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(word);
        if bare.parse::<std::net::IpAddr>().is_ok()
            || bare
                .rsplit_once(':')
                .is_some_and(|(host, _)| host.parse::<std::net::Ipv4Addr>().is_ok())
        {
            return Err(PreflightRefusal::Address);
        }
        if looks_like_host_name(word) {
            return Err(PreflightRefusal::HostName);
        }
    }
    Ok(())
}

/// `a.b.tld`: at least three dot-separated labels of letters, digits and
/// hyphens, the last alphabetic. Codes carry `_` and versions end in digits,
/// so neither matches.
fn looks_like_host_name(word: &str) -> bool {
    let labels: Vec<&str> = word.split('.').collect();
    labels.len() >= 3
        && labels.iter().all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        && labels
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.bytes().all(|byte| byte.is_ascii_alphabetic()))
}

fn section_of(path: &str) -> String {
    public(path.split('/').next().unwrap_or_default()).to_owned()
}

/// A manifest enum's serde name (`source_not_found`, `sensitive_key`).
fn vocabulary(value: Result<serde_json::Value, serde_json::Error>) -> String {
    value
        .ok()
        .and_then(|value| value.as_str().map(public).map(str::to_owned))
        .unwrap_or_else(|| WITHHELD.to_owned())
}
