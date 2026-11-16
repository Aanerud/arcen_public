// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's folders: `%APPDATA%\Arcen\Deck` for configuration,
//! `%LOCALAPPDATA%\Arcen\Deck\Logs` for logs (`DRAFT.md` §3.7), named by
//! the shell's known folders rather than environment variables, which a
//! process can inherit wrong.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_LocalAppData, FOLDERID_RoamingAppData,
    KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::GUID;

/// The configuration folder under the roaming application data: the Deck
/// stores no secret, so roaming is acceptable (`DRAFT.md` §3.7).
#[must_use]
pub fn config_dir_in(roaming_app_data: &Path) -> PathBuf {
    roaming_app_data.join("Arcen").join("Deck")
}

/// The log folder under the local application data.
#[must_use]
pub fn log_dir_in(local_app_data: &Path) -> PathBuf {
    local_app_data.join("Arcen").join("Deck").join("Logs")
}

/// Where Colour Check reports are saved (`Documents\Arcen\Colour Checks`,
/// the macOS Deck's `~/Documents/Arcen/Colour Checks`).
#[must_use]
pub fn colour_checks_dir_in(documents: &Path) -> PathBuf {
    documents.join("Arcen").join("Colour Checks")
}

/// `%APPDATA%\Arcen\Deck`.
#[must_use]
pub fn native_config_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_RoamingAppData).map(|folder| config_dir_in(&folder))
}

/// `%LOCALAPPDATA%\Arcen\Deck\Logs`.
#[must_use]
pub fn native_log_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_LocalAppData).map(|folder| log_dir_in(&folder))
}

/// `Documents\Arcen\Colour Checks`.
#[must_use]
pub fn colour_checks_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Documents).map(|folder| colour_checks_dir_in(&folder))
}

/// The Desktop, where a support bundle goes by default.
#[must_use]
pub fn desktop_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop)
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: `id` is a known-folder GUID constant; the returned string is
    // owned by this function and freed with CoTaskMemFree exactly once.
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: on success the shell returns a NUL-terminated wide string.
    let wide = unsafe { path.as_wide() };
    let folder = (!wide.is_empty()).then(|| PathBuf::from(OsString::from_wide(wide)));
    // SAFETY: the string came from SHGetKnownFolderPath and is freed once.
    unsafe { CoTaskMemFree(Some(path.0.cast())) };
    folder
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_deck_folders_mirror_the_documented_layout() {
        assert_eq!(
            config_dir_in(Path::new(r"C:\Users\u\AppData\Roaming")),
            PathBuf::from(r"C:\Users\u\AppData\Roaming\Arcen\Deck")
        );
        assert_eq!(
            log_dir_in(Path::new(r"C:\Users\u\AppData\Local")),
            PathBuf::from(r"C:\Users\u\AppData\Local\Arcen\Deck\Logs")
        );
        assert_eq!(
            colour_checks_dir_in(Path::new(r"C:\Users\u\Documents")),
            PathBuf::from(r"C:\Users\u\Documents\Arcen\Colour Checks")
        );
    }

    #[test]
    fn the_known_folders_resolve_on_this_machine() {
        let config = native_config_dir().expect("roaming app data");
        assert!(config.ends_with(r"Arcen\Deck"));
        let logs = native_log_dir().expect("local app data");
        assert!(logs.ends_with(r"Arcen\Deck\Logs"));
    }
}
