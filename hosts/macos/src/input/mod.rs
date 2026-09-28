#![allow(unsafe_code)]

//! macOS input injection through `CoreGraphics` events.
//!
//! This is the native adapter only. Ordering, accumulation, attribution and
//! coordinate transforms live in `arcen-input`; this module turns already
//! validated shared events into `CGEvent`s and posts them.
//!
//! The controller tracks what it has pressed. That is not bookkeeping for its
//! own sake: if a session drops while a key or button is held, the physical
//! desktop is left stuck until someone walks over to it. [`release_all`]
//! exists so disconnect, lock and teardown can always hand the machine back in
//! a clean state, and it is the input half of the readiness evidence a Pier
//! needs before it may advertise a usable session.

pub mod keymap;
pub mod pen;
pub mod virtual_keyboard;

use std::collections::BTreeSet;
use std::ffi::c_void;

use arcen_input::{
    KeyboardEvent, PenEdge, PenTool, PenToolState, PointerButton, PointerMotion, PointerMotionMode,
    PointerScroll,
};
use arcen_protocol::messages::{
    GestureMagnifyMsg, GestureRotateMsg, GestureSmartZoomMsg, GestureSwipeMsg, PenEventMsg,
    SwipeDirectionMsg,
};
use serde::Serialize;

/// Opaque `CoreGraphics` event source.
type CGEventSourceRef = *mut c_void;
/// Opaque `CoreGraphics` event.
type CGEventRef = *mut c_void;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativePoint {
    pub x: f64,
    pub y: f64,
}

type CGPoint = NativePoint;

// Event source states. `HIDSystemState` makes injected events observe the same
// modifier and lock state the physical keyboard does.
const K_CG_EVENT_SOURCE_STATE_HID_SYSTEM: i32 = 1;

// Event tap locations. `HID` places events at the bottom of the stack, so the
// window server treats them as though they came from hardware.
const K_CG_HID_EVENT_TAP: u32 = 0;

// Event types.
const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const K_CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
const K_CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

// Mouse buttons.
const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;

// Event fields.
const K_CG_MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;

// Scroll units.
//
// Line, not pixel. The wire delta is a count of wheel notches — the Deck
// accumulates trackpad travel and divides by the same 120 units per notch that
// every other host in this protocol reads it as — so posting it as a pixel
// count scrolled one pixel per notch, which is indistinguishable from
// scrolling being broken and was reported as exactly that.
//
// A line is what a notch means. Applications size it themselves, the way they
// do for a real wheel, rather than this host inventing a pixels-per-notch
// figure that would be wrong in some application or other.
const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;
/// `kCGScrollEventUnitPixel`: continuous travel, as a trackpad produces.
const K_CG_SCROLL_EVENT_UNIT_PIXEL: u32 = 0;
/// `kCGScrollWheelEventIsContinuous`.
const K_CG_SCROLL_WHEEL_EVENT_IS_CONTINUOUS: u32 = 88;
/// `kCGScrollWheelEventScrollPhase`.
const K_CG_SCROLL_WHEEL_EVENT_SCROLL_PHASE: u32 = 99;
/// `CGScrollPhase` values.
const K_CG_SCROLL_PHASE_BEGAN: i64 = 1;
const K_CG_SCROLL_PHASE_CHANGED: i64 = 2;
const K_CG_SCROLL_PHASE_ENDED: i64 = 4;
const K_CG_SCROLL_PHASE_CANCELLED: i64 = 8;

// Modifier flags.
const K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT: u64 = 0x0001_0000;
const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;
const K_CG_EVENT_FLAG_MASK_NUMERIC_PAD: u64 = 0x0020_0000;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state_id: i32) -> CGEventSourceRef;
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    /// The non-variadic form, and deliberately so.
    ///
    /// Apple declares `CGEventCreateScrollWheelEvent` as
    /// `(..., int32_t wheel1, ...)` — variadic. Declaring its second wheel as
    /// an ordinary parameter compiles and links, and is wrong on Apple
    /// silicon: variadic arguments are passed on the stack there while a
    /// fixed parameter goes in a register, so the horizontal delta arrived as
    /// whatever happened to be on the stack. Vertical scrolling looked fine,
    /// which is why it survived.
    ///
    /// `CGEventCreateScrollWheelEvent2` takes all three wheels as fixed
    /// arguments and has existed since 10.13, so there is no reason to
    /// negotiate a variadic ABI at all.
    fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventSetDoubleValueField(event: CGEventRef, field: u32, value: f64);
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGWarpMouseCursorPosition(new_position: CGPoint) -> i32;
    fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;
    fn CGEventSourceKeyState(state_id: i32, key: u16) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(reference: *const c_void);
}

/// Releases a Core Foundation object.
///
/// Exposed so the rest of this crate never declares `CFRelease` a second time.
/// Two declarations of one symbol that disagree about their signature is
/// undefined behaviour rather than an inconsistency, and the compiler only
/// warns about it.
///
/// # Safety
///
/// `reference` must be non-null and must be an object this caller owns a
/// retain count on. The count is consumed.
pub(crate) unsafe fn release_core_foundation(reference: *const c_void) {
    // SAFETY: the caller guarantees a non-null, owned reference, which is
    // exactly what `CFRelease` requires; it aborts on null rather than
    // tolerating it.
    unsafe { CFRelease(reference) }
}

/// Why input could not be injected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum InputError {
    /// `CoreGraphics` refused to create an event source. Without Accessibility
    /// consent this is what a denied grant looks like.
    SourceUnavailable,
    /// `CoreGraphics` refused to create an event.
    EventCreationFailed(&'static str),
    /// The desktop bounds are empty, so no absolute position can be mapped.
    NoDesktopBounds,
    /// Input may only go through the virtual HID devices here, and they are
    /// not available.
    VirtualHidUnavailable,
    /// A gesture message was malformed or outside the host's supported set.
    InvalidGesture(&'static str),
    /// This gesture cannot be injected by the current adapter.
    Unsupported,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceUnavailable => {
                formatter.write_str("could not create a CoreGraphics event source")
            }
            Self::EventCreationFailed(what) => {
                write!(formatter, "could not create a {what} event")
            }
            Self::NoDesktopBounds => formatter.write_str("desktop bounds are empty"),
            Self::VirtualHidUnavailable => formatter
                .write_str("input here goes only through virtual HID, which is unavailable"),
            Self::InvalidGesture(what) => write!(formatter, "invalid {what} gesture"),
            Self::Unsupported => formatter.write_str("input gesture is unsupported"),
        }
    }
}

impl std::error::Error for InputError {}

/// Counters describing what the controller has injected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct InputStats {
    /// Key transitions posted.
    pub key_events: u64,
    /// Pointer moves posted.
    pub pointer_moves: u64,
    /// Pointer button transitions posted.
    pub pointer_buttons: u64,
    /// Scroll events posted.
    pub scroll_events: u64,
    /// Times every held key and button was released.
    pub releases: u64,
    /// Key identifiers this host has no mapping for.
    pub unmapped_keys: u64,
    /// Pen samples injected with their tablet surface intact.
    pub pen_samples: u64,
    /// Pen proximity transitions posted.
    pub pen_proximity_edges: u64,
}

/// The pixel rectangle that normalized coordinates map onto.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DesktopBounds {
    /// Left edge in global display space.
    pub origin_x: f64,
    /// Top edge in global display space.
    pub origin_y: f64,
    /// Width in pixels.
    pub width: f64,
    /// Height in pixels.
    pub height: f64,
}

impl DesktopBounds {
    /// Creates bounds from a display snapshot.
    #[must_use]
    pub const fn new(origin_x: f64, origin_y: f64, width: f64, height: f64) -> Self {
        Self {
            origin_x,
            origin_y,
            width,
            height,
        }
    }

    /// Returns whether these bounds can map a coordinate at all.
    #[must_use]
    pub fn is_usable(self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }

    /// Maps a normalized coordinate onto the desktop, clamped to its edges.
    ///
    /// Clamping rather than rejecting matters: a client whose pointer leaves
    /// the shared region should stop at the edge, not jump somewhere else or
    /// drop the event.
    #[must_use]
    pub fn to_global(self, x: f64, y: f64) -> NativePoint {
        let clamped_x = x.clamp(0.0, 1.0);
        let clamped_y = y.clamp(0.0, 1.0);
        // The far edge is the last point *on* the display. `origin + width`
        // is the first point of whatever lies beyond it, which in global
        // space is the neighbouring display: dragging past the Deck window's
        // right edge put the host cursor on the monitor to the right.
        let last_x = self.origin_x + (self.width - 1.0).max(0.0);
        let last_y = self.origin_y + (self.height - 1.0).max(0.0);
        NativePoint {
            x: (self.origin_x + clamped_x * self.width).min(last_x),
            y: (self.origin_y + clamped_y * self.height).min(last_y),
        }
    }
}

/// Translates a shared modifier mask into `CoreGraphics` event flags.
#[must_use]
pub const fn modifier_flags(modifiers: u32, caps_lock_on: Option<bool>) -> u64 {
    let mut flags = 0_u64;
    if modifiers & keymap::MOD_SHIFT != 0 {
        flags |= K_CG_EVENT_FLAG_MASK_SHIFT;
    }
    if modifiers & keymap::MOD_CTRL != 0 {
        flags |= K_CG_EVENT_FLAG_MASK_CONTROL;
    }
    if modifiers & keymap::MOD_ALT != 0 {
        flags |= K_CG_EVENT_FLAG_MASK_ALTERNATE;
    }
    if modifiers & keymap::MOD_META != 0 {
        flags |= K_CG_EVENT_FLAG_MASK_COMMAND;
    }
    if modifiers & keymap::MOD_KEYPAD != 0 {
        flags |= K_CG_EVENT_FLAG_MASK_NUMERIC_PAD;
    }
    if let Some(true) = caps_lock_on {
        flags |= K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT;
    }
    flags
}

/// Maps a shared pointer button to a `CoreGraphics` button number.
///
/// Buttons 1..=3 are the physical left, middle and right. Anything above that
/// is posted as an "other" button with its own number, which is how macOS
/// carries side buttons.
#[must_use]
pub const fn button_number(button: u8) -> u32 {
    match button {
        1 => K_CG_MOUSE_BUTTON_LEFT,
        2 => K_CG_MOUSE_BUTTON_CENTER,
        3 => K_CG_MOUSE_BUTTON_RIGHT,
        other => other as u32,
    }
}

/// Chooses the event type for a button transition.
#[must_use]
const fn button_event_type(button: u8, pressed: bool) -> u32 {
    match (button, pressed) {
        (1, true) => K_CG_EVENT_LEFT_MOUSE_DOWN,
        (1, false) => K_CG_EVENT_LEFT_MOUSE_UP,
        (3, true) => K_CG_EVENT_RIGHT_MOUSE_DOWN,
        (3, false) => K_CG_EVENT_RIGHT_MOUSE_UP,
        (_, true) => K_CG_EVENT_OTHER_MOUSE_DOWN,
        (_, false) => K_CG_EVENT_OTHER_MOUSE_UP,
    }
}

/// Returns the `NX_TABLET_POINTER_*` value for a shared tool.
#[must_use]
const fn pen_pointer_type(tool: PenTool) -> i64 {
    match tool {
        PenTool::Tip => pen::NX_TABLET_POINTER_PEN,
        PenTool::Eraser => pen::NX_TABLET_POINTER_ERASER,
    }
}

/// Maps a barrel button index onto a mouse button.
///
/// Returns `None` for indices with no agreed meaning, so an unusual pen
/// reports fewer buttons rather than inventing clicks the user did not make.
#[must_use]
const fn barrel_button(index: u8) -> Option<u8> {
    match index {
        0 => Some(3),
        1 => Some(2),
        _ => None,
    }
}

/// Applies tablet point annotations to an event.
///
/// # Safety
///
/// `event` must be a live `CGEventRef` the caller owns.
unsafe fn annotate_tablet_point(event: CGEventRef, point: pen::TabletPoint) {
    // SAFETY: delegated to this function's contract on `event`.
    unsafe {
        CGEventSetIntegerValueField(
            event,
            pen::K_CG_MOUSE_EVENT_SUBTYPE,
            pen::K_CG_MOUSE_SUBTYPE_TABLET_POINT,
        );
        CGEventSetIntegerValueField(
            event,
            pen::K_CG_TABLET_EVENT_DEVICE_ID,
            pen::ARCEN_TABLET_DEVICE_ID,
        );
        CGEventSetIntegerValueField(event, pen::K_CG_TABLET_EVENT_POINT_BUTTONS, point.buttons);
        CGEventSetDoubleValueField(event, pen::K_CG_TABLET_EVENT_POINT_PRESSURE, point.pressure);
        CGEventSetDoubleValueField(event, pen::K_CG_TABLET_EVENT_TILT_X, point.tilt_x);
        CGEventSetDoubleValueField(event, pen::K_CG_TABLET_EVENT_TILT_Y, point.tilt_y);
        CGEventSetDoubleValueField(
            event,
            pen::K_CG_TABLET_EVENT_ROTATION,
            point.rotation_degrees,
        );
    }
}

/// Chooses the motion event type for the currently held buttons.///
/// macOS distinguishes a move from a drag. Posting a plain move while a button
/// is down breaks text selection and window dragging, so the held set decides.
#[must_use]
const fn motion_event_type(held: u32) -> (u32, u32) {
    if held & (1 << 1) != 0 {
        (K_CG_EVENT_LEFT_MOUSE_DRAGGED, K_CG_MOUSE_BUTTON_LEFT)
    } else if held & (1 << 3) != 0 {
        (K_CG_EVENT_RIGHT_MOUSE_DRAGGED, K_CG_MOUSE_BUTTON_RIGHT)
    } else if held != 0 {
        (K_CG_EVENT_OTHER_MOUSE_DRAGGED, K_CG_MOUSE_BUTTON_CENTER)
    } else {
        (K_CG_EVENT_MOUSE_MOVED, K_CG_MOUSE_BUTTON_LEFT)
    }
}

/// Where input is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackend {
    /// `CGEvent` posting for everything.
    CoreGraphics,
    /// Keys through the virtual HID keyboard owned by the injector child at
    /// this path, the pointer through `CGEvent`, which places it exactly. A
    /// keyboard that cannot start degrades to `CGEvent`.
    VirtualKeyboard(&'static str),
    /// Everything through the injector child's devices, and `CoreGraphics`
    /// event calls never made. The login window's backend: there, creating a
    /// mouse event blocked forever inside `SkyLight` and froze the session.
    VirtualHidOnly(&'static str),
}

static INPUT_BACKEND: std::sync::OnceLock<InputBackend> = std::sync::OnceLock::new();

/// Chooses the input backend for every controller this process creates.
///
/// Set once, by the process that serves the desktop, before any session.
/// Anything that never sets it — tests, probes, `serve` — posts `CGEvent`s.
pub fn set_input_backend(backend: InputBackend) {
    let _ = INPUT_BACKEND.set(backend);
}

fn input_backend() -> InputBackend {
    INPUT_BACKEND
        .get()
        .copied()
        .unwrap_or(InputBackend::CoreGraphics)
}

/// Scroll distance, in points, that one wheel detent stands for when a
/// continuous scroll has to go through the virtual pointer's wheel.
const POINTS_PER_DETENT: f64 = 12.0;

/// Injects keyboard and pointer input into the local desktop.
pub struct InputController {
    /// Null when the backend is [`InputBackend::VirtualHidOnly`].
    source: CGEventSourceRef,
    bounds: DesktopBounds,
    /// The virtual HID devices, when this session uses them.
    virtual_keyboard: Option<virtual_keyboard::VirtualHid>,
    /// Whether every event goes through `virtual_keyboard`.
    hid_only: bool,
    /// The whole desktop in points, which the virtual pointer's absolute
    /// coordinates span. Only read in HID-only mode.
    hid_desktop: DesktopBounds,
    held_keys: BTreeSet<u16>,
    held_buttons: u32,
    /// Fractional pixels of continuous scroll not yet posted.
    scroll_remainder: (f64, f64),
    last_position: CGPoint,
    pen: PenToolState,
    stats: InputStats,
}

// SAFETY: a `CGEventSource` is an immutable CF object and the controller owns
// its only reference. All mutation is behind `&mut self`.
unsafe impl Send for InputController {}

impl std::fmt::Debug for InputController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputController")
            .field("bounds", &self.bounds)
            .field("held_keys", &self.held_keys.len())
            .field("held_buttons", &self.held_buttons)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl InputController {
    /// Creates a controller that maps normalized coordinates onto `bounds`.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::NoDesktopBounds`] for an empty desktop and
    /// [`InputError::SourceUnavailable`] when `CoreGraphics` refuses an event
    /// source, which is how a missing Accessibility grant presents.
    pub fn new(bounds: DesktopBounds) -> Result<Self, InputError> {
        if !bounds.is_usable() {
            return Err(InputError::NoDesktopBounds);
        }
        let backend = input_backend();
        let hid_only = matches!(backend, InputBackend::VirtualHidOnly(_));
        if let InputBackend::VirtualHidOnly(program) = backend {
            let devices = virtual_keyboard::VirtualHid::spawn(std::path::Path::new(program))
                .map_err(|error| {
                    tracing::warn!(
                        target: arcen_telemetry::names::target::HID,
                        %error,
                        "no virtual HID devices, and this desktop takes no other input"
                    );
                    InputError::VirtualHidUnavailable
                })?;
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                "keyboard and pointer input go through virtual HID devices"
            );
            return Ok(Self::with(
                std::ptr::null_mut(),
                bounds,
                Some(devices),
                hid_only,
            ));
        }
        // SAFETY: creating an event source has no preconditions.
        let source = unsafe { CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_HID_SYSTEM) };
        if source.is_null() {
            return Err(InputError::SourceUnavailable);
        }
        let virtual_keyboard = match backend {
            InputBackend::CoreGraphics | InputBackend::VirtualHidOnly(_) => None,
            InputBackend::VirtualKeyboard(program) => {
                match virtual_keyboard::VirtualHid::spawn(std::path::Path::new(program)) {
                    Ok(keyboard) => {
                        tracing::info!(
                            target: arcen_telemetry::names::target::HID,
                            "keyboard input goes through a virtual HID keyboard"
                        );
                        Some(keyboard)
                    }
                    // Degraded, not refused: a session that cannot have the
                    // virtual keyboard still types through CGEvent.
                    Err(error) => {
                        tracing::warn!(
                            target: arcen_telemetry::names::target::HID,
                            %error,
                            "no virtual HID keyboard; keyboard input uses CGEvent"
                        );
                        None
                    }
                }
            }
        };
        Ok(Self::with(source, bounds, virtual_keyboard, false))
    }

    fn with(
        source: CGEventSourceRef,
        bounds: DesktopBounds,
        virtual_keyboard: Option<virtual_keyboard::VirtualHid>,
        hid_only: bool,
    ) -> Self {
        Self {
            source,
            bounds,
            virtual_keyboard,
            hid_only,
            hid_desktop: if hid_only {
                whole_desktop(bounds)
            } else {
                bounds
            },
            held_keys: BTreeSet::new(),
            held_buttons: 0,
            scroll_remainder: (0.0, 0.0),
            last_position: NativePoint {
                x: bounds.origin_x + bounds.width / 2.0,
                y: bounds.origin_y + bounds.height / 2.0,
            },
            pen: PenToolState::default(),
            stats: InputStats::default(),
        }
    }

    /// The virtual devices, which must exist in HID-only mode.
    fn devices(&mut self) -> Result<&mut virtual_keyboard::VirtualHid, InputError> {
        self.virtual_keyboard
            .as_mut()
            .ok_or(InputError::VirtualHidUnavailable)
    }

    /// Records that the injector child has gone. In HID-only mode that is the
    /// end of input for this session, and the error ends it, so the next one
    /// starts a fresh child.
    fn devices_failed(&mut self, error: &virtual_keyboard::VirtualKeyboardError) -> InputError {
        tracing::warn!(
            target: arcen_telemetry::names::target::HID,
            %error,
            "virtual HID devices failed"
        );
        self.virtual_keyboard = None;
        InputError::VirtualHidUnavailable
    }

    /// Places the virtual pointer at a position normalized to the session's
    /// display, remembering where it is in global terms.
    ///
    /// The device's coordinates span the whole desktop, so the position goes
    /// through global points first. With one display the two rectangles are
    /// the same; with several, the mapping assumes macOS spreads an absolute
    /// pointer across all of them, which has not been measured.
    fn hid_move(&mut self, x: f64, y: f64) -> Result<(), InputError> {
        let point = self.bounds.to_global(x, y);
        let desktop = self.hid_desktop;
        let (x, y) = (
            (point.x - desktop.origin_x) / desktop.width,
            (point.y - desktop.origin_y) / desktop.height,
        );
        let result = self.devices()?.move_pointer(x, y);
        result.map_err(|error| self.devices_failed(&error))?;
        self.last_position = point;
        Ok(())
    }

    fn hid_button(&mut self, button: u8, pressed: bool) -> Result<(), InputError> {
        let result = self.devices()?.pointer_button(button, pressed);
        result.map_err(|error| self.devices_failed(&error))
    }

    /// Returns the injection counters.
    #[must_use]
    pub const fn stats(&self) -> InputStats {
        self.stats
    }

    /// Whether keyboard input goes through the virtual HID keyboard.
    #[must_use]
    pub const fn uses_virtual_keyboard(&self) -> bool {
        self.virtual_keyboard.is_some()
    }

    /// Returns how many keys are currently held.
    #[must_use]
    pub fn held_key_count(&self) -> usize {
        self.held_keys.len()
    }

    /// Returns whether any pointer button is currently held.
    #[must_use]
    pub const fn has_held_buttons(&self) -> bool {
        self.held_buttons != 0
    }

    /// Replaces the desktop rectangle normalized coordinates map onto.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::NoDesktopBounds`] for an empty desktop.
    pub fn set_bounds(&mut self, bounds: DesktopBounds) -> Result<(), InputError> {
        if !bounds.is_usable() {
            return Err(InputError::NoDesktopBounds);
        }
        self.bounds = bounds;
        if self.hid_only {
            self.hid_desktop = whole_desktop(bounds);
        }
        Ok(())
    }

    /// Injects a key transition.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::EventCreationFailed`] when `CoreGraphics` refuses
    /// the event. An unmapped key is counted and ignored rather than pressing
    /// something else.
    pub fn key_event(&mut self, event: &KeyboardEvent) -> Result<(), InputError> {
        let Some(code) = keymap::qt_key_to_macos(event.key_id, event.modifiers.0) else {
            self.stats.unmapped_keys += 1;
            return Ok(());
        };
        if let Some(keyboard) = self.virtual_keyboard.as_mut() {
            let Some(usage) = virtual_keyboard::macos_key_to_hid_usage(code) else {
                self.stats.unmapped_keys += 1;
                return Ok(());
            };
            match keyboard.set(usage, event.pressed) {
                Ok(()) => {
                    self.stats.key_events += 1;
                    return Ok(());
                }
                // A seventh held key. The key is refused, as an unmapped one
                // is, and the device carries on.
                Err(virtual_keyboard::VirtualKeyboardError::Rollover) => {
                    self.stats.unmapped_keys += 1;
                    return Ok(());
                }
                Err(error) if self.hid_only => return Err(self.devices_failed(&error)),
                // The child went away. Everything it held was released when
                // its input closed; the rest of the session types through
                // CGEvent rather than into nothing.
                Err(error) => {
                    tracing::warn!(
                        target: arcen_telemetry::names::target::HID,
                        %error,
                        "virtual HID keyboard failed; switching to CGEvent"
                    );
                    self.virtual_keyboard = None;
                }
            }
        }
        if self.hid_only {
            return Err(InputError::VirtualHidUnavailable);
        }
        let flags = modifier_flags(event.modifiers.0, event.caps_lock_on);
        self.post_key(code, event.pressed, flags)?;
        if event.pressed {
            self.held_keys.insert(code);
        } else {
            self.held_keys.remove(&code);
        }
        self.stats.key_events += 1;
        Ok(())
    }

    fn post_key(&self, code: u16, pressed: bool, flags: u64) -> Result<(), InputError> {
        // SAFETY: the source is live for the controller's lifetime.
        let event = unsafe { CGEventCreateKeyboardEvent(self.source, code, pressed) };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("keyboard"));
        }
        // SAFETY: `event` is a live event we own.
        unsafe {
            CGEventSetFlags(event, flags);
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    /// Injects absolute pointer motion.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::EventCreationFailed`] when `CoreGraphics` refuses
    /// the event.
    pub fn pointer_motion(&mut self, motion: &PointerMotion) -> Result<(), InputError> {
        if self.hid_only {
            self.hid_move(motion.x, motion.y)?;
            self.stats.pointer_moves += 1;
            return Ok(());
        }
        let point = self.bounds.to_global(motion.x, motion.y);
        self.post_native_motion(point)?;
        self.stats.pointer_moves += 1;
        Ok(())
    }

    pub fn post_native_motion(&mut self, point: NativePoint) -> Result<(), InputError> {
        let (event_type, button) = motion_event_type(self.held_buttons);
        // Absolute placement has to be exact. Posting a mouse event alone goes
        // through the HID system's pointer acceleration, which lands the
        // cursor near the requested point rather than on it, so the remote
        // pointer drifts away from where the client drew it. Warping sets the
        // position exactly; the event that follows is what makes applications
        // see a move or a drag.
        // SAFETY: warping takes a plain point and has no preconditions.
        unsafe {
            CGWarpMouseCursorPosition(point);
            // Warping breaks the association between the hardware mouse and
            // the cursor until it is restored.
            CGAssociateMouseAndMouseCursorPosition(true);
        }
        // SAFETY: the source is live for the controller's lifetime.
        let event = unsafe { CGEventCreateMouseEvent(self.source, event_type, point, button) };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("pointer motion"));
        }
        // SAFETY: `event` is a live event we own.
        unsafe {
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        self.last_position = point;
        Ok(())
    }

    /// Injects a pointer button transition.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::EventCreationFailed`] when `CoreGraphics` refuses
    /// the event.
    pub fn pointer_button(&mut self, button: &PointerButton) -> Result<(), InputError> {
        if self.hid_only {
            if prepends_absolute_position(button.motion_mode) {
                self.hid_move(button.position.x, button.position.y)?;
            }
            self.hid_button(button.button, button.pressed)?;
            self.stats.pointer_buttons += 1;
            return Ok(());
        }
        let current_position = current_pointer_point().unwrap_or(self.last_position);
        let point = button_position(
            button.motion_mode,
            self.bounds,
            button.position,
            current_position,
        );
        self.post_button(button.button, button.pressed, point)?;
        self.stats.pointer_buttons += 1;
        Ok(())
    }

    fn post_button(&mut self, button: u8, pressed: bool, point: CGPoint) -> Result<(), InputError> {
        let event_type = button_event_type(button, pressed);
        let number = button_number(button);
        // SAFETY: the source is live for the controller's lifetime.
        let event = unsafe { CGEventCreateMouseEvent(self.source, event_type, point, number) };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("pointer button"));
        }
        // SAFETY: `event` is a live event we own.
        unsafe {
            if !matches!(button, 1 | 3) {
                CGEventSetIntegerValueField(
                    event,
                    K_CG_MOUSE_EVENT_BUTTON_NUMBER,
                    i64::from(number),
                );
            }
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        let bit = 1_u32 << u32::from(button.min(31));
        if pressed {
            self.held_buttons |= bit;
        } else {
            self.held_buttons &= !bit;
        }
        self.last_position = point;
        Ok(())
    }

    /// Injects a pointer button transition at an already mapped native point.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::EventCreationFailed`] when `CoreGraphics` refuses
    /// the event.
    pub fn native_pointer_button(
        &mut self,
        button: u8,
        pressed: bool,
        point: NativePoint,
    ) -> Result<(), InputError> {
        self.post_button(button, pressed, point)
    }

    /// Injects a scroll sample at an already mapped native point.
    ///
    /// Region input deltas are fixed-point logical pixels; macOS point-unit
    /// scrolling consumes ordinary points, so the adapter converts by the
    /// shared logical-units denominator before reaching here.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::EventCreationFailed`] when `CoreGraphics` refuses
    /// the event.
    pub fn native_pointer_scroll(
        &mut self,
        point: NativePoint,
        delta_x: f64,
        delta_y: f64,
        unit: arcen_input::ScrollUnit,
        phase: arcen_input::ScrollPhase,
    ) -> Result<(), InputError> {
        self.post_native_motion(point)?;
        let (cg_unit, vertical, horizontal) = match unit {
            arcen_input::ScrollUnit::Line => (
                K_CG_SCROLL_EVENT_UNIT_LINE,
                finite_delta(delta_y),
                finite_delta(delta_x),
            ),
            arcen_input::ScrollUnit::Point => {
                let (x, y) = self.scroll_remainder;
                let x = x + if delta_x.is_finite() { delta_x } else { 0.0 };
                let y = y + if delta_y.is_finite() { delta_y } else { 0.0 };
                let (vertical, horizontal) = (finite_delta(y), finite_delta(x));
                self.scroll_remainder = (x - f64::from(horizontal), y - f64::from(vertical));
                if matches!(
                    phase,
                    arcen_input::ScrollPhase::Ended | arcen_input::ScrollPhase::Cancelled
                ) {
                    self.scroll_remainder = (0.0, 0.0);
                }
                (K_CG_SCROLL_EVENT_UNIT_PIXEL, vertical, horizontal)
            }
        };
        let event = unsafe {
            CGEventCreateScrollWheelEvent2(self.source, cg_unit, 2, vertical, horizontal, 0)
        };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("scroll"));
        }
        unsafe {
            if cg_unit == K_CG_SCROLL_EVENT_UNIT_PIXEL {
                let phase = match phase {
                    arcen_input::ScrollPhase::None => 0,
                    arcen_input::ScrollPhase::Began => K_CG_SCROLL_PHASE_BEGAN,
                    arcen_input::ScrollPhase::Changed => K_CG_SCROLL_PHASE_CHANGED,
                    arcen_input::ScrollPhase::Ended => K_CG_SCROLL_PHASE_ENDED,
                    arcen_input::ScrollPhase::Cancelled => K_CG_SCROLL_PHASE_CANCELLED,
                };
                CGEventSetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 1);
                if phase != 0 {
                    CGEventSetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_SCROLL_PHASE, phase);
                }
            }
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        self.stats.scroll_events += 1;
        Ok(())
    }

    /// Injects a pen sample with its tablet surface intact.
    ///
    /// This is Basic Tablet termination: the Deck's Wacom driver has already
    /// read the pen, so this host turns a finished sample into the mouse
    /// events macOS uses to carry tablet data. Edges — proximity, tip, barrel
    /// buttons — come from the shared planner so that a tool leaving the
    /// tablet cannot strand a press, and the annotations ride on every sample
    /// so pressure and tilt stay live while drawing.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::EventCreationFailed`] when `CoreGraphics` refuses
    /// an event.
    pub fn pen_event(&mut self, event: &PenEventMsg) -> Result<(), InputError> {
        if self.hid_only {
            return self.hid_pen_event(event);
        }
        let point = self.bounds.to_global(event.x, event.y);
        let (edges, next) = pen::plan(self.pen, event);
        let annotations = pen::tablet_point(event);

        self.post_pen_edges(&edges, point, annotations)?;

        // A sample away from the tablet has nowhere to land: posting motion
        // for it would move the pointer to wherever the pen last hovered.
        if next.in_proximity {
            self.post_pen_motion(point, annotations)?;
        }
        self.pen = next;
        self.stats.pen_samples += 1;
        Ok(())
    }

    /// Injects a pen sample at an already mapped native point.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::EventCreationFailed`] when `CoreGraphics` refuses
    /// an event.
    pub fn native_pen_event(
        &mut self,
        event: &PenEventMsg,
        point: NativePoint,
    ) -> Result<(), InputError> {
        if self.hid_only {
            return self.hid_pen_event(event);
        }
        let (edges, next) = pen::plan(self.pen, event);
        let annotations = pen::tablet_point(event);
        self.post_pen_edges(&edges, point, annotations)?;
        if next.in_proximity {
            self.post_pen_motion(point, annotations)?;
        }
        self.pen = next;
        self.stats.pen_samples += 1;
        Ok(())
    }

    /// A pen as a plain pointer: the virtual device carries position and
    /// buttons, not pressure or tilt, which is all the login window needs.
    fn hid_pen_event(&mut self, event: &PenEventMsg) -> Result<(), InputError> {
        let (edges, next) = pen::plan(self.pen, event);
        if next.in_proximity {
            self.hid_move(event.x, event.y)?;
        }
        for edge in edges {
            match edge {
                PenEdge::ToolIn(_) | PenEdge::ToolOut(_) => {}
                PenEdge::TipDown => self.hid_button(1, true)?,
                PenEdge::TipUp => self.hid_button(1, false)?,
                PenEdge::Barrel { index, pressed } => {
                    if let Some(button) = barrel_button(index) {
                        self.hid_button(button, pressed)?;
                    }
                }
            }
        }
        self.pen = next;
        self.stats.pen_samples += 1;
        Ok(())
    }

    /// Scrolls through the virtual pointer's wheel, in whole detents.
    fn hid_scroll(&mut self, scroll: &PointerScroll) -> Result<(), InputError> {
        if prepends_absolute_position(scroll.motion_mode) {
            self.hid_move(scroll.position.x, scroll.position.y)?;
        }
        let finite = |value: f64| if value.is_finite() { value } else { 0.0 };
        let (x, y) = match scroll.unit {
            arcen_input::ScrollUnit::Line => (finite(scroll.delta_x), finite(scroll.delta_y)),
            arcen_input::ScrollUnit::Point => {
                let (x, y) = self.scroll_remainder;
                (
                    (x + finite(scroll.delta_x)) / POINTS_PER_DETENT,
                    (y + finite(scroll.delta_y)) / POINTS_PER_DETENT,
                )
            }
        };
        let detents = |value: f64| -> i8 {
            // Clamped inside `i8` first, so the cast is exact.
            #[allow(clippy::cast_possible_truncation)]
            {
                value.trunc().clamp(-127.0, 127.0) as i8
            }
        };
        let (wheel, pan) = (detents(y), detents(x));
        if scroll.unit == arcen_input::ScrollUnit::Point {
            self.scroll_remainder = if matches!(
                scroll.phase,
                arcen_input::ScrollPhase::Ended | arcen_input::ScrollPhase::Cancelled
            ) {
                (0.0, 0.0)
            } else {
                (
                    (x - f64::from(pan)) * POINTS_PER_DETENT,
                    (y - f64::from(wheel)) * POINTS_PER_DETENT,
                )
            };
        }
        // The wire's horizontal delta follows `CoreGraphics`, where positive
        // scrolls left; HID's pan is positive to the right.
        let result = self.devices()?.scroll(wheel, pan.saturating_neg());
        result.map_err(|error| self.devices_failed(&error))?;
        self.stats.scroll_events += 1;
        Ok(())
    }

    /// Posts one planned set of pen edges.
    fn post_pen_edges(
        &mut self,
        edges: &[PenEdge],
        point: CGPoint,
        annotations: pen::TabletPoint,
    ) -> Result<(), InputError> {
        for edge in edges {
            match *edge {
                PenEdge::ToolIn(tool) => {
                    self.post_pen_proximity(point, pen_pointer_type(tool), true)?;
                }
                PenEdge::ToolOut(tool) => {
                    self.post_pen_proximity(point, pen_pointer_type(tool), false)?;
                }
                PenEdge::TipDown => self.post_pen_button(1, true, point, annotations)?,
                PenEdge::TipUp => self.post_pen_button(1, false, point, annotations)?,
                PenEdge::Barrel { index, pressed } => {
                    // Barrel one is the right button and barrel two the middle
                    // one, which is what a Wacom driver does locally. Further
                    // buttons have no agreed meaning, so they are dropped
                    // rather than assigned an invented one.
                    if let Some(button) = barrel_button(index) {
                        self.post_pen_button(button, pressed, point, annotations)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Takes the pen off the tablet, releasing whatever it was holding.
    ///
    /// Planned through the shared planner rather than written out here: asking
    /// it for a sample out of proximity produces the releases and the
    /// `ToolOut` in the order a physical tablet reports them, which is the
    /// same code path a real lift uses.
    ///
    /// Without this, an abrupt disconnect left the tool logically in
    /// proximity. The ordinary button releases that `release_all` does post
    /// carry no tablet annotations, so an application that had seen a pen
    /// enter never saw it leave and went on believing the tablet was live.
    fn release_pen(&mut self) -> Result<(), InputError> {
        if !self.pen.in_proximity {
            return Ok(());
        }
        let (edges, next) = arcen_input::plan_pen_edges(self.pen, self.pen.tool, false, false, 0);
        let point = self.last_position;
        let annotations = pen::tablet_point_away(pen_pointer_type(self.pen.tool));
        self.post_pen_edges(&edges, point, annotations)?;
        self.pen = next;
        Ok(())
    }

    fn post_pen_motion(
        &mut self,
        point: CGPoint,
        annotations: pen::TabletPoint,
    ) -> Result<(), InputError> {
        let (event_type, button) = motion_event_type(self.held_buttons);
        // Absolute placement, for the same reason as the pointer: without the
        // warp the HID system's acceleration lands the tip near the requested
        // point rather than on it, and a drawing application records the drift.
        // SAFETY: warping takes a plain point and has no preconditions.
        unsafe {
            CGWarpMouseCursorPosition(point);
            CGAssociateMouseAndMouseCursorPosition(true);
        }
        // SAFETY: the source is live for the controller's lifetime.
        let event = unsafe { CGEventCreateMouseEvent(self.source, event_type, point, button) };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("pen motion"));
        }
        // SAFETY: `event` is a live event we own and release below.
        unsafe {
            annotate_tablet_point(event, annotations);
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        self.last_position = point;
        Ok(())
    }

    fn post_pen_button(
        &mut self,
        button: u8,
        pressed: bool,
        point: CGPoint,
        annotations: pen::TabletPoint,
    ) -> Result<(), InputError> {
        let event_type = button_event_type(button, pressed);
        let number = button_number(button);
        // SAFETY: the source is live for the controller's lifetime.
        let event = unsafe { CGEventCreateMouseEvent(self.source, event_type, point, number) };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("pen button"));
        }
        // SAFETY: `event` is a live event we own and release below.
        unsafe {
            if !matches!(button, 1 | 3) {
                CGEventSetIntegerValueField(
                    event,
                    K_CG_MOUSE_EVENT_BUTTON_NUMBER,
                    i64::from(number),
                );
            }
            annotate_tablet_point(event, annotations);
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        let bit = 1_u32 << u32::from(button.min(31));
        if pressed {
            self.held_buttons |= bit;
        } else {
            self.held_buttons &= !bit;
        }
        self.last_position = point;
        Ok(())
    }

    fn post_pen_proximity(
        &mut self,
        point: CGPoint,
        pointer_type: i64,
        entering: bool,
    ) -> Result<(), InputError> {
        // SAFETY: the source is live for the controller's lifetime.
        let event = unsafe {
            CGEventCreateMouseEvent(
                self.source,
                K_CG_EVENT_MOUSE_MOVED,
                point,
                K_CG_MOUSE_BUTTON_LEFT,
            )
        };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("pen proximity"));
        }
        // SAFETY: `event` is a live event we own and release below.
        unsafe {
            CGEventSetIntegerValueField(
                event,
                pen::K_CG_MOUSE_EVENT_SUBTYPE,
                pen::K_CG_MOUSE_SUBTYPE_TABLET_PROXIMITY,
            );
            CGEventSetIntegerValueField(
                event,
                pen::K_CG_TABLET_PROXIMITY_EVENT_VENDOR_ID,
                pen::ARCEN_TABLET_VENDOR_ID,
            );
            CGEventSetIntegerValueField(
                event,
                pen::K_CG_TABLET_PROXIMITY_EVENT_DEVICE_ID,
                pen::ARCEN_TABLET_DEVICE_ID,
            );
            CGEventSetIntegerValueField(
                event,
                pen::K_CG_TABLET_PROXIMITY_EVENT_SYSTEM_TABLET_ID,
                pen::ARCEN_TABLET_DEVICE_ID,
            );
            CGEventSetIntegerValueField(
                event,
                pen::K_CG_TABLET_PROXIMITY_EVENT_POINTER_TYPE,
                pointer_type,
            );
            CGEventSetIntegerValueField(
                event,
                pen::K_CG_TABLET_PROXIMITY_EVENT_ENTER_PROXIMITY,
                i64::from(entering),
            );
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        self.stats.pen_proximity_edges += 1;
        Ok(())
    }

    /// Injects a scroll event.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::EventCreationFailed`] when `CoreGraphics` refuses
    /// the event.
    pub fn pointer_scroll(&mut self, scroll: &PointerScroll) -> Result<(), InputError> {
        if self.hid_only {
            return self.hid_scroll(scroll);
        }
        if let Some(point) = scroll_position(scroll.motion_mode, self.bounds, scroll.position) {
            self.post_native_motion(point)?;
        }
        let (unit, vertical, horizontal) = match scroll.unit {
            // Non-finite deltas would become an unpredictable jump after the
            // cast.
            arcen_input::ScrollUnit::Line => (
                K_CG_SCROLL_EVENT_UNIT_LINE,
                finite_delta(scroll.delta_y),
                finite_delta(scroll.delta_x),
            ),
            // Whole pixels, with the fraction carried to the next event so a
            // slow two-finger drag still moves rather than rounding to zero.
            arcen_input::ScrollUnit::Point => {
                let (x, y) = self.scroll_remainder;
                let x = x + if scroll.delta_x.is_finite() {
                    scroll.delta_x
                } else {
                    0.0
                };
                let y = y + if scroll.delta_y.is_finite() {
                    scroll.delta_y
                } else {
                    0.0
                };
                let (vertical, horizontal) = (finite_delta(y), finite_delta(x));
                self.scroll_remainder = (x - f64::from(horizontal), y - f64::from(vertical));
                if matches!(
                    scroll.phase,
                    arcen_input::ScrollPhase::Ended | arcen_input::ScrollPhase::Cancelled
                ) {
                    self.scroll_remainder = (0.0, 0.0);
                }
                (K_CG_SCROLL_EVENT_UNIT_PIXEL, vertical, horizontal)
            }
        };
        // SAFETY: the source is live for the controller's lifetime.
        let event = unsafe {
            CGEventCreateScrollWheelEvent2(self.source, unit, 2, vertical, horizontal, 0)
        };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("scroll"));
        }
        if unit == K_CG_SCROLL_EVENT_UNIT_PIXEL {
            let phase = match scroll.phase {
                arcen_input::ScrollPhase::None => 0,
                arcen_input::ScrollPhase::Began => K_CG_SCROLL_PHASE_BEGAN,
                arcen_input::ScrollPhase::Changed => K_CG_SCROLL_PHASE_CHANGED,
                arcen_input::ScrollPhase::Ended => K_CG_SCROLL_PHASE_ENDED,
                arcen_input::ScrollPhase::Cancelled => K_CG_SCROLL_PHASE_CANCELLED,
            };
            // SAFETY: `event` is a live scroll event we own; these are
            // documented `CGEventField`s for scroll-wheel events.
            unsafe {
                CGEventSetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 1);
                if phase != 0 {
                    CGEventSetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_SCROLL_PHASE, phase);
                }
            }
        }
        // SAFETY: `event` is a live event we own.
        unsafe {
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        self.stats.scroll_events += 1;
        Ok(())
    }

    /// Best-available gesture injection for macOS today.
    ///
    /// The reference stack uses CGEvent for pointer/scroll and reserves
    /// IOHIDUserDevice for HID devices. AppKit exposes no public CGEvent
    /// constructor for pinch/rotate/swipe, so magnify/smart-zoom degrade to
    /// the conventional Control-scroll zoom gesture and swipe degrades to the
    /// platform shortcut most apps/Spaces already consume.
    pub fn gesture_magnify(&mut self, message: &GestureMagnifyMsg) -> Result<(), InputError> {
        message
            .validate()
            .map_err(|_| InputError::InvalidGesture("magnify"))?;
        self.post_control_scroll((message.scale_delta * 240.0).round() as i32)
    }

    /// Rotation has no public CGEvent equivalent, and a remote session must
    /// not end because a Deck sent a gesture this host can only decline.
    pub fn gesture_rotate(&mut self, message: &GestureRotateMsg) -> Result<(), InputError> {
        message
            .validate()
            .map_err(|_| InputError::InvalidGesture("rotate"))?;
        tracing::debug!(
            target: arcen_telemetry::names::target::HID,
            degrees = message.degrees_delta,
            "rotate gesture declined: no public macOS injection"
        );
        Ok(())
    }

    pub fn gesture_smart_zoom(&mut self, message: &GestureSmartZoomMsg) -> Result<(), InputError> {
        message
            .validate()
            .map_err(|_| InputError::InvalidGesture("smart_zoom"))?;
        self.post_control_scroll(120)
    }

    pub fn gesture_swipe(&mut self, message: &GestureSwipeMsg) -> Result<(), InputError> {
        message
            .validate()
            .map_err(|_| InputError::InvalidGesture("swipe"))?;
        let key = match message.direction {
            SwipeDirectionMsg::Left => 0x7B,
            SwipeDirectionMsg::Right => 0x7C,
            SwipeDirectionMsg::Up => 0x7E,
            SwipeDirectionMsg::Down => 0x7D,
        };
        self.post_shortcut(key, K_CG_EVENT_FLAG_MASK_CONTROL)
    }

    fn post_control_scroll(&mut self, vertical: i32) -> Result<(), InputError> {
        if vertical == 0 {
            return Ok(());
        }
        let event = unsafe {
            CGEventCreateScrollWheelEvent2(
                self.source,
                K_CG_SCROLL_EVENT_UNIT_LINE,
                1,
                vertical,
                0,
                0,
            )
        };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("gesture_zoom_scroll"));
        }
        unsafe {
            CGEventSetFlags(event, K_CG_EVENT_FLAG_MASK_CONTROL);
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        self.stats.scroll_events += 1;
        Ok(())
    }

    fn post_shortcut(&mut self, key: u16, flags: u64) -> Result<(), InputError> {
        for pressed in [true, false] {
            let event = unsafe { CGEventCreateKeyboardEvent(self.source, key, pressed) };
            if event.is_null() {
                return Err(InputError::EventCreationFailed("gesture_shortcut"));
            }
            unsafe {
                CGEventSetFlags(event, flags);
                CGEventPost(K_CG_HID_EVENT_TAP, event);
                CFRelease(event.cast_const());
            }
        }
        Ok(())
    }

    /// Releases every key and pointer button this controller is holding.
    ///
    /// This is what keeps a dropped session from leaving the physical desktop
    /// with a modifier or mouse button stuck down. It is safe to call more than
    /// once and reports the first failure while still attempting the rest, so
    /// one refused event cannot strand the others.
    ///
    /// # Errors
    ///
    /// Returns the first [`InputError`] encountered, after attempting every
    /// release.
    pub fn release_all(&mut self) -> Result<(), InputError> {
        let mut first_error = None;

        if let Some(keyboard) = self.virtual_keyboard.as_mut() {
            if keyboard.release_all().is_err() {
                self.virtual_keyboard = None;
            }
        }
        if self.hid_only {
            // The child releases everything itself when its input closes, so
            // a failure here has already been made good.
            self.held_buttons = 0;
            self.pen = PenToolState::default();
            self.stats.releases += 1;
            return Ok(());
        }

        // The pen first. Its tip and barrel are held buttons too, and a pen
        // release posts them as tablet events — tip or barrel up, then the
        // tool leaving proximity, in the order an application expects. Plain
        // mouse-ups for those bits first sent every one twice: once without
        // the tablet subtype while the tool was still in proximity, then again
        // unmatched. `release_pen` clears the bits it releases, so the loop
        // below only sees buttons the pen did not own.
        if let Err(error) = self.release_pen() {
            first_error.get_or_insert(error);
        }

        let buttons: Vec<u8> = (0..32)
            .filter(|bit| self.held_buttons & (1 << bit) != 0)
            .filter_map(|bit| u8::try_from(bit).ok())
            .collect();
        let point = self.last_position;
        for button in buttons {
            if let Err(error) = self.post_button(button, false, point) {
                first_error.get_or_insert(error);
            }
        }
        self.held_buttons = 0;

        let keys: Vec<u16> = self.held_keys.iter().copied().collect();
        for code in keys {
            if let Err(error) = self.post_key(code, false, 0) {
                first_error.get_or_insert(error);
            }
        }
        self.held_keys.clear();

        // Modifiers are released unconditionally. A modifier can be latched by
        // a key press this controller never saw the release for, and a stuck
        // Command key makes the machine unusable for whoever is sitting at it.
        for code in keymap::MODIFIER_CODES {
            if let Err(error) = self.post_key(code, false, 0) {
                first_error.get_or_insert(error);
            }
        }

        self.stats.releases += 1;
        first_error.map_or(Ok(()), Err)
    }
}

/// The rectangle spanning every active display, or `fallback` when the
/// displays cannot be read.
fn whole_desktop(fallback: DesktopBounds) -> DesktopBounds {
    crate::displays::desktop_point_bounds()
        .map(|(x, y, width, height)| DesktopBounds::new(x, y, width, height))
        .filter(|desktop| desktop.is_usable())
        .unwrap_or(fallback)
}

/// Reads the current pointer position in global display coordinates.
///
/// Returns `None` when `CoreGraphics` will not report a location.
#[must_use]
pub fn current_pointer_position() -> Option<(f64, f64)> {
    current_pointer_point().map(|point| (point.x, point.y))
}

fn current_pointer_point() -> Option<CGPoint> {
    // SAFETY: a null source is the documented way to snapshot current state.
    let event = unsafe { CGEventCreate(std::ptr::null_mut()) };
    if event.is_null() {
        return None;
    }
    // SAFETY: `event` is a live event we own.
    let point = unsafe {
        let point = CGEventGetLocation(event);
        CFRelease(event.cast_const());
        point
    };
    Some(point)
}

/// What an input probe observed.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputProbeReport {
    /// Whether an event source could be created at all.
    pub source_available: bool,
    /// Pointer position before the probe moved anything.
    pub original_position: Option<(f64, f64)>,
    /// Position the probe asked for.
    pub requested_position: Option<(f64, f64)>,
    /// Position observed after the move.
    pub observed_position: Option<(f64, f64)>,
    /// Whether the observed position matched the request within a pixel.
    pub motion_took_effect: bool,
    /// Whether the original position was restored afterwards.
    pub restored: bool,
    /// Whether this run proves usable input injection.
    pub usable: bool,
    /// Why the run is not usable, when it is not.
    pub refusal: Option<String>,
}

/// Proves pointer injection reaches the window server.
///
/// This deliberately moves only the pointer and puts it back. It never
/// synthesises key presses, because a probe that types into whatever window
/// happens to be focused is not something anyone should run on a live desktop.
///
/// # Errors
///
/// Returns [`InputError`] when no event source or desktop rectangle is
/// available.
/// Moves the pointer as a real mouse would, for diagnostics only.
///
/// Warps *and* posts the move event, which is what the session path does. The
/// distinction is not academic: a warp alone moves the pointer without telling
/// anybody, so no application processes a mouse-moved event, so no application
/// calls `NSCursor::set`, so the cursor shape never changes. A probe built on
/// warping alone concludes the shape cannot be read, and would be measuring
/// its own omission.
pub fn move_pointer_for_probe(position: (f64, f64)) {
    let point = NativePoint {
        x: position.0,
        y: position.1,
    };
    // SAFETY: warping takes a plain point and has no preconditions.
    unsafe {
        CGWarpMouseCursorPosition(point);
        CGAssociateMouseAndMouseCursorPosition(true);
    }
    // SAFETY: a null event source is valid and means "no specific source".
    let event = unsafe {
        CGEventCreateMouseEvent(
            std::ptr::null_mut(),
            K_CG_EVENT_MOUSE_MOVED,
            point,
            K_CG_MOUSE_BUTTON_LEFT,
        )
    };
    if event.is_null() {
        return;
    }
    // SAFETY: `event` is non-null and owned here; posting then releasing it is
    // the documented lifecycle.
    unsafe {
        CGEventPost(K_CG_HID_EVENT_TAP, event);
        release_core_foundation(event.cast_const().cast());
    }
}

/// Places the pointer at known points and reads back where it landed.
///
/// Proves that this host can move the pointer at all, and that it lands where
/// it was asked to rather than near it, which is the difference between a
/// remote pointer that tracks the hand and one that drifts.
///
/// Restores the pointer to where it started, because a probe that leaves the
/// pointer somewhere else is a probe somebody has to clean up after.
///
/// # Errors
///
/// Returns [`InputError`] when the event source cannot be created or
/// `CoreGraphics` refuses an event.
pub fn probe(bounds: DesktopBounds) -> Result<InputProbeReport, InputError> {
    let original = current_pointer_position();
    let mut controller = match InputController::new(bounds) {
        Ok(controller) => controller,
        Err(InputError::SourceUnavailable) => {
            return Ok(InputProbeReport {
                source_available: false,
                original_position: original,
                requested_position: None,
                observed_position: None,
                motion_took_effect: false,
                restored: false,
                usable: false,
                refusal: Some(
                    "CoreGraphics refused an event source; Accessibility consent is missing"
                        .to_owned(),
                ),
            });
        }
        Err(error) => return Err(error),
    };

    // A point a quarter into the desktop: safely inside any real display and
    // far enough from the edges that clamping cannot mask a failed move.
    let target = bounds.to_global(0.25, 0.25);
    let motion = PointerMotion {
        x: 0.25,
        y: 0.25,
        server_x: None,
        server_y: None,
        metadata: arcen_input::LowLatencyMetadata::default(),
    };
    controller.pointer_motion(&motion)?;
    // `CGEventPost` is asynchronous: the window server applies the move on its
    // own turn, so read back with a bounded poll rather than once immediately.
    let observed = settled_position(target);
    let took_effect =
        observed.is_some_and(|(x, y)| (x - target.x).abs() <= 1.0 && (y - target.y).abs() <= 1.0);

    let mut restored = false;
    if let Some((x, y)) = original {
        if bounds.is_usable() {
            let back = PointerMotion {
                x: ((x - bounds.origin_x) / bounds.width).clamp(0.0, 1.0),
                y: ((y - bounds.origin_y) / bounds.height).clamp(0.0, 1.0),
                server_x: None,
                server_y: None,
                metadata: arcen_input::LowLatencyMetadata::default(),
            };
            restored = controller.pointer_motion(&back).is_ok();
        }
    }

    let refusal = if took_effect {
        None
    } else {
        Some("pointer did not move to the requested position".to_owned())
    };
    Ok(InputProbeReport {
        source_available: true,
        original_position: original,
        requested_position: Some((target.x, target.y)),
        observed_position: observed,
        motion_took_effect: took_effect,
        restored,
        usable: refusal.is_none(),
        refusal,
    })
}

/// Reports whether the system currently sees `key` as held down.
///
/// This reads the HID system's own key state, which is what makes a delivery
/// test possible without a focused window to type into.
#[must_use]
pub fn key_is_down(virtual_key: u16) -> bool {
    // SAFETY: a nullary system query over a plain key code.
    unsafe { CGEventSourceKeyState(K_CG_EVENT_SOURCE_STATE_HID_SYSTEM, virtual_key) }
}

/// What a keyboard probe observed.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyboardProbeReport {
    /// Whether an event source could be created.
    pub source_available: bool,
    /// The virtual key code used for the test.
    pub probe_key: u16,
    /// Whether the key was already held before the probe ran.
    pub key_down_before: bool,
    /// Whether the system saw the key go down after injection.
    pub press_observed: bool,
    /// Whether the system reports the key up after the release.
    ///
    /// Only meaningful when `press_observed` is true: if the press never
    /// landed, the key was already up and this is trivially true.
    pub release_observed: bool,
    /// Whether this run proves keyboard injection is delivered.
    pub usable: bool,
    /// Why the run is not usable, when it is not.
    pub refusal: Option<String>,
}

/// Proves synthetic key events are delivered to the system.
///
/// F13 is used deliberately. It has no default action on a Mac and produces no
/// text, so the probe cannot type into whatever window happens to be focused
/// or trigger a system shortcut. Delivery is confirmed by reading the HID
/// system's key state rather than by looking for characters somewhere.
///
/// The key is released even when the check fails, because leaving a key down
/// is exactly the condition [`InputController::release_all`] exists to prevent.
///
/// # Errors
///
/// Returns [`InputError`] when `CoreGraphics` refuses to create the events.
pub fn probe_keyboard() -> Result<KeyboardProbeReport, InputError> {
    /// F13: present on the layout, no default action, emits no text.
    const PROBE_KEY: u16 = 0x69;

    // SAFETY: creating an event source has no preconditions.
    let source = unsafe { CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_HID_SYSTEM) };
    if source.is_null() {
        return Ok(KeyboardProbeReport {
            source_available: false,
            probe_key: PROBE_KEY,
            key_down_before: false,
            press_observed: false,
            release_observed: false,
            usable: false,
            refusal: Some(
                "CoreGraphics refused an event source; Accessibility consent is missing".to_owned(),
            ),
        });
    }

    let key_down_before = key_is_down(PROBE_KEY);
    let post = |pressed: bool| -> Result<(), InputError> {
        // SAFETY: the source is live for this function's scope.
        let event = unsafe { CGEventCreateKeyboardEvent(source, PROBE_KEY, pressed) };
        if event.is_null() {
            return Err(InputError::EventCreationFailed("keyboard"));
        }
        // SAFETY: `event` is a live event we own.
        unsafe {
            CGEventSetFlags(event, 0);
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    };

    let press_result = post(true);
    let press_observed = press_result.is_ok() && settled_key_state(PROBE_KEY, true);

    // Always attempt the release, even if the press failed or was not seen.
    let release_result = post(false);
    let release_observed = release_result.is_ok() && settled_key_state(PROBE_KEY, false);

    // SAFETY: we own the only reference to the source.
    unsafe { CFRelease(source.cast_const()) };

    press_result?;
    release_result?;

    let refusal = if key_down_before {
        Some(
            "the probe key was already held before the test, so the result is meaningless"
                .to_owned(),
        )
    } else if !press_observed {
        Some("the system did not observe the injected key press".to_owned())
    } else if !release_observed {
        Some("the injected key was not released".to_owned())
    } else {
        None
    };

    Ok(KeyboardProbeReport {
        source_available: true,
        probe_key: PROBE_KEY,
        key_down_before,
        press_observed,
        release_observed,
        usable: refusal.is_none(),
        refusal,
    })
}

/// Polls the system key state until it reaches `expected` or the budget ends.
fn settled_key_state(virtual_key: u16, expected: bool) -> bool {
    for _ in 0..50 {
        if key_is_down(virtual_key) == expected {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// Polls the pointer position until it reaches `target` or the budget expires.
///
/// Returns the last observed position either way, so a failure reports where
/// the pointer actually ended up rather than nothing at all.
fn settled_position(target: CGPoint) -> Option<(f64, f64)> {
    let mut last = current_pointer_position();
    for _ in 0..50 {
        if let Some((x, y)) = last {
            if (x - target.x).abs() <= 1.0 && (y - target.y).abs() <= 1.0 {
                return last;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        last = current_pointer_position();
    }
    last
}

/// Converts a shared scroll delta into a whole-pixel `CoreGraphics` delta.
fn finite_delta(value: f64) -> i32 {
    if value.is_finite() {
        // Truncation is intentional and safe: the value is clamped well inside
        // `i32` first, and `CoreGraphics` takes whole pixels.
        #[allow(clippy::cast_possible_truncation)]
        {
            value.clamp(-100_000.0, 100_000.0) as i32
        }
    } else {
        0
    }
}

const fn prepends_absolute_position(mode: PointerMotionMode) -> bool {
    matches!(mode, PointerMotionMode::Absolute)
}

fn button_position(
    mode: PointerMotionMode,
    bounds: DesktopBounds,
    position: PointerMotion,
    current_position: CGPoint,
) -> CGPoint {
    if prepends_absolute_position(mode) {
        bounds.to_global(position.x, position.y)
    } else {
        current_position
    }
}

fn scroll_position(
    mode: PointerMotionMode,
    bounds: DesktopBounds,
    position: PointerMotion,
) -> Option<CGPoint> {
    prepends_absolute_position(mode).then(|| bounds.to_global(position.x, position.y))
}

impl Drop for InputController {
    fn drop(&mut self) {
        // Never leave the desktop with something held down.
        let _ = self.release_all();
        if !self.source.is_null() {
            // SAFETY: we own the only reference to the source.
            unsafe { CFRelease(self.source.cast_const()) };
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn ending_a_session_takes_the_pen_off_the_tablet() {
        if !crate::desktop_tests_allowed() {
            return;
        }
        // release_all released keys and buttons but left the tool logically in
        // proximity, and the button releases it did post carried no tablet
        // annotations. An application that had seen a pen enter never saw it
        // leave, so an abrupt disconnect left it believing the tablet was
        // still live.
        let Ok(mut controller) = InputController::new(DesktopBounds::new(0.0, 0.0, 1920.0, 1080.0))
        else {
            // No event source on this machine; nothing to assert about.
            return;
        };
        let sample = arcen_protocol::messages::PenEventMsg {
            msg_type: "pen_event".to_owned(),
            x: 0.5,
            y: 0.5,
            pressure: 0.8,
            tilt_x_degrees: 0.0,
            tilt_y_degrees: 0.0,
            rotation_degrees: 0.0,
            buttons: 0,
            in_proximity: true,
            touching: true,
            tool: arcen_protocol::messages::PenToolMsg::Tip,
            sequence: 1,
            timestamp_ns: 0,
            coalescable: false,
            server_x: 0,
            server_y: 0,
        };
        controller.pen_event(&sample).expect("inject");
        assert!(controller.pen.in_proximity, "the pen must be on the tablet");
        assert!(controller.pen.touching, "the tip must be down");

        controller.release_all().expect("release");
        assert!(
            !controller.pen.in_proximity,
            "the tool must have left proximity",
        );
        assert!(!controller.pen.touching, "the tip must have been released");
    }

    #[test]
    fn normalized_coordinates_map_onto_the_desktop_rectangle() {
        let bounds = DesktopBounds::new(100.0, 50.0, 1920.0, 1080.0);
        let top_left = bounds.to_global(0.0, 0.0);
        assert!((top_left.x - 100.0).abs() < f64::EPSILON);
        assert!((top_left.y - 50.0).abs() < f64::EPSILON);
        let bottom_right = bounds.to_global(1.0, 1.0);
        assert!(
            (bottom_right.x - 2019.0).abs() < f64::EPSILON,
            "the far edge stays on this display, not the next one"
        );
        assert!((bottom_right.y - 1129.0).abs() < f64::EPSILON);
    }

    #[test]
    fn coordinates_outside_the_region_clamp_to_its_edge() {
        // A pointer leaving the shared region must stop at the edge rather
        // than jump to an unrelated part of the desktop.
        let bounds = DesktopBounds::new(0.0, 0.0, 800.0, 600.0);
        let over = bounds.to_global(5.0, -3.0);
        assert!((over.x - 799.0).abs() < f64::EPSILON);
        assert!((over.y - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_bounds_are_refused() {
        assert!(!DesktopBounds::new(0.0, 0.0, 0.0, 1080.0).is_usable());
        assert!(!DesktopBounds::new(0.0, 0.0, 1920.0, 0.0).is_usable());
        assert!(DesktopBounds::new(0.0, 0.0, 1920.0, 1080.0).is_usable());
    }

    #[test]
    fn modifier_masks_become_core_graphics_flags() {
        let flags = modifier_flags(keymap::MOD_SHIFT | keymap::MOD_META, None);
        assert_eq!(
            flags & K_CG_EVENT_FLAG_MASK_SHIFT,
            K_CG_EVENT_FLAG_MASK_SHIFT
        );
        assert_eq!(
            flags & K_CG_EVENT_FLAG_MASK_COMMAND,
            K_CG_EVENT_FLAG_MASK_COMMAND
        );
        assert_eq!(flags & K_CG_EVENT_FLAG_MASK_CONTROL, 0);
    }

    #[test]
    fn caps_lock_is_reported_only_when_the_client_knows_it() {
        assert_eq!(
            modifier_flags(0, None) & K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT,
            0
        );
        assert_eq!(
            modifier_flags(0, Some(false)) & K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT,
            0
        );
        assert_eq!(
            modifier_flags(0, Some(true)) & K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT,
            K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT
        );
    }

    #[test]
    fn physical_buttons_map_to_core_graphics_numbers() {
        assert_eq!(button_number(1), K_CG_MOUSE_BUTTON_LEFT);
        assert_eq!(button_number(2), K_CG_MOUSE_BUTTON_CENTER);
        assert_eq!(button_number(3), K_CG_MOUSE_BUTTON_RIGHT);
        // Side buttons keep their own number rather than colliding with one
        // of the three physical buttons.
        assert_eq!(button_number(4), 4);
    }

    #[test]
    fn button_transitions_pick_the_right_event_type() {
        assert_eq!(button_event_type(1, true), K_CG_EVENT_LEFT_MOUSE_DOWN);
        assert_eq!(button_event_type(1, false), K_CG_EVENT_LEFT_MOUSE_UP);
        assert_eq!(button_event_type(3, true), K_CG_EVENT_RIGHT_MOUSE_DOWN);
        assert_eq!(button_event_type(4, true), K_CG_EVENT_OTHER_MOUSE_DOWN);
        assert_eq!(button_event_type(4, false), K_CG_EVENT_OTHER_MOUSE_UP);
    }

    #[test]
    fn motion_becomes_a_drag_while_a_button_is_held() {
        // Posting a plain move while a button is down breaks selection and
        // window dragging on macOS.
        assert_eq!(motion_event_type(0).0, K_CG_EVENT_MOUSE_MOVED);
        assert_eq!(motion_event_type(1 << 1).0, K_CG_EVENT_LEFT_MOUSE_DRAGGED);
        assert_eq!(motion_event_type(1 << 3).0, K_CG_EVENT_RIGHT_MOUSE_DRAGGED);
        assert_eq!(motion_event_type(1 << 4).0, K_CG_EVENT_OTHER_MOUSE_DRAGGED);
    }

    #[test]
    fn non_finite_scroll_deltas_become_zero_rather_than_a_jump() {
        assert_eq!(finite_delta(f64::NAN), 0);
        assert_eq!(finite_delta(f64::INFINITY), 0);
        assert_eq!(finite_delta(f64::NEG_INFINITY), 0);
        assert_eq!(finite_delta(12.9), 12);
        assert_eq!(finite_delta(-7.2), -7);
    }

    #[test]
    fn absurd_scroll_deltas_are_bounded() {
        assert_eq!(finite_delta(1e18), 100_000);
        assert_eq!(finite_delta(-1e18), -100_000);
    }

    #[test]
    fn relative_button_edges_do_not_reinterpret_deltas_as_absolute_points() {
        let bounds = DesktopBounds::new(100.0, 50.0, 800.0, 600.0);
        let last_position = NativePoint { x: 640.0, y: 360.0 };
        let relative_delta = PointerMotion {
            x: -12.0,
            y: 7.0,
            server_x: None,
            server_y: None,
            metadata: arcen_input::LowLatencyMetadata::default(),
        };

        let point = button_position(
            PointerMotionMode::Relative,
            bounds,
            relative_delta,
            last_position,
        );

        assert!((point.x - last_position.x).abs() < f64::EPSILON);
        assert!((point.y - last_position.y).abs() < f64::EPSILON);
    }

    #[test]
    fn scroll_absolute_position_is_preserved_for_the_native_event() {
        let bounds = DesktopBounds::new(100.0, 50.0, 800.0, 600.0);
        let position = PointerMotion {
            x: 0.75,
            y: 0.25,
            server_x: None,
            server_y: None,
            metadata: arcen_input::LowLatencyMetadata::default(),
        };

        let Some(point) = scroll_position(PointerMotionMode::Absolute, bounds, position) else {
            panic!("absolute scroll position");
        };

        assert!((point.x - 700.0).abs() < f64::EPSILON);
        assert!((point.y - 200.0).abs() < f64::EPSILON);
        assert!(scroll_position(PointerMotionMode::Relative, bounds, position).is_none());
    }
}

/// Posts a scroll and reports whether the desktop moved because of it.
///
/// Exists because the unit a scroll delta is posted in cannot be checked by
/// reading the code: both `kCGScrollEventUnitPixel` and
/// `kCGScrollEventUnitLine` compile, both post successfully, and one of them
/// scrolls one pixel per wheel notch, which looks exactly like scrolling being
/// broken. The only way to tell them apart is to scroll something and see.
///
/// # Errors
///
/// Returns [`InputError`] when the events cannot be created or posted.
pub fn probe_scroll(bounds: DesktopBounds, notches: i32) -> Result<ScrollProbeReport, InputError> {
    let mut controller = InputController::new(bounds)?;
    let before = mouse_location();
    for _ in 0..notches.abs() {
        controller.pointer_scroll(&PointerScroll {
            delta_x: 0.0,
            delta_y: f64::from(notches.signum()),
            unit: arcen_input::ScrollUnit::Line,
            phase: arcen_input::ScrollPhase::None,
            motion_mode: PointerMotionMode::Absolute,
            // Centre of the desktop, so the scroll lands somewhere scrollable
            // rather than wherever the pointer happened to be left.
            position: PointerMotion {
                x: 0.5,
                y: 0.5,
                server_x: None,
                server_y: None,
                metadata: arcen_input::LowLatencyMetadata::default(),
            },
        })?;
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    Ok(ScrollProbeReport {
        notches_posted: notches.abs(),
        scroll_events: controller.stats().scroll_events,
        pointer_stayed_put: mouse_location() == before,
    })
}

/// What a scroll probe observed.
#[derive(Debug, serde::Serialize)]
pub struct ScrollProbeReport {
    /// How many wheel notches were posted.
    pub notches_posted: i32,
    /// How many scroll events the controller reports posting.
    pub scroll_events: u64,
    /// Whether the pointer stayed where it was, which a scroll must not move.
    pub pointer_stayed_put: bool,
}

/// Reads the current pointer position.
fn mouse_location() -> (i64, i64) {
    // SAFETY: creating an event with a null source is valid and returns either
    // an owned event or null.
    let event = unsafe { CGEventCreate(std::ptr::null_mut()) };
    if event.is_null() {
        return (0, 0);
    }
    // SAFETY: `event` is a live event owned here and released below.
    let point = unsafe { CGEventGetLocation(event) };
    // SAFETY: released exactly once.
    unsafe { release_core_foundation(event.cast_const().cast()) };
    #[allow(clippy::cast_possible_truncation)]
    (point.x as i64, point.y as i64)
}
