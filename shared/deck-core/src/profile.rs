// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Connection profile files (`.arcen`): one saved connection, shared.
//!
//! A profile is a small JSON object that names a machine and how this Deck
//! should connect to it:
//!
//! ```json
//! {
//!   "format": "arcen-connection",
//!   "version": 1,
//!   "name": "Studio workstation",
//!   "address": "pier.example.internal",
//!   "port": 18444,
//!   "username": "alice",
//!   "overrides": { "typing_mode": "keys", "displays_mode": "windowed" },
//!   "host_identity": { "kind": "spki_sha256", "digest": "<64 hex digits>" }
//! }
//! ```
//!
//! `overrides` is [`ConnectionOverrides`] under the keys `settings.json`
//! uses ([`crate::overrides::OverridableSetting::key`]); every setting it
//! does not name follows the importing Deck's Settings, and the Pier's own
//! policy still decides what a session may do.
//!
//! What a profile never carries: a password, a token, a resume grant, a
//! local file path or anything else that identifies this machine's secrets.
//! Export writes only the fields above (and fields an earlier import kept,
//! filtered the same way); import drops any field whose name or value looks
//! like one ([`withheld`]) and says so.
//!
//! The host identity (the TLS pin) is a claim made by whoever wrote the
//! file. Import never trusts it by itself: [`review_import`] compares it
//! with what this Deck already trusts ([`PinReview`]) and
//! [`commit_import`] remembers it only when the user confirmed it
//! ([`ImportDecision::trust_host_identity`], off by default). Nothing is
//! written until then, and an existing connection to the same machine is
//! never overwritten silently: the user replaces it or keeps both.

use std::fmt::{self, Write as _};
use std::io::Read as _;
use std::path::Path;

use arcen_transport::tls::{PinKind, TlsPin};
use serde_json::{Map, Value};

use crate::config::{
    ConnectionKind, SavedConnectionSettings, SavedConnectionSummary, SavedTransport,
    normalized_connection_host,
};
use crate::overrides::{ConnectionOverrides, OverridableSetting, Override};
use crate::settings::{ClientConfigRepository, DisplaysMode, SavedConnectionsStore, SecurityMode};
use crate::trusted_pins::{StoredPin, TrustedPinStore};

/// The file extension, without the dot.
pub const PROFILE_EXTENSION: &str = "arcen";
/// The value of `"format"` that marks a connection profile.
pub const PROFILE_FORMAT: &str = "arcen-connection";
/// The largest file import reads. A profile is a few hundred bytes.
pub const MAX_PROFILE_BYTES: u64 = 64 * 1024;
/// The deepest nesting import accepts. Known fields nest two levels.
pub const MAX_PROFILE_DEPTH: usize = 16;
/// The longest connection name, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// The longest DNS name.
const MAX_HOST_BYTES: usize = 253;
/// Where a saved connection keeps the fields an import did not understand
/// (`{"fields": {...}, "overrides": {...}}`), so a later export writes them
/// back. Namespaced so a file can never set a key `connections.json` reads.
pub const PROFILE_UNKNOWN_KEY: &str = "profile_unknown";
/// How many withheld field names a summary lists.
const MAX_LISTED_FIELDS: usize = 12;

const KNOWN_FIELDS: [&str; 8] = [
    "format",
    "version",
    "name",
    "address",
    "port",
    "username",
    "overrides",
    "host_identity",
];

/// The profile format version. Additions within a version are additive
/// (older Decks keep fields they do not know); a change an older Deck
/// would misread raises it, and an older Deck refuses the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProfileVersion(u32);

impl ProfileVersion {
    /// The version this Deck writes and the newest it reads.
    pub const CURRENT: Self = Self(1);

    /// The version a file states, if this Deck can read it.
    ///
    /// # Errors
    ///
    /// [`ProfileError::NewerVersion`] for a version above [`Self::CURRENT`],
    /// [`ProfileError::NotAProfile`] for zero.
    pub const fn parse(version: u64) -> Result<Self, ProfileError> {
        if version == 0 {
            return Err(ProfileError::NotAProfile);
        }
        if version > Self::CURRENT.0 as u64 {
            return Err(ProfileError::NewerVersion { found: version });
        }
        #[allow(clippy::cast_possible_truncation, reason = "bounded by CURRENT above")]
        Ok(Self(version as u32))
    }

    /// The number written to a file.
    #[must_use]
    pub const fn number(self) -> u32 {
        self.0
    }
}

/// A validated machine address: a host name or IP address and a UDP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileAddress {
    host: String,
    port: u16,
}

impl ProfileAddress {
    /// Validates `host` and `port`. Host names are lower-cased and IP
    /// addresses written in their canonical form, without brackets.
    ///
    /// # Errors
    ///
    /// [`ProfileError::InvalidAddress`] or [`ProfileError::InvalidPort`].
    pub fn new(host: &str, port: u16) -> Result<Self, ProfileError> {
        if port == 0 {
            return Err(ProfileError::InvalidPort);
        }
        Ok(Self {
            host: validated_host(host)?,
            port,
        })
    }

    /// The host name or address.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The UDP port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// `host:port`, the key the trust store keeps a host identity under
    /// ([`crate::connection_draft::ConnectionDraft::endpoint`]).
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Whether `connection` names the same machine.
    #[must_use]
    pub fn names(&self, connection: &SavedConnectionSummary) -> bool {
        connection.port == self.port && normalized_connection_host(&connection.host) == self.host
    }
}

impl fmt::Display for ProfileAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(formatter, "[{}]:{}", self.host, self.port)
        } else {
            write!(formatter, "{}:{}", self.host, self.port)
        }
    }
}

fn validated_host(raw: &str) -> Result<String, ProfileError> {
    let trimmed = raw.trim();
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(trimmed);
    if let Ok(address) = unbracketed.parse::<std::net::IpAddr>() {
        let unusable = address.is_unspecified()
            || address.is_multicast()
            || matches!(address, std::net::IpAddr::V4(v4) if v4.is_broadcast());
        if unusable {
            return Err(ProfileError::InvalidAddress);
        }
        return Ok(address.to_string());
    }
    let name = unbracketed.strip_suffix('.').unwrap_or(unbracketed);
    let labels_ok = !name.is_empty()
        && name.len() <= MAX_HOST_BYTES
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
        // A dotted all-digit name is a mistyped IP address, not a host.
        && !name
            .rsplit('.')
            .next()
            .is_some_and(|last| last.bytes().all(|byte| byte.is_ascii_digit()));
    if !labels_ok {
        return Err(ProfileError::InvalidAddress);
    }
    Ok(name.to_ascii_lowercase())
}

/// The host identity a profile states: a typed SHA-256 TLS pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostFingerprint(TlsPin);

impl HostFingerprint {
    /// Wraps a pin.
    #[must_use]
    pub const fn new(pin: TlsPin) -> Self {
        Self(pin)
    }

    /// Reads `{"kind": "spki_sha256" | "certificate_sha256", "digest": hex}`.
    ///
    /// # Errors
    ///
    /// [`ProfileError::InvalidHostIdentity`] for any other shape, kind or
    /// digest length.
    pub fn from_json(value: &Value) -> Result<Self, ProfileError> {
        let object = value.as_object().ok_or(ProfileError::InvalidHostIdentity)?;
        if object.keys().any(|key| key != "kind" && key != "digest") {
            return Err(ProfileError::InvalidHostIdentity);
        }
        let (Some(Value::String(kind)), Some(Value::String(digest))) =
            (object.get("kind"), object.get("digest"))
        else {
            return Err(ProfileError::InvalidHostIdentity);
        };
        let stored = StoredPin {
            kind: kind.clone(),
            digest: digest.clone(),
            pinned_at: None,
            label: None,
        };
        // `to_pin` accepts exactly 64 hex digits of a known kind.
        stored
            .to_pin()
            .map(Self)
            .ok_or(ProfileError::InvalidHostIdentity)
    }

    /// The JSON a profile writes.
    #[must_use]
    pub fn to_json(self) -> Value {
        let stored = StoredPin::from_pin(&self.0, None, None);
        serde_json::json!({ "kind": stored.kind, "digest": stored.digest })
    }

    /// The pin.
    #[must_use]
    pub const fn pin(self) -> TlsPin {
        self.0
    }

    /// Upper-case hex pairs separated by colons, as the certificate screen
    /// shows them.
    #[must_use]
    pub fn display(self) -> String {
        self.0
            .digest
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// What the fingerprint is of, in words.
    #[must_use]
    pub const fn kind_label(self) -> &'static str {
        match self.0.kind {
            PinKind::SubjectPublicKeyInfoSha256 => "public key (SHA-256)",
            PinKind::CertificateSha256 => "certificate (SHA-256)",
        }
    }
}

/// Why a profile could not be read, written or imported. Each message is
/// for the person who chose the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// The file is larger than [`MAX_PROFILE_BYTES`].
    TooLarge,
    /// The path is a folder, device or pipe, not a file.
    NotAFile,
    /// Reading the file failed.
    Unreadable(String),
    /// The file is not JSON.
    NotJson,
    /// The file nests deeper than [`MAX_PROFILE_DEPTH`].
    TooDeep,
    /// The file is JSON, but not a connection profile.
    NotAProfile,
    /// A newer Arcen wrote the file in a format this Deck cannot read.
    NewerVersion {
        /// The version the file states.
        found: u64,
    },
    /// A required field is absent.
    Missing(&'static str),
    /// A known field has the wrong JSON type.
    WrongType {
        /// The field.
        field: &'static str,
        /// What it must be, in words.
        expected: &'static str,
    },
    /// The name is empty, too long or has invisible control characters.
    InvalidName,
    /// The address is not a host name or usable IP address.
    InvalidAddress,
    /// The port is not 1 to 65535.
    InvalidPort,
    /// The username is too long or has invisible control characters.
    InvalidUsername,
    /// The host identity is not a 64-digit SHA-256 fingerprint of a known
    /// kind.
    InvalidHostIdentity,
    /// Writing the file failed.
    Unwritable(String),
    /// Saving the imported connection failed.
    NotSaved(String),
    /// The saved connections changed after the summary was shown.
    Changed,
    /// Another Deck window changed the machine's trusted identity after
    /// the summary was shown: nothing was saved, and the summary is shown
    /// again with the identity trusted now.
    IdentityChanged,
    /// The connection would make a file larger than [`MAX_PROFILE_BYTES`],
    /// which no Deck imports. Nothing was written.
    TooLargeToExport,
    /// The import needs a choice: replace the existing connection or keep
    /// both.
    ChoiceRequired,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str(
                "This file is too large to be an Arcen connection (the limit is 64 KB).",
            ),
            Self::NotAFile => formatter.write_str("This is a folder or a device, not a file."),
            Self::Unreadable(error) => write!(formatter, "The file could not be read: {error}"),
            Self::NotJson | Self::NotAProfile => {
                formatter.write_str("This is not an Arcen connection file.")
            }
            Self::TooDeep => formatter.write_str(
                "This is not an Arcen connection file: its contents are nested too deeply.",
            ),
            Self::NewerVersion { found } => write!(
                formatter,
                "This connection was saved by a newer version of Arcen (format {found}). \
                 Update Arcen Deck to import it."
            ),
            Self::Missing(field) => write!(
                formatter,
                "This connection file has no {}.",
                field_words(field)
            ),
            Self::WrongType { field, expected } => write!(
                formatter,
                "This connection file is damaged: its {} must be {expected}.",
                field_words(field)
            ),
            Self::InvalidName => formatter.write_str(
                "The connection's name is empty, longer than 128 bytes, or contains \
                 invisible control characters.",
            ),
            Self::InvalidAddress => {
                formatter.write_str("The address is not a valid host name or IP address.")
            }
            Self::InvalidPort => formatter.write_str("The port must be a number from 1 to 65535."),
            Self::InvalidUsername => formatter.write_str(
                "The username is longer than 255 bytes or contains invisible control \
                 characters.",
            ),
            Self::InvalidHostIdentity => formatter
                .write_str("The host identity in this file is not a valid SHA-256 fingerprint."),
            Self::Unwritable(error) => write!(formatter, "The file could not be saved: {error}"),
            Self::NotSaved(error) => write!(formatter, "The connection was not saved: {error}"),
            Self::Changed => formatter.write_str(
                "Your saved connections changed while the summary was open. Import the file \
                 again.",
            ),
            Self::IdentityChanged => formatter.write_str(
                "The identity this Deck trusts for this machine changed in another Arcen Deck \
                 window. Nothing was saved: check it again below.",
            ),
            Self::TooLargeToExport => formatter.write_str(
                "This connection carries too much extra data to fit in a connection file (the \
                 limit is 64 KB). Nothing was written.",
            ),
            Self::ChoiceRequired => formatter.write_str(
                "A connection to this machine already exists: choose Replace or Keep both.",
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

const fn field_words(field: &str) -> &str {
    match field.as_bytes() {
        b"name" => "name",
        b"address" => "address",
        b"port" => "port",
        b"username" => "username",
        b"overrides" => "settings",
        b"host_identity" => "host identity",
        b"version" => "format version",
        _ => field,
    }
}

/// One connection, as a profile file states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionProfile {
    /// The format version the file was read as (always
    /// [`ProfileVersion::CURRENT`] when written).
    pub version: ProfileVersion,
    /// What the connection is called.
    pub name: String,
    /// The machine.
    pub address: ProfileAddress,
    /// The account to sign in as; the password is always asked for.
    pub username: Option<String>,
    /// This connection's own settings.
    pub overrides: ConnectionOverrides,
    /// The host identity the author trusted. Never trusted on import
    /// without the user's confirmation.
    pub host_identity: Option<HostFingerprint>,
    /// Top-level fields this Deck does not understand, kept for export.
    pub unknown: Map<String, Value>,
    /// `overrides` keys (or values) this Deck does not understand, kept.
    pub unknown_overrides: Map<String, Value>,
    /// Fields dropped because they looked like a password, token, grant or
    /// local path (dotted names).
    pub withheld: Vec<String>,
    /// Fields dropped because their names are not plain text (control,
    /// invisible or reordering characters, or longer than
    /// [`MAX_FIELD_NAME_BYTES`]); counted, never shown.
    pub unreadable_fields: usize,
}

impl ConnectionProfile {
    /// Reads a profile from the bytes of a file.
    ///
    /// # Errors
    ///
    /// Every [`ProfileError`] a file can cause: size, JSON, depth, shape,
    /// version and each field's type and value.
    pub fn parse(bytes: &[u8]) -> Result<Self, ProfileError> {
        if bytes.len() as u64 > MAX_PROFILE_BYTES {
            return Err(ProfileError::TooLarge);
        }
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        let value = serde_json::from_slice::<Value>(bytes).map_err(|error| {
            // serde_json stops at its own recursion limit (128) first.
            if error.to_string().contains("recursion limit") {
                ProfileError::TooDeep
            } else {
                ProfileError::NotJson
            }
        })?;
        if depth(&value) > MAX_PROFILE_DEPTH {
            return Err(ProfileError::TooDeep);
        }
        let Value::Object(mut object) = value else {
            return Err(ProfileError::NotAProfile);
        };
        if object.get("format").and_then(Value::as_str) != Some(PROFILE_FORMAT) {
            return Err(ProfileError::NotAProfile);
        }
        let version = match object.get("version") {
            None => return Err(ProfileError::Missing("version")),
            Some(Value::Number(number)) => {
                ProfileVersion::parse(number.as_u64().ok_or(ProfileError::NotAProfile)?)?
            }
            Some(_) => {
                return Err(ProfileError::WrongType {
                    field: "version",
                    expected: "a whole number",
                });
            }
        };
        let name = validated_name(required_string(&object, "name")?)?;
        let host = required_string(&object, "address")?;
        let port = match object.get("port") {
            None | Some(Value::Null) => SavedTransport::Quic.default_port(),
            Some(Value::Number(number)) => number
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port != 0)
                .ok_or(ProfileError::InvalidPort)?,
            Some(_) => {
                return Err(ProfileError::WrongType {
                    field: "port",
                    expected: "a number",
                });
            }
        };
        let address = ProfileAddress::new(host, port)?;
        let username = match object.get("username") {
            None | Some(Value::Null) => None,
            Some(Value::String(username)) if username.trim().is_empty() => None,
            Some(Value::String(username)) => Some(validated_username(username)?),
            Some(_) => {
                return Err(ProfileError::WrongType {
                    field: "username",
                    expected: "text",
                });
            }
        };
        let host_identity = match object.get("host_identity") {
            None | Some(Value::Null) => None,
            Some(value) => Some(HostFingerprint::from_json(value)?),
        };
        let mut dropped = Dropped::default();
        let (overrides, unknown_overrides) = match object.remove("overrides") {
            None | Some(Value::Null) => (ConnectionOverrides::default(), Map::new()),
            Some(Value::Object(overrides)) => read_overrides(overrides, &mut dropped)?,
            Some(_) => {
                return Err(ProfileError::WrongType {
                    field: "overrides",
                    expected: "an object",
                });
            }
        };
        for field in KNOWN_FIELDS {
            object.remove(field);
        }
        let unknown = without_withheld(object, "", &mut dropped);
        Ok(Self {
            version,
            name,
            address,
            username,
            overrides,
            host_identity,
            unknown,
            unknown_overrides,
            withheld: dropped.withheld,
            unreadable_fields: dropped.unreadable,
        })
    }

    /// Reads a profile file, refusing anything but a regular file of at
    /// most [`MAX_PROFILE_BYTES`].
    ///
    /// # Errors
    ///
    /// [`ProfileError::NotAFile`], [`ProfileError::TooLarge`],
    /// [`ProfileError::Unreadable`] or any error of [`Self::parse`].
    pub fn read(path: &Path) -> Result<Self, ProfileError> {
        let unreadable = |error: std::io::Error| ProfileError::Unreadable(error.to_string());
        let metadata = std::fs::metadata(path).map_err(unreadable)?;
        if !metadata.is_file() {
            return Err(ProfileError::NotAFile);
        }
        if metadata.len() > MAX_PROFILE_BYTES {
            return Err(ProfileError::TooLarge);
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(unreadable)?
            .take(MAX_PROFILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(unreadable)?;
        Self::parse(&bytes)
    }

    /// The profile of a saved connection. The host identity is this Deck's
    /// remembered one for its address, if any; the fields an earlier import
    /// kept come back, filtered again. Nothing else of the connection is
    /// written: no password, grant, path or other setting.
    ///
    /// # Errors
    ///
    /// [`ProfileError::InvalidName`], [`ProfileError::InvalidAddress`] or
    /// [`ProfileError::InvalidPort`] when the saved connection would not
    /// import.
    pub fn from_saved(
        connection: &SavedConnectionSummary,
        pins: &TrustedPinStore,
    ) -> Result<Self, ProfileError> {
        let address = ProfileAddress::new(&connection.host, connection.port)?;
        let resolved = pins.resolved();
        let host_identity = resolved
            .get(&format!("{}:{}", connection.host, connection.port))
            .or_else(|| resolved.get(&address.endpoint()))
            .copied()
            .map(HostFingerprint);
        let kept = connection
            .unknown
            .get(PROFILE_UNKNOWN_KEY)
            .and_then(Value::as_object);
        let kept_part = |part: &str| {
            kept.and_then(|kept| kept.get(part))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
        };
        let mut dropped = Dropped::default();
        let unknown = without_withheld(kept_part("fields"), "", &mut dropped);
        let unknown_overrides =
            without_withheld(kept_part("overrides"), "overrides.", &mut dropped);
        Ok(Self {
            version: ProfileVersion::CURRENT,
            name: validated_name(&connection.name)?,
            address,
            username: connection.settings.remembered_username.clone(),
            overrides: connection.settings.overrides,
            host_identity,
            unknown,
            unknown_overrides,
            withheld: dropped.withheld,
            unreadable_fields: dropped.unreadable,
        })
    }

    /// The file's JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        for (key, value) in &self.unknown {
            if !KNOWN_FIELDS.contains(&key.as_str()) {
                object.insert(key.clone(), value.clone());
            }
        }
        object.insert("format".into(), PROFILE_FORMAT.into());
        object.insert("version".into(), ProfileVersion::CURRENT.number().into());
        object.insert("name".into(), self.name.clone().into());
        object.insert("address".into(), self.address.host().into());
        object.insert("port".into(), self.address.port().into());
        if let Some(username) = &self.username {
            object.insert("username".into(), username.clone().into());
        }
        let mut overrides = Map::new();
        self.overrides.write_into(&mut overrides);
        for (key, value) in &self.unknown_overrides {
            overrides
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        if !overrides.is_empty() {
            object.insert("overrides".into(), overrides.into());
        }
        if let Some(identity) = self.host_identity {
            object.insert("host_identity".into(), identity.to_json());
        }
        Value::Object(object)
    }

    /// The file's bytes: pretty JSON with a final newline.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(&self.to_json()).unwrap_or_default();
        bytes.push(b'\n');
        bytes
    }

    /// The bytes [`Self::write`] writes: [`Self::to_bytes`], or compact
    /// JSON when the pretty form would be larger than any Deck imports.
    ///
    /// # Errors
    ///
    /// [`ProfileError::TooLargeToExport`] when even the compact form is.
    pub fn file_bytes(&self) -> Result<Vec<u8>, ProfileError> {
        let fits =
            |bytes: &[u8]| u64::try_from(bytes.len()).is_ok_and(|len| len <= MAX_PROFILE_BYTES);
        let pretty = self.to_bytes();
        if fits(&pretty) {
            return Ok(pretty);
        }
        let mut compact = serde_json::to_vec(&self.to_json()).unwrap_or_default();
        compact.push(b'\n');
        if fits(&compact) {
            Ok(compact)
        } else {
            Err(ProfileError::TooLargeToExport)
        }
    }

    /// Writes the profile to `path` atomically (a temporary file in the
    /// same folder, synced, then renamed over it), only when the file is
    /// one a Deck can import: otherwise `path` is left as it was.
    ///
    /// # Errors
    ///
    /// [`ProfileError::TooLargeToExport`], [`ProfileError::Unwritable`].
    pub fn write(&self, path: &Path) -> Result<(), ProfileError> {
        let bytes = self.file_bytes()?;
        crate::config::atomic_write_json(path, &bytes)
            .map_err(|error| ProfileError::Unwritable(error.to_string()))
    }
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ProfileError> {
    match object.get(field) {
        None | Some(Value::Null) => Err(ProfileError::Missing(field)),
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(ProfileError::WrongType {
            field,
            expected: "text",
        }),
    }
}

/// Characters that reorder or hide text: a name or username carrying them
/// could show one thing in the summary and be another.
fn is_invisible_or_reordering(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

fn validated_name(name: &str) -> Result<String, ProfileError> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.chars().any(is_invisible_or_reordering)
    {
        return Err(ProfileError::InvalidName);
    }
    Ok(name.to_owned())
}

fn validated_username(username: &str) -> Result<String, ProfileError> {
    if username.chars().any(is_invisible_or_reordering) {
        return Err(ProfileError::InvalidUsername);
    }
    crate::config::validated_remembered_username(username).ok_or(ProfileError::InvalidUsername)
}

fn depth(value: &Value) -> usize {
    match value {
        Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        Value::Object(fields) => 1 + fields.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Reads `overrides` strictly: a known key must have its JSON type; a
/// value of the right type this Deck does not understand (a newer Deck's
/// choice) is kept, not applied.
fn read_overrides(
    overrides: Map<String, Value>,
    dropped: &mut Dropped,
) -> Result<(ConnectionOverrides, Map<String, Value>), ProfileError> {
    let mut known = Map::new();
    let mut unknown = Map::new();
    for (key, value) in overrides {
        let Some(setting) = OverridableSetting::ALL
            .into_iter()
            .find(|setting| setting.key() == key)
        else {
            unknown.insert(key, value);
            continue;
        };
        let understood = match (setting, &value) {
            (_, Value::Null) => continue,
            (
                OverridableSetting::ClipboardEnabled | OverridableSetting::SystemShortcuts,
                Value::Bool(_),
            ) => true,
            (OverridableSetting::ClipboardEnabled | OverridableSetting::SystemShortcuts, _) => {
                return Err(ProfileError::WrongType {
                    field: "overrides",
                    expected: "true or false for clipboard_enabled and system_shortcuts",
                });
            }
            (_, Value::String(token)) => token_understood(setting, token),
            (_, _) => {
                return Err(ProfileError::WrongType {
                    field: "overrides",
                    expected: "text for each setting but clipboard_enabled and system_shortcuts",
                });
            }
        };
        if understood {
            known.insert(key, value);
        } else {
            unknown.insert(key, value);
        }
    }
    let unknown = without_withheld(unknown, "overrides.", dropped);
    Ok((ConnectionOverrides::take_from(&mut known), unknown))
}

fn token_understood(setting: OverridableSetting, token: &str) -> bool {
    match setting {
        OverridableSetting::TypingMode => {
            arcen_input::text::TypingMode::from_token(token).is_some()
        }
        OverridableSetting::OptionKeys => {
            arcen_input::text::OptionKeyPreference::from_token(token).is_some()
        }
        OverridableSetting::KeyboardShortcuts => {
            arcen_input::keyboard::ShortcutPreference::from_token(token).is_some()
        }
        OverridableSetting::DisplaysMode => DisplaysMode::from_wire(token).is_some(),
        OverridableSetting::ClipboardEnabled | OverridableSetting::SystemShortcuts => false,
    }
}

/// Whether a field must never travel in a profile: its name says it is a
/// password, token, grant, key or local path.
fn withheld(key: &str) -> bool {
    const ANYWHERE: [&str; 8] = [
        "password",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "credential",
        "apikey",
        "privatekey",
    ];
    const WORDS: [&str; 18] = [
        "pwd",
        "pass",
        "grant",
        "grants",
        "cookie",
        "cookies",
        "private",
        "otp",
        "resume",
        "auth",
        "key",
        "path",
        "paths",
        "file",
        "files",
        "dir",
        "directory",
        "folder",
    ];
    let lower = key.to_ascii_lowercase();
    let compact: String = lower.chars().filter(char::is_ascii_alphanumeric).collect();
    if ANYWHERE.iter().any(|word| compact.contains(word)) {
        return true;
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in key.chars() {
        let boundary = !character.is_ascii_alphanumeric()
            || (character.is_ascii_uppercase() && previous_lower);
        if boundary && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        if character.is_ascii_alphanumeric() {
            current.push(character.to_ascii_lowercase());
        }
        previous_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
    }
    if !current.is_empty() {
        words.push(current);
    }
    words.iter().any(|word| WORDS.contains(&word.as_str()))
}

/// Whether a string value is a local path (or a file URL).
fn looks_like_local_path(value: &str) -> bool {
    let value = value.trim();
    let bytes = value.as_bytes();
    value.starts_with('/')
        || value.starts_with("~/")
        || value.starts_with("\\\\")
        || value.to_ascii_lowercase().starts_with("file:")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/'))
}

/// The longest field name a profile keeps.
pub const MAX_FIELD_NAME_BYTES: usize = 64;

/// What a filter dropped.
#[derive(Debug, Default)]
struct Dropped {
    /// Dotted names of fields withheld as secrets or paths.
    withheld: Vec<String>,
    /// Fields whose names are not plain text.
    unreadable: usize,
}

/// Whether a field name is plain text: short, with no control, invisible
/// or reordering character that could make the summary show something
/// that is not there.
fn readable_field_name(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_FIELD_NAME_BYTES
        && !key.chars().any(is_invisible_or_reordering)
}

/// Whether the summary may show a field name as it is: ASCII letters,
/// digits, `_`, `.` and `-` only. Any other name is only counted.
fn displayable_field_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// `fields` without anything [`withheld`], path-like or with an unreadable
/// name, at any depth; what was dropped is recorded in `dropped`.
fn without_withheld(
    fields: Map<String, Value>,
    prefix: &str,
    dropped: &mut Dropped,
) -> Map<String, Value> {
    fields
        .into_iter()
        .filter_map(|(key, value)| {
            if !readable_field_name(&key) {
                dropped.unreadable += 1;
                return None;
            }
            let name = format!("{prefix}{key}");
            if withheld(&key) {
                dropped.withheld.push(name);
                return None;
            }
            filtered_value(value, &name, dropped).map(|value| (key, value))
        })
        .collect()
}

fn filtered_value(value: Value, name: &str, dropped: &mut Dropped) -> Option<Value> {
    match value {
        Value::String(text) if looks_like_local_path(&text) => {
            dropped.withheld.push(name.to_owned());
            None
        }
        Value::Object(fields) => Some(Value::Object(without_withheld(
            fields,
            &format!("{name}."),
            dropped,
        ))),
        Value::Array(items) => Some(Value::Array(
            items
                .into_iter()
                .filter_map(|item| filtered_value(item, name, dropped))
                .collect(),
        )),
        other => Some(other),
    }
}

/// A file name for `name`'s profile: letters, digits, spaces, `-`, `_`
/// and `.`, at most 64 characters, then `.arcen`.
#[must_use]
pub fn suggested_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let cleaned =
        cleaned.trim_matches(|character: char| character == '.' || character.is_whitespace());
    let stem = if cleaned.is_empty() {
        "connection"
    } else {
        cleaned
    };
    format!("{stem}.{PROFILE_EXTENSION}")
}

/// The saved connection an import would collide with.
#[derive(Debug, Clone, PartialEq)]
pub struct ExistingConnection {
    /// Its position in the saved connections.
    pub index: usize,
    /// Exactly as it was when the summary was made (an update checks it).
    pub connection: SavedConnectionSummary,
}

/// The host identity in a profile, against what this Deck trusts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinReview {
    /// The file states no identity: the first connection asks, as always.
    NotIncluded,
    /// The file's identity is the one this Deck already trusts.
    AlreadyTrusted(HostFingerprint),
    /// This Deck trusts no identity for the machine yet.
    New(HostFingerprint),
    /// This Deck trusts a different identity for the machine. Trusting the
    /// file's replaces it; that needs both fingerprints shown.
    Differs {
        /// The identity this Deck trusts now.
        saved: HostFingerprint,
        /// The identity the file states.
        file: HostFingerprint,
    },
}

impl PinReview {
    /// Whether trusting the file's identity would change what this Deck
    /// trusts, so the user must decide.
    #[must_use]
    pub const fn needs_decision(self) -> bool {
        matches!(self, Self::New(_) | Self::Differs { .. })
    }
}

/// How the user wants a colliding import saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportChoice {
    /// No connection collides: add it.
    Add,
    /// Replace the existing connection to the same machine.
    Replace,
    /// Keep the existing connection and add this one beside it.
    KeepBoth,
}

/// The user's answer to an import summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportDecision {
    /// Where the connection goes.
    pub choice: ImportChoice,
    /// Whether to trust the file's host identity. Default no.
    pub trust_host_identity: bool,
}

/// One line of an import summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryLine {
    /// What it is.
    pub label: String,
    /// Its value, in words.
    pub value: String,
}

/// A parsed profile waiting for the user: the pending connection's facts,
/// what it collides with and what its identity means. Nothing is saved
/// until [`commit_import`].
#[derive(Debug, Clone, PartialEq)]
pub struct ImportReview {
    /// The file's contents.
    pub profile: ConnectionProfile,
    /// The saved connection to the same machine, if any.
    pub existing: Option<ExistingConnection>,
    /// The file's host identity against this Deck's.
    pub pin: PinReview,
    /// The identity remembered, as the summary was made, under the key the
    /// imported connection's identity is held to
    /// ([`Self::identity_endpoint`]). Saving one checks it is still that.
    pub endpoint_pin: Option<TlsPin>,
}

/// Compares a profile with the saved connections and remembered
/// identities. Pure: reads nothing, writes nothing.
#[must_use]
pub fn review_import(
    profile: ConnectionProfile,
    saved: &[SavedConnectionSummary],
    pins: &TrustedPinStore,
) -> ImportReview {
    let existing = saved
        .iter()
        .position(|connection| profile.address.names(connection))
        .map(|index| ExistingConnection {
            index,
            connection: saved[index].clone(),
        });
    let resolved = pins.resolved();
    // The saved connection's own spelling first: Replace keeps it, and it
    // is the key its pin is enforced under.
    let trusted = existing
        .as_ref()
        .and_then(|existing| {
            resolved.get(&format!(
                "{}:{}",
                existing.connection.host, existing.connection.port
            ))
        })
        .or_else(|| resolved.get(&profile.address.endpoint()));
    let pin = match (profile.host_identity, trusted) {
        (None, _) => PinReview::NotIncluded,
        (Some(file), None) => PinReview::New(file),
        (Some(file), Some(saved)) if saved.matches(&file.pin()) => PinReview::AlreadyTrusted(file),
        (Some(file), Some(saved)) => PinReview::Differs {
            saved: HostFingerprint(*saved),
            file,
        },
    };
    let endpoint_pin = resolved
        .get(&identity_endpoint(&profile, existing.as_ref()))
        .copied();
    ImportReview {
        profile,
        existing,
        pin,
        endpoint_pin,
    }
}

/// `host:port` as the imported connection will spell them (the existing
/// connection's spelling beside or in place of it, the file's otherwise):
/// the key its identity is remembered under.
fn identity_endpoint(profile: &ConnectionProfile, existing: Option<&ExistingConnection>) -> String {
    existing.map_or_else(
        || profile.address.endpoint(),
        |existing| format!("{}:{}", existing.connection.host, existing.connection.port),
    )
}

impl ImportReview {
    /// The choice the dialog starts on: Add when nothing collides,
    /// otherwise none yet (the user must pick).
    #[must_use]
    pub const fn default_choice(&self) -> Option<ImportChoice> {
        if self.existing.is_none() {
            Some(ImportChoice::Add)
        } else {
            None
        }
    }

    /// The connection's facts, one line each.
    #[must_use]
    pub fn connection_lines(&self) -> Vec<SummaryLine> {
        let line = |label: &str, value: String| SummaryLine {
            label: label.to_owned(),
            value,
        };
        vec![
            line("Name", self.profile.name.clone()),
            line("Address", self.profile.address.to_string()),
            line(
                "Username",
                self.profile
                    .username
                    .clone()
                    .unwrap_or_else(|| "Asked when you connect".to_owned()),
            ),
        ]
    }

    /// The settings this connection keeps its own value for, in words.
    /// Everything not listed follows Settings.
    #[must_use]
    pub fn override_lines(&self) -> Vec<SummaryLine> {
        override_lines(&self.profile.overrides)
    }

    /// Fields kept unread for a newer Deck, and fields dropped as secrets
    /// or paths, as short lists, and how many fields were ignored for
    /// names that are not plain text (empty when there are none).
    ///
    /// Only names of ASCII letters, digits, `_`, `.` and `-` are shown; any
    /// other is counted, so a field name cannot put words of its own into
    /// the summary.
    #[must_use]
    pub fn field_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let kept: Vec<&str> = self
            .profile
            .unknown
            .keys()
            .map(String::as_str)
            .chain(self.profile.unknown_overrides.keys().map(String::as_str))
            .collect();
        if !kept.is_empty() {
            notes.push(format!(
                "Kept, not used by this version of Arcen: {}.",
                listed(&kept)
            ));
        }
        if !self.profile.withheld.is_empty() {
            let withheld: Vec<&str> = self.profile.withheld.iter().map(String::as_str).collect();
            notes.push(format!(
                "Ignored, because Arcen never imports passwords, tokens or file paths: {}.",
                listed(&withheld)
            ));
        }
        match self.profile.unreadable_fields {
            0 => {}
            1 => notes.push("1 field ignored.".to_owned()),
            count => notes.push(format!("{count} fields ignored.")),
        }
        notes
    }

    /// The host identity, in words.
    #[must_use]
    pub fn identity_text(&self) -> String {
        match self.pin {
            PinReview::NotIncluded => {
                "Not included. Arcen will show the host's identity when you first connect."
                    .to_owned()
            }
            PinReview::AlreadyTrusted(_) => {
                "Included, and the same as the identity you already trust for this host.".to_owned()
            }
            PinReview::New(_) => "Included. Trust it only if you trust the person who sent \
                                  this file and can check the fingerprint with the host's \
                                  administrator."
                .to_owned(),
            PinReview::Differs { .. } => "Different from the identity you already trust for \
                                          this host. Someone may be trying to impersonate it. \
                                          Keep your current identity unless the host's \
                                          administrator told you its identity changed."
                .to_owned(),
        }
    }
}

/// Up to [`MAX_LISTED_FIELDS`] displayable names, then how many others.
fn listed(names: &[&str]) -> String {
    let shown: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| displayable_field_name(name))
        .take(MAX_LISTED_FIELDS)
        .collect();
    let others = names.len() - shown.len();
    let mut text = shown.join(", ");
    match (shown.is_empty(), others) {
        (_, 0) => {}
        (true, 1) => text.push_str("1 field"),
        (true, _) => {
            let _ = write!(text, "{others} fields");
        }
        (false, _) => {
            let _ = write!(text, " and {others} more");
        }
    }
    text
}

/// A connection's own settings, in words.
#[must_use]
pub fn override_lines(overrides: &ConnectionOverrides) -> Vec<SummaryLine> {
    use arcen_input::keyboard::ShortcutPreference;
    use arcen_input::text::{OptionKeyMode, OptionKeyPreference, TypingMode};
    let mut lines = Vec::new();
    let mut line = |label: &str, value: &str| {
        lines.push(SummaryLine {
            label: label.to_owned(),
            value: value.to_owned(),
        });
    };
    if let Override::Own(typing) = overrides.typing_mode {
        line(
            "Typing",
            match typing {
                TypingMode::Auto | TypingMode::Characters => "Characters",
                TypingMode::Keys => "Keys only",
            },
        );
    }
    if let Override::Own(keys) = overrides.option_keys {
        line(
            "Option key",
            match keys {
                OptionKeyPreference::Automatic => "Automatic",
                OptionKeyPreference::Chosen(OptionKeyMode::BothAlt) => "Both Option keys are Alt",
                OptionKeyPreference::Chosen(OptionKeyMode::LeftAltRightCharacters) => {
                    "Left Option is Alt, right Option types characters"
                }
                OptionKeyPreference::Chosen(OptionKeyMode::BothCharacters) => {
                    "Both Option keys type characters"
                }
            },
        );
    }
    if let Override::Own(shortcuts) = overrides.keyboard_shortcuts {
        line(
            "⌘ key",
            match shortcuts {
                ShortcutPreference::Automatic => "Automatic (by host)",
                ShortcutPreference::WindowsApp => "⌘ is the Windows key, Mac edit shortcuts",
                ShortcutPreference::SwapCommandControl => "⌘ and Control swapped",
                ShortcutPreference::CommandIsControl => "⌘ is Control",
            },
        );
    }
    if let Override::Own(displays) = overrides.displays_mode {
        line("Displays", displays.label());
    }
    if let Override::Own(clipboard) = overrides.clipboard_enabled {
        line(
            "Clipboard",
            if clipboard {
                "Shared when Settings allows it"
            } else {
                "Off for this connection"
            },
        );
    }
    if let Override::Own(to_host) = overrides.system_shortcuts {
        line(
            "System shortcuts",
            if to_host {
                "Go to the remote computer"
            } else {
                "Stay on this computer"
            },
        );
    }
    lines
}

/// What [`commit_import`] did.
#[derive(Debug)]
pub struct ImportCommitted {
    /// The saved connections after the import.
    pub store: SavedConnectionsStore,
    /// The imported connection's id.
    pub connection_id: String,
    /// Whether the file's host identity is now trusted.
    pub identity_trusted: bool,
    /// Whether an identity it is held to is trusted for this session only
    /// (it could not be saved).
    pub pin_unsaved: bool,
    /// A status line for Home.
    pub status: String,
}

/// Saves an import the user confirmed: the connection (added, replacing
/// the colliding one, or beside it under a distinct name) and, only when
/// `decision.trust_host_identity`, the file's host identity.
///
/// # Errors
///
/// [`ProfileError::ChoiceRequired`] when the choice does not fit the
/// review, [`ProfileError::Changed`] when the colliding connection changed
/// meanwhile, [`ProfileError::IdentityChanged`] when another Deck window
/// changed the machine's trusted identity meanwhile (nothing is saved;
/// `pins` is then the store on disk, to review again),
/// [`ProfileError::NotSaved`] when saving failed.
pub fn commit_import(
    review: &ImportReview,
    decision: ImportDecision,
    repository: &ClientConfigRepository,
    security_mode: SecurityMode,
    saved: &[SavedConnectionSummary],
    pins: &mut TrustedPinStore,
) -> Result<ImportCommitted, ProfileError> {
    let profile = &review.profile;
    // The identity the saved connection must be held to, under the key it
    // connects with (`ConnectionDraft::endpoint`): the file's when the user
    // confirmed it; otherwise the one the summary showed as trusted, which
    // may have been found under another spelling of the address. A
    // connection saved by import is never left unpinned when the summary
    // showed a trusted identity, or one the user declined to replace.
    let confirmed = decision.trust_host_identity && review.pin.needs_decision();
    let held = match review.pin {
        PinReview::New(file) | PinReview::Differs { file, .. } if confirmed => Some(file),
        PinReview::AlreadyTrusted(trusted) | PinReview::Differs { saved: trusted, .. } => {
            Some(trusted)
        }
        PinReview::NotIncluded | PinReview::New(_) => None,
    };
    // Before anything is saved, and held until the identity is: the
    // identities as they are now. The summary's view of this machine's
    // identity must still be true, or the user is asked again; only this
    // machine's entry is written, so another window's identities stay.
    let locked = match held {
        Some(_) => Some(lock_reviewed_identity(review, repository, pins)?),
        None => None,
    };
    let saved_as = match (decision.choice, &review.existing) {
        (ImportChoice::Replace, Some(existing)) => replace_existing(profile, existing, repository)?,
        (ImportChoice::Add, None) | (ImportChoice::KeepBoth, Some(_)) => {
            // A connection to the machine saved since the summary was made
            // is never overwritten or duplicated without being asked.
            if decision.choice == ImportChoice::Add
                && saved
                    .iter()
                    .any(|connection| profile.address.names(connection))
            {
                return Err(ProfileError::Changed);
            }
            add_beside(
                profile,
                review.existing.as_ref(),
                repository,
                security_mode,
                saved,
            )?
        }
        (ImportChoice::Add | ImportChoice::Replace | ImportChoice::KeepBoth, _) => {
            return Err(ProfileError::ChoiceRequired);
        }
    };
    let SavedAs {
        store,
        connection_id,
        name,
        endpoint,
    } = saved_as;
    let mut status = format!("Imported \u{201c}{name}\u{201d}");
    let mut identity_trusted = matches!(review.pin, PinReview::AlreadyTrusted(_));
    let unsaved_note = match (held, locked) {
        (Some(held), Some(locked)) => hold_identity(held, &endpoint, &name, locked, pins),
        _ => None,
    };
    let pin_unsaved = unsaved_note.is_some();
    match (confirmed, unsaved_note) {
        (true, None) => {
            identity_trusted = true;
            status.push_str(" and trusted its host identity");
        }
        (_, Some(note)) => {
            let _ = write!(
                status,
                "; its host identity is trusted for this session only{note}"
            );
        }
        (false, None) => {}
    }
    status.push('.');
    Ok(ImportCommitted {
        store,
        connection_id,
        identity_trusted,
        pin_unsaved,
        status,
    })
}

/// The identities, locked, for an import that will hold its connection to
/// one: or why they cannot be saved (the note for the status line).
///
/// # Errors
///
/// [`ProfileError::IdentityChanged`] when the identity under the imported
/// connection's key is no longer the one the summary saw; `pins` is then
/// the store on disk.
fn lock_reviewed_identity(
    review: &ImportReview,
    repository: &ClientConfigRepository,
    pins: &mut TrustedPinStore,
) -> Result<Result<crate::trusted_pins::LockedPins, String>, ProfileError> {
    use crate::trusted_pins::PinWriteError;
    match repository
        .trusted_pins_file()
        .ok_or(PinWriteError::Unavailable)
        .and_then(|path| crate::trusted_pins::lock(&path))
    {
        Ok(locked) => {
            let key = identity_endpoint(&review.profile, review.existing.as_ref());
            if locked.expect(&key, review.endpoint_pin).is_err() {
                *pins = locked.store().clone();
                return Err(ProfileError::IdentityChanged);
            }
            Ok(Ok(locked))
        }
        Err(PinWriteError::Unavailable) => Ok(Err(String::new())),
        Err(error) => Ok(Err(format!(" ({error})"))),
    }
}

/// Holds the imported connection at `endpoint` to `held`: written under
/// the lock taken before it was saved when that is not already what is on
/// disk, otherwise (or when writing fails) for this session only. Returns
/// the status note when it is for this session only.
fn hold_identity(
    held: HostFingerprint,
    endpoint: &str,
    name: &str,
    locked: Result<crate::trusted_pins::LockedPins, String>,
    pins: &mut TrustedPinStore,
) -> Option<String> {
    let label = Some(name.to_owned());
    match locked {
        Ok(locked)
            if locked
                .current(endpoint)
                .is_some_and(|now| now.matches(&held.pin())) =>
        {
            *pins = locked.store().clone();
            None
        }
        Ok(locked) => match locked.commit(endpoint, &held.pin(), label.clone()) {
            Ok(written) => {
                *pins = written;
                None
            }
            Err(error) => {
                pins.remember(endpoint, &held.pin(), label);
                Some(format!(" ({error})"))
            }
        },
        Err(_)
            if pins
                .resolved()
                .get(endpoint)
                .is_some_and(|now| now.matches(&held.pin())) =>
        {
            None
        }
        Err(note) => {
            pins.remember(endpoint, &held.pin(), label);
            Some(note)
        }
    }
}

/// Where [`commit_import`] saved the connection.
struct SavedAs {
    store: SavedConnectionsStore,
    connection_id: String,
    name: String,
    /// `host:port` exactly as the saved connection spells them: the key
    /// its pin is looked up under.
    endpoint: String,
}

/// Replaces `existing` with the profile's connection, keeping its id,
/// its host and port exactly as saved (they name the same machine, and a
/// remembered identity is keyed by that spelling) and everything the
/// profile does not describe.
fn replace_existing(
    profile: &ConnectionProfile,
    existing: &ExistingConnection,
    repository: &ClientConfigRepository,
) -> Result<SavedAs, ProfileError> {
    let original = &existing.connection;
    let mut updated = original.clone();
    updated.name.clone_from(&profile.name);
    updated
        .settings
        .remembered_username
        .clone_from(&profile.username);
    updated.settings.unknown.remove("remembered_username");
    updated.settings.overrides = profile.overrides;
    match kept_fields(profile) {
        Some(kept) => updated.unknown.insert(PROFILE_UNKNOWN_KEY.to_owned(), kept),
        None => updated.unknown.remove(PROFILE_UNKNOWN_KEY),
    };
    let store = repository
        .update_connection(&original.id, original, updated)
        .map_err(|error| match error {
            crate::settings::ClientConfigError::ConnectionChanged(_)
            | crate::settings::ClientConfigError::ConnectionMissing(_) => ProfileError::Changed,
            other => ProfileError::NotSaved(other.to_string()),
        })?;
    Ok(SavedAs {
        store,
        connection_id: original.id.clone(),
        name: profile.name.clone(),
        endpoint: format!("{}:{}", original.host, original.port),
    })
}

/// Adds the profile's connection as a new one, under a name no saved
/// connection has. Beside an existing connection to the same machine it
/// takes that connection's host and port exactly as spelled, so both are
/// held to one remembered identity.
fn add_beside(
    profile: &ConnectionProfile,
    existing: Option<&ExistingConnection>,
    repository: &ClientConfigRepository,
    security_mode: SecurityMode,
    saved: &[SavedConnectionSummary],
) -> Result<SavedAs, ProfileError> {
    let name = distinct_name(&profile.name, saved);
    let (host, port) = existing.map_or_else(
        || (profile.address.host().to_owned(), profile.address.port()),
        |existing| (existing.connection.host.clone(), existing.connection.port),
    );
    let id = crate::config::new_saved_connection_id(
        ConnectionKind::DirectMachine,
        &host,
        port,
        true,
        security_mode,
        &name,
        saved.len(),
    );
    let mut unknown = Map::new();
    if let Some(kept) = kept_fields(profile) {
        unknown.insert(PROFILE_UNKNOWN_KEY.to_owned(), kept);
    }
    let connection = SavedConnectionSummary {
        id: id.clone(),
        kind: ConnectionKind::DirectMachine,
        name: name.clone(),
        host: host.clone(),
        port,
        use_tls: true,
        transport: SavedTransport::Quic,
        identity_security_mode: Some(security_mode.key().to_owned()),
        settings: SavedConnectionSettings {
            remembered_username: profile.username.clone(),
            overrides: profile.overrides,
            ..SavedConnectionSettings::default()
        },
        unknown,
    };
    let store = repository
        .add_connection(connection)
        .map_err(|error| ProfileError::NotSaved(error.to_string()))?;
    Ok(SavedAs {
        store,
        connection_id: id,
        name,
        endpoint: format!("{host}:{port}"),
    })
}

fn kept_fields(profile: &ConnectionProfile) -> Option<Value> {
    if profile.unknown.is_empty() && profile.unknown_overrides.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "fields": profile.unknown,
        "overrides": profile.unknown_overrides,
    }))
}

/// `name`, or `name (2)`, `name (3)`… when a saved connection has it.
fn distinct_name(name: &str, saved: &[SavedConnectionSummary]) -> String {
    let taken = |candidate: &str| {
        saved
            .iter()
            .any(|connection| connection.name.trim().eq_ignore_ascii_case(candidate))
    };
    if !taken(name) {
        return name.to_owned();
    }
    // At most `saved.len()` names are taken, so one of these is free.
    (2..=saved.len() + 2)
        .map(|ordinal| format!("{name} ({ordinal})"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| name.to_owned())
}

/// Exports `connection` to `path`. Returns a status line for Home.
///
/// # Errors
///
/// When the connection would not import, or the file cannot be written.
pub fn export_connection(
    connection: &SavedConnectionSummary,
    pins: &TrustedPinStore,
    path: &Path,
) -> Result<String, ProfileError> {
    let profile = ConnectionProfile::from_saved(connection, pins)?;
    profile.write(path)?;
    let identity = if profile.host_identity.is_some() {
        " with its host identity"
    } else {
        ""
    };
    Ok(format!(
        "Exported \u{201c}{}\u{201d}{identity}. The file has no password.",
        profile.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_input::text::TypingMode;

    const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A folder in the build's target directory, not the system's.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap();
        let folder = exe
            .parent()
            .unwrap()
            .join(format!("arcen-profile-tests-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn file(extra: &serde_json::Value) -> Vec<u8> {
        let mut object = serde_json::json!({
            "format": "arcen-connection",
            "version": 1,
            "name": "Studio",
            "address": "pier.example.internal",
            "port": 18444,
            "username": "alice",
        });
        for (key, value) in extra.as_object().unwrap() {
            object[key] = value.clone();
        }
        serde_json::to_vec(&object).unwrap()
    }

    fn parse(extra: &serde_json::Value) -> Result<ConnectionProfile, ProfileError> {
        ConnectionProfile::parse(&file(extra))
    }

    fn saved(name: &str, host: &str) -> SavedConnectionSummary {
        SavedConnectionSummary {
            id: format!("connection-{name}"),
            kind: ConnectionKind::DirectMachine,
            name: name.to_owned(),
            host: host.to_owned(),
            port: 18_444,
            use_tls: true,
            transport: SavedTransport::Quic,
            identity_security_mode: Some(SecurityMode::Medium.key().to_owned()),
            settings: SavedConnectionSettings::default(),
            unknown: Map::new(),
        }
    }

    fn pin(digest: &str) -> TlsPin {
        StoredPin {
            kind: "spki_sha256".to_owned(),
            digest: digest.to_owned(),
            pinned_at: None,
            label: None,
        }
        .to_pin()
        .unwrap()
    }

    #[test]
    fn a_profile_reads_every_known_field() {
        let profile = parse(&serde_json::json!({
            "overrides": {"typing_mode": "keys", "displays_mode": "windowed", "clipboard_enabled": false},
            "host_identity": {"kind": "spki_sha256", "digest": DIGEST_A.to_uppercase()},
        }))
        .unwrap();
        assert_eq!(profile.version, ProfileVersion::CURRENT);
        assert_eq!(profile.name, "Studio");
        assert_eq!(profile.address.endpoint(), "pier.example.internal:18444");
        assert_eq!(profile.username.as_deref(), Some("alice"));
        assert_eq!(
            profile.overrides.typing_mode,
            Override::Own(TypingMode::Keys)
        );
        assert_eq!(
            profile.overrides.displays_mode,
            Override::Own(DisplaysMode::Windowed)
        );
        assert_eq!(profile.overrides.clipboard_enabled, Override::Own(false));
        assert_eq!(profile.host_identity.unwrap().pin(), pin(DIGEST_A));
        assert!(profile.unknown.is_empty() && profile.withheld.is_empty());
    }

    #[test]
    fn oversized_input_is_refused_before_it_is_parsed() {
        let mut bytes = file(&serde_json::json!({}));
        bytes.pop();
        bytes.extend_from_slice(b",\"padding\":\"");
        bytes.resize(usize::try_from(MAX_PROFILE_BYTES).unwrap() + 1, b'x');
        assert_eq!(
            ConnectionProfile::parse(&bytes),
            Err(ProfileError::TooLarge)
        );

        let folder = scratch("oversized");
        let path = folder.join("big.arcen");
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(ConnectionProfile::read(&path), Err(ProfileError::TooLarge));
        assert_eq!(
            ConnectionProfile::read(&folder),
            Err(ProfileError::NotAFile)
        );
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn deep_nesting_is_refused_at_either_limit() {
        let deep = |levels: usize| {
            let mut value = serde_json::json!(1);
            for _ in 0..levels {
                value = serde_json::json!({ "n": value });
            }
            value
        };
        assert_eq!(
            parse(&serde_json::json!({ "future": deep(MAX_PROFILE_DEPTH) })),
            Err(ProfileError::TooDeep)
        );
        assert!(parse(&serde_json::json!({ "future": deep(4) })).is_ok());
        // Past serde_json's own recursion limit: refused, never a stack
        // overflow.
        let mut bytes = b"{\"format\":\"arcen-connection\",\"x\":".to_vec();
        bytes.extend(std::iter::repeat_n(b'[', 10_000));
        bytes.extend(std::iter::repeat_n(b']', 10_000));
        bytes.push(b'}');
        assert_eq!(ConnectionProfile::parse(&bytes), Err(ProfileError::TooDeep));
    }

    #[test]
    fn anything_but_a_profile_object_is_not_a_profile() {
        for bytes in [
            &b"[]"[..],
            b"\"arcen-connection\"",
            b"42",
            b"null",
            b"{}",
            b"{\"format\":\"something-else\",\"version\":1}",
            b"{\"format\":\"arcen-connection\",\"version\":0}",
        ] {
            assert_eq!(
                ConnectionProfile::parse(bytes),
                Err(ProfileError::NotAProfile),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
        assert_eq!(
            ConnectionProfile::parse(b"{not json"),
            Err(ProfileError::NotJson)
        );
        assert_eq!(ConnectionProfile::parse(b""), Err(ProfileError::NotJson));
    }

    #[test]
    fn a_newer_format_is_refused_with_a_reason() {
        let error = parse(&serde_json::json!({ "version": 2 })).unwrap_err();
        assert_eq!(error, ProfileError::NewerVersion { found: 2 });
        assert!(error.to_string().contains("Update Arcen Deck"));
    }

    #[test]
    fn known_fields_must_have_their_types() {
        let wrong = |extra: serde_json::Value| parse(&extra).unwrap_err();
        assert!(matches!(
            wrong(serde_json::json!({"name": 7})),
            ProfileError::WrongType { field: "name", .. }
        ));
        assert!(matches!(
            wrong(serde_json::json!({"address": ["pier"]})),
            ProfileError::WrongType {
                field: "address",
                ..
            }
        ));
        assert!(matches!(
            wrong(serde_json::json!({"port": "18444"})),
            ProfileError::WrongType { field: "port", .. }
        ));
        assert!(matches!(
            wrong(serde_json::json!({"version": "1"})),
            ProfileError::WrongType {
                field: "version",
                ..
            }
        ));
        assert!(matches!(
            wrong(serde_json::json!({"username": true})),
            ProfileError::WrongType {
                field: "username",
                ..
            }
        ));
        assert!(matches!(
            wrong(serde_json::json!({"overrides": ["keys"]})),
            ProfileError::WrongType {
                field: "overrides",
                ..
            }
        ));
        assert!(matches!(
            wrong(serde_json::json!({"overrides": {"typing_mode": 1}})),
            ProfileError::WrongType {
                field: "overrides",
                ..
            }
        ));
        assert!(matches!(
            wrong(serde_json::json!({"overrides": {"clipboard_enabled": "no"}})),
            ProfileError::WrongType {
                field: "overrides",
                ..
            }
        ));
        assert!(matches!(
            wrong(serde_json::json!({"overrides": {"system_shortcuts": "on"}})),
            ProfileError::WrongType {
                field: "overrides",
                ..
            }
        ));
        for port in [0, 65_536, -1] {
            assert_eq!(
                wrong(serde_json::json!({ "port": port })),
                ProfileError::InvalidPort
            );
        }
        assert_eq!(
            wrong(serde_json::json!({"port": 1.5})),
            ProfileError::InvalidPort
        );
        let mut without_name =
            serde_json::from_slice::<Value>(&file(&serde_json::json!({}))).unwrap();
        without_name.as_object_mut().unwrap().remove("name");
        assert_eq!(
            ConnectionProfile::parse(&serde_json::to_vec(&without_name).unwrap()),
            Err(ProfileError::Missing("name"))
        );
    }

    #[test]
    fn names_usernames_and_addresses_are_validated() {
        let wrong = |extra: serde_json::Value| parse(&extra).unwrap_err();
        for name in [
            "",
            "   ",
            "Studio\u{202E}gpj.exe",
            "tab\there",
            &"x".repeat(129),
        ] {
            assert_eq!(
                wrong(serde_json::json!({ "name": name })),
                ProfileError::InvalidName,
                "{name:?}"
            );
        }
        for username in ["al\u{0000}ice", "al\u{202E}ice", &"u".repeat(256)] {
            assert_eq!(
                wrong(serde_json::json!({ "username": username })),
                ProfileError::InvalidUsername
            );
        }
        for address in [
            "",
            "http://pier.example.internal",
            "pier.example.internal/path",
            "alice@pier.example.internal",
            "pier example",
            "-pier.example.internal",
            "999.1.1.1",
            "0.0.0.0",
            "255.255.255.255",
            "ff02::1",
            "pier.example.internal:18444",
            "pïer.example.internal",
        ] {
            assert_eq!(
                wrong(serde_json::json!({ "address": address })),
                ProfileError::InvalidAddress,
                "{address}"
            );
        }
        let read = |address: &str| {
            parse(&serde_json::json!({ "address": address }))
                .unwrap()
                .address
                .host()
                .to_owned()
        };
        assert_eq!(read(" Pier.Example.Internal. "), "pier.example.internal");
        assert_eq!(read("[2001:db8::1]"), "2001:db8::1");
        assert_eq!(read("203.0.113.10"), "203.0.113.10");
        assert_eq!(
            parse(&serde_json::json!({ "address": "2001:db8::1" }))
                .unwrap()
                .address
                .to_string(),
            "[2001:db8::1]:18444"
        );
        assert!(parse(&serde_json::json!({ "username": "", "port": null })).is_ok_and(
            |profile| profile.username.is_none() && profile.address.port() == 18_444
        ));
    }

    #[test]
    fn a_host_identity_of_the_wrong_shape_or_length_is_refused() {
        for identity in [
            serde_json::json!({"kind": "spki_sha256", "digest": &DIGEST_A[..62]}),
            serde_json::json!({"kind": "spki_sha256", "digest": format!("{DIGEST_A}aa")}),
            serde_json::json!({"kind": "spki_sha256", "digest": "zz".repeat(32)}),
            serde_json::json!({"kind": "md5", "digest": DIGEST_A}),
            serde_json::json!({"kind": "spki_sha256"}),
            serde_json::json!({"kind": "spki_sha256", "digest": DIGEST_A, "trusted": true}),
            serde_json::json!(DIGEST_A),
            serde_json::json!({"kind": "spki_sha256", "digest": 7}),
        ] {
            assert_eq!(
                parse(&serde_json::json!({ "host_identity": identity })),
                Err(ProfileError::InvalidHostIdentity),
                "{identity}"
            );
        }
        assert!(
            parse(&serde_json::json!({"host_identity": {"kind": "certificate_sha256", "digest": DIGEST_B}}))
                .is_ok()
        );
    }

    #[test]
    fn unknown_fields_are_kept_and_secrets_and_paths_are_dropped() {
        let profile = parse(&serde_json::json!({
            "future_feature": {"level": 2, "note": "kept"},
            "password": "hunter2",
            "accessToken": "abc",
            "resume_grant": "grant-bytes",
            "log_path": "kept? no",
            "notes": "/Users/artist/Library/secret.txt",
            "nested": {"api_key": "x", "colour": "teal", "where": "C:\\Users\\artist"},
            "overrides": {
                "typing_mode": "telepathy",
                "future_setting": "on",
                "auth_token": "abc",
                "displays_mode": "windowed",
            },
        }))
        .unwrap();
        assert_eq!(
            profile.unknown.keys().collect::<Vec<_>>(),
            ["future_feature", "nested"]
        );
        assert_eq!(
            profile.unknown["nested"],
            serde_json::json!({"colour": "teal"})
        );
        assert_eq!(
            profile.unknown_overrides,
            serde_json::json!({"typing_mode": "telepathy", "future_setting": "on"})
                .as_object()
                .unwrap()
                .clone()
        );
        assert_eq!(profile.overrides.typing_mode, Override::Inherit);
        assert_eq!(
            profile.overrides.displays_mode,
            Override::Own(DisplaysMode::Windowed)
        );
        let mut withheld = profile.withheld.clone();
        withheld.sort();
        assert_eq!(
            withheld,
            [
                "accessToken",
                "log_path",
                "nested.api_key",
                "nested.where",
                "notes",
                "overrides.auth_token",
                "password",
                "resume_grant",
            ]
        );
        let bytes = String::from_utf8(profile.to_bytes()).unwrap();
        for secret in [
            "hunter2",
            "grant-bytes",
            "/Users/artist",
            "auth_token",
            "api_key",
        ] {
            assert!(!bytes.contains(secret), "{secret} in {bytes}");
        }
        assert!(bytes.contains("future_feature") && bytes.contains("telepathy"));
    }

    #[test]
    fn export_writes_no_password_grant_path_or_other_setting() {
        let mut connection = saved("Studio", "pier.example.internal");
        connection.settings.remembered_username = Some("alice".to_owned());
        connection.settings.overrides.typing_mode = Override::Own(TypingMode::Keys);
        connection
            .settings
            .unknown
            .insert("password".into(), "hunter2".into());
        connection
            .settings
            .unknown
            .insert("local_setting".into(), "this-mac-only".into());
        connection
            .unknown
            .insert("resume_grant".into(), "grant-bytes".into());
        connection
            .unknown
            .insert("security_mode".into(), "medium".into());
        connection.unknown.insert(
            PROFILE_UNKNOWN_KEY.into(),
            serde_json::json!({
                "fields": {"future": 1, "session_token": "tok", "folder": "/Users/artist"},
                "overrides": {"future_setting": "on", "secret": "s"},
            }),
        );
        let mut pins = TrustedPinStore::default();
        pins.remember("pier.example.internal:18444", &pin(DIGEST_A), None);
        let profile = ConnectionProfile::from_saved(&connection, &pins).unwrap();
        let json = profile.to_json();
        assert_eq!(
            json,
            serde_json::json!({
                "format": "arcen-connection",
                "version": 1,
                "name": "Studio",
                "address": "pier.example.internal",
                "port": 18444,
                "username": "alice",
                "overrides": {"typing_mode": "keys", "future_setting": "on"},
                "host_identity": {"kind": "spki_sha256", "digest": DIGEST_A},
                "future": 1,
            })
        );
        let text = String::from_utf8(profile.to_bytes()).unwrap();
        for absent in [
            "hunter2",
            "this-mac-only",
            "grant-bytes",
            "tok",
            "/Users",
            "medium",
            "connection-Studio",
        ] {
            assert!(!text.contains(absent), "{absent}");
        }
    }

    #[test]
    fn export_round_trips_through_an_atomic_write() {
        let folder = scratch("roundtrip");
        let path = folder.join("Studio.arcen");
        std::fs::write(&path, b"an older export").unwrap();
        let mut connection = saved("Studio", "2001:db8::1");
        connection.settings.overrides.displays_mode = Override::Own(DisplaysMode::SinglePrimary);
        connection.settings.overrides.clipboard_enabled = Override::Own(false);
        let status = export_connection(&connection, &TrustedPinStore::default(), &path).unwrap();
        assert!(status.contains("no password"));
        let read = ConnectionProfile::read(&path).unwrap();
        assert_eq!(
            read,
            ConnectionProfile::from_saved(&connection, &TrustedPinStore::default()).unwrap()
        );
        assert_eq!(read.overrides, connection.settings.overrides);
        assert!(read.host_identity.is_none());
        // Only the file: the temporary was renamed over it.
        let names: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["Studio.arcen"]);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn an_export_is_always_a_file_a_deck_can_import_or_nothing_is_written() {
        let folder = scratch("export-size");
        let path = folder.join("Studio.arcen");
        let mut connection = saved("Studio", "203.0.113.10");
        // Kept fields from an earlier import: compact they fit, but pretty
        // printing indents every one of them past the limit.
        let fields: serde_json::Map<String, serde_json::Value> = (0..2_000)
            .map(|index| (format!("f{index:04}"), serde_json::json!([[["x"]]])))
            .collect();
        connection.unknown.insert(
            PROFILE_UNKNOWN_KEY.to_owned(),
            serde_json::json!({ "fields": fields, "overrides": {} }),
        );
        let profile =
            ConnectionProfile::from_saved(&connection, &TrustedPinStore::default()).unwrap();
        assert!(profile.to_bytes().len() as u64 > MAX_PROFILE_BYTES);
        export_connection(&connection, &TrustedPinStore::default(), &path).unwrap();
        let written = std::fs::read(&path).unwrap();
        assert!(written.len() as u64 <= MAX_PROFILE_BYTES);
        assert_eq!(ConnectionProfile::read(&path).unwrap(), profile);

        // Too large even compact: refused, and the file there stays.
        std::fs::write(&path, b"an older export").unwrap();
        let fields: serde_json::Map<String, serde_json::Value> = (0..4_000)
            .map(|index| (format!("f{index:04}"), serde_json::json!("x".repeat(16))))
            .collect();
        connection.unknown.insert(
            PROFILE_UNKNOWN_KEY.to_owned(),
            serde_json::json!({ "fields": fields, "overrides": {} }),
        );
        assert_eq!(
            export_connection(&connection, &TrustedPinStore::default(), &path),
            Err(ProfileError::TooLargeToExport)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"an older export");
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn the_pin_review_compares_the_files_identity_with_the_trusted_one() {
        let saved_connections = [saved("Studio", "PIER.example.internal")];
        let with_identity = |digest: &str| {
            parse(&serde_json::json!({"host_identity": {"kind": "spki_sha256", "digest": digest}}))
                .unwrap()
        };
        let mut pins = TrustedPinStore::default();
        let review =
            |profile, pins: &TrustedPinStore| review_import(profile, &saved_connections, pins);
        assert_eq!(
            review(parse(&serde_json::json!({})).unwrap(), &pins).pin,
            PinReview::NotIncluded
        );
        let new = review(with_identity(DIGEST_A), &pins);
        assert!(matches!(new.pin, PinReview::New(_)) && new.pin.needs_decision());
        // Trusted under the saved connection's own spelling of the host.
        pins.remember("PIER.example.internal:18444", &pin(DIGEST_A), None);
        let same = review(with_identity(DIGEST_A), &pins);
        assert!(matches!(same.pin, PinReview::AlreadyTrusted(_)) && !same.pin.needs_decision());
        let differs = review(with_identity(DIGEST_B), &pins);
        assert_eq!(
            differs.pin,
            PinReview::Differs {
                saved: HostFingerprint::new(pin(DIGEST_A)),
                file: HostFingerprint::new(pin(DIGEST_B)),
            }
        );
        assert!(differs.identity_text().contains("impersonate"));
        assert_eq!(
            HostFingerprint::new(pin(DIGEST_A)).display(),
            ["AA"; 32].join(":")
        );
    }

    #[test]
    fn an_identity_is_trusted_only_when_the_user_confirms_it() {
        let folder = scratch("pins");
        let repository = ClientConfigRepository::at(folder.clone());
        let existing = saved("Studio", "pier.example.internal");
        let store = repository.add_connection(existing.clone()).unwrap();
        let mut pins = TrustedPinStore::default();
        pins.remember("pier.example.internal:18444", &pin(DIGEST_A), None);
        crate::trusted_pins::save(&repository.trusted_pins_file().unwrap(), &pins).unwrap();
        let profile = parse(
            &serde_json::json!({"host_identity": {"kind": "spki_sha256", "digest": DIGEST_B}}),
        )
        .unwrap();
        let review = review_import(profile, &store.connections, &pins);
        assert!(matches!(review.pin, PinReview::Differs { .. }));

        // Default: not trusted; the identity the user trusted stays.
        let kept = commit_import(
            &review,
            ImportDecision {
                choice: ImportChoice::KeepBoth,
                trust_host_identity: false,
            },
            &repository,
            SecurityMode::Medium,
            &store.connections,
            &mut pins,
        )
        .unwrap();
        assert!(!kept.identity_trusted);
        assert!(pins.resolved()["pier.example.internal:18444"].matches(&pin(DIGEST_A)));
        let on_disk = crate::trusted_pins::load(&repository.trusted_pins_file().unwrap());
        assert!(on_disk.resolved()["pier.example.internal:18444"].matches(&pin(DIGEST_A)));

        // Confirmed: the file's identity replaces it, on disk too.
        let review = review_import(review.profile, &kept.store.connections, &pins);
        let replaced = commit_import(
            &review,
            ImportDecision {
                choice: ImportChoice::Replace,
                trust_host_identity: true,
            },
            &repository,
            SecurityMode::Medium,
            &kept.store.connections,
            &mut pins,
        )
        .unwrap();
        assert!(replaced.identity_trusted);
        let on_disk = crate::trusted_pins::load(&repository.trusted_pins_file().unwrap());
        assert!(on_disk.resolved()["pier.example.internal:18444"].matches(&pin(DIGEST_B)));
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn an_import_writes_only_its_machines_identity_and_asks_again_if_it_changed() {
        let folder = scratch("pins-locked");
        let repository = ClientConfigRepository::at(folder.clone());
        let path = repository.trusted_pins_file().unwrap();
        let identity = |digest: &str| serde_json::json!({"host_identity": {"kind": "spki_sha256", "digest": digest}});
        // This window read no identities; another has since trusted a
        // second machine.
        let mut pins = TrustedPinStore::default();
        let mut elsewhere = TrustedPinStore::default();
        elsewhere.remember("render.example.internal:18444", &pin(DIGEST_B), None);
        crate::trusted_pins::save(&path, &elsewhere).unwrap();
        let review = review_import(parse(&identity(DIGEST_A)).unwrap(), &[], &pins);
        let trust = ImportDecision {
            choice: ImportChoice::Add,
            trust_host_identity: true,
        };
        let added = commit_import(
            &review,
            trust,
            &repository,
            SecurityMode::Medium,
            &[],
            &mut pins,
        )
        .unwrap();
        assert!(added.identity_trusted && !added.pin_unsaved);
        let on_disk = crate::trusted_pins::load(&path).resolved();
        assert!(on_disk["pier.example.internal:18444"].matches(&pin(DIGEST_A)));
        assert!(
            on_disk["render.example.internal:18444"].matches(&pin(DIGEST_B)),
            "another window's identity was rolled back"
        );
        assert_eq!(pins.resolved(), on_disk);

        // A summary that saw A: another window rotates the machine to B
        // before Import. Nothing is saved, and the window now knows B.
        let review = review_import(
            parse(&identity(DIGEST_A)).unwrap(),
            &added.store.connections,
            &pins,
        );
        let mut rotated = crate::trusted_pins::load(&path);
        rotated.remember("pier.example.internal:18444", &pin(DIGEST_B), None);
        crate::trusted_pins::save(&path, &rotated).unwrap();
        let error = commit_import(
            &review,
            ImportDecision {
                choice: ImportChoice::Replace,
                trust_host_identity: true,
            },
            &repository,
            SecurityMode::Medium,
            &added.store.connections,
            &mut pins,
        )
        .unwrap_err();
        assert_eq!(error, ProfileError::IdentityChanged);
        assert!(pins.resolved()["pier.example.internal:18444"].matches(&pin(DIGEST_B)));
        assert!(
            crate::trusted_pins::load(&path).resolved()["pier.example.internal:18444"]
                .matches(&pin(DIGEST_B))
        );
        assert_eq!(
            crate::config::load_saved_connections(&repository)
                .unwrap()
                .unwrap()
                .connections,
            added.store.connections,
            "nothing was saved"
        );
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one scenario, step by step")]
    fn an_existing_connection_is_replaced_or_kept_but_never_overwritten_silently() {
        let folder = scratch("collide");
        let repository = ClientConfigRepository::at(folder.clone());
        let mut existing = saved("Studio", "pier.example.internal");
        existing
            .settings
            .unknown
            .insert("local_only".into(), "kept".into());
        let store = repository.add_connection(existing.clone()).unwrap();
        let mut pins = TrustedPinStore::default();
        let profile = parse(&serde_json::json!({
            "name": "Studio",
            "username": "bob",
            "overrides": {"typing_mode": "keys", "future_setting": "on"},
            "future_feature": true,
        }))
        .unwrap();
        let review = review_import(profile, &store.connections, &pins);
        assert_eq!(
            review.existing.as_ref().map(|existing| existing.index),
            Some(0)
        );
        assert_eq!(review.default_choice(), None);
        let decide = |choice| ImportDecision {
            choice,
            trust_host_identity: false,
        };
        assert_eq!(
            commit_import(
                &review,
                decide(ImportChoice::Add),
                &repository,
                SecurityMode::Medium,
                &store.connections,
                &mut pins,
            )
            .unwrap_err(),
            ProfileError::ChoiceRequired
        );

        let both = commit_import(
            &review,
            decide(ImportChoice::KeepBoth),
            &repository,
            SecurityMode::Medium,
            &store.connections,
            &mut pins,
        )
        .unwrap();
        assert_eq!(both.store.connections.len(), 2);
        let added = both
            .store
            .connections
            .iter()
            .find(|connection| connection.id == both.connection_id)
            .unwrap();
        assert_eq!(added.name, "Studio (2)");
        assert_ne!(added.id, existing.id);
        assert_eq!(added.settings.remembered_username.as_deref(), Some("bob"));

        let review = review_import(review.profile, &both.store.connections, &pins);
        let replaced = commit_import(
            &review,
            decide(ImportChoice::Replace),
            &repository,
            SecurityMode::Medium,
            &both.store.connections,
            &mut pins,
        )
        .unwrap();
        assert_eq!(replaced.connection_id, existing.id);
        assert_eq!(replaced.store.connections.len(), 2);
        let updated = &replaced.store.connections[0];
        assert_eq!(
            updated.settings.overrides.typing_mode,
            Override::Own(TypingMode::Keys)
        );
        assert_eq!(updated.settings.unknown["local_only"], "kept");

        // The kept fields survive connections.json and come back on export.
        let reloaded = crate::config::load_saved_connections(&repository)
            .unwrap()
            .unwrap();
        let exported = ConnectionProfile::from_saved(&reloaded.connections[0], &pins)
            .unwrap()
            .to_json();
        assert_eq!(exported["future_feature"], true);
        assert_eq!(exported["overrides"]["future_setting"], "on");
        assert!(exported.get("local_only").is_none());

        // A stale summary is refused rather than applied to a changed list.
        assert_eq!(
            commit_import(
                &review,
                decide(ImportChoice::Replace),
                &repository,
                SecurityMode::Medium,
                &replaced.store.connections,
                &mut pins,
            )
            .unwrap_err(),
            ProfileError::Changed
        );
        let fresh = review_import(
            parse(&serde_json::json!({"address": "other.example.internal"})).unwrap(),
            &[],
            &pins,
        );
        assert_eq!(fresh.default_choice(), Some(ImportChoice::Add));
        assert_eq!(
            commit_import(
                &review_import(fresh.profile.clone(), &[], &pins),
                decide(ImportChoice::Add),
                &repository,
                SecurityMode::Medium,
                &[saved("Other", "other.example.internal")],
                &mut pins,
            )
            .unwrap_err(),
            ProfileError::Changed
        );
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn the_summary_says_what_will_be_added() {
        let review = review_import(
            parse(&serde_json::json!({
                "overrides": {"typing_mode": "keys", "clipboard_enabled": false},
                "password": "x",
                "future": 1,
            }))
            .unwrap(),
            &[],
            &TrustedPinStore::default(),
        );
        let facts: Vec<_> = review
            .connection_lines()
            .into_iter()
            .map(|line| format!("{}: {}", line.label, line.value))
            .collect();
        assert_eq!(
            facts,
            [
                "Name: Studio",
                "Address: pier.example.internal:18444",
                "Username: alice"
            ]
        );
        let overrides: Vec<_> = review
            .override_lines()
            .into_iter()
            .map(|line| format!("{}: {}", line.label, line.value))
            .collect();
        assert_eq!(
            overrides,
            ["Typing: Keys only", "Clipboard: Off for this connection"]
        );
        let notes = review.field_notes();
        assert!(notes[0].contains("future") && notes[1].contains("password"));
        assert!(review.identity_text().starts_with("Not included"));
    }

    /// The key a connection's pin is enforced under
    /// (`ConnectionDraft::endpoint`).
    fn enforced_key(connection: &SavedConnectionSummary) -> String {
        format!("{}:{}", connection.host, connection.port)
    }

    #[test]
    fn replace_keeps_the_saved_spelling_so_the_remembered_identity_still_applies() {
        for (saved_host, file_address) in [
            ("STUDIO-PC.Example.Internal", "studio-pc.example.internal"),
            ("[2001:db8::1]", "2001:db8::1"),
            ("2001:DB8::1", "[2001:db8::1]"),
        ] {
            let folder = scratch("spelling");
            let repository = ClientConfigRepository::at(folder.clone());
            let existing = saved("Studio", saved_host);
            let store = repository.add_connection(existing.clone()).unwrap();
            let mut pins = TrustedPinStore::default();
            pins.remember(&enforced_key(&existing), &pin(DIGEST_A), None);
            // A Deck's identities are the ones on disk.
            crate::trusted_pins::save(&repository.trusted_pins_file().unwrap(), &pins).unwrap();
            let profile = parse(&serde_json::json!({
                "address": file_address,
                "host_identity": {"kind": "spki_sha256", "digest": DIGEST_A},
            }))
            .unwrap();
            let review = review_import(profile, &store.connections, &pins);
            assert!(
                matches!(review.pin, PinReview::AlreadyTrusted(_)),
                "{saved_host}: {:?}",
                review.pin
            );
            let replaced = commit_import(
                &review,
                ImportDecision {
                    choice: ImportChoice::Replace,
                    trust_host_identity: false,
                },
                &repository,
                SecurityMode::Medium,
                &store.connections,
                &mut pins,
            )
            .unwrap();
            let updated = &replaced.store.connections[0];
            assert_eq!(updated.host, saved_host);
            assert_eq!(updated.port, existing.port);
            assert!(
                pins.resolved()[&enforced_key(updated)].matches(&pin(DIGEST_A)),
                "{saved_host}"
            );

            // A changed identity the user confirms on Replace lands under
            // the same key, replacing the old one rather than sitting beside
            // it unused.
            let profile = parse(&serde_json::json!({
                "address": file_address,
                "host_identity": {"kind": "spki_sha256", "digest": DIGEST_B},
            }))
            .unwrap();
            let review = review_import(profile, &replaced.store.connections, &pins);
            assert!(matches!(review.pin, PinReview::Differs { .. }));
            let again = commit_import(
                &review,
                ImportDecision {
                    choice: ImportChoice::Replace,
                    trust_host_identity: true,
                },
                &repository,
                SecurityMode::Medium,
                &replaced.store.connections,
                &mut pins,
            )
            .unwrap();
            let updated = &again.store.connections[0];
            assert_eq!(updated.host, saved_host);
            assert!(pins.resolved()[&enforced_key(updated)].matches(&pin(DIGEST_B)));
            let _ = std::fs::remove_dir_all(folder);
        }
    }

    /// One import against a saved connection spelled `saved_host`, with
    /// `pins` remembered and `file_digest` in the file. Returns the saved
    /// connection the import produced (or replaced) and the pins after it.
    fn import_beside(
        tag: &str,
        saved_host: &str,
        pins: &[(&str, &str)],
        file_digest: Option<&str>,
        decision: ImportDecision,
    ) -> (
        PinReview,
        SavedConnectionSummary,
        TrustedPinStore,
        TrustedPinStore,
    ) {
        let folder = scratch(tag);
        let repository = ClientConfigRepository::at(folder.clone());
        let store = repository
            .add_connection(saved("Studio", saved_host))
            .unwrap();
        let mut trusted = TrustedPinStore::default();
        for (key, digest) in pins {
            trusted.remember(key, &pin(digest), None);
        }
        // A Deck's identities are the ones on disk.
        crate::trusted_pins::save(&repository.trusted_pins_file().unwrap(), &trusted).unwrap();
        let mut extra = serde_json::json!({"address": "studio-pc.example.internal"});
        if let Some(digest) = file_digest {
            extra["host_identity"] = serde_json::json!({"kind": "spki_sha256", "digest": digest});
        }
        let review = review_import(parse(&extra).unwrap(), &store.connections, &trusted);
        let committed = commit_import(
            &review,
            decision,
            &repository,
            SecurityMode::Medium,
            &store.connections,
            &mut trusted,
        )
        .unwrap();
        let connection = committed
            .store
            .connections
            .iter()
            .find(|connection| connection.id == committed.connection_id)
            .unwrap()
            .clone();
        let on_disk = repository
            .trusted_pins_file()
            .map(|path| crate::trusted_pins::load(&path))
            .unwrap();
        let _ = std::fs::remove_dir_all(folder);
        (review.pin, connection, trusted, on_disk)
    }

    const KEEP_BOTH: ImportDecision = ImportDecision {
        choice: ImportChoice::KeepBoth,
        trust_host_identity: false,
    };
    const REPLACE: ImportDecision = ImportDecision {
        choice: ImportChoice::Replace,
        trust_host_identity: false,
    };

    #[test]
    fn keep_both_shares_the_saved_spelling_and_its_identity() {
        let saved_key = "STUDIO-PC.Example.Internal:18444";
        for file_digest in [Some(DIGEST_A), None] {
            let (review, added, pins, _) = import_beside(
                "keepboth",
                "STUDIO-PC.Example.Internal",
                &[(saved_key, DIGEST_A)],
                file_digest,
                KEEP_BOTH,
            );
            assert!(!review.needs_decision());
            assert_eq!(added.name, "Studio (2)");
            assert_eq!(enforced_key(&added), saved_key);
            assert!(pins.resolved()[&enforced_key(&added)].matches(&pin(DIGEST_A)));
        }
    }

    #[test]
    fn an_identity_trusted_only_under_the_normalised_address_follows_a_replace() {
        let (review, replaced, pins, on_disk) = import_beside(
            "normalised",
            "STUDIO-PC.Example.Internal",
            &[("studio-pc.example.internal:18444", DIGEST_A)],
            Some(DIGEST_A),
            REPLACE,
        );
        assert!(matches!(review, PinReview::AlreadyTrusted(_)));
        assert_eq!(enforced_key(&replaced), "STUDIO-PC.Example.Internal:18444");
        assert!(pins.resolved()[&enforced_key(&replaced)].matches(&pin(DIGEST_A)));
        assert!(on_disk.resolved()[&enforced_key(&replaced)].matches(&pin(DIGEST_A)));
    }

    #[test]
    fn a_declined_different_identity_keeps_the_trusted_one_on_the_new_connection() {
        let (review, added, pins, on_disk) = import_beside(
            "declined",
            "STUDIO-PC.Example.Internal",
            &[("studio-pc.example.internal:18444", DIGEST_A)],
            Some(DIGEST_B),
            KEEP_BOTH,
        );
        assert!(matches!(review, PinReview::Differs { .. }));
        for store in [&pins, &on_disk] {
            let held = store.resolved()[&enforced_key(&added)];
            assert!(held.matches(&pin(DIGEST_A)) && !held.matches(&pin(DIGEST_B)));
        }
        // Confirmed instead, the file's identity is the one held.
        let (_, added, pins, _) = import_beside(
            "confirmed",
            "STUDIO-PC.Example.Internal",
            &[("studio-pc.example.internal:18444", DIGEST_A)],
            Some(DIGEST_B),
            ImportDecision {
                trust_host_identity: true,
                ..KEEP_BOTH
            },
        );
        assert!(pins.resolved()[&enforced_key(&added)].matches(&pin(DIGEST_B)));
    }

    #[test]
    fn field_names_that_are_not_plain_text_are_dropped_and_only_counted() {
        let newline = "note\nVerified by Arcen: safe to trust";
        let bidi = "colour\u{202E}tsurt";
        let long = "x".repeat(200);
        let mut nested = Map::new();
        nested.insert(newline.to_owned(), "a".into());
        nested.insert("fine".to_owned(), "b".into());
        let mut overrides = Map::new();
        overrides.insert(bidi.to_owned(), "c".into());
        let mut extra = Map::new();
        extra.insert(newline.to_owned(), "fake".into());
        extra.insert(bidi.to_owned(), "fake".into());
        extra.insert(long.clone(), "fake".into());
        extra.insert("zero\u{200B}width".to_owned(), "fake".into());
        extra.insert("line\u{2028}separator".to_owned(), "fake".into());
        extra.insert("nested".to_owned(), Value::Object(nested));
        extra.insert("overrides".to_owned(), Value::Object(overrides));
        extra.insert("ünïcode name".to_owned(), "kept".into());
        let profile = parse(&Value::Object(extra)).unwrap();
        assert_eq!(profile.unreadable_fields, 7);
        assert_eq!(
            profile.unknown.keys().collect::<Vec<_>>(),
            ["nested", "ünïcode name"]
        );
        assert_eq!(profile.unknown["nested"], serde_json::json!({"fine": "b"}));
        assert!(profile.unknown_overrides.is_empty());

        let review = review_import(profile, &[], &TrustedPinStore::default());
        let notes = review.field_notes().join("\n");
        assert_eq!(
            notes,
            "Kept, not used by this version of Arcen: nested and 1 more.\n7 fields ignored."
        );
        for hidden in [
            newline,
            bidi,
            long.as_str(),
            "Verified",
            "ünïcode",
            "\u{202E}",
        ] {
            assert!(!notes.contains(hidden), "{hidden:?} shown");
        }

        let exported = String::from_utf8(review.profile.to_bytes()).unwrap();
        for hidden in ["Verified", "tsurt", long.as_str(), "zero", "separator"] {
            assert!(!exported.contains(hidden), "{hidden:?} exported");
        }
        // And from a saved connection whose kept fields were written by an
        // older build.
        let mut connection = saved("Studio", "pier.example.internal");
        connection.unknown.insert(
            PROFILE_UNKNOWN_KEY.into(),
            serde_json::json!({ "fields": { newline: 1, "kept": 2 }, "overrides": { bidi: 3 } }),
        );
        let profile =
            ConnectionProfile::from_saved(&connection, &TrustedPinStore::default()).unwrap();
        assert_eq!(profile.unreadable_fields, 2);
        assert_eq!(profile.to_json()["kept"], 2);
        assert!(
            !String::from_utf8(profile.to_bytes())
                .unwrap()
                .contains("Verified")
        );
    }

    #[test]
    fn only_plain_ascii_names_are_listed_and_the_rest_are_counted() {
        assert_eq!(listed(&["a", "b.c", "d-e_f"]), "a, b.c, d-e_f");
        assert_eq!(listed(&["a", "with space", "ünï"]), "a and 2 more");
        assert_eq!(listed(&["with space"]), "1 field");
        assert_eq!(listed(&["with space", "ünï"]), "2 fields");
        let many: Vec<String> = (0..15).map(|index| format!("f{index}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(listed(&many).ends_with("f11 and 3 more"));
    }

    #[test]
    fn suggested_file_names_are_safe() {
        assert_eq!(suggested_file_name("Studio"), "Studio.arcen");
        assert_eq!(suggested_file_name("../etc/passwd"), "-etc-passwd.arcen");
        assert_eq!(suggested_file_name("a:b\\c"), "a-b-c.arcen");
        assert_eq!(suggested_file_name(" .. "), "connection.arcen");
        assert_eq!(
            suggested_file_name(&"x".repeat(200)).len(),
            64 + ".arcen".len()
        );
    }
}
