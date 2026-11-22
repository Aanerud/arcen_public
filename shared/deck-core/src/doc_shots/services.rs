// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The services a rendered scene sees: placeholder folders under the user
//! `alice`, no displays, no decoder, no clipboard, no network.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use arcen_input::CapabilityAvailability;
use arcen_protocol::messages::{BuildIdentityMsg, ClientMonitor, DisplayColorMsg};

use super::DocPlatform;
use crate::DeckServices;
use crate::clipboard::PlatformClipboard;
use crate::decode::DecodeCapabilities;
use crate::directories::DeckDirectories;
use crate::microphone::{
    MicrophoneCancel, MicrophoneCaptureError, MicrophoneFrameSink, MicrophoneStream,
};
use crate::network::{NetworkIdentityPermission, NetworkProbeResult, OutgoingInterface};

/// [`DeckServices`] for the manual's scenes.
#[derive(Debug, Clone, Copy)]
pub(super) struct DocShotServices(pub(super) DocPlatform);

impl DocShotServices {
    fn home(self) -> PathBuf {
        PathBuf::from(match self.0 {
            DocPlatform::Mac => "/Users/alice",
            DocPlatform::Windows => r"C:\Users\alice",
        })
    }

    fn join(self, parts: &[&str]) -> PathBuf {
        // Joined with the Deck's own separator, whatever machine renders it.
        let separator = match self.0 {
            DocPlatform::Mac => "/",
            DocPlatform::Windows => r"\",
        };
        let mut path = self.home().display().to_string();
        for part in parts {
            path.push_str(separator);
            path.push_str(part);
        }
        PathBuf::from(path)
    }
}

impl DeckServices for DocShotServices {
    fn build_identity(&self) -> BuildIdentityMsg {
        arcen_protocol::build_identity::this_build(
            "arcen-deck-doc-shots",
            env!("CARGO_PKG_VERSION"),
        )
    }

    fn platform_summary(&self) -> String {
        match self.0 {
            DocPlatform::Mac => "macOS".to_owned(),
            DocPlatform::Windows => "Windows".to_owned(),
        }
    }

    fn native_config_dir(&self) -> Option<PathBuf> {
        None
    }

    fn native_log_dir(&self) -> Option<PathBuf> {
        Some(match self.0 {
            DocPlatform::Mac => self.join(&["Library", "Logs", "Arcen Deck"]),
            DocPlatform::Windows => self.join(&["AppData", "Local", "Arcen", "Deck", "Logs"]),
        })
    }

    fn directories(&self) -> DeckDirectories {
        // Never the environment's: a rendered scene names no real folder.
        DeckDirectories {
            config: None,
            logs: self.native_log_dir(),
        }
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
        true
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
            permission: NetworkIdentityPermission::NotRequested,
        }
    }

    fn outgoing_interface(&self, _peer: IpAddr) -> OutgoingInterface {
        OutgoingInterface::default()
    }

    fn timezone(&self) -> Option<String> {
        None
    }

    fn native_usb_component_state(&self) -> Option<arcen_usb_bridge::NativeUsbComponentState> {
        // A Deck the component was never installed on.
        Some(arcen_usb_bridge::NativeUsbComponentState::NotInstalled)
    }

    fn default_support_folder(&self) -> Option<PathBuf> {
        Some(self.join(&["Desktop"]))
    }

    fn can_choose_folder(&self) -> bool {
        true
    }
}
