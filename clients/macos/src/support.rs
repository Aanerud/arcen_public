// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The macOS side of the support bundle: what only macOS can say (the
//! Metal GPU, the Mac model, this Deck's diagnostic reports) and the
//! Finder/pasteboard/open-panel actions. The bundle itself is shared
//! (`arcen_deck_core::support_bundle`).

use std::path::{Path, PathBuf};

use arcen_deck_core::support_bundle::{CrashReportKind, CrashReportSource, SupportDetails};

/// The executable name macOS uses for this Deck's diagnostic reports
/// (`CFBundleExecutable`).
const EXECUTABLE: &str = "arcen-deck";

/// The bundle's component.
#[must_use]
pub fn component() -> arcen_telemetry::BundleComponent {
    arcen_telemetry::BundleComponent {
        name: "arcen-deck-macos".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        os: "macos".to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
    }
}

/// The Metal GPU's name (`Apple M4`), when there is one.
#[must_use]
pub fn gpu_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        use objc2_metal::MTLDevice;
        objc2_metal::MTLCreateSystemDefaultDevice().map(|device| device.name().to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut size = 0_usize;
    // SAFETY: a NUL-terminated name and a null buffer: the kernel only
    // writes the needed size into `size`.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size == 0 || size > 1024 {
        return None;
    }
    let mut buffer = vec![0_u8; size];
    // SAFETY: `buffer` is `size` bytes long and `size` says so; nothing is
    // set (null `newp`, length 0).
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(size);
    let text = String::from_utf8_lossy(&buffer);
    Some(text.trim_end_matches('\0').trim().to_owned())
}

/// The platform facts and crash reports for the bundle.
#[must_use]
pub fn details() -> SupportDetails {
    let platform = crate::platform::current();
    #[cfg(target_os = "macos")]
    let (model, cpu) = (
        sysctl_string(c"hw.model"),
        sysctl_string(c"machdep.cpu.brand_string"),
    );
    #[cfg(not(target_os = "macos"))]
    let (model, cpu): (Option<String>, Option<String>) = (None, None);
    SupportDetails {
        platform: serde_json::json!({
            "summary": platform.to_string(),
            "label": platform.label(),
            "mac_model": model,
            "cpu": cpu,
            "gpu": gpu_name(),
            "decode_path": platform
                .assess_decode_path(crate::pipeline::video_decoder::probe_decode_capabilities()
                    .hardware_accelerated)
                .token(),
            "native_tablet_built": cfg!(feature = "native-tablet"),
        }),
        crash_reports: crash_reports(),
    }
}

/// This Deck's diagnostic reports in `~/Library/Logs/DiagnosticReports`
/// (and its `Retired` folder).
fn crash_reports() -> Vec<CrashReportSource> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let root = home.join("Library").join("Logs").join("DiagnosticReports");
    let mut found = Vec::new();
    for directory in [root.clone(), root.join("Retired")] {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&format!("{EXECUTABLE}-")) && name.ends_with(".ips") {
                found.push(CrashReportSource {
                    path: entry.path(),
                    kind: CrashReportKind::MacDiagnosticReport,
                });
            }
        }
    }
    found
}

/// `~/Desktop`.
#[must_use]
pub fn desktop() -> Option<PathBuf> {
    let desktop = PathBuf::from(std::env::var_os("HOME")?).join("Desktop");
    desktop.is_dir().then_some(desktop)
}

/// An open panel that chooses one folder (main thread only).
#[must_use]
pub fn choose_folder(start: Option<&Path>) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};
        use objc2_foundation::{NSString, NSURL};
        let mtm = objc2::MainThreadMarker::new()?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setCanCreateDirectories(true);
        panel.setPrompt(Some(&NSString::from_str("Choose")));
        panel.setMessage(Some(&NSString::from_str(
            "Choose where to save the support bundle.",
        )));
        if let Some(start) = start {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&start.display().to_string()));
            panel.setDirectoryURL(Some(&url));
        }
        if panel.runModal() != NSModalResponseOK {
            return None;
        }
        panel
            .URL()
            .and_then(|url| url.path())
            .map(|path| PathBuf::from(path.to_string()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = start;
        None
    }
}

/// A save panel for a connection profile (`.arcen`), proposing
/// `file_name` on the Desktop (main thread only). The panel confirms
/// replacing an existing file.
#[must_use]
pub fn choose_profile_to_save(file_name: &str) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSModalResponseOK, NSSavePanel};
        use objc2_foundation::NSString;
        let mtm = objc2::MainThreadMarker::new()?;
        let panel = NSSavePanel::savePanel(mtm);
        panel.setCanCreateDirectories(true);
        panel.setExtensionHidden(false);
        panel.setNameFieldStringValue(&NSString::from_str(file_name));
        panel.setMessage(Some(&NSString::from_str(
            "The file holds the connection's address, username and settings. It never holds \
             a password.",
        )));
        if let Some(desktop) = desktop() {
            panel.setDirectoryURL(Some(&file_url(&desktop)));
        }
        if panel.runModal() != NSModalResponseOK {
            return None;
        }
        panel
            .URL()
            .and_then(|url| url.path())
            .map(|path| PathBuf::from(path.to_string()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = file_name;
        None
    }
}

/// An open panel that chooses one connection profile (main thread only).
/// Any file may be chosen; the shared reader validates it.
#[must_use]
pub fn choose_profile_to_open() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};
        use objc2_foundation::NSString;
        let mtm = objc2::MainThreadMarker::new()?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(false);
        panel.setCanChooseFiles(true);
        panel.setAllowsMultipleSelection(false);
        panel.setPrompt(Some(&NSString::from_str("Import")));
        panel.setMessage(Some(&NSString::from_str(
            "Choose an Arcen connection file (.arcen).",
        )));
        if panel.runModal() != NSModalResponseOK {
            return None;
        }
        panel
            .URL()
            .and_then(|url| url.path())
            .map(|path| PathBuf::from(path.to_string()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn file_url(path: &Path) -> objc2::rc::Retained<objc2_foundation::NSURL> {
    objc2_foundation::NSURL::fileURLWithPath(&objc2_foundation::NSString::from_str(
        &path.display().to_string(),
    ))
}

/// Shows `path` selected in Finder (`open -R`).
#[must_use]
pub fn reveal(path: &Path) -> bool {
    std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .spawn()
        .is_ok()
}

/// Puts `text` on the pasteboard as plain text (`pbcopy`).
#[must_use]
pub fn copy_text(text: &str) -> bool {
    use std::io::Write;
    let Ok(mut child) = std::process::Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    else {
        return false;
    };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|status| status.success()) && written
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// What this Deck's bundle reports and omits, pinned: the shared
    /// writer's manifest of an empty `arcen-deck-macos` (the full-fixture golden is
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
