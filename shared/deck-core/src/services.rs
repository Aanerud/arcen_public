// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! [`DeckServices`]: everything the portable Deck asks of the operating
//! system.
//!
//! Modelled on the macOS Deck's actual seams (`clients/macos/src/transport/
//! websocket.rs`, `ui/app.rs`, `logging/mod.rs`, `display/mod.rs`;
//! ADR 0024): each method replaces one `crate::<platform module>` call the
//! session engine or the shared screens make today. A shell implements it
//! once (`clients/macos` on `AppKit`, `clients/windows` on Win32) and hands an
//! `Arc<dyn DeckServices>` to the engine.
//!
//! Methods are cheap unless documented otherwise. The implementation is
//! `Send + Sync` and every method may be called from any thread, except
//! those marked **UI thread**: the clipboard and the display facts, which
//! macOS can only read on the main thread (`NSPasteboard`, `NSScreen`).
//! The engine never calls a UI-thread method itself; the shell reads them
//! on its UI thread and hands the engine a snapshot (as the macOS Deck
//! does today for the hello's monitor roster).

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use arcen_input::CapabilityAvailability;
use arcen_protocol::messages::{BuildIdentityMsg, ClientMonitor, DisplayColorMsg};

use crate::clipboard::PlatformClipboard;
use crate::decode::DecodeCapabilities;
use crate::directories::DeckDirectories;
use crate::microphone::{
    MicrophoneCancel, MicrophoneCaptureError, MicrophoneFrameSink, MicrophoneStream,
};
use crate::network::{NetworkProbeResult, OutgoingInterface};

/// The platform services of one Deck process.
pub trait DeckServices: Send + Sync + 'static {
    // --- Identity ---------------------------------------------------------

    /// This build's identity for the hello and every log
    /// (`arcen_protocol::build_identity::this_build("arcen-deck-<os>", …)`).
    fn build_identity(&self) -> BuildIdentityMsg;

    /// One line naming the operating system, its version and the
    /// architecture, for the log header and support bundles (for example
    /// `macOS 26.1 arm64` or `Windows 11 26100 x64`).
    fn platform_summary(&self) -> String;

    // --- Directories ------------------------------------------------------

    /// The platform's own folder for persisted Deck state, before
    /// overrides (macOS `~/Library/Application Support/Arcen`, Windows
    /// `%APPDATA%\Arcen\Deck` from `SHGetKnownFolderPath`). `None` when the
    /// platform cannot name one.
    fn native_config_dir(&self) -> Option<PathBuf>;

    /// The platform's own folder for logs, before overrides (macOS
    /// `~/Library/Logs/Arcen`, Windows `%LOCALAPPDATA%\Arcen\Deck\Logs`).
    fn native_log_dir(&self) -> Option<PathBuf>;

    /// Config and log directories with `ARCEN_CONFIG_DIR`/`ARCEN_LOG_DIR`
    /// applied. Shells do not override this.
    fn directories(&self) -> DeckDirectories {
        DeckDirectories::from_process_env(self.native_config_dir(), self.native_log_dir())
    }

    // --- Video ------------------------------------------------------------

    /// What the hardware decoder proved it decodes, for the hello. May run
    /// real decoder sessions the first time (cached for the process on
    /// macOS; on Windows cached per adapter and re-probed when the session
    /// window moves to a display on another adapter). Never an assumption.
    fn decode_capabilities(&self) -> DecodeCapabilities;

    /// [`Self::decode_capabilities`] with the GPU's name and the reason for
    /// every refused profile, for Settings' explanation of an unavailable
    /// Grading or HDR preset and for the support bundle. Same cost and
    /// thread rules as [`Self::decode_capabilities`]. The default knows
    /// only the flags.
    fn decode_report(&self) -> crate::decode_report::DecodeReport {
        crate::decode_report::DecodeReport::from_capabilities(
            String::new(),
            self.decode_capabilities(),
        )
    }

    // --- Displays (UI thread) ---------------------------------------------

    /// **UI thread.** The Deck's active displays, primary first, with their
    /// names, arrangement and colour facts (`ClientMonitor::color`). Empty
    /// means "let the Pier choose". Off the UI thread macOS returns monitors
    /// without colour or names, so a caller elsewhere would silently lose
    /// HDR.
    fn displays(&self) -> Vec<ClientMonitor>;

    /// **UI thread.** The gamut and HDR headroom the OS states for one display
    /// (`ClientMonitor::id`). macOS: EDR headroom; Windows:
    /// `IDXGIOutput6::GetDesc1` with headroom = `MaxLuminance` ÷ the SDR
    /// white level (`arcen_media::display_color`). `None` when unknown.
    fn display_color(&self, display_id: u32) -> Option<DisplayColorMsg>;

    /// **UI thread.** Whether any display can show HDR; HDR streaming is
    /// offered only when one can. Shells do not override this: the rule is
    /// shared.
    fn any_display_is_hdr(&self) -> bool {
        let displays = self.displays();
        arcen_media::display_color::any_display_is_hdr(
            displays.iter().filter_map(|monitor| monitor.color.as_ref()),
        )
    }

    // --- Input ------------------------------------------------------------

    /// Whether this build captures typed pen samples (macOS `AppKit` tablet
    /// events; Windows `WM_POINTER` pen). Combine with the user's setting
    /// through [`typed_pen_capability`].
    fn captures_typed_pen(&self) -> bool;

    /// Whether a pen tablet is attached or was used this run.
    /// Informational (Tablet Monitor, diagnostics); never gates typed pen.
    fn tablet_presence(&self) -> CapabilityAvailability;

    // --- Clipboard --------------------------------------------------------

    /// **UI thread.** The system clipboard; `None` when the platform has
    /// none or it cannot be opened now. The returned clipboard stays on the
    /// UI thread.
    fn open_clipboard(&self) -> Option<Box<dyn PlatformClipboard>>;

    // --- Microphone -------------------------------------------------------

    /// Starts microphone capture into `sink`.
    ///
    /// Blocks until capture runs or fails, which includes a permission
    /// prompt (bounded at 120 s); returns
    /// [`MicrophoneCaptureError::Cancelled`] promptly once `cancel` is set.
    ///
    /// # Errors
    ///
    /// Why capture could not start; `PermissionDenied` is reported to the
    /// Pier as such.
    fn start_microphone(
        &self,
        sink: Arc<dyn MicrophoneFrameSink>,
        cancel: MicrophoneCancel,
    ) -> Result<Box<dyn MicrophoneStream>, MicrophoneCaptureError>;

    // --- Network ----------------------------------------------------------

    /// Probes the local path to `host:port` (interface kind, scope, link
    /// speed, MTU) for the hello's network snapshot. Resolves the name;
    /// may block briefly.
    fn probe_network(&self, host: &str, port: u16) -> NetworkProbeResult;

    /// The interface traffic to `peer` leaves through, and its MTU.
    fn outgoing_interface(&self, peer: IpAddr) -> OutgoingInterface;

    // --- Native tablet and raw HID ------------------------------------------

    /// Whether this build carries the Native tablet bridge (macOS: the
    /// privileged helper; Windows: the optional Arcen USB component). A
    /// build without it never tries to capture a tablet.
    fn native_tablet_supported(&self) -> bool {
        false
    }

    /// Captures the attached tablet for the Native tablet bridge. Called
    /// only when [`Self::native_tablet_supported`] and the Pier offered the
    /// bridge.
    ///
    /// # Errors
    ///
    /// Why no tablet could be captured; the session keeps the requested
    /// mode for the Pier to refuse visibly.
    #[cfg(feature = "runtime")]
    fn start_usb_tablet_responder(
        &self,
    ) -> crate::runtime::usb::BoxFuture<
        'static,
        Result<Box<dyn crate::runtime::usb::UsbTabletResponder>, String>,
    > {
        Box::pin(async { Err("this Deck has no Native tablet bridge".to_owned()) })
    }

    /// Where the Native tablet component stands, for the settings screen's
    /// Native tablet row. `None` when this build has no Native tablet
    /// bridge. May cost a system round trip (macOS: XPC to `smd`; Windows:
    /// the service control manager and a device scan): read it once per
    /// settings visit and after an action, never per frame.
    fn native_usb_component_state(&self) -> Option<arcen_usb_bridge::NativeUsbComponentState> {
        None
    }

    /// Carries out the component control's action
    /// ([`arcen_usb_bridge::NativeUsbComponentState::action`]). It may show
    /// a system prompt (administrator approval) and wait for it, so call it
    /// off the UI thread; then read [`Self::native_usb_component_state`]
    /// again.
    ///
    /// # Errors
    ///
    /// A one-line reason the settings screen shows under the option.
    fn run_native_usb_component_action(
        &self,
        action: arcen_usb_bridge::NativeComponentAction,
    ) -> Result<(), String> {
        let _ = action;
        Err("this Deck has no Native tablet bridge".to_owned())
    }

    /// SEC-raw-hid: the local opt-in for experimental raw-HID capture.
    /// Always `false` unless the platform's lab-only feature is built in.
    fn experimental_raw_hid_opt_in(&self) -> bool {
        false
    }

    /// SEC-raw-hid: starts experimental raw-HID capture into `events`; the
    /// returned guard stops it when dropped. `None` when unsupported.
    #[cfg(feature = "runtime")]
    fn start_raw_hid_capture(
        &self,
        events: tokio::sync::mpsc::Sender<crate::runtime::raw_hid::HidEvent>,
    ) -> Option<Box<dyn std::any::Any + Send>> {
        let _ = events;
        None
    }

    // --- Locale -----------------------------------------------------------

    /// The system IANA time-zone identifier (for example `Europe/Oslo`),
    /// sent so the Pier's desktop shows local time. `None` when unknown.
    fn timezone(&self) -> Option<String>;

    // --- Support ----------------------------------------------------------

    /// What only the platform knows for a support bundle: GPU/adapter and
    /// OS facts as JSON, and where this Deck's crash reports are. May read
    /// the file system and query the GPU; call it off the UI thread.
    fn support_details(&self) -> crate::support_bundle::SupportDetails {
        crate::support_bundle::SupportDetails::default()
    }

    /// Where a support bundle is saved unless the user picks a folder (the
    /// Desktop). `None` falls back to the log folder.
    fn default_support_folder(&self) -> Option<PathBuf> {
        None
    }

    /// Whether [`Self::choose_folder`] shows a dialog (the panel offers
    /// "Change…" only then).
    fn can_choose_folder(&self) -> bool {
        false
    }

    /// **UI thread.** Asks the user for a folder, starting at `start`, with
    /// the platform's own dialog. `None` when cancelled or unsupported.
    fn choose_folder(&self, start: Option<&std::path::Path>) -> Option<PathBuf> {
        let _ = start;
        None
    }

    /// Shows `path` selected in Finder or Explorer. Returns whether the
    /// file browser started.
    fn reveal_in_file_browser(&self, path: &std::path::Path) -> bool {
        let _ = path;
        false
    }

    /// **UI thread.** Puts `text` on the system clipboard as plain text, for
    /// "Copy path". `false` when the platform leaves it to the UI toolkit.
    fn copy_text(&self, text: &str) -> bool {
        let _ = text;
        false
    }
}

/// Whether typed pen input can be offered: the user left it enabled and
/// this build captures pen events.
///
/// Deliberately independent of tablet presence: advertising it costs
/// nothing until a pen sample arrives, while tying it to presence left a
/// late-connected tablet without pressure until the next reconnect (the
/// hello cannot be renegotiated mid-session).
#[must_use]
pub const fn typed_pen_capability(
    enabled: bool,
    captures_typed_pen: bool,
) -> CapabilityAvailability {
    if enabled && captures_typed_pen {
        CapabilityAvailability::Available
    } else {
        CapabilityAvailability::Unavailable
    }
}

/// Services that offer nothing, for unit tests of shared code and of a
/// platform's composition (no displays, decoder, microphone or network
/// facts). Never a product's services.
#[cfg(feature = "runtime")]
pub mod test_support {
    use super::{
        Arc, BuildIdentityMsg, CapabilityAvailability, ClientMonitor, DeckServices,
        DecodeCapabilities, DisplayColorMsg, IpAddr, MicrophoneCancel, MicrophoneCaptureError,
        MicrophoneFrameSink, MicrophoneStream, NetworkProbeResult, OutgoingInterface, PathBuf,
        PlatformClipboard,
    };

    /// No displays, no decoder, no microphone, no network facts.
    #[derive(Debug, Default)]
    pub struct UnavailableServices;

    impl DeckServices for UnavailableServices {
        fn build_identity(&self) -> BuildIdentityMsg {
            arcen_protocol::build_identity::this_build("arcen-deck-core-test", "0.0.0")
        }

        fn platform_summary(&self) -> String {
            "test".to_owned()
        }

        fn native_config_dir(&self) -> Option<PathBuf> {
            None
        }

        fn native_log_dir(&self) -> Option<PathBuf> {
            None
        }

        fn decode_capabilities(&self) -> DecodeCapabilities {
            DecodeCapabilities::default()
        }

        fn displays(&self) -> Vec<ClientMonitor> {
            Vec::new()
        }

        fn display_color(&self, _display_id: u32) -> Option<DisplayColorMsg> {
            None
        }

        fn captures_typed_pen(&self) -> bool {
            false
        }

        fn tablet_presence(&self) -> CapabilityAvailability {
            CapabilityAvailability::Unknown
        }

        fn open_clipboard(&self) -> Option<Box<dyn PlatformClipboard>> {
            None
        }

        fn start_microphone(
            &self,
            _sink: Arc<dyn MicrophoneFrameSink>,
            _cancel: MicrophoneCancel,
        ) -> Result<Box<dyn MicrophoneStream>, MicrophoneCaptureError> {
            Err(MicrophoneCaptureError::UnsupportedPlatform)
        }

        fn probe_network(&self, _host: &str, _port: u16) -> NetworkProbeResult {
            NetworkProbeResult {
                snapshot: None,
                permission: crate::network::NetworkIdentityPermission::NotRequested,
            }
        }

        fn outgoing_interface(&self, _peer: IpAddr) -> OutgoingInterface {
            OutgoingInterface::default()
        }

        fn timezone(&self) -> Option<String> {
            None
        }
    }

    /// [`UnavailableServices`] with a decoder that proved `0`.
    #[derive(Debug, Default)]
    pub struct DecoderServices(pub DecodeCapabilities);

    impl DeckServices for DecoderServices {
        fn build_identity(&self) -> BuildIdentityMsg {
            UnavailableServices.build_identity()
        }

        fn platform_summary(&self) -> String {
            UnavailableServices.platform_summary()
        }

        fn native_config_dir(&self) -> Option<PathBuf> {
            None
        }

        fn native_log_dir(&self) -> Option<PathBuf> {
            None
        }

        fn decode_capabilities(&self) -> DecodeCapabilities {
            self.0
        }

        fn displays(&self) -> Vec<ClientMonitor> {
            Vec::new()
        }

        fn display_color(&self, _display_id: u32) -> Option<DisplayColorMsg> {
            None
        }

        fn captures_typed_pen(&self) -> bool {
            false
        }

        fn tablet_presence(&self) -> CapabilityAvailability {
            CapabilityAvailability::Unknown
        }

        fn open_clipboard(&self) -> Option<Box<dyn PlatformClipboard>> {
            None
        }

        fn start_microphone(
            &self,
            sink: Arc<dyn MicrophoneFrameSink>,
            cancel: MicrophoneCancel,
        ) -> Result<Box<dyn MicrophoneStream>, MicrophoneCaptureError> {
            UnavailableServices.start_microphone(sink, cancel)
        }

        fn probe_network(&self, host: &str, port: u16) -> NetworkProbeResult {
            UnavailableServices.probe_network(host, port)
        }

        fn outgoing_interface(&self, peer: IpAddr) -> OutgoingInterface {
            UnavailableServices.outgoing_interface(peer)
        }

        fn timezone(&self) -> Option<String> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_pen_needs_the_setting_and_a_capturing_build() {
        assert_eq!(
            typed_pen_capability(true, true),
            CapabilityAvailability::Available
        );
        assert_eq!(
            typed_pen_capability(false, true),
            CapabilityAvailability::Unavailable
        );
        assert_eq!(
            typed_pen_capability(true, false),
            CapabilityAvailability::Unavailable
        );
    }
}
