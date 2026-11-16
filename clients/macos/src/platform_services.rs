// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The macOS Deck's [`DeckServices`]: each method is the AppKit/CoreGraphics/
//! AVAudioEngine call the session engine used to make directly (ADR 0024
//! extraction step 4).

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use arcen_deck_core::decode::{DecodeCapabilities, DecoderBackendFamily};
use arcen_deck_core::microphone::{
    MicrophoneCancel, MicrophoneCaptureError, MicrophoneFrameSink, MicrophoneStream,
};
use arcen_deck_core::network::{NetworkIdentityPermission, NetworkProbeResult, OutgoingInterface};
use arcen_deck_core::DeckServices;
use arcen_input::CapabilityAvailability;
use arcen_protocol::messages::{BuildIdentityMsg, ClientMonitor, DisplayColorMsg};

/// The macOS platform services. Stateless; one shared instance.
#[derive(Debug, Default)]
pub struct MacDeckServices;

/// The process's services, shared by every session.
#[must_use]
pub fn services() -> Arc<dyn DeckServices> {
    static SERVICES: OnceLock<Arc<dyn DeckServices>> = OnceLock::new();
    Arc::clone(SERVICES.get_or_init(|| Arc::new(MacDeckServices)))
}

/// `~/Library/Application Support/Arcen` (`~/.config/arcen` off macOS).
#[must_use]
pub fn native_config_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Arcen"),
        )
    } else {
        Some(home.join(".config").join("arcen"))
    }
}

/// `~/Library/Logs/Arcen` (`~/.local/state/arcen/logs` off macOS).
#[must_use]
pub fn native_log_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        Some(home.join("Library").join("Logs").join("Arcen"))
    } else {
        Some(home.join(".local").join("state").join("arcen").join("logs"))
    }
}

/// The support bundle's component (`arcen-deck-macos`).
#[must_use]
pub fn support_bundle_component() -> arcen_telemetry::BundleComponent {
    crate::support::component()
}

/// The VideoToolbox probe in the shared vocabulary.
#[must_use]
pub fn shared_decode_capabilities(
    probed: crate::pipeline::video_decoder::DecodeCapabilities,
) -> DecodeCapabilities {
    DecodeCapabilities {
        backend: DecoderBackendFamily::VideoToolbox,
        h264: probed.h264,
        h265: probed.h265,
        av1: probed.av1,
        yuv444: probed.yuv444,
        main10: probed.main10,
        main12: probed.main12,
        full_range: probed.full_range,
        identity_matrix: probed.identity_matrix,
        bt601_matrix: probed.bt601_matrix,
        bt2020_ncl_matrix: probed.bt2020_ncl_matrix,
        hardware_accelerated: probed.hardware_accelerated,
    }
}

impl DeckServices for MacDeckServices {
    fn build_identity(&self) -> BuildIdentityMsg {
        crate::build_identity::current()
    }

    fn platform_summary(&self) -> String {
        crate::platform::current().to_string()
    }

    fn native_config_dir(&self) -> Option<PathBuf> {
        native_config_dir()
    }

    fn native_log_dir(&self) -> Option<PathBuf> {
        native_log_dir()
    }

    fn decode_capabilities(&self) -> DecodeCapabilities {
        shared_decode_capabilities(crate::pipeline::video_decoder::probe_decode_capabilities())
    }

    fn decode_report(&self) -> arcen_deck_core::decode_report::DecodeReport {
        static NAME: OnceLock<String> = OnceLock::new();
        let name = NAME.get_or_init(|| {
            crate::support::gpu_name().map_or_else(
                || "VideoToolbox".to_owned(),
                |gpu| format!("{gpu} (VideoToolbox)"),
            )
        });
        arcen_deck_core::decode_report::DecodeReport::from_capabilities(
            name.clone(),
            self.decode_capabilities(),
        )
    }

    fn displays(&self) -> Vec<ClientMonitor> {
        crate::display::enumerate()
    }

    fn display_color(&self, display_id: u32) -> Option<DisplayColorMsg> {
        crate::display::display_color(display_id)
    }

    fn captures_typed_pen(&self) -> bool {
        cfg!(target_os = "macos")
    }

    fn tablet_presence(&self) -> CapabilityAvailability {
        crate::tablet::tablet_presence()
    }

    fn open_clipboard(&self) -> Option<Box<dyn arcen_deck_core::clipboard::PlatformClipboard>> {
        #[cfg(target_os = "macos")]
        {
            crate::clipboard::open_native()
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    fn start_microphone(
        &self,
        sink: Arc<dyn MicrophoneFrameSink>,
        cancel: MicrophoneCancel,
    ) -> Result<Box<dyn MicrophoneStream>, MicrophoneCaptureError> {
        crate::microphone::MicrophoneCapture::start(sink, cancel.stop, cancel.session_closed)
            .map(|capture| Box::new(capture) as Box<dyn MicrophoneStream>)
    }

    fn probe_network(&self, host: &str, port: u16) -> NetworkProbeResult {
        let probed = crate::netinfo::probe(host, port);
        NetworkProbeResult {
            snapshot: probed.snapshot,
            permission: match probed.permission {
                crate::netinfo::NetworkIdentityPermission::NotRequested => {
                    NetworkIdentityPermission::NotRequested
                }
                crate::netinfo::NetworkIdentityPermission::Unavailable => {
                    NetworkIdentityPermission::Unavailable
                }
            },
        }
    }

    fn outgoing_interface(&self, peer: IpAddr) -> OutgoingInterface {
        let (description, mtu) = crate::netinfo::outgoing_interface(peer);
        OutgoingInterface { description, mtu }
    }

    fn timezone(&self) -> Option<String> {
        crate::timezone::current_identifier()
    }

    fn support_details(&self) -> arcen_deck_core::support_bundle::SupportDetails {
        crate::support::details()
    }

    fn default_support_folder(&self) -> Option<PathBuf> {
        crate::support::desktop()
    }

    fn can_choose_folder(&self) -> bool {
        cfg!(target_os = "macos")
    }

    fn choose_folder(&self, start: Option<&std::path::Path>) -> Option<PathBuf> {
        crate::support::choose_folder(start)
    }

    fn reveal_in_file_browser(&self, path: &std::path::Path) -> bool {
        crate::support::reveal(path)
    }

    fn copy_text(&self, text: &str) -> bool {
        crate::support::copy_text(text)
    }

    fn native_tablet_supported(&self) -> bool {
        cfg!(feature = "native-tablet")
    }

    #[cfg(feature = "native-tablet")]
    fn start_usb_tablet_responder(
        &self,
    ) -> arcen_deck_core::runtime::usb::BoxFuture<
        'static,
        Result<Box<dyn arcen_deck_core::runtime::usb::UsbTabletResponder>, String>,
    > {
        Box::pin(async {
            crate::usb_bridge::UsbHardResponder::start()
                .await
                .map(|responder| {
                    Box::new(responder)
                        as Box<dyn arcen_deck_core::runtime::usb::UsbTabletResponder>
                })
        })
    }

    #[cfg(feature = "native-tablet")]
    fn native_usb_component_state(&self) -> Option<arcen_usb_bridge::NativeUsbComponentState> {
        Some(crate::usb_helper_install::install_state().component_state())
    }

    #[cfg(feature = "native-tablet")]
    fn run_native_usb_component_action(
        &self,
        action: arcen_usb_bridge::NativeComponentAction,
    ) -> Result<(), String> {
        use crate::usb_helper_install::{self as helper, HelperAction, HelperInstallState};
        let action = match action {
            arcen_usb_bridge::NativeComponentAction::Install => HelperAction::Install,
            arcen_usb_bridge::NativeComponentAction::Approve => HelperAction::Approve,
            // A launchd daemon starts itself once approved, and macOS needs no
            // per-tablet preparation.
            arcen_usb_bridge::NativeComponentAction::Start
            | arcen_usb_bridge::NativeComponentAction::Prepare
            | arcen_usb_bridge::NativeComponentAction::Recheck => HelperAction::Recheck,
        };
        match action {
            HelperAction::Install => {
                // Registering usually leaves the daemon waiting in Login
                // Items: take the user straight there.
                if helper::register()? == HelperInstallState::RequiresApproval {
                    helper::open_login_items_settings();
                }
            }
            HelperAction::Approve => helper::open_login_items_settings(),
            HelperAction::Recheck => {}
        }
        Ok(())
    }

    #[cfg(feature = "experimental-raw-hid")]
    fn experimental_raw_hid_opt_in(&self) -> bool {
        crate::hid::experimental_raw_hid_client_opt_in()
    }

    #[cfg(feature = "experimental-raw-hid")]
    fn start_raw_hid_capture(
        &self,
        events: tokio::sync::mpsc::Sender<arcen_deck_core::runtime::raw_hid::HidEvent>,
    ) -> Option<Box<dyn std::any::Any + Send>> {
        Some(Box::new(crate::hid::HidSession::start(events)))
    }
}
