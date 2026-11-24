// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Platform services.
//!
//! [`WindowsDeckServices`] is the Windows [`DeckServices`]: config in
//! `%APPDATA%\Arcen\Deck`, logs in `%LOCALAPPDATA%\Arcen\Deck\Logs`
//! (known folders), build identity, network facts, time zone, displays,
//! clipboard, microphone and pen presence. Each method does what the macOS
//! Deck's counterpart named in the trait does, with Windows' facts.

#![allow(
    unsafe_code,
    reason = "Win32 is a C API; every unsafe block states its contract"
)]

pub mod build;
pub mod folders;
pub mod installed_copies;
pub mod motion;
pub mod netinfo;
pub mod password_file;
pub mod pen;
pub mod power;
pub mod registry;
pub mod support;
pub mod timezone;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};

use arcen_deck_core::DeckServices;
use arcen_deck_core::clipboard::PlatformClipboard;
use arcen_deck_core::decode::DecodeCapabilities;
use arcen_deck_core::microphone::{
    MicrophoneCancel, MicrophoneCaptureError, MicrophoneFrameSink, MicrophoneStream,
};
use arcen_deck_core::network::{NetworkProbeResult, OutgoingInterface};
use arcen_input::CapabilityAvailability;
use arcen_protocol::messages::{BuildIdentityMsg, ClientMonitor, DisplayColorMsg};

/// What the decoder proved it decodes on the system's default adapter;
/// supplied by the `decode` module (`probe_decode_capabilities`).
pub type DecodeProbe = fn() -> DecodeCapabilities;

/// What the decoder proved it decodes on one adapter, named by its LUID
/// ([`crate::display::DxgiOutputFacts::adapter_luid`]); supplied by the
/// `decode` module (`probe_for_adapter`), which caches per adapter.
pub type AdapterDecodeProbe = fn(i64) -> DecodeCapabilities;

/// No adapter noted yet (Windows never allocates LUID 0).
const NO_ADAPTER: i64 = 0;

/// The D3D11 decoder's probe for the adapter `luid` names (cached per
/// adapter by the decoder).
fn probe_adapter(luid: i64) -> DecodeCapabilities {
    crate::decode::probe_for_adapter(luid_from_i64(luid))
}

/// Splits [`crate::display::DxgiOutputFacts::adapter_luid`] back into a
/// Win32 `LUID`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a LUID is a 32-bit low part and a 32-bit high part"
)]
pub const fn luid_from_i64(luid: i64) -> windows::Win32::Foundation::LUID {
    windows::Win32::Foundation::LUID {
        LowPart: luid as u32,
        HighPart: (luid >> 32) as i32,
    }
}

/// What the shell does once at start, before any window: opt out of
/// background power throttling (the macOS Deck disables App Nap) and ask
/// Windows Error Reporting to keep minidumps of this executable's crashes
/// (`logging::register_crash_dumps`). Failures are logged, never fatal.
pub fn prepare_process() {
    if !power::disable_background_throttling() {
        tracing::warn!(
            target: arcen_deck_core::log_target::SESSION,
            "Windows refused the power throttling opt-out"
        );
    }
    let name = std::env::current_exe().ok().and_then(|exe| {
        exe.file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
    });
    if let Some(Err(code)) = name.map(|name| crate::logging::register_crash_dumps(&name)) {
        tracing::warn!(
            target: arcen_deck_core::log_target::SESSION,
            code,
            "crash dump registration failed"
        );
    }
}

/// The Windows Deck's platform services.
pub struct WindowsDeckServices {
    decode_probe: DecodeProbe,
    adapter_probe: Option<AdapterDecodeProbe>,
    session_adapter: AtomicI64,
    decode: OnceLock<DecodeCapabilities>,
    identity: OnceLock<BuildIdentityMsg>,
    summary: OnceLock<String>,
}

impl std::fmt::Debug for WindowsDeckServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsDeckServices")
            .finish_non_exhaustive()
    }
}

impl Default for WindowsDeckServices {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsDeckServices {
    /// The services of this Deck. Decode capabilities come only from the
    /// D3D11 decoder's real probes ([`crate::decode::probe`]): the system's
    /// default adapter until the shell names the session window's display
    /// ([`Self::note_session_display`]), then that display's adapter. A
    /// probe that proves nothing claims nothing. See also
    /// [`prepare_process`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            decode_probe: crate::decode::probe_decode_capabilities,
            adapter_probe: Some(probe_adapter),
            session_adapter: AtomicI64::new(NO_ADAPTER),
            decode: OnceLock::new(),
            identity: OnceLock::new(),
            summary: OnceLock::new(),
        }
    }

    /// Uses `probe` for [`DeckServices::decode_capabilities`] on the
    /// default adapter instead of the D3D11 decoder's (tests, tools).
    #[must_use]
    pub fn with_decode_probe(mut self, probe: DecodeProbe) -> Self {
        self.decode_probe = probe;
        self
    }

    /// Uses `probe` for the adapter the session window is on, once the
    /// shell has named it ([`Self::note_session_display`]), instead of the
    /// D3D11 decoder's (tests, tools); before that, the default adapter's
    /// answer stands.
    #[must_use]
    pub fn with_adapter_decode_probe(mut self, probe: AdapterDecodeProbe) -> Self {
        self.adapter_probe = Some(probe);
        self
    }

    /// **UI thread.** The session window is now on display `display_id`:
    /// decode capabilities follow that display's adapter (a machine with
    /// two GPUs can decode differently on each). Returns whether the
    /// adapter changed, so the shell knows the next hello may differ.
    pub fn note_session_display(&self, display_id: u32) -> bool {
        crate::display::adapter_luid(display_id).is_some_and(|luid| self.note_session_adapter(luid))
    }

    /// [`Self::note_session_display`] with the adapter's LUID.
    pub fn note_session_adapter(&self, luid: i64) -> bool {
        luid != NO_ADAPTER && self.session_adapter.swap(luid, Ordering::AcqRel) != luid
    }
}

impl DeckServices for WindowsDeckServices {
    fn build_identity(&self) -> BuildIdentityMsg {
        self.identity.get_or_init(build::current).clone()
    }

    fn platform_summary(&self) -> String {
        self.summary
            .get_or_init(|| {
                build::platform_summary(
                    &build::running_windows(),
                    build::architecture_token(std::env::consts::ARCH),
                    build::native_architecture(),
                )
            })
            .clone()
    }

    fn native_config_dir(&self) -> Option<PathBuf> {
        folders::native_config_dir()
    }

    fn native_log_dir(&self) -> Option<PathBuf> {
        folders::native_log_dir()
    }

    fn decode_capabilities(&self) -> DecodeCapabilities {
        let adapter = self.session_adapter.load(Ordering::Acquire);
        match self.adapter_probe {
            Some(probe) if adapter != NO_ADAPTER => probe(adapter),
            _ => *self.decode.get_or_init(self.decode_probe),
        }
    }

    fn decode_report(&self) -> arcen_deck_core::decode_report::DecodeReport {
        let capabilities = self.decode_capabilities();
        let adapter = self.session_adapter.load(Ordering::Acquire);
        let luid = if adapter == NO_ADAPTER {
            crate::decode::hardware_adapters()
                .ok()
                .and_then(|adapters| adapters.into_iter().next())
                .map(|(_, _, _, _, luid)| luid)
        } else {
            Some(luid_from_i64(adapter))
        };
        let Some(luid) = luid else {
            return arcen_deck_core::decode_report::DecodeReport::from_capabilities(
                "no hardware video adapter",
                capabilities,
            );
        };
        let report = crate::decode::probe_report_for_adapter(luid);
        arcen_deck_core::decode_report::DecodeReport {
            decoder: report.adapter,
            capabilities,
            refusals: report
                .refusals
                .into_iter()
                .map(
                    |(profile, reason)| arcen_deck_core::decode_report::DecodeRefusal {
                        profile: profile.to_owned(),
                        reason,
                    },
                )
                .collect(),
        }
    }

    fn displays(&self) -> Vec<ClientMonitor> {
        crate::display::enumerate()
    }

    fn display_color(&self, display_id: u32) -> Option<DisplayColorMsg> {
        crate::display::display_color(display_id)
    }

    fn captures_typed_pen(&self) -> bool {
        // The `input` stream's WM_POINTER pen path captures typed pen
        // samples on every Windows Deck build.
        true
    }

    fn tablet_presence(&self) -> CapabilityAvailability {
        pen::tablet_presence()
    }

    fn open_clipboard(&self) -> Option<Box<dyn PlatformClipboard>> {
        crate::clipboard::WindowsClipboard::open()
            .map(|clipboard| Box::new(clipboard) as Box<dyn PlatformClipboard>)
    }

    fn start_microphone(
        &self,
        sink: Arc<dyn MicrophoneFrameSink>,
        cancel: MicrophoneCancel,
    ) -> Result<Box<dyn MicrophoneStream>, MicrophoneCaptureError> {
        crate::audio::microphone::start(sink, &cancel)
            .map(|stream| Box::new(stream) as Box<dyn MicrophoneStream>)
    }

    fn probe_network(&self, host: &str, port: u16) -> NetworkProbeResult {
        netinfo::probe(host, port)
    }

    fn outgoing_interface(&self, peer: IpAddr) -> OutgoingInterface {
        netinfo::outgoing_interface(peer)
    }

    fn support_details(&self) -> arcen_deck_core::support_bundle::SupportDetails {
        let adapter = self.session_adapter.load(Ordering::Acquire);
        support::details((adapter != NO_ADAPTER).then_some(adapter))
    }

    fn default_support_folder(&self) -> Option<PathBuf> {
        folders::desktop_dir()
    }

    fn can_choose_folder(&self) -> bool {
        true
    }

    fn choose_folder(&self, start: Option<&std::path::Path>) -> Option<PathBuf> {
        support::choose_folder(start)
    }

    fn choose_profile_to_save(&self, suggested_file_name: &str) -> Option<PathBuf> {
        support::choose_profile_to_save(suggested_file_name)
    }

    fn choose_profile_to_open(&self) -> Option<PathBuf> {
        support::choose_profile_to_open()
    }

    fn reveal_in_file_browser(&self, path: &std::path::Path) -> bool {
        support::reveal(path)
    }

    fn timezone(&self) -> Option<String> {
        timezone::current_identifier()
    }

    // Native tablet: the optional Arcen USB component (`arcen-usb-client`).

    fn native_tablet_supported(&self) -> bool {
        true
    }

    fn start_usb_tablet_responder(
        &self,
    ) -> arcen_deck_core::runtime::usb::BoxFuture<
        'static,
        Result<Box<dyn arcen_deck_core::runtime::usb::UsbTabletResponder>, String>,
    > {
        Box::pin(arcen_usb_client::start_responder())
    }

    /// SCM calls and a HID scan: once per settings visit.
    fn native_usb_component_state(&self) -> Option<arcen_usb_client::NativeUsbComponentState> {
        Some(arcen_usb_client::component_state())
    }

    /// May show a UAC prompt and wait up to two minutes: off the UI thread.
    fn run_native_usb_component_action(
        &self,
        action: arcen_usb_client::NativeComponentAction,
    ) -> Result<(), String> {
        arcen_usb_client::run_action(action)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use arcen_deck_core::decode::DecoderBackendFamily;

    use super::*;

    fn proven(h265: bool) -> DecodeCapabilities {
        DecodeCapabilities {
            backend: DecoderBackendFamily::D3d11Video,
            h264: true,
            h265,
            hardware_accelerated: Some(true),
            ..DecodeCapabilities::default()
        }
    }

    #[test]
    fn the_default_adapter_probe_answers_once() {
        fn probe() -> DecodeCapabilities {
            proven(true)
        }
        let services = WindowsDeckServices::new().with_decode_probe(probe);
        assert!(services.decode_capabilities().h265);
        assert_eq!(
            services.decode_capabilities().decoder_backend_label(),
            "d3d11va-hw"
        );
    }

    #[test]
    fn decode_capabilities_follow_the_session_window_adapter() {
        fn default_adapter() -> DecodeCapabilities {
            proven(false)
        }
        fn on_adapter(luid: i64) -> DecodeCapabilities {
            proven(luid == 7)
        }
        let services = WindowsDeckServices::new()
            .with_decode_probe(default_adapter)
            .with_adapter_decode_probe(on_adapter);
        assert!(!services.decode_capabilities().h265);
        assert!(services.note_session_adapter(7));
        assert!(services.decode_capabilities().h265);
        assert!(!services.note_session_adapter(7));
        assert!(services.note_session_adapter(9));
        assert!(!services.decode_capabilities().h265);
        assert!(!services.note_session_adapter(NO_ADAPTER));
    }

    #[test]
    fn an_adapter_luid_survives_the_round_trip() {
        let luid = luid_from_i64((-3_i64 << 32) | 0x8000_0001);
        assert_eq!((luid.LowPart, luid.HighPart), (0x8000_0001, -3));
    }

    #[test]
    fn services_report_this_machine() {
        let services = WindowsDeckServices::new();
        assert_eq!(services.build_identity().product, build::PRODUCT);
        assert!(services.platform_summary().starts_with("Windows"));
        let dirs = services.directories();
        assert!(dirs.config.is_some() && dirs.logs.is_some());
        assert!(services.timezone().is_some());
    }
}
