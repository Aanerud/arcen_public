// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's config repository: saved connections (`connections.json`),
//! settings persistence (`settings.json`), the remembered-username
//! migration and atomic writes. Moved from the macOS Deck (ADR 0024
//! extraction step 2); the root comes from `DeckServices::directories`.
//!
//! On-disk compatibility is pinned by the golden fixtures in
//! `shared/deck-core/tests/fixtures/deck-config/`.

#![allow(
    clippy::missing_errors_doc,
    reason = "every fallible function here fails with ClientConfigError, which documents each case"
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_protocol::messages::{CursorMode, TabletModeMsg};
use sha2::{Digest, Sha256};

use crate::settings::{
    ClientConfigError, ClientConfigRepository, ClientSettings, ColorFidelity,
    ColorFidelitySettings, DisplaysMode, LogLevel, PerformanceMode, RemoteUiScale,
    SAVED_CONNECTION_SETTINGS_VERSION, SAVED_CONNECTIONS_VERSION, SavedConnectionsStore,
    SecurityMode, tablet_mode_allows_typed_pen,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionKind {
    DirectMachine,
}

/// The transport a saved connection records (`"transport"` in
/// `connections.json`). QUIC is the only one: an entry saved as `"wss"` by an
/// older Deck loads as QUIC (see [`parse_saved_connections`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedTransport {
    Quic,
}

impl SavedTransport {
    #[must_use]
    pub const fn as_key(self) -> &'static str {
        match self {
            Self::Quic => "quic",
        }
    }

    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Quic => 18_444,
        }
    }

    #[must_use]
    pub const fn accepts_port(self, port: u16) -> bool {
        match self {
            Self::Quic => port != 0,
        }
    }

    #[must_use]
    pub const fn port_validation_message(self) -> &'static str {
        match self {
            Self::Quic => "QUIC (UDP) requires a port from 1 to 65535; 18444 is the default.",
        }
    }
}

impl ConnectionKind {
    #[must_use]
    pub const fn icon(self) -> &'static str {
        match self {
            Self::DirectMachine => "◉",
        }
    }

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::DirectMachine => "Direct Machine",
        }
    }

    #[must_use]
    pub const fn short_description(self) -> &'static str {
        match self {
            Self::DirectMachine => "Connect to a specific workstation by address.",
        }
    }

    #[must_use]
    pub const fn primary_action(self) -> &'static str {
        match self {
            Self::DirectMachine => "Connect",
        }
    }

    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            // Direct-to-Pier product sessions use QUIC on UDP 18444.
            Self::DirectMachine => 18444,
        }
    }

    #[must_use]
    pub const fn as_key(self) -> &'static str {
        match self {
            Self::DirectMachine => "direct",
        }
    }

    /// Every saved connection is a direct machine; an unknown or retired
    /// kind key loads as one.
    #[must_use]
    pub fn from_key(_key: &str) -> Self {
        Self::DirectMachine
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedConnectionSettings {
    pub version: u32,
    pub remembered_username: Option<String>,
    pub tablet_mode_requested: arcen_protocol::messages::TabletModeMsg,
    /// This connection's own Displays choice, or `None` to follow the
    /// application-wide default.
    ///
    /// Deliberately an `Option` rather than a concrete mode: the display
    /// layout a host can serve is a property of that host, not of the user's
    /// general preference. A host that advertises `max_monitors: 1` rejects
    /// Match My Layout outright, and before this the only remedy was to
    /// change the global setting and change it back after switching hosts.
    /// `None` (the default, and the value every previously saved connection
    /// parses as) preserves exactly the old application-wide behaviour.
    pub displays_mode: Option<crate::settings::DisplaysMode>,
    /// What Command, Control and Option are on this connection's host.
    /// The default (and every connection saved before it existed) follows
    /// the host's platform, learnt from its hello.
    pub keyboard: arcen_input::keyboard::KeyboardSettings,
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

impl Default for SavedConnectionSettings {
    fn default() -> Self {
        Self {
            version: 1,
            remembered_username: None,
            tablet_mode_requested: arcen_protocol::messages::TabletModeMsg::LocalTermination,
            displays_mode: None,
            keyboard: arcen_input::keyboard::KeyboardSettings::default(),
            unknown: serde_json::Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedConnectionSummary {
    pub id: String,
    pub kind: ConnectionKind,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    pub transport: SavedTransport,
    pub identity_security_mode: Option<String>,
    pub settings: SavedConnectionSettings,
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

pub const MAX_REMEMBERED_USERNAME_BYTES: usize = 255;

pub fn validated_remembered_username(username: &str) -> Option<String> {
    let username = username.trim();
    (!username.is_empty()
        && username.len() <= MAX_REMEMBERED_USERNAME_BYTES
        && !username.chars().any(char::is_control))
    .then(|| username.to_string())
}

#[must_use]
pub fn normalized_connection_host(host: &str) -> String {
    let host = host.trim();
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    unbracketed.parse::<std::net::IpAddr>().map_or_else(
        |_| unbracketed.trim_end_matches('.').to_ascii_lowercase(),
        |address| address.to_string(),
    )
}

#[must_use]
pub fn migrated_saved_connection_id(
    kind: ConnectionKind,
    host: &str,
    port: u16,
    use_tls: bool,
    security_mode: Option<&str>,
    name: &str,
    ordinal: usize,
) -> String {
    let _ = use_tls;
    let transport_key = SavedTransport::Quic.as_key();
    let material = format!(
        "v1|{}|{}|{}|{}|{}|{}|{}",
        kind.as_key(),
        transport_key,
        normalized_connection_host(host),
        port,
        security_mode.unwrap_or("*"),
        name.trim(),
        ordinal,
    );
    let digest = format!("{:x}", Sha256::digest(material.as_bytes()));
    format!("connection-{}", &digest[..32])
}

#[must_use]
pub fn new_saved_connection_id(
    kind: ConnectionKind,
    host: &str,
    port: u16,
    use_tls: bool,
    security_mode: SecurityMode,
    name: &str,
    ordinal: usize,
) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    migrated_saved_connection_id(
        kind,
        host,
        port,
        use_tls,
        Some(security_mode.key()),
        &format!("{name}|{}|{nonce}", std::process::id()),
        ordinal,
    )
}

#[must_use]
pub fn find_saved_connection_index(
    connections: &[SavedConnectionSummary],
    kind: ConnectionKind,
    host: &str,
    port: u16,
    use_tls: bool,
    transport: SavedTransport,
    security_mode: SecurityMode,
) -> Option<usize> {
    let host = normalized_connection_host(host);
    let endpoint_matches = |connection: &SavedConnectionSummary| {
        connection.kind == kind
            && normalized_connection_host(&connection.host) == host
            && connection.port == port
            && connection.use_tls == use_tls
            && connection.transport == transport
    };
    let exact: Vec<_> = connections
        .iter()
        .enumerate()
        .filter(|(_, connection)| {
            endpoint_matches(connection)
                && connection.identity_security_mode.as_deref() == Some(security_mode.key())
        })
        .map(|(index, _)| index)
        .collect();
    if exact.len() == 1 {
        return exact.first().copied();
    }
    if !exact.is_empty() {
        return None;
    }
    let legacy: Vec<_> = connections
        .iter()
        .enumerate()
        .filter(|(_, connection)| {
            endpoint_matches(connection) && connection.identity_security_mode.is_none()
        })
        .map(|(index, _)| index)
        .collect();
    (legacy.len() == 1).then(|| legacy[0])
}

/// A distributable Deck starts with no environment-specific host knowledge.
#[must_use]
pub fn default_saved_connections() -> Vec<SavedConnectionSummary> {
    Vec::new()
}

/// Load the persisted saved-connections list. Absence is normal; an existing
/// malformed or unreadable document is an explicit error and is never replaced
/// with starter data.
pub fn load_saved_connections(
    repository: &ClientConfigRepository,
) -> Result<Option<SavedConnectionsStore>, ClientConfigError> {
    let path = repository
        .connections_file()
        .ok_or(ClientConfigError::ConfigUnavailable)?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    parse_saved_connections(&bytes).map(Some)
}

#[allow(clippy::too_many_lines, reason = "one schema walk, moved verbatim")]
pub fn parse_saved_connections(bytes: &[u8]) -> Result<SavedConnectionsStore, ClientConfigError> {
    let value = serde_json::from_slice::<serde_json::Value>(bytes)?;
    let mut unknown = value
        .as_object()
        .cloned()
        .ok_or(ClientConfigError::Invalid(
            "saved connections top-level value must be an object",
        ))?;
    let array = unknown
        .remove("connections")
        .and_then(|value| value.as_array().cloned())
        .ok_or(ClientConfigError::Invalid(
            "saved connections must contain a connections array",
        ))?;
    let version = unknown
        .remove("version")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(1);
    let last_selected_connection_id = unknown
        .remove("last_selected_connection_id")
        .and_then(|value| value.as_str().map(str::to_string));
    let legacy_username_fallback = unknown
        .remove("legacy_username_fallback")
        .and_then(|value| value.as_str().map(str::to_string))
        .and_then(|value| validated_remembered_username(&value));
    let mut connections = Vec::new();
    let mut unparsed_connections = Vec::new();
    for (ordinal, raw) in array.into_iter().enumerate() {
        let parsed = (|| {
            let mut entry = raw.as_object()?.clone();
            let name = entry.remove("name")?.as_str()?.to_string();
            let host = entry.remove("host")?.as_str()?.to_string();
            let kind = entry
                .remove("kind")
                .and_then(|value| value.as_str().map(str::to_string))
                .as_deref()
                .map_or(ConnectionKind::DirectMachine, ConnectionKind::from_key);
            let transport = entry
                .remove("transport")
                .and_then(|value| value.as_str().map(str::to_string));
            // Before QUIC, every saved connection was WSS (TCP 18443): an
            // entry without a transport, or with `"wss"`, migrates to QUIC.
            let legacy_wss = transport.as_deref().is_none_or(|value| value == "wss");
            let transport = transport.unwrap_or_else(|| "quic".to_string());
            let saved_transport = match transport.as_str() {
                "quic" | "wss" => SavedTransport::Quic,
                _ => return None,
            };
            let parsed_port = entry
                .remove("port")
                .and_then(|value| value.as_u64())
                .and_then(|value| u16::try_from(value).ok())
                .filter(|value| *value != 0)
                .unwrap_or_else(|| saved_transport.default_port());
            // The WSS product port becomes the QUIC one; any other port is
            // the user's own choice and is kept.
            let port = if legacy_wss && parsed_port == 18_443 {
                SavedTransport::Quic.default_port()
            } else {
                parsed_port
            };
            let use_tls = true;
            let identity_security_mode = entry
                .remove("security_mode")
                .and_then(|value| value.as_str().map(str::to_string));
            let settings = parse_saved_connection_settings(entry.remove("settings"));
            let id = entry
                .remove("id")
                .and_then(|value| value.as_str().map(str::to_string))
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| {
                    migrated_saved_connection_id(
                        kind,
                        &host,
                        port,
                        use_tls,
                        identity_security_mode.as_deref(),
                        &name,
                        ordinal,
                    )
                });
            Some(SavedConnectionSummary {
                id,
                kind,
                name,
                host,
                port,
                use_tls,
                transport: saved_transport,
                identity_security_mode,
                settings,
                unknown: entry,
            })
        })();
        if let Some(connection) = parsed {
            connections.push(connection);
        } else {
            unparsed_connections.push(raw);
        }
    }
    Ok(SavedConnectionsStore {
        version,
        connections,
        unparsed_connections,
        last_selected_connection_id,
        legacy_username_fallback,
        unknown,
        fingerprint: Some(Sha256::digest(bytes).into()),
    })
}

pub fn parse_saved_connection_settings(
    value: Option<serde_json::Value>,
) -> SavedConnectionSettings {
    let Some(mut unknown) = value.and_then(|value| value.as_object().cloned()) else {
        return SavedConnectionSettings::default();
    };
    let version = unknown
        .remove("version")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(SAVED_CONNECTION_SETTINGS_VERSION);
    let remembered_username = match unknown.get("remembered_username") {
        Some(serde_json::Value::String(value)) => {
            let value = validated_remembered_username(value);
            unknown.remove("remembered_username");
            value
        }
        Some(serde_json::Value::Null) => {
            unknown.remove("remembered_username");
            None
        }
        _ => None,
    };
    let tablet_mode_requested = match unknown
        .remove("tablet_mode_requested")
        .and_then(|value| value.as_str().map(str::to_string))
        .as_deref()
    {
        // `wacom_usb_bridge` is the 0.15 spelling, still written so an older
        // Deck reads this file; the neutral spelling is accepted too.
        Some("wacom_usb_bridge" | "usb_tablet_bridge") => TabletModeMsg::UsbTabletBridge,
        Some("disabled_mouse_compat") => TabletModeMsg::DisabledMouseCompat,
        _ => TabletModeMsg::LocalTermination,
    };
    // An unrecognised or absent value resolves to `None` -- follow the
    // application default -- and is removed from `unknown` either way so a
    // stale key can never shadow the parsed field on the next write.
    let displays_mode = unknown
        .remove("displays_mode")
        .and_then(|value| value.as_str().map(str::to_string))
        .as_deref()
        .and_then(DisplaysMode::from_wire);
    // Absent or unrecognised: the host platform's own mapping. Saved
    // connections from before these settings existed read as that.
    let shortcuts = unknown
        .remove("keyboard_shortcuts")
        .and_then(|value| value.as_str().map(str::to_string))
        .as_deref()
        .and_then(arcen_input::keyboard::ShortcutPreference::from_token)
        .unwrap_or_default();
    let left_option_altgr = unknown
        .remove("left_option_altgr")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    // Absent or unrecognised: the application-wide typing setting.
    let typing = unknown
        .remove("keyboard_mode")
        .and_then(|value| value.as_str().map(str::to_string))
        .as_deref()
        .and_then(arcen_input::text::KeyboardMode::from_token);
    SavedConnectionSettings {
        version,
        remembered_username,
        tablet_mode_requested,
        displays_mode,
        keyboard: arcen_input::keyboard::KeyboardSettings {
            shortcuts,
            left_option_altgr,
            typing,
        },
        unknown,
    }
}

pub fn serialize_saved_connections(
    store: &SavedConnectionsStore,
) -> Result<Vec<u8>, ClientConfigError> {
    let mut items: Vec<serde_json::Value> = store
        .connections
        .iter()
        .map(|c| {
            let mut settings = c.settings.unknown.clone();
            settings.insert("version".to_string(), c.settings.version.into());
            if let Some(username) = &c.settings.remembered_username {
                settings.insert(
                    "remembered_username".to_string(),
                    serde_json::Value::String(username.clone()),
                );
            } else {
                settings
                    .entry("remembered_username".to_string())
                    .or_insert(serde_json::Value::Null);
            }
            settings.insert(
                "tablet_mode_requested".to_string(),
                match c.settings.tablet_mode_requested {
                    TabletModeMsg::LocalTermination => "local_termination",
                    TabletModeMsg::UsbTabletBridge => "wacom_usb_bridge",
                    TabletModeMsg::DisabledMouseCompat => "disabled_mouse_compat",
                }
                .into(),
            );
            // Absent rather than null when this connection follows the
            // application default, so an untouched connection's stored
            // shape is unchanged from before this setting existed.
            match c.settings.displays_mode {
                Some(mode) => {
                    settings.insert("displays_mode".to_string(), mode.as_wire().into());
                }
                None => {
                    settings.remove("displays_mode");
                }
            }
            // Absent while they are the defaults, so an untouched connection
            // is stored exactly as before.
            let keyboard = c.settings.keyboard;
            if keyboard.shortcuts == arcen_input::keyboard::ShortcutPreference::Automatic {
                settings.remove("keyboard_shortcuts");
            } else {
                settings.insert(
                    "keyboard_shortcuts".to_string(),
                    keyboard.shortcuts.token().into(),
                );
            }
            if keyboard.left_option_altgr {
                settings.insert("left_option_altgr".to_string(), true.into());
            } else {
                settings.remove("left_option_altgr");
            }
            match keyboard.typing {
                Some(mode) => {
                    settings.insert("keyboard_mode".to_string(), mode.token().into());
                }
                None => {
                    settings.remove("keyboard_mode");
                }
            }
            let mut entry = c.unknown.clone();
            entry.insert("id".to_string(), c.id.clone().into());
            entry.insert("kind".to_string(), c.kind.as_key().into());
            entry.insert("name".to_string(), c.name.clone().into());
            entry.insert("host".to_string(), c.host.clone().into());
            entry.insert("port".to_string(), c.port.into());
            entry.insert("transport".to_string(), c.transport.as_key().into());
            if let Some(mode) = &c.identity_security_mode {
                entry.insert("security_mode".to_string(), mode.clone().into());
            } else {
                entry.remove("security_mode");
            }
            entry.insert("settings".to_string(), settings.into());
            entry.into()
        })
        .collect();
    items.extend(store.unparsed_connections.iter().cloned());
    let mut value = store.unknown.clone();
    value.insert(
        "version".to_string(),
        store.version.max(SAVED_CONNECTIONS_VERSION).into(),
    );
    value.insert("connections".to_string(), items.into());
    if let Some(id) = &store.last_selected_connection_id {
        value.insert("last_selected_connection_id".to_string(), id.clone().into());
    } else {
        value.remove("last_selected_connection_id");
    }
    if let Some(username) = &store.legacy_username_fallback {
        value.insert(
            "legacy_username_fallback".to_string(),
            username.clone().into(),
        );
    } else {
        value.remove("legacy_username_fallback");
    }
    Ok(serde_json::to_vec_pretty(&serde_json::Value::Object(
        value,
    ))?)
}

pub fn write_saved_connections(
    repository: &ClientConfigRepository,
    store: &mut SavedConnectionsStore,
) -> Result<(), ClientConfigError> {
    let path = repository
        .connections_file()
        .ok_or(ClientConfigError::ConfigUnavailable)?;
    let bytes = serialize_saved_connections(store)?;
    atomic_write_json(&path, &bytes)?;
    store.fingerprint = Some(Sha256::digest(&bytes).into());
    Ok(())
}

/// Persists `store` as a whole (tests and tools).
///
/// # Panics
///
/// When the snapshot cannot be written.
#[allow(
    clippy::expect_used,
    reason = "a test and tooling helper that documents its panic"
)]
pub fn save_saved_connections(repository: &ClientConfigRepository, store: &SavedConnectionsStore) {
    repository
        .save_connections_snapshot(store)
        .expect("test saved-connections snapshot should persist");
}

static ATOMIC_FILE_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub enum AtomicWriteStep {
    Created,
    Synced,
}

#[derive(Debug)]
pub struct PendingAtomicFile {
    pub path: std::path::PathBuf,
    pub file: std::fs::File,
    pub renamed: bool,
}

impl Drop for PendingAtomicFile {
    fn drop(&mut self) {
        if !self.renamed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub fn atomic_write_json(path: &std::path::Path, bytes: &[u8]) -> Result<(), ClientConfigError> {
    atomic_write_json_with_hook(path, bytes, |_| Ok(()))
}

pub fn atomic_write_json_with_hook(
    path: &std::path::Path,
    bytes: &[u8],
    mut hook: impl FnMut(AtomicWriteStep) -> std::io::Result<()>,
) -> Result<(), ClientConfigError> {
    use std::io::Write;

    let parent = path.parent().ok_or(ClientConfigError::ConfigUnavailable)?;
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .ok_or(ClientConfigError::ConfigUnavailable)?
        .to_string_lossy();
    let mut pending = None;
    for _ in 0..64 {
        let nonce = ATOMIC_FILE_NONCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()));
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => {
                pending = Some(PendingAtomicFile {
                    path: temporary,
                    file,
                    renamed: false,
                });
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut pending = pending.ok_or_else(|| {
        ClientConfigError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a unique atomic-write file",
        ))
    })?;

    // A symbolic link in the configuration's place is refused on every OS
    // (std reads links on Windows too); only the mode bits below are Unix's.
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ClientConfigError::Invalid(
            "refusing to replace a symbolic-link configuration file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata.permissions().mode(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0o600,
            Err(error) => return Err(error.into()),
        };
        pending
            .file
            .set_permissions(std::fs::Permissions::from_mode(mode))?;
    }

    hook(AtomicWriteStep::Created)?;
    pending.file.write_all(bytes)?;
    pending.file.sync_all()?;
    hook(AtomicWriteStep::Synced)?;
    std::fs::rename(&pending.path, path)?;
    pending.renamed = true;
    // Persist the rename itself. A directory handle can be fsynced on Unix;
    // Windows refuses to open a directory as a file (access denied) and
    // commits the rename through the file system journal instead.
    if cfg!(unix) {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub fn migrate_legacy_username(
    store: &mut SavedConnectionsStore,
    repository: &ClientConfigRepository,
) -> Result<(), ClientConfigError> {
    let username = store
        .legacy_username_fallback
        .take()
        .or_else(|| load_legacy_remembered_username(repository));
    let Some(username) = username else {
        return Ok(());
    };
    let identified = store
        .last_selected_connection_id
        .as_ref()
        .and_then(|id| store.connections.iter_mut().find(|saved| saved.id == *id));
    if let Some(saved) = identified
        && saved.settings.remembered_username.is_none()
    {
        saved.settings.remembered_username = Some(username);
        saved.settings.unknown.remove("remembered_username");
    }
    store.version = store.version.max(SAVED_CONNECTIONS_VERSION);
    if store.fingerprint.is_some() {
        *store = repository.save_connections_snapshot(store)?;
    }
    clear_legacy_remembered_username(repository)?;
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "one schema walk, moved verbatim; out-of-range numbers keep their historical wrap"
)]
pub fn load_client_settings(
    repository: &ClientConfigRepository,
) -> Result<ClientSettings, ClientConfigError> {
    let mut settings = ClientSettings::default();
    let path = repository
        .settings_file()
        .ok_or(ClientConfigError::ConfigUnavailable)?;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(settings),
        Err(error) => return Err(error.into()),
    };
    let value = serde_json::from_str::<serde_json::Value>(&text)?;
    if !value.is_object() {
        return Err(ClientConfigError::Invalid(
            "top-level value must be an object",
        ));
    }
    if let Some(level) = value.get("log_level").and_then(serde_json::Value::as_u64) {
        settings.log_level = LogLevel::from_index(level as u8);
    }
    if let Some(mode) = value
        .get("security_mode")
        .and_then(serde_json::Value::as_str)
    {
        settings.security_mode = SecurityMode::from_key(mode);
    }
    if let Some(mode) = value
        .get("performance_mode")
        .and_then(serde_json::Value::as_str)
    {
        settings.performance_mode = PerformanceMode::from_key(mode);
    }
    if let Some(mode) = value
        .get("displays_mode")
        .and_then(serde_json::Value::as_str)
    {
        settings.displays_mode = DisplaysMode::from_key(mode);
    }
    if let Some(mode) = value
        .get("cursor_preference")
        .and_then(serde_json::Value::as_str)
    {
        settings.cursor_preference = match mode {
            "host" => CursorMode::Host,
            _ => CursorMode::Local,
        };
    }
    if let Some(enabled) = value
        .get("clipboard_enabled")
        .and_then(serde_json::Value::as_bool)
    {
        settings.clipboard_enabled = enabled;
    }
    // Microphone consent never survives process launch. Ignore legacy values,
    // including malformed ones, rather than treating them as current consent.
    settings.microphone_enabled = false;
    if let Some(flag) = value
        .get("hidpi_streaming")
        .and_then(serde_json::Value::as_bool)
    {
        settings.hidpi_streaming = flag;
    }
    if let Some(flag) = value
        .get("fullscreen_uses_notch_area")
        .and_then(serde_json::Value::as_bool)
    {
        settings.fullscreen_uses_notch_area = flag;
    }
    if let Some(percent) = value
        .get("remote_ui_scale_percent")
        .and_then(serde_json::Value::as_u64)
    {
        settings.remote_ui_scale_percent = RemoteUiScale::from_percent(percent as u32).as_percent();
    }
    if let Some(mask) = value
        .get("full_color_display_mask")
        .and_then(serde_json::Value::as_u64)
    {
        settings.full_color_display_mask = u8::try_from(mask).unwrap_or(u8::MAX) & 0x0f;
    }
    if let Some(mask) = value
        .get("hidpi_display_mask")
        .and_then(serde_json::Value::as_u64)
    {
        settings.hidpi_display_mask = u8::try_from(mask).unwrap_or(u8::MAX) & 0x0f;
    }
    if let Some(enabled) = value
        .get("tablet_input_enabled")
        .and_then(serde_json::Value::as_bool)
    {
        settings.tablet_input_enabled = enabled;
    }
    settings.tablet_mode_requested = if settings.tablet_input_enabled {
        TabletModeMsg::LocalTermination
    } else {
        TabletModeMsg::DisabledMouseCompat
    };
    if let Some(mode) = value
        .get("tablet_mode_requested")
        .and_then(serde_json::Value::as_str)
    {
        settings.tablet_mode_requested = match mode {
            // Legacy 0.15 token (still written) or the neutral spelling.
            "wacom_usb_bridge" | "usb_tablet_bridge" => TabletModeMsg::UsbTabletBridge,
            "disabled_mouse_compat" => TabletModeMsg::DisabledMouseCompat,
            _ => TabletModeMsg::LocalTermination,
        };
        settings.tablet_input_enabled =
            tablet_mode_allows_typed_pen(settings.tablet_mode_requested);
    }
    if let Some(enabled) = value
        .get("exact_still_image")
        .and_then(serde_json::Value::as_bool)
    {
        settings.exact_still_image = enabled;
    }
    let mut color_fidelity = ColorFidelitySettings::default();
    if let Some(key) = value
        .get("color_fidelity")
        .and_then(serde_json::Value::as_str)
    {
        color_fidelity.preset = ColorFidelity::from_key(key);
    }
    if let Some(token) = value
        .get("color_fidelity_codec_override")
        .and_then(serde_json::Value::as_str)
    {
        color_fidelity.overrides.codec = arcen_media::VideoCodec::from_token(token);
    }
    if let Some(token) = value
        .get("color_fidelity_chroma_override")
        .and_then(serde_json::Value::as_str)
    {
        color_fidelity.overrides.chroma = arcen_media::ChromaSubsampling::from_token(token);
    }
    if let Some(token) = value
        .get("color_fidelity_bit_depth_override")
        .and_then(serde_json::Value::as_str)
    {
        color_fidelity.overrides.bit_depth = arcen_media::BitDepth::from_token(token);
    }
    if let Some(token) = value
        .get("color_fidelity_range_override")
        .and_then(serde_json::Value::as_str)
    {
        color_fidelity.overrides.range = arcen_media::ColorRange::from_token(token);
    }
    if let Some(token) = value
        .get("color_fidelity_matrix_override")
        .and_then(serde_json::Value::as_str)
    {
        color_fidelity.overrides.matrix = arcen_media::ColorMatrix::from_token(token);
    }
    if let Some(id) = value
        .get("color_fidelity_variant_override")
        .and_then(serde_json::Value::as_str)
    {
        color_fidelity.variant_override = arcen_media::video::VideoVariant::from_id(id).ok();
    }
    settings.color_fidelity = color_fidelity;
    // Unlike the colour axes -- whose overrides are `Option`, so an
    // unreadable token legitimately means "no override" -- intent has no
    // absent state. An unknown or non-string value therefore leaves the
    // default in place rather than clearing anything, so a settings.json
    // written by a newer build (or hand-edited) can only ever fail back to
    // Legacy builds persisted this independently. It is intentionally ignored
    // now: the user-facing Colour Fidelity preset is the sole authority.
    settings.encode_intent = settings.color_fidelity.preset.encode_intent();
    if let Some(mode) = value
        .get("typing_mode")
        .and_then(serde_json::Value::as_str)
        .and_then(arcen_input::text::TypingMode::from_token)
    {
        settings.typing_mode = mode;
    }
    // `option_keys` knows Automatic; older builds wrote only
    // `option_key_mode`, their default included (see `from_settings`).
    settings.option_key_mode = arcen_input::text::OptionKeyPreference::from_settings(
        value.get("option_keys").and_then(serde_json::Value::as_str),
        value
            .get("option_key_mode")
            .and_then(serde_json::Value::as_str),
    );
    if let Some(chars) = value
        .get("paste_confirm_above")
        .and_then(serde_json::Value::as_u64)
        .and_then(|chars| usize::try_from(chars).ok())
    {
        settings.paste.confirm_above = chars;
    }
    if let Some(rate) = value
        .get("paste_chars_per_second")
        .and_then(serde_json::Value::as_u64)
        .and_then(|rate| u32::try_from(rate).ok())
    {
        settings.paste.chars_per_second = rate;
    }
    settings.paste = settings.paste.clamped();
    Ok(settings)
}

#[allow(clippy::too_many_lines, reason = "one schema write, moved verbatim")]
pub fn save_client_settings(
    repository: &ClientConfigRepository,
    settings: &ClientSettings,
) -> Result<(), ClientConfigError> {
    let path = repository
        .settings_file()
        .ok_or(ClientConfigError::ConfigUnavailable)?;
    let mut value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)?
            .as_object()
            .cloned()
            .ok_or(ClientConfigError::Invalid(
                "client settings top-level value must be an object",
            ))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => return Err(error.into()),
    };
    value.insert("log_level".to_string(), settings.log_level.index().into());
    value.insert(
        "security_mode".to_string(),
        settings.security_mode.key().into(),
    );
    value.insert(
        "performance_mode".to_string(),
        settings.performance_mode.key().into(),
    );
    value.insert(
        "displays_mode".to_string(),
        settings.displays_mode.key().into(),
    );
    value.insert(
        "cursor_preference".to_string(),
        match settings.cursor_preference {
            CursorMode::Local => "local",
            CursorMode::Host => "host",
        }
        .into(),
    );
    value.insert(
        "clipboard_enabled".to_string(),
        settings.clipboard_enabled.into(),
    );
    value.insert(
        "hidpi_streaming".to_string(),
        settings.hidpi_streaming.into(),
    );
    value.insert(
        "typing_mode".to_string(),
        settings.typing_mode.token().into(),
    );
    value.insert(
        "option_keys".to_string(),
        settings.option_key_mode.token().into(),
    );
    // For older builds, which read only this key.
    value.insert(
        "option_key_mode".to_string(),
        settings
            .option_key_mode
            .resolve(arcen_protocol::messages::HostPlatform::Unknown)
            .token()
            .into(),
    );
    value.insert(
        "paste_confirm_above".to_string(),
        settings.paste.confirm_above.into(),
    );
    value.insert(
        "paste_chars_per_second".to_string(),
        settings.paste.chars_per_second.into(),
    );
    value.insert(
        "fullscreen_uses_notch_area".to_string(),
        settings.fullscreen_uses_notch_area.into(),
    );
    value.insert(
        "remote_ui_scale_percent".to_string(),
        settings.remote_ui_scale_percent.into(),
    );
    value.insert(
        "full_color_display_mask".to_string(),
        settings.full_color_display_mask.into(),
    );
    value.insert(
        "hidpi_display_mask".to_string(),
        settings.hidpi_display_mask.into(),
    );
    value.insert(
        "tablet_input_enabled".to_string(),
        settings.tablet_input_enabled.into(),
    );
    value.insert(
        "exact_still_image".to_string(),
        settings.exact_still_image.into(),
    );
    value.insert(
        "tablet_mode_requested".to_string(),
        match settings.tablet_mode_requested {
            TabletModeMsg::LocalTermination => "local_termination",
            TabletModeMsg::UsbTabletBridge => "wacom_usb_bridge",
            TabletModeMsg::DisabledMouseCompat => "disabled_mouse_compat",
        }
        .into(),
    );
    value.insert(
        "color_fidelity".to_string(),
        settings.color_fidelity.preset.key().into(),
    );
    match settings.color_fidelity.overrides.codec {
        Some(codec) => {
            value.insert(
                "color_fidelity_codec_override".to_string(),
                codec.token().into(),
            );
        }
        None => {
            value.remove("color_fidelity_codec_override");
        }
    }
    match settings.color_fidelity.overrides.chroma {
        Some(chroma) => {
            value.insert(
                "color_fidelity_chroma_override".to_string(),
                chroma.token().into(),
            );
        }
        None => {
            value.remove("color_fidelity_chroma_override");
        }
    }
    match settings.color_fidelity.overrides.bit_depth {
        Some(bit_depth) => {
            value.insert(
                "color_fidelity_bit_depth_override".to_string(),
                bit_depth.token().into(),
            );
        }
        None => {
            value.remove("color_fidelity_bit_depth_override");
        }
    }
    match settings.color_fidelity.overrides.range {
        Some(range) => {
            value.insert(
                "color_fidelity_range_override".to_string(),
                range.token().into(),
            );
        }
        None => {
            value.remove("color_fidelity_range_override");
        }
    }
    match settings.color_fidelity.overrides.matrix {
        Some(matrix) => {
            value.insert(
                "color_fidelity_matrix_override".to_string(),
                matrix.token().into(),
            );
        }
        None => {
            value.remove("color_fidelity_matrix_override");
        }
    }
    match settings.color_fidelity.variant_override {
        Some(variant) => {
            value.insert(
                "color_fidelity_variant_override".to_string(),
                variant.id().into(),
            );
        }
        None => {
            value.remove("color_fidelity_variant_override");
        }
    }
    value.insert(
        "encode_intent".to_string(),
        settings
            .color_fidelity
            .preset
            .encode_intent()
            .token()
            .into(),
    );
    let bytes = serde_json::to_vec_pretty(&serde_json::Value::Object(value))?;
    atomic_write_json(&path, &bytes)
}

pub fn load_legacy_remembered_username(repository: &ClientConfigRepository) -> Option<String> {
    if let Some(username) = repository
        .remembered_username_file()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|username| validated_remembered_username(&username))
    {
        return Some(username);
    }
    let value = repository
        .settings_file()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())?;
    value
        .get("remembered_username")
        .and_then(serde_json::Value::as_str)
        .and_then(validated_remembered_username)
}

pub fn clear_legacy_remembered_username(
    repository: &ClientConfigRepository,
) -> Result<(), ClientConfigError> {
    if let Some(path) = repository.remembered_username_file() {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let Some(path) = repository.settings_file() else {
        return Ok(());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut value = serde_json::from_str::<serde_json::Value>(&text)?;
    let object = value.as_object_mut().ok_or(ClientConfigError::Invalid(
        "client settings top-level value must be an object",
    ))?;
    if object.remove("remembered_username").is_some() {
        atomic_write_json(&path, &serde_json::to_vec_pretty(&value)?)?;
    }
    Ok(())
}
