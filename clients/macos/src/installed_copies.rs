// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Other copies of this Deck that share its bundle identifier: Launch
//! Services' registered copies and the running applications, handed to the
//! shared decision (`arcen_deck_core::installed_copies`).

/// The warning to show at start when another copy of this Deck may run in
/// its place; each copy is also logged. `None` for a binary run outside an
/// app bundle.
#[cfg(target_os = "macos")]
#[must_use]
pub fn startup_notice() -> Option<String> {
    use arcen_deck_core::installed_copies::{stale_copies, startup_notice, CopyFinding, ThisCopy};
    use objc2_foundation::NSBundle;

    let bundle = NSBundle::mainBundle();
    let identifier = bundle.bundleIdentifier()?.to_string();
    let this_path = home_relative(bundle.bundlePath().to_string());
    if !this_path.ends_with(".app") {
        return None;
    }
    let this_version = info(&bundle, "CFBundleShortVersionString").unwrap_or_default();
    let this_build = info(&bundle, "CFBundleVersion").unwrap_or_default();
    let findings = stale_copies(
        &ThisCopy {
            path: &this_path,
            version: &this_version,
            build: &this_build,
        },
        &copies_of(&identifier),
    );
    for finding in &findings {
        let (running, version) = match finding {
            CopyFinding::RunningElsewhere { version, .. } => (true, version),
            CopyFinding::OtherVersion { version, .. } => (false, version),
        };
        tracing::warn!(
            target: crate::logging::target::UI,
            copy = %finding.path(),
            copy_version = version.as_deref().unwrap_or("unknown"),
            running,
            this_version = %this_version,
            "another Arcen Deck shares this bundle identifier"
        );
    }
    startup_notice(&this_version, &findings)
}

/// Every copy Launch Services has registered for `identifier`, and every
/// running application of it (a copy may be both).
#[cfg(target_os = "macos")]
fn copies_of(identifier: &str) -> Vec<arcen_deck_core::installed_copies::InstalledCopy> {
    use arcen_deck_core::installed_copies::InstalledCopy;
    use objc2_app_kit::{NSRunningApplication, NSWorkspace};
    use objc2_foundation::{NSBundle, NSString, NSURL};

    let identifier = NSString::from_str(identifier);
    let describe = |url: &NSURL, running: bool| {
        let path = url.path()?.to_string();
        // Launch Services may still list a copy that was deleted.
        if !std::path::Path::new(&path).is_dir() {
            return None;
        }
        let path = home_relative(path);
        let other = NSBundle::bundleWithURL(url);
        Some(InstalledCopy {
            version: other
                .as_deref()
                .and_then(|other| info(other, "CFBundleShortVersionString")),
            build: other
                .as_deref()
                .and_then(|other| info(other, "CFBundleVersion")),
            path,
            running,
        })
    };
    let mut copies = Vec::new();
    for application in NSRunningApplication::runningApplicationsWithBundleIdentifier(&identifier) {
        if let Some(url) = application.bundleURL() {
            copies.extend(describe(&url, true));
        }
    }
    for url in NSWorkspace::sharedWorkspace().URLsForApplicationsWithBundleIdentifier(&identifier) {
        copies.extend(describe(&url, false));
    }
    copies
}

#[cfg(target_os = "macos")]
fn info(bundle: &objc2_foundation::NSBundle, key: &str) -> Option<String> {
    bundle
        .objectForInfoDictionaryKey(&objc2_foundation::NSString::from_str(key))
        .and_then(|value| value.downcast::<objc2_foundation::NSString>().ok())
        .map(|value| value.to_string())
}

/// A path as the user reads it, and as a log may carry it: the home
/// directory is `~`.
fn home_relative(path: String) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && rest.starts_with('/') => format!("~{rest}"),
        _ => path,
    }
}

/// No bundle identifier outside macOS.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn startup_notice() -> Option<String> {
    let _ = home_relative;
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use arcen_deck_core::installed_copies::{stale_copies, startup_notice, ThisCopy};

    /// Lab evidence on a Mac with Decks installed: what this Mac's Launch
    /// Services reports, and what a 0.20.2 Deck in /Applications would say.
    /// `cargo test -p arcen-deck-macos installed_copies -- --ignored --nocapture`
    #[test]
    #[ignore = "reads this Mac's Launch Services"]
    fn this_macs_registered_decks() {
        let copies = super::copies_of("deck.arcen.tech");
        for copy in &copies {
            println!("copy {copy:?}");
        }
        let this = ThisCopy {
            path: "/Applications/Arcen Deck.app",
            version: "0.20.2",
            build: "0",
        };
        let findings = stale_copies(&this, &copies);
        println!("findings {findings:?}");
        println!("notice {:?}", startup_notice("0.20.2", &findings));
    }
}
