// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Logging.
//!
//! Log files through `arcen-observability` into the `DeckServices` log
//! directory (`%LOCALAPPDATA%\Arcen\Deck\Logs`, `ARCEN_LOG_DIR` honoured),
//! through the macOS Deck's own runtime (`arcen_deck_core::logging`: same
//! format, same daily rotation, same records); crash minidumps
//! through Windows Error Reporting ([`register_crash_dumps`]); and Settings
//! › Logs › "📂 Show in folder" ([`show_in_folder`]).
//!
//! The support bundle (PARITY Q-10) is shared
//! (`arcen_deck_core::support_bundle`); its Windows facts and these
//! minidumps are gathered by `platform::support`.

#![allow(
    unsafe_code,
    reason = "Win32 is a C API; every unsafe block states its contract"
)]

use std::path::{Path, PathBuf};

use arcen_deck_core::DeckServices;
use arcen_deck_core::logging::LogInit;
use arcen_telemetry::{OperationalProfile, TelemetryPlatform};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use crate::platform::build::DeckBuildFacts;
use crate::platform::registry::{Hive, RegistryValue, read_dword, write_values};

/// WER's per-user crash-dump settings, one subkey per executable name.
pub const WER_LOCAL_DUMPS_KEY: &str =
    r"Software\Microsoft\Windows\Windows Error Reporting\LocalDumps";
/// Minidumps only (`DumpType` 1): a full dump could hold a password still
/// in memory between typing and zeroising.
pub const WER_DUMP_TYPE_MINI: u32 = 1;
/// How many dumps WER keeps before replacing the oldest.
pub const WER_DUMP_COUNT: u32 = 5;
/// Where the dumps land, beside the logs.
pub const CRASH_DUMP_FOLDER: &str = r"%LOCALAPPDATA%\Arcen\Deck\CrashDumps";

/// Starts the Deck's log files in `services`' log directory
/// (`arcen_deck_core::logging::startup_profile` honours `ARCEN_LOG_LEVEL`).
pub fn init(services: &dyn DeckServices, profile: OperationalProfile) -> Option<PathBuf> {
    let started = arcen_deck_core::logging::init(log_init(
        profile,
        services.directories().logs,
        &DeckBuildFacts::running(),
    ));
    // The macOS Deck's `deck_platform` record: which OS the log came from.
    tracing::info!(
        target: arcen_deck_core::log_target::SESSION,
        event = "deck_platform",
        platform_summary = %services.platform_summary(),
        arch = std::env::consts::ARCH,
        "Deck platform profile",
    );
    started
}

/// What the shared runtime needs: `CLIENT_START`'s schema bundle fields
/// carry the executable (a Windows Deck has no bundle).
#[must_use]
pub fn log_init(
    profile: OperationalProfile,
    dir: Option<PathBuf>,
    build: &DeckBuildFacts,
) -> LogInit {
    LogInit {
        profile,
        dir,
        platform: TelemetryPlatform::Windows,
        start_facts: vec![
            ("bundle_short_version", build.version.clone()),
            ("bundle_version", build.version.clone()),
            ("build_id", build.build_id.clone()),
            ("source_revision", build.source_revision.clone()),
            ("bundle_path", build.executable_path.clone()),
        ],
    }
}

/// The WER values for one executable (`DumpFolder`, `DumpCount`,
/// `DumpType`).
#[must_use]
pub fn crash_dump_values() -> [(&'static str, RegistryValue); 3] {
    [
        (
            "DumpFolder",
            RegistryValue::ExpandString(CRASH_DUMP_FOLDER.to_owned()),
        ),
        ("DumpCount", RegistryValue::Dword(WER_DUMP_COUNT)),
        ("DumpType", RegistryValue::Dword(WER_DUMP_TYPE_MINI)),
    ]
}

/// The per-user key for `executable_name` (`arcen-deck.exe`).
#[must_use]
pub fn crash_dump_key(executable_name: &str) -> String {
    format!(r"{WER_LOCAL_DUMPS_KEY}\{executable_name}")
}

/// Asks Windows Error Reporting to keep minidumps of `executable_name`
/// crashes in [`CRASH_DUMP_FOLDER`], per user and without elevation, unless
/// the installer already registered them machine-wide. Idempotent.
///
/// # Errors
///
/// The Win32 error code of the registry write.
pub fn register_crash_dumps(executable_name: &str) -> Result<(), u32> {
    if executable_name.is_empty()
        || executable_name.contains(['\\', '/'])
        || !executable_name.to_ascii_lowercase().ends_with(".exe")
    {
        return Err(87);
    }
    // WER writes to its default `%LOCALAPPDATA%\CrashDumps` instead when
    // the configured folder does not exist at the crash (seen on the lab).
    if let Some(folder) = crash_dump_dir() {
        let _ = std::fs::create_dir_all(folder);
    }
    // The MSI registers the same values machine-wide; a per-user copy would
    // outlive an uninstall (an MSI cannot clean other users' hives).
    if read_dword(
        Hive::LocalMachine,
        &crash_dump_key(executable_name),
        "DumpType",
    )
    .is_some()
    {
        return Ok(());
    }
    write_values(
        Hive::CurrentUser,
        &crash_dump_key(executable_name),
        &crash_dump_values(),
    )
}

/// The folder [`CRASH_DUMP_FOLDER`] names, expanded.
#[must_use]
pub fn crash_dump_dir() -> Option<PathBuf> {
    crate::platform::folders::native_log_dir()
        .and_then(|logs| logs.parent().map(|deck| deck.join("CrashDumps")))
}

/// Opens Explorer on `dir` (creating it first), as the macOS Deck's
/// "📂 Show in folder" opens Finder. Returns whether Explorer started.
#[must_use]
pub fn show_in_folder(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let path = HSTRING::from(dir.as_os_str());
    // SAFETY: a static verb and a NUL-terminated folder path; no window,
    // parameters or working directory.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("explore"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    result.0 as isize > 32
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn crash_dumps_are_minidumps_beside_the_logs() {
        assert_eq!(
            crash_dump_key("arcen-deck.exe"),
            r"Software\Microsoft\Windows\Windows Error Reporting\LocalDumps\arcen-deck.exe"
        );
        let values = crash_dump_values();
        assert!(values.contains(&("DumpType", RegistryValue::Dword(1))));
        assert!(values.contains(&(
            "DumpFolder",
            RegistryValue::ExpandString(r"%LOCALAPPDATA%\Arcen\Deck\CrashDumps".to_owned())
        )));
        assert!(
            crash_dump_dir()
                .expect("local app data")
                .ends_with(r"Arcen\Deck\CrashDumps")
        );
    }

    #[test]
    fn the_start_record_names_the_executable_in_the_bundle_fields() {
        let build = DeckBuildFacts::running();
        let init = log_init(OperationalProfile::Info, None, &build);
        assert_eq!(init.platform, TelemetryPlatform::Windows);
        assert!(
            init.start_facts
                .contains(&("bundle_path", build.executable_path.clone()))
        );
        // What `arcen_deck_core::logging::init` adds before the shell's facts.
        let mut fields = arcen_telemetry::StructuredFields::default();
        for (name, value) in [
            ("version", env!("CARGO_PKG_VERSION").to_owned()),
            ("os", std::env::consts::OS.to_owned()),
            ("arch", std::env::consts::ARCH.to_owned()),
        ]
        .into_iter()
        .chain(init.start_facts)
        {
            fields
                .insert(name, arcen_telemetry::FieldValue::String(value))
                .expect("field");
        }
        let sid = arcen_telemetry::CorrelationId::from_uuid_v4_bytes([7; 16]);
        let validated = arcen_telemetry::ValidatedLifecycleEvent::new(
            arcen_telemetry::LifecycleEventKind::ClientStart,
            sid,
            fields,
        );
        assert!(validated.is_ok(), "{validated:?}");
    }

    #[test]
    fn only_a_plain_executable_name_is_registered() {
        assert_eq!(register_crash_dumps(r"..\evil.exe"), Err(87));
        assert_eq!(register_crash_dumps("arcen-deck"), Err(87));
        assert_eq!(register_crash_dumps(""), Err(87));
    }
}
