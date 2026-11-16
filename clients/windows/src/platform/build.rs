// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Which Deck is running, and on which Windows.

use std::path::Path;

use arcen_protocol::messages::BuildIdentityMsg;

/// The product name every Windows Deck build reports.
pub const PRODUCT: &str = "arcen-deck-windows";

/// This build's identity for the hello and every log.
#[must_use]
pub fn current() -> BuildIdentityMsg {
    arcen_protocol::build_identity::this_build(PRODUCT, crate::VERSION)
}

/// What `arcen-deck.exe --version` prints: the version, the build stamp the
/// release build set, and the AGPL source offer.
#[must_use]
pub fn version_text(identity: &BuildIdentityMsg) -> String {
    format!(
        "{PRODUCT} {version}\nbuild_id={build_id} revision={revision} profile={profile} signing={signing}\n{offer}\nSource: {url}\n",
        version = identity.version,
        build_id = identity.build_id,
        revision = identity.source_revision,
        profile = identity.feature_profile,
        signing = identity.signing_state.as_deref().unwrap_or("unsigned"),
        offer = arcen_deck_core::about::SOURCE_OFFER,
        url = arcen_deck_core::about::SOURCE_URL,
    )
}

/// Which Deck is running: the version, the build stamp and where the
/// executable lives (the macOS Deck's `DeckBuildFacts`, with the executable
/// in place of the bundle). Logged at start so a log names its build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckBuildFacts {
    /// `CARGO_PKG_VERSION`.
    pub version: String,
    /// The release build's stamp, or `development`.
    pub build_id: String,
    /// The source revision, or `unknown`.
    pub source_revision: String,
    /// The executable, with the profile folder written as `~`.
    pub executable_path: String,
}

impl DeckBuildFacts {
    /// The facts of the running process.
    #[must_use]
    pub fn running() -> Self {
        let executable = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        Self::from_parts(
            &current(),
            &executable,
            std::env::var_os("USERPROFILE").as_deref().map(Path::new),
        )
    }

    /// Assembles the facts; the profile folder is written as `~` so a log
    /// does not carry the account name.
    #[must_use]
    pub fn from_parts(
        identity: &BuildIdentityMsg,
        executable: &str,
        profile: Option<&Path>,
    ) -> Self {
        let executable_path =
            match profile.and_then(|profile| Path::new(executable).strip_prefix(profile).ok()) {
                Some(relative) if !relative.as_os_str().is_empty() => {
                    format!("~\\{}", relative.display())
                }
                _ => executable.to_owned(),
            };
        Self {
            version: identity.version.clone(),
            build_id: identity.build_id.clone(),
            source_revision: identity.source_revision.clone(),
            executable_path,
        }
    }

    /// One line for a log message or a report.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} build {} rev {} at {}",
            self.version, self.build_id, self.source_revision, self.executable_path
        )
    }
}

/// The running Windows, as `CurrentVersion` in the registry states it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsVersion {
    /// `CurrentBuildNumber`, for example 26100.
    pub build: u32,
    /// `UBR`, the update revision.
    pub update_revision: Option<u32>,
    /// `DisplayVersion`, for example `24H2`.
    pub display_version: Option<String>,
}

impl WindowsVersion {
    /// Marketing name: build 22000 and later is Windows 11 (the registry's
    /// `ProductName` still says Windows 10 there).
    #[must_use]
    pub const fn product(&self) -> &'static str {
        if self.build >= 22_000 {
            "Windows 11"
        } else {
            "Windows 10"
        }
    }
}

/// The architecture token of a build target (`x64`, `arm64`).
#[must_use]
pub fn architecture_token(target_arch: &str) -> &str {
    match target_arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        other => other,
    }
}

/// The `DeckServices::platform_summary` line, for example
/// `Windows 11 24H2 26100.2894 x64` or `… x64 (emulated on arm64)`.
#[must_use]
pub fn platform_summary(
    version: &WindowsVersion,
    compiled_arch: &str,
    native_arch: Option<&str>,
) -> String {
    let mut line = version.product().to_owned();
    if let Some(display) = version.display_version.as_deref().filter(|v| !v.is_empty()) {
        line.push(' ');
        line.push_str(display);
    }
    if version.build != 0 {
        line.push(' ');
        line.push_str(&version.build.to_string());
        if let Some(revision) = version.update_revision {
            line.push('.');
            line.push_str(&revision.to_string());
        }
    }
    line.push(' ');
    line.push_str(compiled_arch);
    if let Some(native) = native_arch.filter(|native| *native != compiled_arch) {
        line.push_str(" (emulated on ");
        line.push_str(native);
        line.push(')');
    }
    line
}

/// Reads the running Windows version.
#[must_use]
pub fn running_windows() -> WindowsVersion {
    use super::registry::{Hive, read_dword, read_string};
    const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    WindowsVersion {
        build: read_string(Hive::LocalMachine, KEY, "CurrentBuildNumber")
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0),
        update_revision: read_dword(Hive::LocalMachine, KEY, "UBR"),
        display_version: read_string(Hive::LocalMachine, KEY, "DisplayVersion"),
    }
}

/// The machine's own architecture when this process runs emulated
/// (`IsWow64Process2`); `None` when native or unknown.
#[must_use]
pub fn native_architecture() -> Option<&'static str> {
    use windows::Win32::System::SystemInformation::{
        IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};

    let mut process = IMAGE_FILE_MACHINE::default();
    let mut native = IMAGE_FILE_MACHINE::default();
    // SAFETY: the pseudo-handle of this process and two live out-parameters.
    unsafe { IsWow64Process2(GetCurrentProcess(), &raw mut process, Some(&raw mut native)) }
        .ok()?;
    match native {
        IMAGE_FILE_MACHINE_ARM64 => Some("arm64"),
        IMAGE_FILE_MACHINE_AMD64 => Some("x64"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn identity(build_id: Option<&str>, signing: Option<&str>) -> BuildIdentityMsg {
        arcen_protocol::build_identity::from_environment(
            PRODUCT,
            "0.20.0",
            arcen_protocol::build_identity::BuildEnvironment {
                build_id,
                source_revision: build_id.map(|_| "0a1b2c3d"),
                signing_state: signing,
                ..Default::default()
            },
            None,
        )
    }

    #[test]
    fn version_text_names_the_build_stamp_and_the_source_offer() {
        let text = version_text(&identity(
            Some("20261114T1000Z-0.20.0-0a1b2c3d"),
            Some("authenticode"),
        ));
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("arcen-deck-windows 0.20.0"));
        assert_eq!(
            lines.next(),
            Some(
                "build_id=20261114T1000Z-0.20.0-0a1b2c3d revision=0a1b2c3d profile=quic-default signing=authenticode"
            )
        );
        assert_eq!(lines.next(), Some(arcen_deck_core::about::SOURCE_OFFER));
        assert!(text.contains(arcen_deck_core::about::SOURCE_URL));

        let development = version_text(&identity(None, None));
        assert!(development.contains("build_id=development revision=unknown"));
        assert!(development.contains("signing=unsigned"));
    }

    #[test]
    fn build_facts_hide_the_account_name() {
        let facts = DeckBuildFacts::from_parts(
            &identity(Some("b"), None),
            r"C:\Users\someone\Arcen\arcen-deck.exe",
            Some(Path::new(r"C:\Users\someone")),
        );
        assert_eq!(facts.executable_path, r"~\Arcen\arcen-deck.exe");
        assert_eq!(facts.build_id, "b");
        let installed = DeckBuildFacts::from_parts(
            &identity(None, None),
            r"C:\Program Files\Arcen\arcen-deck.exe",
            Some(Path::new(r"C:\Users\someone")),
        );
        assert_eq!(
            installed.summary(),
            r"0.20.0 build development rev unknown at C:\Program Files\Arcen\arcen-deck.exe"
        );
    }

    #[test]
    fn the_platform_summary_names_windows_11_from_the_build() {
        let version = WindowsVersion {
            build: 26_100,
            update_revision: Some(2894),
            display_version: Some("24H2".to_owned()),
        };
        assert_eq!(
            platform_summary(&version, "x64", None),
            "Windows 11 24H2 26100.2894 x64"
        );
        assert_eq!(
            platform_summary(&version, "x64", Some("arm64")),
            "Windows 11 24H2 26100.2894 x64 (emulated on arm64)"
        );
        assert_eq!(
            platform_summary(&version, "x64", Some("x64")),
            "Windows 11 24H2 26100.2894 x64"
        );
        let old = WindowsVersion {
            build: 19_045,
            ..WindowsVersion::default()
        };
        assert_eq!(platform_summary(&old, "x64", None), "Windows 10 19045 x64");
        assert_eq!(architecture_token("x86_64"), "x64");
        assert_eq!(architecture_token("aarch64"), "arm64");
    }
}
