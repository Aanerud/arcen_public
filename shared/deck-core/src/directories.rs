// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Where a Deck keeps its configuration and logs.
//!
//! The platform names its native folders ([`DeckServices::native_config_dir`],
//! [`DeckServices::native_log_dir`]); the overrides that deployment
//! automation and the lab use are applied here, once, for every Deck.
//!
//! | Deck | Config (`settings.json`, `connections.json`, `trusted_pins.json`) | Logs |
//! | --- | --- | --- |
//! | macOS | `~/Library/Application Support/Arcen` | `~/Library/Logs/Arcen` |
//! | Windows | `%APPDATA%\Arcen\Deck` | `%LOCALAPPDATA%\Arcen\Deck\Logs` |
//!
//! No secret is ever stored in either folder (ADR 0024).
//!
//! [`DeckServices::native_config_dir`]: crate::DeckServices::native_config_dir
//! [`DeckServices::native_log_dir`]: crate::DeckServices::native_log_dir

use std::ffi::OsString;
use std::path::PathBuf;

/// Overrides the configuration directory on every Deck.
pub const CONFIG_DIR_ENV: &str = "ARCEN_CONFIG_DIR";
/// Overrides the log directory on every Deck.
pub const LOG_DIR_ENV: &str = "ARCEN_LOG_DIR";

/// The resolved configuration and log directories of one Deck process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeckDirectories {
    /// Persisted client state. `None` when neither an override nor the
    /// platform names one; the Deck then keeps settings in memory only.
    pub config: Option<PathBuf>,
    /// Log files. `None` disables file logging.
    pub logs: Option<PathBuf>,
}

impl DeckDirectories {
    /// Applies [`CONFIG_DIR_ENV`] and [`LOG_DIR_ENV`] over the platform's
    /// native folders. A set variable wins even when the platform has a
    /// folder, exactly as the macOS Deck has always behaved.
    #[must_use]
    pub fn resolve(
        native_config: Option<PathBuf>,
        native_logs: Option<PathBuf>,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        Self {
            config: env(CONFIG_DIR_ENV).map(PathBuf::from).or(native_config),
            logs: env(LOG_DIR_ENV).map(PathBuf::from).or(native_logs),
        }
    }

    /// [`Self::resolve`] with the process environment.
    #[must_use]
    pub fn from_process_env(native_config: Option<PathBuf>, native_logs: Option<PathBuf>) -> Self {
        Self::resolve(native_config, native_logs, |name| std::env::var_os(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn overrides_win_over_native_folders() {
        let dirs = DeckDirectories::resolve(
            Some(PathBuf::from("native-config")),
            Some(PathBuf::from("native-logs")),
            env_with(&[(CONFIG_DIR_ENV, "lab-config"), (LOG_DIR_ENV, "lab-logs")]),
        );
        assert_eq!(dirs.config, Some(PathBuf::from("lab-config")));
        assert_eq!(dirs.logs, Some(PathBuf::from("lab-logs")));
    }

    #[test]
    fn native_folders_apply_without_overrides_and_each_axis_is_independent() {
        let dirs = DeckDirectories::resolve(
            Some(PathBuf::from("native-config")),
            None,
            env_with(&[(LOG_DIR_ENV, "lab-logs")]),
        );
        assert_eq!(dirs.config, Some(PathBuf::from("native-config")));
        assert_eq!(dirs.logs, Some(PathBuf::from("lab-logs")));
        assert_eq!(
            DeckDirectories::resolve(None, None, env_with(&[])),
            DeckDirectories::default()
        );
    }
}
