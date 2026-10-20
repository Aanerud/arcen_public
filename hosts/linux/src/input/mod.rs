pub mod edges;
pub mod eis;
pub mod keymap;
pub mod pen;
pub mod region_adapter;
pub mod wayland;

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
    unavailable_drops: AtomicU64,
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
    /// Input refused because the session input connection was down.
    pub fn unavailable_drops(&self) -> u64 {
        self.unavailable_drops.load(Ordering::Relaxed)
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
    /// A pointer coordinate was not finite.
    #[error("invalid pointer coordinate")]
    InvalidCoordinate,
    /// The session-bound Wayland injector lost its session connection for
    /// the rest of the attachment.
    #[error("the Wayland session input connection is lost")]
    InjectorLost,
    /// The session-bound Wayland injector's connection broke and is being
    /// restored; this input was not delivered.
    #[error("the Wayland session input connection is being restored")]
    InjectorReconnecting,
    /// This session types key positions only, not committed text.
    #[error("committed text is not typed in this session")]
    TextUnavailable,
    /// The Wayland injection queue overflowed and was replaced by a release
    /// of every held key, button and touch.
    #[error("the Wayland input queue overflowed; all held input was released")]
    QueueCollapsed,
}

/// The injector one attachment uses, chosen once from the session's facts.
pub enum PierInput {
    /// uinput devices on the session's Xorg seat.
    Uinput(InputController),
    /// The session-bound injector of a headless Wayland session.
    Wayland(wayland::WaylandInjector),
}

macro_rules! delegate_input {
    ($($(#[$doc:meta])* $name:ident($message:ty);)+) => {
        $(
            $(#[$doc])*
            ///
            /// # Errors
            ///
            /// When the backend cannot inject it.
            pub fn $name(&mut self, message: &$message) -> Result<(), InputError> {
                match self {
                    Self::Uinput(controller) => controller.$name(message),
                    Self::Wayland(injector) => injector.$name(message),
                }
            }
        )+
    };
}

macro_rules! uinput_only {
    ($($(#[$doc:meta])* $name:ident($message:ty);)+) => {
        $(
            $(#[$doc])*
            ///
            /// # Errors
            ///
            /// When the backend cannot inject it; a Wayland session routes
            /// no region pen.
            pub fn $name(&mut self, message: &$message) -> Result<(), InputError> {
                match self {
                    Self::Uinput(controller) => controller.$name(message),
                    Self::Wayland(_) => Err(unsupported_on_wayland(stringify!($name))),
                }
            }
        )+
    };
}

/// What a Wayland session does not route: region-scoped pen (a typed pen
/// sample reaches the emulated stylus, when the session bound one).
fn unsupported_on_wayland(name: &str) -> InputError {
    if name.contains("pen") {
        InputError::PenUnavailable
    } else {
        InputError::RegionUnavailable
    }
}

impl PierInput {
    delegate_input! {
        /// A Deck key event.
        key_event(arcen_protocol::messages::KeyEventMsg);
        /// Absolute motion.
        mouse_move(arcen_protocol::messages::MouseMoveMsg);
        /// Relative motion.
        mouse_move_relative(arcen_protocol::messages::MouseMoveRelativeMsg);
        /// A button edge.
        mouse_button(arcen_protocol::messages::MouseButtonMsg);
        /// Scroll.
        mouse_scroll(arcen_protocol::messages::MouseScrollMsg);
        /// A typed pen sample: the uinput tablet tool, or a Wayland
        /// session's emulated stylus.
        pen_event(arcen_protocol::messages::PenEventMsg);
    }

    delegate_input! {
        /// Region pointer enter.
        region_pointer_enter(arcen_protocol::messages::RegionPointerEnterMsg);
        /// Region pointer leave.
        region_pointer_leave(arcen_protocol::messages::RegionPointerLeaveMsg);
        /// Region pointer motion.
        region_pointer_motion(arcen_protocol::messages::RegionPointerMotionMsg);
        /// Region pointer button.
        region_pointer_button(arcen_protocol::messages::RegionPointerButtonMsg);
        /// Region scroll.
        region_pointer_scroll(arcen_protocol::messages::RegionPointerScrollMsg);
    }

    uinput_only! {
        /// Region pen sample.
        region_pen_event(arcen_protocol::messages::RegionPenEventMsg);
    }

    /// Releases held keys only.
    ///
    /// # Errors
    ///
    /// When the backend cannot inject the releases.
    pub fn reset_keyboard_held(&mut self) -> Result<(), InputError> {
        match self {
            Self::Uinput(controller) => controller.reset_keyboard_held(),
            Self::Wayland(injector) => injector.reset_keyboard_held(),
        }
    }

    /// Releases every held key, button, touch and pen state.
    ///
    /// # Errors
    ///
    /// When the backend cannot inject the releases.
    pub fn reset_held(&mut self) -> Result<(), InputError> {
        match self {
            Self::Uinput(controller) => controller.reset_held(),
            Self::Wayland(injector) => injector.reset_held(),
        }
    }

    /// Whether typed pen input reaches the session.
    #[must_use]
    pub fn pen_available(&self) -> bool {
        self.pen_capabilities().pen_available()
    }

    /// What typed pen input delivers, axis by axis: the uinput tablet
    /// tool's fixed set, or what a Wayland session's emulated stylus bound.
    #[must_use]
    pub fn pen_capabilities(&self) -> arcen_input::pen_emulation::PenCapabilityTruth {
        match self {
            Self::Uinput(controller) => {
                arcen_input::pen_emulation::PenCapabilityTruth::linux_uinput(
                    controller.pen_available(),
                )
            }
            // A multi-display session's pen samples are region-scoped, which
            // the emulated stylus does not take yet.
            Self::Wayland(injector) if injector.region_input_available() => {
                arcen_input::pen_emulation::PenCapabilityTruth::UNAVAILABLE
            }
            Self::Wayland(injector) => injector.pen_capabilities(),
        }
    }

    /// Why a Wayland session has no pen, with the evidence shown to the
    /// Deck; `None` for uinput or a session with a stylus.
    #[must_use]
    pub fn pen_unavailable(&self) -> Option<arcen_input::pen_emulation::StylusUnavailable> {
        match self {
            Self::Uinput(_) => None,
            Self::Wayland(injector) => injector.pen_unavailable().or_else(|| {
                injector
                    .region_input_available()
                    .then_some(arcen_input::pen_emulation::StylusUnavailable::MultiMonitor)
            }),
        }
    }

    /// Committed text from the Deck. An Xorg session's uinput keyboard
    /// types key positions only, and advertises committed text unavailable.
    ///
    /// # Errors
    ///
    /// [`InputError::TextUnavailable`] on uinput; when the Wayland
    /// connection is down.
    pub fn text_commit(
        &mut self,
        message: &arcen_protocol::messages::TextCommitMsg,
    ) -> Result<(), InputError> {
        match self {
            Self::Uinput(_) => Err(InputError::TextUnavailable),
            Self::Wayland(injector) => injector.text_commit(message),
        }
    }

    /// Whether committed text is typed, and characters beyond the session's
    /// layout: `None` when it is not typed at all.
    #[must_use]
    pub fn text_capability(&self) -> Option<bool> {
        match self {
            Self::Uinput(_) => None,
            Self::Wayland(injector) => injector.text_capability(),
        }
    }

    /// The Deck-bound text results and layouts; see
    /// [`wayland::WaylandInjector::take_text_notices`].
    pub fn take_text_notices(&mut self) -> Option<tokio::sync::mpsc::Receiver<String>> {
        match self {
            Self::Uinput(_) => None,
            Self::Wayland(injector) => injector.take_text_notices(),
        }
    }

    /// Whether this attachment owns a region input adapter.
    #[must_use]
    pub fn region_input_available(&self) -> bool {
        match self {
            Self::Uinput(controller) => controller.region_input_available(),
            Self::Wayland(injector) => injector.region_input_available(),
        }
    }

    /// Destroys the virtual tablet device for Hard USB; see
    /// [`InputController::release_tablet_device`].
    pub fn release_tablet_device(&mut self) -> bool {
        match self {
            Self::Uinput(controller) => controller.release_tablet_device(),
            Self::Wayland(_) => false,
        }
    }

    /// Follows a Wayland injector's connection; `None` for uinput.
    #[must_use]
    pub fn wayland_status(&self) -> Option<tokio::sync::watch::Receiver<wayland::InjectorStatus>> {
        match self {
            Self::Uinput(_) => None,
            Self::Wayland(injector) => Some(injector.status()),
        }
    }

    /// Backend token for logs.
    #[must_use]
    pub const fn backend(&self) -> &'static str {
        match self {
            Self::Uinput(_) => "uinput",
            Self::Wayland(_) => "wayland_remote_desktop",
        }
    }
}

/// Which injector an attachment uses, decided once from the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputBackendChoice {
    /// The operator disabled input.
    Disabled,
    /// uinput on the session's Xorg seat.
    Uinput,
    /// The session-bound injector of this headless Wayland session.
    Wayland(wayland::WaylandInputTarget),
    /// A Wayland session whose injector target cannot be established:
    /// refuse the attachment. uinput would reach the physical seat, not
    /// this session.
    Refused(&'static str),
}

/// Chooses the attachment's injector from the session's facts. A Wayland
/// session never falls back to uinput.
#[must_use]
pub fn choose_input_backend(
    input_enabled: bool,
    session: Option<&crate::session::lifecycle::SessionMetadata>,
) -> InputBackendChoice {
    if !input_enabled {
        return InputBackendChoice::Disabled;
    }
    let Some(session) = session else {
        return InputBackendChoice::Uinput;
    };
    match (&session.wayland, session.session_type == "wayland") {
        (None, false) => InputBackendChoice::Uinput,
        (None, true) => InputBackendChoice::Refused("Wayland session without session input facts"),
        (Some(_), false) => {
            InputBackendChoice::Refused("session input facts on a non-Wayland session")
        }
        (Some(ready), true) => match wayland::WaylandInputTarget::from_session(
            ready,
            session.uid,
            &session.session_id,
            session.agent_pid,
        ) {
            Ok(target) => InputBackendChoice::Wayland(target),
            Err(wayland::WaylandInputError::RuntimeDir) => InputBackendChoice::Refused(
                "Wayland input socket outside the session's runtime directory",
            ),
            Err(_) => InputBackendChoice::Refused("inconsistent Wayland session input facts"),
        },
    }
}

#[cfg(target_os = "linux")]
pub mod seat_pen;
#[cfg(target_os = "linux")]
mod uinput;
#[cfg(target_os = "linux")]
pub use uinput::InputController;

// SEC-raw-hid. The experimental raw-HID pen-tablet passthrough (any device
// whose report descriptor classifies as a pen tablet; admission is
// `arcen_usb_bridge::RawHidAdmission`) is quarantined: it does not exist in
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

/// True only when this host binary was compiled with the default-off
/// `experimental-raw-hid` feature AND an operator explicitly set
/// `ARCEN_EXPERIMENTAL_RAW_HID=1` in the process environment. This is the
/// single runtime opt-in gate for the raw-HID pen-tablet passthrough; production
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

#[cfg(test)]
mod backend_choice_tests {
    use super::*;
    use crate::session::lifecycle::SessionMetadata;
    use crate::session::wayland::{
        private_runtime_dir, ReadyOutput, ReadyStream, WaylandAgentReady, INPUT_SOCKET_NAME,
        WAYLAND_DISPLAY_NAME,
    };

    fn session(session_type: &str, wayland: bool) -> SessionMetadata {
        let runtime_dir = private_runtime_dir(1001, "c9");
        SessionMetadata {
            username: "artist".into(),
            uid: 1001,
            session_id: "c9".into(),
            session_type: session_type.into(),
            desktop: "gnome".into(),
            display: String::new(),
            agent_pid: 4242,
            generation: 1,
            reconnected: false,
            timezone: None,
            multi_monitor_plan: None,
            multi_monitor_carrier: None,
            audio: None,
            requested_outputs: None,
            x11_desktop_depth: None,
            wayland: wayland.then(|| WaylandAgentReady {
                compositor: "gnome-shell".into(),
                input_socket: runtime_dir.join(INPUT_SOCKET_NAME),
                runtime_dir,
                wayland_display: WAYLAND_DISPLAY_NAME.into(),
                outputs: vec![ReadyOutput {
                    connector: "Meta-0".into(),
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                }],
                streams: vec![ReadyStream {
                    connector: "Meta-0".into(),
                    node_id: 44,
                    width: 1920,
                    height: 1080,
                    x: 0,
                    y: 0,
                }],
                input_device_types: 7,
                generation: 1,
                compositor_render_node: None,
                compositor_gpu_pci: None,
                compositor_gpu_vendor: None,
                output_mode: arcen_session::wayland_outputs::VirtualOutputMode::StartupMonitors,
                output_socket: None,
                x11: None,
                cursor_mode: Default::default(),
                clipboard: None,
            }),
        }
    }

    #[test]
    fn a_wayland_session_uses_its_own_injector_and_never_uinput() {
        assert!(matches!(
            choose_input_backend(true, Some(&session("wayland", true))),
            InputBackendChoice::Wayland(_)
        ));
        assert!(matches!(
            choose_input_backend(true, Some(&session("wayland", false))),
            InputBackendChoice::Refused(_)
        ));
        assert!(matches!(
            choose_input_backend(true, Some(&session("x11", true))),
            InputBackendChoice::Refused(_)
        ));
        let mut foreign = session("wayland", true);
        foreign.uid = 1002;
        assert!(matches!(
            choose_input_backend(true, Some(&foreign)),
            InputBackendChoice::Refused(_)
        ));
        assert_eq!(
            choose_input_backend(true, Some(&session("x11", false))),
            InputBackendChoice::Uinput
        );
        assert_eq!(choose_input_backend(true, None), InputBackendChoice::Uinput);
        assert_eq!(
            choose_input_backend(false, Some(&session("wayland", true))),
            InputBackendChoice::Disabled
        );
    }
}

#[cfg(test)]
mod experimental_raw_hid_tests {
    use super::*;

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

/// The identity the session's virtual devices are named for.
///
/// # Errors
///
/// Returns an error when `display` is not a dedicated X display; uinput is
/// then refused rather than creating devices every X server would admit.
pub fn session_input_identity(
    display: &str,
) -> Result<
    arcen_input::virtual_device::SessionInputIdentity,
    arcen_input::virtual_device::SessionInputIdentityError,
> {
    arcen_input::virtual_device::SessionInputIdentity::for_x_display(display)
}

#[cfg(not(target_os = "linux"))]
pub struct InputController;

#[cfg(not(target_os = "linux"))]
impl InputController {
    pub fn new(
        _device_w: u32,
        _device_h: u32,
        _region_input: Option<region_adapter::RegionInputAdapter>,
        _identity: arcen_input::virtual_device::SessionInputIdentity,
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
