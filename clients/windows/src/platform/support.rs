// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows side of the support bundle: GPUs and their drivers, the
//! Windows build, crash-dump registration and this Deck's WER minidumps,
//! the Desktop, the folder dialog and Explorer. The bundle itself is
//! shared (`arcen_deck_core::support_bundle`).

#![allow(
    unsafe_code,
    reason = "DXGI and the shell dialog are COM; every unsafe block states its contract"
)]

use std::path::{Path, PathBuf};

use arcen_deck_core::support_bundle::{CrashReportKind, CrashReportSource, SupportDetails};

use super::registry::{Hive, read_dword};

/// The bundle's component.
#[must_use]
pub fn component() -> arcen_telemetry::BundleComponent {
    arcen_telemetry::BundleComponent {
        name: "arcen-deck-windows".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        os: "windows".to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    }
}

/// A user-mode driver version (`IDXGIAdapter::CheckInterfaceSupport`) as
/// Windows prints it: `32.0.15.6094`.
#[must_use]
pub fn driver_version(umd: i64) -> String {
    let parts = umd.to_be_bytes();
    let word = |index: usize| u16::from_be_bytes([parts[index], parts[index + 1]]);
    format!("{}.{}.{}.{}", word(0), word(2), word(4), word(6))
}

/// Every DXGI adapter: name, PCI ids, LUID, memory, driver version.
fn adapters() -> Vec<serde_json::Value> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIDevice, IDXGIFactory1,
    };
    use windows::core::Interface;
    let mut found = Vec::new();
    // SAFETY: plain DXGI enumeration; every returned interface is owned
    // and released on drop; `GetDesc1` fills an owned struct.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return found;
        };
        let mut index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            index += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_owned();
            let driver = adapter
                .CheckInterfaceSupport(&IDXGIDevice::IID)
                .ok()
                .map(driver_version);
            let luid =
                (i64::from(desc.AdapterLuid.HighPart) << 32) | i64::from(desc.AdapterLuid.LowPart);
            found.push(serde_json::json!({
                "name": name,
                "vendor_id": format!("{:04x}", desc.VendorId),
                "device_id": format!("{:04x}", desc.DeviceId),
                "luid": format!("{luid:016x}"),
                "dedicated_video_memory_mb": desc.DedicatedVideoMemory / (1024 * 1024),
                "software": desc.Flags & u32::try_from(DXGI_ADAPTER_FLAG_SOFTWARE.0).unwrap_or(0) != 0,
                "driver_version": driver,
            }));
        }
    }
    found
}

/// This Deck's WER minidumps (`arcen-deck*.dmp`): in the Deck's
/// crash-dump folder, and in WER's default `%LOCALAPPDATA%\CrashDumps`,
/// where WER writes when the configured folder did not exist at the crash.
fn crash_reports() -> Vec<CrashReportSource> {
    let mut folders: Vec<PathBuf> = crate::logging::crash_dump_dir().into_iter().collect();
    if let Some(default) = super::folders::native_log_dir().and_then(|logs| {
        // `%LOCALAPPDATA%\Arcen\Deck\Logs` → `%LOCALAPPDATA%\CrashDumps`.
        logs.ancestors()
            .nth(3)
            .map(|local| local.join("CrashDumps"))
    }) {
        folders.push(default);
    }
    let mut found = Vec::new();
    for directory in folders {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        found.extend(
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                    name.starts_with("arcen-deck")
                        && path
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("dmp"))
                })
                .map(|entry| CrashReportSource {
                    path: entry.path(),
                    kind: CrashReportKind::WindowsMinidump,
                }),
        );
    }
    found
}

/// The platform facts and crash reports for the bundle.
#[must_use]
pub fn details(session_adapter: Option<i64>) -> SupportDetails {
    let version = super::build::running_windows();
    let executable = std::env::current_exe().ok().and_then(|exe| {
        exe.file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
    });
    let wer = executable.as_deref().map(|name| {
        let key = crate::logging::crash_dump_key(name);
        serde_json::json!({
            "machine_dump_type": read_dword(Hive::LocalMachine, &key, "DumpType"),
            "user_dump_type": read_dword(Hive::CurrentUser, &key, "DumpType"),
            "dump_folder": crate::logging::CRASH_DUMP_FOLDER,
        })
    });
    SupportDetails {
        platform: serde_json::json!({
            "windows": {
                "product": version.product(),
                "display_version": version.display_version,
                "build": version.build,
                "update_revision": version.update_revision,
            },
            "native_architecture": super::build::native_architecture(),
            "adapters": adapters(),
            "session_adapter_luid": session_adapter.map(|luid| format!("{luid:016x}")),
            "crash_dumps": wer,
            "native_tablet_built": true,
        }),
        crash_reports: crash_reports(),
    }
}

/// The folder dialog (`IFileOpenDialog` with `FOS_PICKFOLDERS`), modal
/// to no window; UI thread.
#[must_use]
pub fn choose_folder(start: Option<&Path>) -> Option<PathBuf> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
        SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    };
    use windows::core::HSTRING;
    // SAFETY: COM on this (UI) thread; balanced with CoUninitialize when
    // this call initialised it; every interface is owned and released on
    // drop; the display name is freed once with CoTaskMemFree.
    unsafe {
        let initialised = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let chosen = (|| -> Option<PathBuf> {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let options = dialog.GetOptions().ok()?;
            dialog
                .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
                .ok()?;
            let _ = dialog.SetTitle(&HSTRING::from("Choose where to save the support bundle"));
            if let Some(start) = start
                && let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                    &HSTRING::from(start.as_os_str()),
                    None,
                )
            {
                let _ = dialog.SetFolder(&item);
            }
            dialog.Show(None).ok()?;
            let item = dialog.GetResult().ok()?;
            let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = name.to_string().ok().map(PathBuf::from);
            CoTaskMemFree(Some(name.0.cast()));
            path
        })();
        if initialised.is_ok() {
            CoUninitialize();
        }
        chosen
    }
}

/// Explorer with `path` selected (`explorer.exe /select,"path"`).
#[must_use]
pub fn reveal(path: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    // Windows paths cannot contain `"`, so quoting the path is enough.
    if path.as_os_str().to_string_lossy().contains('"') {
        return false;
    }
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_versions_read_as_windows_prints_them() {
        let umd = (32_i64 << 48) | (15 << 16) | 0x17ce;
        assert_eq!(driver_version(umd), "32.0.15.6094");
    }

    /// What this Deck's bundle reports and omits, pinned: the shared
    /// writer's manifest of an empty `arcen-deck-windows` (the full-fixture golden is
    /// deck-core's). Re-bless after an intended change with
    /// `ARCEN_BLESS_SUPPORT_GOLDEN=1`.
    #[test]
    fn the_bundle_manifest_matches_the_golden_one() {
        const GOLDEN: &str = "tests/data/support-bundle/manifest.golden.json";
        let rendered = arcen_deck_core::support_bundle::reference_manifest(component())
            .expect("reference bundle");
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
        if std::env::var_os("ARCEN_BLESS_SUPPORT_GOLDEN").is_some() {
            std::fs::create_dir_all(path.parent().expect("folder")).expect("folder");
            std::fs::write(&path, &rendered).expect("bless golden");
        }
        let golden = std::fs::read_to_string(&path)
            .expect("golden")
            .replace("\r\n", "\n");
        assert_eq!(rendered, golden, "manifest differs from {GOLDEN}");
    }
}
