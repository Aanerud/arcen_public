// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Registration of the privileged USB helper as a launchd daemon.
//!
//! This is what removes the recurring `sudo`. The *helper* runs as root, not
//! the app (ADR 0011) — but the user
//! should never type a password per session. `SMAppService.daemon` installs the
//! daemon once, launchd starts it as root from then on, and the only user-facing
//! step is a single approval in System Settings.
//!
//! `SMAppService` exists only on macOS 13 and later. Deck itself runs on
//! macOS 12, so every entry point here asks the Deck platform profile
//! ([`crate::platform`]) first and reports [`HelperInstallState::UnsupportedOs`]
//! instead of touching the API.
//!
//! See `docs/adr/0011-macos-privileged-usb-helper.md`.

use arcen_usb_bridge::{DeckPlatform, NativeComponentAction, NativeUsbComponentState};
use objc2_foundation::NSString;
use objc2_service_management::{SMAppService, SMAppServiceStatus};

fn helper_supported() -> bool {
    crate::platform::current().native_usb_helper_supported()
}

/// Filename of the bundled LaunchDaemon plist, relative to
/// `Contents/Library/LaunchDaemons/`.
pub const DAEMON_PLIST: &str = "tech.arcen.deck.usbhelper.plist";

/// What the daemon registration currently is, from Deck's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperInstallState {
    /// launchd has the daemon and the user approved it. Nothing to do.
    Enabled,
    /// Registered, but the user has not approved it in System Settings yet.
    RequiresApproval,
    /// Never registered, or explicitly removed.
    NotRegistered,
    /// BackgroundTaskManagement holds no record for this daemon.
    ///
    /// Despite the name this does **not** mean the plist is missing. macOS
    /// reports status 3 here when BTM answers `record not found`, which is the
    /// ordinary state of a daemon that has simply never been registered — the
    /// system log shows `Setting up BundleProgram keys` succeeding immediately
    /// before it. Treat it like [`Self::NotRegistered`] and register.
    NotFound,
    /// This macOS predates `SMAppService`, so the helper cannot be installed.
    UnsupportedOs,
    /// A status this build does not know about.
    Unknown(isize),
}

impl HelperInstallState {
    /// The shared component state this registration status means. Every word
    /// the Deck shows about the helper comes from there, so the macOS and
    /// Windows Decks say the same thing for the same state.
    #[must_use]
    pub const fn component_state(self) -> NativeUsbComponentState {
        match self {
            Self::Enabled => NativeUsbComponentState::Ready,
            Self::RequiresApproval => NativeUsbComponentState::AwaitingApproval,
            Self::NotRegistered | Self::NotFound => NativeUsbComponentState::NotInstalled,
            Self::UnsupportedOs => NativeUsbComponentState::UnsupportedOs,
            Self::Unknown(_) => NativeUsbComponentState::Unknown,
        }
    }

    /// Human-readable guidance, so the UI never has to invent wording for a
    /// state it did not expect.
    #[must_use]
    pub fn guidance(self) -> String {
        self.component_state().guidance(DeckPlatform::MacOs)
    }

    /// Whether Hard USB can be attempted without further user action.
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Enabled)
    }

    /// Label for the status control that sits beside the Native tablet option.
    ///
    /// Short enough to live on the option's own row: the row states where the
    /// helper stands, and the description below explains why it is needed.
    #[must_use]
    pub fn action_label(self) -> &'static str {
        self.component_state().action_label(DeckPlatform::MacOs)
    }

    /// What clicking that control must do.
    ///
    /// `NotFound` installs rather than reporting a missing file: macOS returns
    /// it for a daemon that has simply never been registered, so the only
    /// useful response is the same one `NotRegistered` gets.
    #[must_use]
    pub fn action(self) -> HelperAction {
        match self.component_state().action() {
            NativeComponentAction::Install => HelperAction::Install,
            NativeComponentAction::Approve => HelperAction::Approve,
            // A launchd daemon starts itself once approved, and macOS needs no
            // per-tablet preparation, so neither maps to a different step.
            NativeComponentAction::Start
            | NativeComponentAction::Prepare
            | NativeComponentAction::Recheck => HelperAction::Recheck,
        }
    }
}

/// What the helper status control does when clicked, for the state it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperAction {
    /// Register the daemon with launchd.
    Install,
    /// Send the user to Login Items, where a pending daemon is approved.
    Approve,
    /// Nothing to change; read the status again.
    Recheck,
}

// `SMAppServiceStatus` is a newtype over NSInteger with associated constants,
// not a Rust enum, so this compares rather than matches.
fn map_status(status: SMAppServiceStatus) -> HelperInstallState {
    if status == SMAppServiceStatus::Enabled {
        HelperInstallState::Enabled
    } else if status == SMAppServiceStatus::RequiresApproval {
        HelperInstallState::RequiresApproval
    } else if status == SMAppServiceStatus::NotRegistered {
        HelperInstallState::NotRegistered
    } else if status == SMAppServiceStatus::NotFound {
        HelperInstallState::NotFound
    } else {
        HelperInstallState::Unknown(status.0)
    }
}

fn daemon() -> objc2::rc::Retained<SMAppService> {
    let plist = NSString::from_str(DAEMON_PLIST);
    // SAFETY: `daemonServiceWithPlistName:` takes an NSString and returns a retained
    // SMAppService; both are ordinary Objective-C object lifetimes managed by
    // objc2's `Retained`.
    unsafe { SMAppService::daemonServiceWithPlistName(&plist) }
}

/// Where this process thinks its main bundle is, and whether the daemon plist
/// is actually there. `NotFound` is otherwise a dead end to debug: it does not
/// distinguish "wrong bundle" from "missing plist" from "unsigned".
#[must_use]
pub fn diagnostics() -> (String, String, bool) {
    use objc2_foundation::NSBundle;
    let bundle = NSBundle::mainBundle();
    let bundle_path = bundle.bundlePath().to_string();
    let plist_path = format!("{bundle_path}/Contents/Library/LaunchDaemons/{DAEMON_PLIST}");
    let exists = std::path::Path::new(&plist_path).is_file();
    (bundle_path, plist_path, exists)
}

/// Reads the current registration state without changing anything.
#[must_use]
pub fn install_state() -> HelperInstallState {
    if !helper_supported() {
        return HelperInstallState::UnsupportedOs;
    }
    // SAFETY: `status` is a property read on a live SMAppService instance.
    map_status(unsafe { daemon().status() })
}

/// Registers the daemon with launchd.
///
/// The first call raises one administrator authentication prompt. After that,
/// launchd starts the helper as root automatically and the user is never asked
/// again, which is the whole point of this over `sudo`.
///
/// # Errors
///
/// Returns the localized failure description when registration is refused, for
/// example because the user cancelled the authentication prompt.
pub fn register() -> Result<HelperInstallState, String> {
    if !helper_supported() {
        return Ok(HelperInstallState::UnsupportedOs);
    }
    let service = daemon();
    // SAFETY: `registerAndReturnError:` is the documented registration entry
    // point; objc2 surfaces the NSError out-param as a Result.
    let result = unsafe { service.registerAndReturnError() };
    // SAFETY: see `install_state`.
    let state = map_status(unsafe { service.status() });
    match result {
        Ok(()) => Ok(state),
        // "Operation not permitted" on first registration is the documented
        // normal path, not a failure: the daemon is recorded and then waits for
        // the user in Login Items. Trust the resulting status over the error.
        Err(_)
            if matches!(
                state,
                HelperInstallState::RequiresApproval | HelperInstallState::Enabled
            ) =>
        {
            Ok(state)
        }
        Err(error) => Err(format!(
            "could not install the Arcen USB helper: {}",
            error.localizedDescription()
        )),
    }
}

/// Removes the daemon registration.
///
/// # Errors
///
/// Returns the localized failure description when launchd refuses removal.
pub fn unregister() -> Result<(), String> {
    if !helper_supported() {
        return Ok(());
    }
    let service = daemon();
    // SAFETY: mirror of `register`, with the same object lifetime rules.
    unsafe { service.unregisterAndReturnError() }.map_err(|error| {
        format!(
            "could not remove the Arcen USB helper: {}",
            error.localizedDescription()
        )
    })
}

/// Opens the Login Items pane so the user can approve a pending daemon.
pub fn open_login_items_settings() {
    if !helper_supported() {
        return;
    }
    // SAFETY: a class method with no arguments that only presents system UI.
    unsafe { SMAppService::openSystemSettingsLoginItems() };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_has_actionable_guidance() {
        for state in [
            HelperInstallState::Enabled,
            HelperInstallState::RequiresApproval,
            HelperInstallState::NotRegistered,
            HelperInstallState::NotFound,
            HelperInstallState::UnsupportedOs,
            HelperInstallState::Unknown(42),
        ] {
            assert!(
                !state.guidance().is_empty(),
                "state {state:?} must explain itself to the user"
            );
        }
    }

    #[test]
    fn only_enabled_is_ready() {
        assert!(HelperInstallState::Enabled.is_ready());
        for state in [
            HelperInstallState::RequiresApproval,
            HelperInstallState::NotRegistered,
            HelperInstallState::NotFound,
            HelperInstallState::UnsupportedOs,
            HelperInstallState::Unknown(0),
        ] {
            assert!(
                !state.is_ready(),
                "{state:?} must not be treated as ready for Hard USB"
            );
        }
    }

    #[test]
    fn the_deck_shows_the_shared_component_wording() {
        for state in [
            HelperInstallState::Enabled,
            HelperInstallState::RequiresApproval,
            HelperInstallState::NotRegistered,
            HelperInstallState::NotFound,
            HelperInstallState::UnsupportedOs,
            HelperInstallState::Unknown(9),
        ] {
            let shared = state.component_state();
            assert_eq!(state.guidance(), shared.guidance(DeckPlatform::MacOs));
            assert_eq!(
                state.action_label(),
                shared.action_label(DeckPlatform::MacOs)
            );
            assert_eq!(state.is_ready(), shared.is_ready(), "{state:?}");
        }
    }

    #[test]
    fn requires_approval_points_at_login_items() {
        assert!(HelperInstallState::RequiresApproval
            .guidance()
            .contains("Login Items"));
    }

    #[test]
    fn not_found_is_treated_exactly_like_not_registered() {
        // macOS reports a never-registered daemon as NotFound, because
        // BackgroundTaskManagement answers "record not found" — verified in the
        // system log, where `Setting up BundleProgram keys` succeeds moments
        // before status 3 is returned. So NotFound must not be described as a
        // missing helper, and must lead to the same action: register it.
        assert_eq!(
            HelperInstallState::NotFound.guidance(),
            HelperInstallState::NotRegistered.guidance()
        );
        assert!(!HelperInstallState::NotFound.is_ready());
        assert_eq!(
            HelperInstallState::NotFound.action(),
            HelperAction::Install,
            "a never-registered daemon must offer installation, not a dead end"
        );
        assert_eq!(
            HelperInstallState::NotFound.action_label(),
            HelperInstallState::NotRegistered.action_label()
        );
    }

    #[test]
    fn every_state_offers_a_labelled_control() {
        for state in [
            HelperInstallState::Enabled,
            HelperInstallState::RequiresApproval,
            HelperInstallState::NotRegistered,
            HelperInstallState::NotFound,
            HelperInstallState::UnsupportedOs,
            HelperInstallState::Unknown(7),
        ] {
            assert!(
                !state.action_label().is_empty(),
                "state {state:?} must give its control a label"
            );
        }
    }

    /// The control must never offer an install that cannot help. A pending
    /// daemon is already registered — registering again does not clear the
    /// approval, so only Login Items can.
    #[test]
    fn requires_approval_sends_the_user_to_login_items() {
        assert_eq!(
            HelperInstallState::RequiresApproval.action(),
            HelperAction::Approve
        );
    }

    /// An installed helper still needs a way to be re-read: approval can be
    /// revoked in System Settings while Deck is running.
    #[test]
    fn enabled_still_offers_a_recheck() {
        assert_eq!(HelperInstallState::Enabled.action(), HelperAction::Recheck);
    }

    /// Only states that are genuinely not registered may offer installation,
    /// so the control cannot invite a redundant admin prompt.
    #[test]
    fn install_is_offered_only_when_registration_is_missing() {
        for state in [
            HelperInstallState::Enabled,
            HelperInstallState::RequiresApproval,
            HelperInstallState::UnsupportedOs,
            HelperInstallState::Unknown(0),
        ] {
            assert_ne!(
                state.action(),
                HelperAction::Install,
                "{state:?} is already registered; installing again cannot help"
            );
        }
    }

    /// On macOS 12 nothing can be installed, so the control must neither offer
    /// an install nor send the user to a Login Items pane that cannot help.
    #[test]
    fn unsupported_os_explains_itself_and_offers_no_install() {
        let state = HelperInstallState::UnsupportedOs;
        assert!(!state.is_ready());
        assert_eq!(state.action(), HelperAction::Recheck);
        assert!(state.guidance().contains("macOS 13"));
        assert!(state.action_label().contains("macOS 13"));
    }
}
