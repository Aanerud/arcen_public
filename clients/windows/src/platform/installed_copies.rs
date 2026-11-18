// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Other copies of this Deck: every running `arcen-deck.exe` and the copy
//! the installed package put in place (its uninstall entry's
//! `InstallLocation`), each with the version and build its version resource
//! names, handed to the shared decision
//! (`arcen_deck_core::installed_copies`), as the macOS Deck does with Launch
//! Services.

use std::path::{Path, PathBuf};

use arcen_deck_core::installed_copies::{
    CopyFinding, InstalledCopy, ThisCopy, stale_copies, startup_notice as notice,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use super::registry::{self, Hive};

/// The Deck's executable, as the package installs it.
const EXECUTABLE: &str = "arcen-deck.exe";
/// The package's `Name` (`packaging/windows-deck/wix/ArcenDeck.wxs`).
const PACKAGE_NAME: &str = "Arcen Deck";
const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";

/// The warning to show at start when another copy of this Deck may run in
/// its place; each copy is also logged.
#[must_use]
pub fn startup_notice() -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let identity = super::build::current();
    let this_path = shown(&executable, profile.as_deref());
    let mut paths: Vec<(PathBuf, bool)> = running_copies()
        .into_iter()
        .map(|path| (path, true))
        .collect();
    paths.extend(package_copies().into_iter().map(|path| (path, false)));
    let copies: Vec<InstalledCopy> = paths
        .iter()
        .map(|(path, running)| {
            let (version, build) = version_resource(path);
            InstalledCopy {
                path: shown(path, profile.as_deref()),
                version,
                build,
                running: *running,
            }
        })
        .collect();
    let findings = stale_copies(
        &ThisCopy {
            path: &this_path,
            version: &identity.version,
            build: &identity.build_id,
        },
        &copies,
    );
    for finding in &findings {
        let (running, version) = match finding {
            CopyFinding::RunningElsewhere { version, .. } => (true, version),
            CopyFinding::OtherVersion { version, .. } => (false, version),
        };
        tracing::warn!(
            target: "arcen::ui",
            copy = %finding.path(),
            copy_version = version.as_deref().unwrap_or("unknown"),
            running,
            this_version = %identity.version,
            "another Arcen Deck is on this machine"
        );
    }
    notice(&identity.version, &findings)
}

/// `path` with the profile folder written as `~`, as the Deck logs it.
fn shown(path: &Path, profile: Option<&Path>) -> String {
    match profile.and_then(|profile| path.strip_prefix(profile).ok()) {
        Some(relative) if !relative.as_os_str().is_empty() => {
            format!("~\\{}", relative.display())
        }
        _ => path.display().to_string(),
    }
}

/// The executables of every other running `arcen-deck.exe` this account
/// may query.
fn running_copies() -> Vec<PathBuf> {
    // SAFETY: a process snapshot has no preconditions; the handle is closed
    // below.
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    // SAFETY: no preconditions.
    let this = unsafe { GetCurrentProcessId() };
    let mut paths = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: `snapshot` is a live process snapshot; `entry` is a live
    // PROCESSENTRY32W whose dwSize is set.
    let mut more = unsafe { Process32FirstW(snapshot, &raw mut entry) }.is_ok();
    while more {
        let end = entry
            .szExeFile
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
        if entry.th32ProcessID != this && name.eq_ignore_ascii_case(EXECUTABLE) {
            paths.extend(process_image(entry.th32ProcessID));
        }
        // SAFETY: as for Process32FirstW.
        more = unsafe { Process32NextW(snapshot, &raw mut entry) }.is_ok();
    }
    // SAFETY: the snapshot handle is closed once.
    let _ = unsafe { CloseHandle(snapshot) };
    paths
}

/// The executable of process `pid`; `None` when it cannot be queried
/// (another account's process).
fn process_image(pid: u32) -> Option<PathBuf> {
    // SAFETY: OpenProcess has no memory preconditions; the handle is closed
    // below.
    let process: HANDLE =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = [0_u16; 1024];
    let mut length = 1024_u32;
    // SAFETY: `process` is open for PROCESS_QUERY_LIMITED_INFORMATION;
    // `buffer` is writable for `length` characters.
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut length,
        )
    };
    // SAFETY: closed once.
    let _ = unsafe { CloseHandle(process) };
    result.ok()?;
    let length = usize::try_from(length).ok()?.min(buffer.len());
    Some(PathBuf::from(String::from_utf16(&buffer[..length]).ok()?))
}

/// The executable each installed `Arcen Deck` package put in place, when it
/// is still there.
fn package_copies() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for hive in [Hive::LocalMachine, Hive::CurrentUser] {
        for product in registry::subkeys(hive, UNINSTALL) {
            let key = format!(r"{UNINSTALL}\{product}");
            if registry::read_string(hive, &key, "DisplayName").as_deref() != Some(PACKAGE_NAME) {
                continue;
            }
            let Some(location) = registry::read_string(hive, &key, "InstallLocation") else {
                continue;
            };
            let executable = Path::new(location.trim()).join(EXECUTABLE);
            if executable.is_file() {
                paths.push(executable);
            }
        }
    }
    paths
}

/// The version and build stamp an Arcen executable's version resource names
/// (`FileVersion`, `ArcenBuildId`; `packaging/windows-deck/build.ps1`). A
/// development build has none.
fn version_resource(path: &Path) -> (Option<String>, Option<String>) {
    let name = HSTRING::from(path.as_os_str());
    // SAFETY: `name` is a NUL-terminated HSTRING that outlives the call.
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(name.as_ptr()), None) };
    if size == 0 || size > 1 << 20 {
        return (None, None);
    }
    let Ok(length) = usize::try_from(size) else {
        return (None, None);
    };
    let mut block = vec![0_u8; length];
    // SAFETY: `block` is writable for `size` bytes and outlives the call.
    if unsafe { GetFileVersionInfoW(PCWSTR(name.as_ptr()), 0, size, block.as_mut_ptr().cast()) }
        .is_err()
    {
        return (None, None);
    }
    let value = |field: &str| -> Option<String> {
        let query = HSTRING::from(format!(r"\StringFileInfo\040904b0\{field}"));
        let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut characters = 0_u32;
        // SAFETY: `block` holds the version resource read above and
        // outlives the returned pointer's use; the out-parameters are live.
        let found = unsafe {
            VerQueryValueW(
                block.as_ptr().cast(),
                PCWSTR(query.as_ptr()),
                &raw mut pointer,
                &raw mut characters,
            )
        };
        if !found.as_bool() || pointer.is_null() || characters == 0 {
            return None;
        }
        let characters = usize::try_from(characters).ok()?;
        let start = block.as_ptr() as usize;
        let at = pointer as usize;
        // The value must lie inside the block Windows filled.
        if at < start || at + characters * 2 > start + block.len() {
            return None;
        }
        // SAFETY: bounds-checked against `block` above; a version string is
        // UTF-16 and two-byte aligned inside the resource.
        let units = unsafe { std::slice::from_raw_parts(pointer.cast::<u16>(), characters) };
        let end = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        let text = String::from_utf16(&units[..end]).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    };
    (value("FileVersion"), value("ArcenBuildId"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lists what the running machine shows:
    /// `cargo test -p arcen-deck-windows installed_copies -- --ignored --nocapture`
    #[test]
    #[ignore = "reads this machine's processes and installed packages"]
    fn lists_the_copies_on_this_machine() {
        for path in running_copies() {
            println!("running {} {:?}", path.display(), version_resource(&path));
        }
        for path in package_copies() {
            println!("package {} {:?}", path.display(), version_resource(&path));
        }
        println!("notice {:?}", startup_notice());
    }
}
