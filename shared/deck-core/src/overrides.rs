// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Settings are this Deck and its user; a saved connection is that machine.
//!
//! [`crate::settings::ClientSettings`] holds the application-wide defaults.
//! A saved connection may keep its own value for the settings that depend on
//! the host ([`OverridableSetting`]); anything it does not override follows
//! the default. [`Override`] makes "follows the default" and "this
//! connection's own value" two states of one type rather than a sentinel
//! value, and [`Override::resolve`] says which one won ([`SettingSource`]),
//! so a screen can show "App default (Characters)".
//!
//! [`ConnectionOverrides`] is stored inside each connection's `settings`
//! object in `connections.json`, under the same `snake_case` keys that
//! `settings.json` uses for the defaults ([`OverridableSetting::key`]). A
//! connection profile file can carry the same object unchanged.

use arcen_input::keyboard::{KeyboardSettings, ShortcutPreference};
use arcen_input::text::{OptionKeyPreference, TypingMode};

use crate::config::SavedConnectionSummary;
use crate::settings::{ClientSettings, DisplaysMode};

/// A connection's value for one setting: the application default, or its
/// own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Override<T> {
    /// Follow the application-wide setting.
    #[default]
    Inherit,
    /// This connection's own value, whatever the setting says.
    Own(T),
}

/// Where an effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    /// The application-wide setting.
    AppDefault,
    /// The connection's own value.
    ThisConnection,
}

/// A setting's value in force for a connection, and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective<T> {
    /// The value in force.
    pub value: T,
    /// Which layer decided it.
    pub source: SettingSource,
}

impl<T: Copy> Override<T> {
    /// The value in force, given the application default.
    #[must_use]
    pub fn resolve(self, app_default: T) -> Effective<T> {
        match self {
            Self::Inherit => Effective {
                value: app_default,
                source: SettingSource::AppDefault,
            },
            Self::Own(value) => Effective {
                value,
                source: SettingSource::ThisConnection,
            },
        }
    }

    /// The connection's own value, if it has one.
    #[must_use]
    pub const fn own(self) -> Option<T> {
        match self {
            Self::Inherit => None,
            Self::Own(value) => Some(value),
        }
    }

    /// Whether the connection keeps its own value.
    #[must_use]
    pub const fn is_own(self) -> bool {
        matches!(self, Self::Own(_))
    }
}

impl<T> From<Option<T>> for Override<T> {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Inherit, Self::Own)
    }
}

/// The settings a connection can keep its own value for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverridableSetting {
    /// Typing: Characters or Keys only.
    TypingMode,
    /// What the Option keys do (a Mac Deck), or `AltGr` (a PC Deck).
    OptionKeys,
    /// What Command (⌘) is on the host.
    KeyboardShortcuts,
    /// Displays.
    DisplaysMode,
    /// The clipboard. Only a connection profile file sets it, and it can
    /// only withhold the clipboard: the user's consent in Settings is the
    /// ceiling ([`ConnectionOverrides::effective_clipboard_enabled`]).
    ClipboardEnabled,
}

impl OverridableSetting {
    /// Every overridable setting.
    pub const ALL: [Self; 5] = [
        Self::TypingMode,
        Self::OptionKeys,
        Self::KeyboardShortcuts,
        Self::DisplaysMode,
        Self::ClipboardEnabled,
    ];

    /// The key, the same in `settings.json`, in a saved connection's
    /// `settings` object and in a connection profile file.
    /// `keyboard_shortcuts` has no application-wide setting yet; its
    /// default is [`ShortcutPreference::Automatic`].
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::TypingMode => "typing_mode",
            Self::OptionKeys => "option_keys",
            Self::KeyboardShortcuts => "keyboard_shortcuts",
            Self::DisplaysMode => "displays_mode",
            Self::ClipboardEnabled => "clipboard_enabled",
        }
    }
}

/// A connection's own values; every field follows the application default
/// unless set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnectionOverrides {
    /// Typing. Only [`TypingMode::Auto`] (shown as Characters) and
    /// [`TypingMode::Keys`] are stored; "always characters" is no longer a
    /// setting, only a session's Connection-menu choice.
    pub typing_mode: Override<TypingMode>,
    /// The Option keys (a Mac Deck) or `AltGr` (a PC Deck).
    pub option_keys: Override<OptionKeyPreference>,
    /// What Command is on the host.
    pub keyboard_shortcuts: Override<ShortcutPreference>,
    /// Displays.
    pub displays_mode: Override<DisplaysMode>,
    /// The clipboard; see [`Self::effective_clipboard_enabled`].
    pub clipboard_enabled: Override<bool>,
}

impl ConnectionOverrides {
    /// Whether this connection keeps its own value for `setting`.
    #[must_use]
    pub const fn is_own(&self, setting: OverridableSetting) -> bool {
        match setting {
            OverridableSetting::TypingMode => self.typing_mode.is_own(),
            OverridableSetting::OptionKeys => self.option_keys.is_own(),
            OverridableSetting::KeyboardShortcuts => self.keyboard_shortcuts.is_own(),
            OverridableSetting::DisplaysMode => self.displays_mode.is_own(),
            OverridableSetting::ClipboardEnabled => self.clipboard_enabled.is_own(),
        }
    }

    /// Which layer decides `setting` for this connection, given the
    /// application-wide values: the same resolvers the session connects
    /// with.
    #[must_use]
    pub fn source(&self, setting: OverridableSetting, app: AppDefaults) -> SettingSource {
        match setting {
            OverridableSetting::TypingMode => self.effective_typing_mode(app.typing_mode).source,
            OverridableSetting::OptionKeys => self.effective_option_keys(app.option_keys).source,
            OverridableSetting::KeyboardShortcuts => self.effective_keyboard_shortcuts().source,
            OverridableSetting::DisplaysMode => {
                self.effective_displays_mode(app.displays_mode).source
            }
            OverridableSetting::ClipboardEnabled => {
                self.effective_clipboard_enabled(app.clipboard_enabled)
                    .source
            }
        }
    }

    /// Typing in force, before a session's Connection-menu choice.
    #[must_use]
    pub fn effective_typing_mode(&self, app: TypingMode) -> Effective<TypingMode> {
        self.typing_mode.resolve(app)
    }

    /// The Option keys in force.
    #[must_use]
    pub fn effective_option_keys(
        &self,
        app: OptionKeyPreference,
    ) -> Effective<OptionKeyPreference> {
        self.option_keys.resolve(app)
    }

    /// What Command is on the host. There is no application-wide choice
    /// yet: the default is the host platform's own mapping.
    #[must_use]
    pub fn effective_keyboard_shortcuts(&self) -> Effective<ShortcutPreference> {
        self.keyboard_shortcuts
            .resolve(ShortcutPreference::Automatic)
    }

    /// Displays in force.
    #[must_use]
    pub fn effective_displays_mode(&self, app: DisplaysMode) -> Effective<DisplaysMode> {
        self.displays_mode.resolve(app)
    }

    /// The clipboard in force. The user's consent in Settings is the
    /// ceiling: a connection can turn the clipboard off, never on.
    #[must_use]
    pub const fn effective_clipboard_enabled(&self, app: bool) -> Effective<bool> {
        match self.clipboard_enabled {
            Override::Own(own) if app || !own => Effective {
                value: app && own,
                source: SettingSource::ThisConnection,
            },
            Override::Inherit | Override::Own(_) => Effective {
                value: app,
                source: SettingSource::AppDefault,
            },
        }
    }

    /// The keyboard settings a session applies: Command from this
    /// connection, everything else the platform's.
    #[must_use]
    pub fn keyboard_settings(&self) -> KeyboardSettings {
        KeyboardSettings {
            shortcuts: self.effective_keyboard_shortcuts().value,
            left_option_altgr: false,
            typing: None,
        }
    }

    /// `app` with this connection's values in force.
    #[must_use]
    pub fn apply_to(&self, app: &ClientSettings) -> ClientSettings {
        ClientSettings {
            typing_mode: self.effective_typing_mode(app.typing_mode).value,
            option_key_mode: self.effective_option_keys(app.option_key_mode).value,
            displays_mode: self.effective_displays_mode(app.displays_mode).value,
            clipboard_enabled: self
                .effective_clipboard_enabled(app.clipboard_enabled)
                .value,
            ..*app
        }
    }

    /// Reads the overrides from a settings object, removing every key it
    /// understands; everything else stays in `object`. An absent key
    /// follows the application default, and so does a value this Deck does
    /// not understand (a newer Deck's choice): that value stays in `object`
    /// so a save keeps it on disk, until the user sets this override here
    /// ([`Self::replace_in`]).
    pub fn take_from(object: &mut serde_json::Map<String, serde_json::Value>) -> Self {
        fn take<T>(
            object: &mut serde_json::Map<String, serde_json::Value>,
            setting: OverridableSetting,
            read: impl FnOnce(&serde_json::Value) -> Option<T>,
        ) -> Option<T> {
            let value = read(object.get(setting.key())?)?;
            object.remove(setting.key());
            Some(value)
        }
        let token = |value: &serde_json::Value| value.as_str().map(str::to_owned);
        let typing_mode = take(object, OverridableSetting::TypingMode, |value| {
            TypingMode::from_token(&token(value)?).map(stored_typing_mode)
        });
        let option_keys = take(object, OverridableSetting::OptionKeys, |value| {
            OptionKeyPreference::from_token(&token(value)?)
        });
        let keyboard_shortcuts = take(object, OverridableSetting::KeyboardShortcuts, |value| {
            ShortcutPreference::from_token(&token(value)?)
        });
        let displays_mode = take(object, OverridableSetting::DisplaysMode, |value| {
            DisplaysMode::from_wire(&token(value)?)
        });
        let clipboard_enabled = take(
            object,
            OverridableSetting::ClipboardEnabled,
            serde_json::Value::as_bool,
        );
        Self {
            typing_mode: typing_mode.into(),
            option_keys: option_keys.into(),
            keyboard_shortcuts: keyboard_shortcuts.into(),
            displays_mode: displays_mode.into(),
            clipboard_enabled: clipboard_enabled.into(),
        }
    }

    /// Whether `self` and `other` differ for `setting`.
    #[must_use]
    pub fn differs(&self, other: &Self, setting: OverridableSetting) -> bool {
        match setting {
            OverridableSetting::TypingMode => self.typing_mode != other.typing_mode,
            OverridableSetting::OptionKeys => self.option_keys != other.option_keys,
            OverridableSetting::KeyboardShortcuts => {
                self.keyboard_shortcuts != other.keyboard_shortcuts
            }
            OverridableSetting::DisplaysMode => self.displays_mode != other.displays_mode,
            OverridableSetting::ClipboardEnabled => {
                self.clipboard_enabled != other.clipboard_enabled
            }
        }
    }

    /// Sets the user's `edited` overrides in place of `self`: for each
    /// setting the user changed, a value this Deck did not understand
    /// (kept in `unknown` by [`Self::take_from`]) is replaced; every
    /// setting the user left alone keeps it.
    pub fn replace_in(
        &mut self,
        edited: Self,
        unknown: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        for setting in OverridableSetting::ALL {
            if self.differs(&edited, setting) {
                unknown.remove(setting.key());
            }
        }
        *self = edited;
    }

    /// Writes the overrides into a settings object: a connection's own
    /// value under its key. Where it follows the default nothing is
    /// written (so a connection that overrides nothing is stored as
    /// before), and a value `object` already holds there, one this Deck
    /// did not understand, stays.
    pub fn write_into(&self, object: &mut serde_json::Map<String, serde_json::Value>) {
        let mut put = |setting: OverridableSetting, value: Option<serde_json::Value>| {
            if let Some(value) = value {
                object.insert(setting.key().to_owned(), value);
            }
        };
        put(
            OverridableSetting::TypingMode,
            self.typing_mode
                .own()
                .map(|mode| stored_typing_mode(mode).token().into()),
        );
        put(
            OverridableSetting::OptionKeys,
            self.option_keys.own().map(|keys| keys.token().into()),
        );
        put(
            OverridableSetting::KeyboardShortcuts,
            self.keyboard_shortcuts
                .own()
                .map(|shortcuts| shortcuts.token().into()),
        );
        put(
            OverridableSetting::DisplaysMode,
            self.displays_mode.own().map(|mode| mode.as_wire().into()),
        );
        put(
            OverridableSetting::ClipboardEnabled,
            self.clipboard_enabled.own().map(serde_json::Value::Bool),
        );
    }
}

/// The Typing a setting stores: "always characters" is now Characters
/// ([`TypingMode::Auto`]).
#[must_use]
pub const fn stored_typing_mode(mode: TypingMode) -> TypingMode {
    match mode {
        TypingMode::Characters | TypingMode::Auto => TypingMode::Auto,
        TypingMode::Keys => TypingMode::Keys,
    }
}

impl serde::Serialize for ConnectionOverrides {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serde_json::Map::new();
        self.write_into(&mut object);
        object.serialize(serializer)
    }
}

/// Reads the known keys; unknown ones are dropped here. A caller that must
/// keep them (a profile import) reads the object with
/// [`ConnectionOverrides::take_from`] and keeps what is left.
impl<'de> serde::Deserialize<'de> for ConnectionOverrides {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut object = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        Ok(Self::take_from(&mut object))
    }
}

/// The application-wide values a connection's own settings fall back to,
/// as Edit Connection shows them ("App default (Characters)").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppDefaults {
    /// Settings › Input › Typing.
    pub typing_mode: TypingMode,
    /// The Option keys (`AltGr` on a PC Deck). Settings no longer shows
    /// them; the default is Automatic.
    pub option_keys: OptionKeyPreference,
    /// Settings › Displays.
    pub displays_mode: DisplaysMode,
    /// Settings › Input › Clipboard: the user's consent, which no
    /// connection can exceed.
    pub clipboard_enabled: bool,
}

impl AppDefaults {
    /// The defaults in `settings`.
    #[must_use]
    pub const fn from_settings(settings: &ClientSettings) -> Self {
        Self {
            typing_mode: stored_typing_mode(settings.typing_mode),
            option_keys: settings.option_key_mode,
            displays_mode: settings.displays_mode,
            clipboard_enabled: settings.clipboard_enabled,
        }
    }
}

impl Default for AppDefaults {
    fn default() -> Self {
        Self::from_settings(&ClientSettings::default())
    }
}

/// How many saved connections use their own value for `setting` given the
/// application-wide values `app` (Settings shows "N connections use their
/// own"): a connection counts only where its own value is the one in force
/// ([`ConnectionOverrides::source`]), so a clipboard a connection "allows"
/// while Settings shares none is not counted.
#[must_use]
pub fn connections_with_own(
    connections: &[SavedConnectionSummary],
    app: AppDefaults,
    setting: OverridableSetting,
) -> usize {
    connections
        .iter()
        .filter(|connection| {
            connection.settings.overrides.source(setting, app) == SettingSource::ThisConnection
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_input::text::OptionKeyMode;

    #[test]
    fn a_connection_follows_the_app_default_until_it_has_its_own_value() {
        let inherit = ConnectionOverrides::default();
        assert_eq!(
            inherit.effective_typing_mode(TypingMode::Keys),
            Effective {
                value: TypingMode::Keys,
                source: SettingSource::AppDefault
            }
        );
        let own = ConnectionOverrides {
            typing_mode: Override::Own(TypingMode::Auto),
            displays_mode: Override::Own(DisplaysMode::Windowed),
            ..ConnectionOverrides::default()
        };
        assert_eq!(
            own.effective_typing_mode(TypingMode::Keys),
            Effective {
                value: TypingMode::Auto,
                source: SettingSource::ThisConnection
            }
        );
        let app = ClientSettings::default();
        let applied = own.apply_to(&app);
        assert_eq!(applied.displays_mode, DisplaysMode::Windowed);
        assert_eq!(applied.typing_mode, TypingMode::Auto);
        assert_eq!(applied.option_key_mode, app.option_key_mode);
        assert_eq!(
            inherit.effective_keyboard_shortcuts().value,
            ShortcutPreference::Automatic
        );
    }

    #[test]
    fn a_connection_can_withhold_the_clipboard_but_never_grant_it() {
        let off = ConnectionOverrides {
            clipboard_enabled: Override::Own(false),
            ..ConnectionOverrides::default()
        };
        let on = ConnectionOverrides {
            clipboard_enabled: Override::Own(true),
            ..ConnectionOverrides::default()
        };
        assert_eq!(
            off.effective_clipboard_enabled(true),
            Effective {
                value: false,
                source: SettingSource::ThisConnection
            }
        );
        assert_eq!(
            on.effective_clipboard_enabled(false),
            Effective {
                value: false,
                source: SettingSource::AppDefault
            }
        );
        assert!(on.effective_clipboard_enabled(true).value);
        assert!(!off.apply_to(&ClientSettings::default()).clipboard_enabled);
    }

    #[test]
    fn overrides_round_trip_under_the_settings_keys_and_keep_other_keys() {
        let overrides = ConnectionOverrides {
            typing_mode: Override::Own(TypingMode::Keys),
            option_keys: Override::Own(OptionKeyMode::BothCharacters.into()),
            keyboard_shortcuts: Override::Own(ShortcutPreference::WindowsApp),
            displays_mode: Override::Own(DisplaysMode::SinglePrimary),
            clipboard_enabled: Override::Own(false),
        };
        let json = serde_json::to_value(overrides).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "typing_mode": "keys",
                "option_keys": "both_characters",
                "keyboard_shortcuts": "windows_app",
                "displays_mode": "single_primary",
                "clipboard_enabled": false,
            })
        );
        assert_eq!(
            serde_json::from_value::<ConnectionOverrides>(json).unwrap(),
            overrides
        );
        // Inherit writes nothing, so an untouched connection keeps its shape.
        assert_eq!(
            serde_json::to_value(ConnectionOverrides::default()).unwrap(),
            serde_json::json!({})
        );
        let mut object = serde_json::json!({
            "typing_mode": "telepathy",
            "displays_mode": "windowed",
            "future_setting": "kept",
        })
        .as_object()
        .unwrap()
        .clone();
        let read = ConnectionOverrides::take_from(&mut object);
        assert_eq!(read.typing_mode, Override::Inherit);
        assert_eq!(read.displays_mode, Override::Own(DisplaysMode::Windowed));
        // A value this Deck does not understand stays for the next save.
        assert_eq!(object.len(), 2);
        assert_eq!(object["typing_mode"], "telepathy");
        assert_eq!(object["future_setting"], "kept");
    }

    #[test]
    fn a_newer_decks_value_survives_every_save_until_the_user_sets_that_override() {
        use crate::settings::{ClientConfigRepository, SavedConnectionsStore};
        let root = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("arcen-overrides-newer-value");
        let _ = std::fs::remove_dir_all(&root);
        let repository = ClientConfigRepository::at(root.clone());
        let path = repository.connections_file().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "connections": [{
                    "id": "studio", "name": "Studio", "host": "203.0.113.10",
                    "port": 18444, "transport": "quic",
                    "settings": {
                        "version": 2,
                        "typing_mode": "telepathy",
                        "displays_mode": "hologram",
                        "option_keys": "newer_choice",
                    },
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        let on_disk = |key: &str| -> serde_json::Value {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            value["connections"][0]["settings"][key].clone()
        };
        // Inherit at runtime.
        let store = crate::config::load_saved_connections(&repository)
            .unwrap()
            .unwrap();
        let saved = store.connections[0].clone();
        assert_eq!(saved.settings.overrides.typing_mode, Override::Inherit);
        assert_eq!(saved.settings.overrides.displays_mode, Override::Inherit);
        // Selecting the connection saves the store: nothing is lost.
        let store: SavedConnectionsStore = repository.set_last_selected("studio").unwrap();
        assert_eq!(on_disk("typing_mode"), "telepathy");
        assert_eq!(on_disk("displays_mode"), "hologram");
        // The user sets Displays (only): that value is replaced, the rest
        // stays.
        let original = store.connections[0].clone();
        let mut updated = original.clone();
        let mut edited = updated.settings.overrides;
        edited.displays_mode = Override::Own(DisplaysMode::Windowed);
        updated
            .settings
            .overrides
            .replace_in(edited, &mut updated.settings.unknown);
        let store = repository
            .update_connection("studio", &original, updated)
            .unwrap();
        assert_eq!(on_disk("displays_mode"), "windowed");
        assert_eq!(on_disk("typing_mode"), "telepathy");
        assert_eq!(on_disk("option_keys"), "newer_choice");
        // Back to the default: the key goes, and the newer value with it.
        let original = store.connections[0].clone();
        let mut updated = original.clone();
        let mut edited = updated.settings.overrides;
        edited.displays_mode = Override::Inherit;
        updated
            .settings
            .overrides
            .replace_in(edited, &mut updated.settings.unknown);
        repository
            .update_connection("studio", &original, updated)
            .unwrap();
        assert_eq!(on_disk("displays_mode"), serde_json::Value::Null);
        assert_eq!(on_disk("typing_mode"), "telepathy");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn always_characters_is_stored_as_characters() {
        let mut object = serde_json::json!({"typing_mode": "characters"})
            .as_object()
            .unwrap()
            .clone();
        let read = ConnectionOverrides::take_from(&mut object);
        assert_eq!(read.typing_mode, Override::Own(TypingMode::Auto));
        let mut written = serde_json::Map::new();
        ConnectionOverrides {
            typing_mode: Override::Own(TypingMode::Characters),
            ..ConnectionOverrides::default()
        }
        .write_into(&mut written);
        assert_eq!(written["typing_mode"], "auto");
    }

    #[test]
    fn every_key_but_the_command_key_is_a_settings_json_key() {
        let mut written = serde_json::Map::new();
        crate::config::write_client_settings_into(&ClientSettings::default(), &mut written);
        for setting in OverridableSetting::ALL {
            assert_eq!(
                written.get(setting.key()).is_some(),
                setting != OverridableSetting::KeyboardShortcuts,
                "{}",
                setting.key()
            );
        }
    }

    #[test]
    fn counts_the_connections_that_keep_their_own_value() {
        let connection = |overrides| {
            let settings = crate::config::SavedConnectionSettings {
                overrides,
                ..crate::config::SavedConnectionSettings::default()
            };
            SavedConnectionSummary {
                id: "id".to_owned(),
                kind: crate::config::ConnectionKind::DirectMachine,
                name: "name".to_owned(),
                host: "203.0.113.10".to_owned(),
                port: 18_444,
                use_tls: true,
                transport: crate::config::SavedTransport::Quic,
                identity_security_mode: None,
                settings,
                unknown: serde_json::Map::new(),
            }
        };
        let connections = [
            connection(ConnectionOverrides {
                typing_mode: Override::Own(TypingMode::Keys),
                ..ConnectionOverrides::default()
            }),
            connection(ConnectionOverrides::default()),
            connection(ConnectionOverrides {
                typing_mode: Override::Own(TypingMode::Auto),
                displays_mode: Override::Own(DisplaysMode::Windowed),
                ..ConnectionOverrides::default()
            }),
        ];
        assert_eq!(
            connections_with_own(
                &connections,
                AppDefaults::default(),
                OverridableSetting::TypingMode
            ),
            2
        );
        assert_eq!(
            connections_with_own(
                &connections,
                AppDefaults::default(),
                OverridableSetting::DisplaysMode
            ),
            1
        );
        assert_eq!(
            connections_with_own(
                &connections,
                AppDefaults::default(),
                OverridableSetting::OptionKeys
            ),
            0
        );
    }

    #[test]
    fn a_connection_counts_as_its_own_only_where_its_value_is_in_force() {
        let connection = |overrides| SavedConnectionSummary {
            id: "id".to_owned(),
            kind: crate::config::ConnectionKind::DirectMachine,
            name: "name".to_owned(),
            host: "203.0.113.10".to_owned(),
            port: 18_444,
            use_tls: true,
            transport: crate::config::SavedTransport::Quic,
            identity_security_mode: None,
            settings: crate::config::SavedConnectionSettings {
                overrides,
                ..crate::config::SavedConnectionSettings::default()
            },
            unknown: serde_json::Map::new(),
        };
        let clipboard = |own| {
            connection(ConnectionOverrides {
                clipboard_enabled: Override::Own(own),
                ..ConnectionOverrides::default()
            })
        };
        let connections = [clipboard(true), clipboard(false), clipboard(true)];
        let sharing = AppDefaults {
            clipboard_enabled: true,
            ..AppDefaults::default()
        };
        let not_sharing = AppDefaults {
            clipboard_enabled: false,
            ..sharing
        };
        let count =
            |app| connections_with_own(&connections, app, OverridableSetting::ClipboardEnabled);
        assert_eq!(count(sharing), 3);
        // With sharing off, "allow" cannot raise the ceiling: only the
        // connection that turns it off itself still uses its own.
        assert_eq!(count(not_sharing), 1);
        for setting in OverridableSetting::ALL {
            assert_eq!(
                connections_with_own(
                    &[connection(ConnectionOverrides::default())],
                    sharing,
                    setting
                ),
                0,
                "{}",
                setting.key()
            );
        }
    }
}
