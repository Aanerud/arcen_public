pub mod eis;
pub mod keymap;
pub mod pen;
pub mod region_adapter;

use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

#[derive(Debug, Default)]
pub struct InputStats {
    key_events: AtomicU64,
    mouse_moves: AtomicU64,
    mouse_buttons: AtomicU64,
    scroll_events: AtomicU64,
    pen_events: AtomicU64,
    resets: AtomicU64,
    unmapped_keys: AtomicU64,
}

impl InputStats {
    pub fn key_events(&self) -> u64 {
        self.key_events.load(Ordering::Relaxed)
    }
    pub fn mouse_moves(&self) -> u64 {
        self.mouse_moves.load(Ordering::Relaxed)
    }
    pub fn mouse_buttons(&self) -> u64 {
        self.mouse_buttons.load(Ordering::Relaxed)
    }
    pub fn scroll_events(&self) -> u64 {
        self.scroll_events.load(Ordering::Relaxed)
    }
    pub fn pen_events(&self) -> u64 {
        self.pen_events.load(Ordering::Relaxed)
    }
    pub fn resets(&self) -> u64 {
        self.resets.load(Ordering::Relaxed)
    }
    pub fn unmapped_keys(&self) -> u64 {
        self.unmapped_keys.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Error)]
pub enum InputError {
    #[error("uinput is unavailable on this platform")]
    Unsupported,
    #[error("uinput device failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid uinput geometry {0}x{1}")]
    InvalidGeometry(u32, u32),
    #[error("invalid mouse button {0}")]
    InvalidButton(u8),
    #[error("region-scoped input is unavailable for this session")]
    RegionUnavailable,
    #[error("region input adapter failed: {0}")]
    RegionAdapter(#[from] region_adapter::RegionAdapterError),
    /// The tablet-tool uinput device was never created (probe/create failed
    /// before `ServerHello`, or this platform never attempted it). Distinct
    /// from `Unsupported` because mouse/keyboard remain fully functional on
    /// the same [`InputController`] when only the pen backend is unavailable.
    #[error("pen/tablet uinput device is unavailable")]
    PenUnavailable,
}

#[cfg(target_os = "linux")]
mod uinput;
#[cfg(target_os = "linux")]
pub use uinput::InputController;

// SEC-raw-hid. The experimental raw-HID vendor passthrough
// (Wacom/Huion/XP-Pen/UC-Logic/Gaomon) is quarantined: it does not exist in
// the binary at all unless built with the default-off `experimental-raw-hid`
// Cargo feature. It is not a USB bridge — see `legal/ORIGINS.md`
// and `docs/architecture` for the true-bridge requirements this must not be
// mistaken for.
#[cfg(all(target_os = "linux", feature = "experimental-raw-hid"))]
pub mod uhid;
#[cfg(all(target_os = "linux", feature = "experimental-raw-hid"))]
pub use uhid::UhidDevice;

/// Process-environment variable that must be set to exactly `"1"` for the
/// experimental raw-HID passthrough to activate at runtime. The
/// `experimental-raw-hid` Cargo feature alone never enables it — both gates
/// are required, and there is no config-file equivalent.
pub const EXPERIMENTAL_RAW_HID_ENV: &str = "ARCEN_EXPERIMENTAL_RAW_HID";

/// Vendor IDs the quarantined raw-HID passthrough recognizes. Kept in one
/// place so the host's admission check and the client's capture filter agree
/// on exactly the same five supported tablet vendors.
pub const EXPERIMENTAL_RAW_HID_VENDOR_IDS: &[u16] = &[
    0x056A, // Wacom
    0x256c, // Huion
    0x28bd, // XP-Pen
    0x5543, // UC-Logic
    0x0b57, // Gaomon
];

/// True only when this host binary was compiled with the default-off
/// `experimental-raw-hid` feature AND an operator explicitly set
/// `ARCEN_EXPERIMENTAL_RAW_HID=1` in the process environment. This is the
/// single runtime opt-in gate for the raw-HID vendor passthrough; production
/// and default builds must always return `false` here.
pub fn experimental_raw_hid_runtime_enabled() -> bool {
    #[cfg(feature = "experimental-raw-hid")]
    {
        std::env::var(EXPERIMENTAL_RAW_HID_ENV).as_deref() == Ok("1")
    }
    #[cfg(not(feature = "experimental-raw-hid"))]
    {
        false
    }
}

/// True only for the fixed vendor allow-list backing the experimental raw-HID
/// path. Enforced independently on the host even though the client is
/// expected to already filter by vendor — the host must never trust a peer's
/// own filtering before a kernel-facing HID descriptor is parsed.
pub fn is_experimental_raw_hid_vendor(vendor_id: u16) -> bool {
    EXPERIMENTAL_RAW_HID_VENDOR_IDS.contains(&vendor_id)
}

#[cfg(test)]
mod experimental_raw_hid_tests {
    use super::*;

    #[test]
    fn vendor_allow_list_matches_only_known_tablet_vendors() {
        assert!(is_experimental_raw_hid_vendor(0x056A)); // Wacom
        assert!(is_experimental_raw_hid_vendor(0x256c)); // Huion
        assert!(is_experimental_raw_hid_vendor(0x28bd)); // XP-Pen
        assert!(is_experimental_raw_hid_vendor(0x5543)); // UC-Logic
        assert!(is_experimental_raw_hid_vendor(0x0b57)); // Gaomon
        assert!(!is_experimental_raw_hid_vendor(0x0000));
        assert!(!is_experimental_raw_hid_vendor(0xFFFF));
    }

    /// The default build (no `experimental-raw-hid` feature) must never
    /// activate raw HID even if an operator's environment happens to carry
    /// the opt-in variable — the Cargo feature gate is authoritative, not
    /// the environment alone.
    #[cfg(not(feature = "experimental-raw-hid"))]
    #[test]
    fn default_build_never_enables_raw_hid_regardless_of_env() {
        std::env::set_var(EXPERIMENTAL_RAW_HID_ENV, "1");
        assert!(!experimental_raw_hid_runtime_enabled());
        std::env::remove_var(EXPERIMENTAL_RAW_HID_ENV);
    }

    /// Even a binary compiled with the feature must still require the exact
    /// explicit runtime opt-in; unset or non-"1" values must stay closed.
    #[cfg(feature = "experimental-raw-hid")]
    #[test]
    fn feature_enabled_build_still_requires_explicit_env_opt_in() {
        std::env::remove_var(EXPERIMENTAL_RAW_HID_ENV);
        assert!(!experimental_raw_hid_runtime_enabled());
        std::env::set_var(EXPERIMENTAL_RAW_HID_ENV, "yes");
        assert!(!experimental_raw_hid_runtime_enabled());
        std::env::set_var(EXPERIMENTAL_RAW_HID_ENV, "1");
        assert!(experimental_raw_hid_runtime_enabled());
        std::env::remove_var(EXPERIMENTAL_RAW_HID_ENV);
    }
}

#[cfg(not(target_os = "linux"))]
pub struct InputController;

#[cfg(not(target_os = "linux"))]
impl InputController {
    pub fn new(
        _device_w: u32,
        _device_h: u32,
        _region_input: Option<region_adapter::RegionInputAdapter>,
    ) -> Result<(Self, std::sync::Arc<InputStats>), InputError> {
        Err(InputError::Unsupported)
    }

    pub fn key_event(
        &mut self,
        _message: &arcen_protocol::messages::KeyEventMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn reset_keyboard_held(&mut self) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn reset_held(&mut self) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn mouse_move(
        &mut self,
        _message: &arcen_protocol::messages::MouseMoveMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn mouse_move_relative(
        &mut self,
        _message: &arcen_protocol::messages::MouseMoveRelativeMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn mouse_button(
        &mut self,
        _message: &arcen_protocol::messages::MouseButtonMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn mouse_scroll(
        &mut self,
        _message: &arcen_protocol::messages::MouseScrollMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn pen_event(
        &mut self,
        _message: &arcen_protocol::messages::PenEventMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn region_pointer_enter(
        &mut self,
        _message: &arcen_protocol::messages::RegionPointerEnterMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn region_pointer_leave(
        &mut self,
        _message: &arcen_protocol::messages::RegionPointerLeaveMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn region_pointer_motion(
        &mut self,
        _message: &arcen_protocol::messages::RegionPointerMotionMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn region_pointer_button(
        &mut self,
        _message: &arcen_protocol::messages::RegionPointerButtonMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn region_pointer_scroll(
        &mut self,
        _message: &arcen_protocol::messages::RegionPointerScrollMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub fn region_pen_event(
        &mut self,
        _message: &arcen_protocol::messages::RegionPenEventMsg,
    ) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    pub const fn pen_available(&self) -> bool {
        false
    }
    pub const fn release_tablet_device(&mut self) -> bool {
        false
    }
    pub const fn region_input_available(&self) -> bool {
        false
    }
}
