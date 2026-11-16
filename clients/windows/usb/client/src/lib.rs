// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows Deck's side of the optional Arcen USB component (ADR 0024).
//!
//! The Deck stays unprivileged. It only:
//!
//! - **reports where the component stands** ([`component_state`]): the
//!   operating system, the `ArcenUsbComponent` service, and whether the
//!   attached tablet's model is prepared, resolved by the shared
//!   [`arcen_usb_bridge::NativeComponentFacts::resolve`] so both Decks rank
//!   problems and word them the same way;
//! - **runs the control's action** ([`run_action`]): open the installer,
//!   start the service, or run `arcen-usb-prepare.exe` with one
//!   administrator approval;
//! - **opens the verified pipe** ([`start_responder`]): the pipe must be
//!   served by the running `ArcenUsbComponent` service process, and the
//!   Deck opens it at identification level only, so the service can never
//!   act as the user. Everything after the open is the shared component
//!   client (`arcen_deck_core::runtime::usb_component`), the same code the
//!   macOS Deck uses with its helper.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

#[cfg(windows)]
mod win;

use arcen_deck_core::runtime::usb::UsbTabletResponder;
use arcen_usb_bridge::NativeComponentFacts;
use arcen_usb_bridge::winusb_package::{TabletPreparation, is_device_instance_id};

pub use arcen_usb_bridge::{DeckPlatform, NativeComponentAction, NativeUsbComponentState};

/// The platform whose wording the Windows Deck shows.
pub const PLATFORM: DeckPlatform = DeckPlatform::Windows;

/// How messages name the other end of the pipe.
pub const PEER: &str = "Arcen USB component";

/// First Windows 11 build.
pub const WINDOWS_11_FIRST_BUILD: u32 = 22_000;

/// Win32 exit codes that mean a system policy kept the service from
/// running: `ERROR_ACCESS_DISABLED_BY_POLICY` (1260) and
/// `ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION` (4551, App Control and Smart
/// App Control).
pub const POLICY_BLOCK_CODES: [u32; 2] = [1260, 4551];

/// What the service control manager reports about `ArcenUsbComponent`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServiceFacts {
    /// The service is registered.
    pub installed: bool,
    /// It is running.
    pub running: bool,
    /// An administrator set it to Disabled.
    pub disabled: bool,
    /// Its last Win32 exit code (0 when none).
    pub exit_code: u32,
}

/// The shared facts, from what the Windows adapter observed.
#[must_use]
pub fn facts_from(
    os_supported: bool,
    service: ServiceFacts,
    tablet: &TabletPreparation,
) -> NativeComponentFacts {
    NativeComponentFacts {
        unsupported_os: !os_supported,
        blocked_by_policy: service.installed
            && !service.running
            && POLICY_BLOCK_CODES.contains(&service.exit_code),
        installed: service.installed,
        awaiting_approval: false,
        running: service.running,
        refusal: None,
        device_prepared: tablet.device_prepared(),
    }
}

/// Whether this Windows can run the component: Windows 11 on x64.
#[must_use]
pub const fn os_supported(build: Option<u32>, x64: bool) -> bool {
    match build {
        Some(build) => x64 && build >= WINDOWS_11_FIRST_BUILD,
        None => false,
    }
}

/// The command line `arcen-usb-prepare.exe` gets for one tablet instance.
///
/// # Errors
///
/// The instance ID is not one a command line can carry safely.
pub fn prepare_parameters(instance_id: &str) -> Result<String, String> {
    if !is_device_instance_id(instance_id) || instance_id.contains('"') {
        return Err(format!(
            "the tablet's device instance ID cannot be passed on: {instance_id:?}"
        ));
    }
    Ok(format!("--instance \"{instance_id}\""))
}

/// Whether the process serving the component pipe may be trusted: it must
/// be the running service's own process. `accept_other` is the lab build's
/// console-mode service, which the service control manager did not start.
///
/// # Errors
///
/// The refusal, naming both processes.
pub fn check_pipe_server(
    server: u32,
    service: Option<u32>,
    accept_other: bool,
) -> Result<(), String> {
    match service {
        Some(service) if service == server => Ok(()),
        _ if accept_other => Ok(()),
        Some(service) => Err(format!(
            "refusing the {PEER} pipe: process {server} serves it, not the service (process \
             {service}); another program may be impersonating the component"
        )),
        None => Err(format!(
            "refusing the {PEER} pipe: process {server} serves it while the service is not \
             running; another program may be impersonating the component"
        )),
    }
}

/// The executable a service's command line names: the quoted path, or the
/// text up to and including the first `.exe`.
#[must_use]
pub fn binary_from_command_line(command_line: &str) -> Option<std::path::PathBuf> {
    let line = command_line.trim();
    let path = if let Some(quoted) = line.strip_prefix('"') {
        quoted.split_once('"')?.0
    } else {
        let end = line.to_ascii_lowercase().find(".exe")? + ".exe".len();
        &line[..end]
    };
    (!path.is_empty()).then(|| std::path::PathBuf::from(path))
}

/// The message for an action that cannot change anything.
pub(crate) fn nothing_to_do(action: NativeComponentAction) -> Result<(), String> {
    match action {
        NativeComponentAction::Approve | NativeComponentAction::Recheck => Ok(()),
        other => Err(format!("{other:?} is not available on this system")),
    }
}

/// Every fact the Deck can observe about the component, now. Costs a few
/// service control manager calls and a scan of HID devices: read it once per
/// settings visit, not per frame.
#[must_use]
pub fn component_facts() -> NativeComponentFacts {
    #[cfg(windows)]
    {
        win::facts()
    }
    #[cfg(not(windows))]
    {
        facts_from(false, ServiceFacts::default(), &TabletPreparation::NoTablet)
    }
}

/// Where the component stands ([`component_facts`], resolved).
#[must_use]
pub fn component_state() -> NativeUsbComponentState {
    component_facts().resolve()
}

/// Carries out the settings control's action. `Install` opens the component
/// installer (beside the Deck, or the release page); `Start` starts the
/// service (asking for administrator approval when the user may not);
/// `Prepare` runs `arcen-usb-prepare.exe` for the attached tablet with one
/// administrator approval. Blocks until the elevated step ends (at most two
/// minutes): call it off the UI thread.
///
/// # Errors
///
/// A one-line reason to show under the option.
pub fn run_action(action: NativeComponentAction) -> Result<(), String> {
    #[cfg(windows)]
    {
        win::run_action(action)
    }
    #[cfg(not(windows))]
    {
        nothing_to_do(action)
    }
}

/// Connects to the component, which captures the prepared tablet for this
/// session; the tablet goes back to Windows when the responder shuts down or
/// is dropped.
///
/// # Errors
///
/// Why no tablet could be captured, in the Deck's words.
#[cfg_attr(not(windows), allow(clippy::unused_async))]
pub async fn start_responder() -> Result<Box<dyn UsbTabletResponder>, String> {
    #[cfg(windows)]
    {
        let client = win::pipe::connect().await?;
        let device = client.device();
        tracing::info!(
            target: arcen_deck_core::log_target::USB,
            vendor_id = device.vendor_id,
            product_id = device.product_id,
            backend = client.backend(),
            "Arcen USB component captured the tablet for Native tablet"
        );
        Ok(Box::new(client))
    }
    #[cfg(not(windows))]
    {
        Err("the Arcen USB component exists on Windows only".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTANCE: &str = "USB\\VID_056A&PID_0317\\8&2A1B3C4D&0&1";

    fn running() -> ServiceFacts {
        ServiceFacts {
            installed: true,
            running: true,
            ..ServiceFacts::default()
        }
    }

    #[test]
    fn windows_11_on_x64_is_required() {
        assert!(os_supported(Some(26_200), true));
        assert!(os_supported(Some(WINDOWS_11_FIRST_BUILD), true));
        assert!(!os_supported(Some(19_045), true), "Windows 10");
        assert!(!os_supported(Some(26_200), false), "ARM64");
        assert!(!os_supported(None, true), "unknown build");
    }

    #[test]
    fn the_facts_resolve_to_the_shared_states() {
        let prepared = TabletPreparation::Prepared {
            instance_id: INSTANCE.to_owned(),
        };
        let unprepared = TabletPreparation::NeedsPreparing {
            instance_id: INSTANCE.to_owned(),
        };
        let none = TabletPreparation::NoTablet;
        let state =
            |os, service, tablet: &TabletPreparation| facts_from(os, service, tablet).resolve();
        assert_eq!(
            state(true, running(), &prepared),
            NativeUsbComponentState::Ready
        );
        assert_eq!(
            state(true, running(), &none),
            NativeUsbComponentState::Ready
        );
        assert_eq!(
            state(true, running(), &unprepared),
            NativeUsbComponentState::DeviceNotPrepared
        );
        assert_eq!(
            state(true, ServiceFacts::default(), &prepared),
            NativeUsbComponentState::NotInstalled
        );
        let stopped = ServiceFacts {
            installed: true,
            exit_code: 1077,
            ..ServiceFacts::default()
        };
        assert_eq!(
            state(true, stopped, &prepared),
            NativeUsbComponentState::ServiceStopped
        );
        for code in POLICY_BLOCK_CODES {
            let blocked = ServiceFacts {
                exit_code: code,
                ..stopped
            };
            assert_eq!(
                state(true, blocked, &prepared),
                NativeUsbComponentState::BlockedByPolicy
            );
        }
        assert_eq!(
            state(false, running(), &prepared),
            NativeUsbComponentState::UnsupportedOs
        );
    }

    #[test]
    fn every_windows_state_offers_a_windows_action() {
        for (state, action) in [
            (
                NativeUsbComponentState::NotInstalled,
                NativeComponentAction::Install,
            ),
            (
                NativeUsbComponentState::ServiceStopped,
                NativeComponentAction::Start,
            ),
            (
                NativeUsbComponentState::DeviceNotPrepared,
                NativeComponentAction::Prepare,
            ),
            (
                NativeUsbComponentState::Ready,
                NativeComponentAction::Recheck,
            ),
        ] {
            assert_eq!(state.action(), action);
            assert!(!state.guidance(PLATFORM).contains("Mac"));
        }
    }

    #[test]
    fn only_a_plain_instance_id_reaches_the_prepare_command_line() {
        assert_eq!(
            prepare_parameters(INSTANCE).unwrap(),
            format!("--instance \"{INSTANCE}\"")
        );
        for hostile in [
            "USB\\VID_056A&PID_0317\\1\" --remove-all \"",
            "USB\\VID_056A&PID_0317\\a b",
            "USB\\VID_056A&PID_0317",
            "",
        ] {
            assert!(prepare_parameters(hostile).is_err(), "{hostile:?}");
        }
    }

    #[test]
    fn only_the_running_service_may_serve_the_pipe() {
        assert!(check_pipe_server(4_242, Some(4_242), false).is_ok());
        let impostor = check_pipe_server(4_243, Some(4_242), false).unwrap_err();
        assert!(impostor.contains("process 4243 serves it, not the service (process 4242)"));
        let stopped = check_pipe_server(4_243, None, false).unwrap_err();
        assert!(stopped.contains("not running"), "{stopped}");
        assert!(
            check_pipe_server(4_243, None, true).is_ok(),
            "lab console service"
        );
    }

    #[test]
    fn the_service_binary_is_read_from_its_command_line() {
        let path = |line| binary_from_command_line(line).map(|path| path.display().to_string());
        assert_eq!(
            path(r#""C:\Program Files\Arcen\USB\arcen-usb-service.exe""#).as_deref(),
            Some(r"C:\Program Files\Arcen\USB\arcen-usb-service.exe")
        );
        assert_eq!(
            path(r#""C:\Program Files\Arcen\USB\arcen-usb-service.exe" --flag"#).as_deref(),
            Some(r"C:\Program Files\Arcen\USB\arcen-usb-service.exe")
        );
        assert_eq!(
            path(r"C:\Arcen\USB\arcen-usb-service.EXE run").as_deref(),
            Some(r"C:\Arcen\USB\arcen-usb-service.EXE")
        );
        assert_eq!(path(r#""unterminated"#), None);
        assert_eq!(path("no executable here"), None);
        assert_eq!(path(r#""""#), None);
    }

    #[test]
    fn approving_and_rechecking_change_nothing_and_succeed() {
        assert_eq!(nothing_to_do(NativeComponentAction::Approve), Ok(()));
        assert_eq!(nothing_to_do(NativeComponentAction::Recheck), Ok(()));
        assert!(nothing_to_do(NativeComponentAction::Install).is_err());
    }
}
