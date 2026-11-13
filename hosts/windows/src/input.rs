// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Native input injection via `SendInput` — the Windows analog of the Linux
//! host's uinput/XTest path. Ported from `server/win_input_injector.py`, with
//! two deliberate improvements:
//!
//! 1. **Atomic move+click.** A `mouse_button` is delivered as a single
//!    `SendInput` array `[move, button]`, so the click always lands exactly at
//!    the reported position with no chance of an interleaved move landing it
//!    elsewhere (a flicker suspect in the Python two-call path).
//! 2. **Honest per-event logging.** Every injected event logs its resolved
//!    absolute coordinates at debug — this is the data we read to root-cause the
//!    reported mouse flicker.
//!
//! Legacy single-monitor pointer messages carry **normalized 0..1 floats**
//! relative to the remote-video rect. Multi-monitor input arrives through the
//! shared region adapter and reaches this module only after it has been mapped
//! to signed desktop pixels and `0..=65535` virtual-desktop axes. Despite its
//! legacy `scan_code` field name, protocol-v3 keyboard values are **Qt key
//! identifiers**. The compact modifier mask is synchronized before the key
//! edge, then the Qt key is injected as the PC scan code of its position
//! (`KEYEVENTF_SCANCODE`, the shared `arcen_input::keyboard::scancode`
//! table), so this host's own keyboard layout decides the character exactly
//! as for a keyboard plugged into it. Keys without a scan code there (keypad,
//! locks, Pause, Print) keep their Windows Virtual-Key.
//!
//! Committed text (`TextCommitMsg`: input-method results, Characters mode,
//! Paste as Keystrokes) is typed per character by the shared
//! [`arcen_input::text::route_char`] rule: by its key on the foreground
//! window's own layout when `VkKeyScanExW` finds it at a plain, Shift,
//! `AltGr` or Shift+`AltGr` level, `ToUnicodeEx` proves that key types
//! exactly that character with the lock keys as they are, no key is held
//! and the layout is not an input method's; otherwise as Unicode
//! (`KEYEVENTF_UNICODE`). Keys work where Unicode packets do not (consoles,
//! applications that read scan codes); Unicode types what the layout lacks.
//! The text is never logged.
//!
//! Typed pen samples (`PenEventMsg`) are a separate, original Windows backend:
//! earlier implementations downgraded pen to mouse and never
//! implemented native pressure, so this module injects a real `PT_PEN`
//! synthetic pointer via the public `CreateSyntheticPointerDevice` /
//! `InjectSyntheticPointerInput` / `DestroySyntheticPointerDevice` API
//! (Windows 10 1809+) instead of routing pen through `SendInput`. Pen input is
//! outside the existing low-level mouse hooks entirely (see `deskside.rs`),
//! since it never travels through `MOUSEINPUT`.
//!
//! Non-Windows builds compile a no-op stub so the crate type-checks on macOS.

use crate::display::DesktopRect;
use crate::logging::INPUT;
use crate::multi_monitor_input::{
    MappedRegionButton, MappedRegionPen, MappedRegionPoint, MappedRegionScroll,
};
use arcen_input::keyboard::scancode::{
    set1_scancode, Set1Scancode, LEFT_ALT, LEFT_CONTROL, LEFT_META, LEFT_SHIFT,
};
use arcen_protocol::messages::{
    KeyEventMsg, MouseButtonMsg, MouseMoveMsg, MouseMoveRelativeMsg, MouseScrollMsg, PenEventMsg,
    PenToolMsg, PointerMotionMode, ScrollUnitMsg, TextCommitMsg,
};

const MOD_SHIFT: u32 = 0x01;
const MOD_CTRL: u32 = 0x02;
const MOD_ALT: u32 = 0x04;
const MOD_META: u32 = 0x08;
const MOD_KEYPAD: u32 = 0x10;

fn wheel_delta_from_scroll(value: f64, unit: ScrollUnitMsg) -> i32 {
    if value == 0.0 || !value.is_finite() {
        return 0;
    }
    let scaled = match unit {
        ScrollUnitMsg::Line => value * 120.0,
        ScrollUnitMsg::Point => value,
    };
    let rounded = scaled.round();
    if rounded == 0.0 {
        value.signum() as i32
    } else if rounded > f64::from(i32::MAX) {
        i32::MAX
    } else if rounded < f64::from(i32::MIN) {
        i32::MIN
    } else {
        rounded as i32
    }
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const VK_CAPITAL: u16 = 0x14;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const VK_NUMLOCK: u16 = 0x90;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const VK_SCROLL: u16 = 0x91;

/// One key as the injector presses and releases it.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum HostKey {
    /// A set-1 scan code: the session's keyboard layout decides the
    /// character, as for a local keyboard.
    Scan(Set1Scancode),
    /// A Virtual-Key, for keys the scan code table leaves to it.
    Vk { vk: u16, extended: bool },
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
impl HostKey {
    /// The lock key's Virtual-Key, if this is Caps, Num or Scroll Lock.
    fn lock_vk(self) -> Option<u16> {
        match self {
            Self::Vk { vk, .. } if matches!(vk, VK_CAPITAL | VK_NUMLOCK | VK_SCROLL) => Some(vk),
            _ => None,
        }
    }
}

/// The key a protocol-v3 Qt key identifier + compact modifier mask injects:
/// its position's scan code, else its Virtual-Key.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn qt_key_to_host_key(qt_key: u32, modifiers: u32) -> Option<HostKey> {
    set1_scancode(qt_key, modifiers)
        .map(HostKey::Scan)
        .or_else(|| {
            qt_key_to_windows(qt_key, modifiers).map(|(vk, extended)| HostKey::Vk { vk, extended })
        })
}

/// Protocol-v3 Qt key identifier + compact modifier mask -> Windows VK plus
/// whether SendInput must mark the key as extended.
fn qt_key_to_windows(qt_key: u32, modifiers: u32) -> Option<(u16, bool)> {
    if modifiers & MOD_KEYPAD != 0 {
        match qt_key {
            0x30..=0x39 => return Some((0x60 + (qt_key - 0x30) as u16, false)),
            0x2A => return Some((0x6A, false)), // multiply
            0x2B => return Some((0x6B, false)), // add
            0x2D => return Some((0x6D, false)), // subtract
            0x2E => return Some((0x6E, false)), // decimal
            0x2F => return Some((0x6F, true)),  // divide
            _ => {}
        }
    }

    let mapping = match qt_key {
        0x41..=0x5A | 0x30..=0x39 => (qt_key as u16, false),
        0x20 => (0x20, false),                      // Space
        0x0100_0000 => (0x1B, false),               // Escape
        0x0100_0001 | 0x0100_0002 => (0x09, false), // Tab / Backtab
        0x0100_0003 => (0x08, false),               // Backspace
        0x0100_0004 => (0x0D, false),               // Return
        0x0100_0005 => (0x0D, true),                // Keypad Enter
        0x0100_0006 => (0x2D, true),                // Insert
        0x0100_0007 => (0x2E, true),                // Delete
        0x0100_0008 => (0x13, false),               // Pause
        0x0100_0009 => (0x2C, false),               // Print
        0x0100_000B => (0x0C, false),               // Clear
        0x0100_0010 => (0x24, true),                // Home
        0x0100_0011 => (0x23, true),                // End
        0x0100_0012 => (0x25, true),                // Left
        0x0100_0013 => (0x26, true),                // Up
        0x0100_0014 => (0x27, true),                // Right
        0x0100_0015 => (0x28, true),                // Down
        0x0100_0016 => (0x21, true),                // PageUp
        0x0100_0017 => (0x22, true),                // PageDown
        0x0100_0020 => (0xA0, false),               // Left Shift
        0x0100_0021 => (0xA2, false),               // Left Control
        0x0100_0022 => (0x5B, false),               // Left Windows
        0x0100_0023 => (0xA4, false),               // Left Alt
        0x0100_0024 => (VK_CAPITAL, false),
        0x0100_0025 => (VK_NUMLOCK, false),
        0x0100_0026 => (VK_SCROLL, false),
        0x0100_0030..=0x0100_003B => (0x70 + (qt_key - 0x0100_0030) as u16, false),
        0x2D | 0x5F => (0xBD, false), // - _
        0x3D | 0x2B => (0xBB, false), // = +
        0x5B | 0x7B => (0xDB, false), // [ {
        0x5D | 0x7D => (0xDD, false), // ] }
        0x5C | 0x7C => (0xDC, false), // \ |
        0x3B | 0x3A => (0xBA, false), // ; :
        0x27 | 0x22 => (0xDE, false), // ' "
        0x60 | 0x7E => (0xC0, false), // ` ~
        0x2C | 0x3C => (0xBC, false), // , <
        0x2E | 0x3E => (0xBE, false), // . >
        0x2F | 0x3F => (0xBF, false), // / ?
        0x21 => (0x31, false),        // !
        0x40 => (0x32, false),        // @
        0x23 => (0x33, false),        // #
        0x24 => (0x34, false),        // $
        0x25 => (0x35, false),        // %
        0x5E => (0x36, false),        // ^
        0x26 => (0x37, false),        // &
        0x2A => (0x38, false),        // *
        0x28 => (0x39, false),        // (
        0x29 => (0x30, false),        // )
        _ => return None,
    };
    Some(mapping)
}

#[cfg(test)]
fn qt_key_to_vk(qt_key: u32, modifiers: u32) -> Option<u16> {
    qt_key_to_windows(qt_key, modifiers).map(|(vk, _)| vk)
}

/// An unassigned Virtual-Key (AutoHotkey's "menu mask key"): a key event
/// that types nothing and that no application handles, injected only to
/// tell Windows a key went with the Windows key.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const VK_MENU_MASK: u16 = 0xE8;

/// Keeps a Windows key used only with the pointer from opening Start.
///
/// Windows opens Start when the Windows key is released with no other key
/// between its press and release; a click, a scroll or a drag does not
/// count. A Mac client sends Command+click as Windows+click, so letting go
/// of Command after it would open Start. When the Windows key goes up after
/// pointer input and no key, the injector first injects [`VK_MENU_MASK`] —
/// no visible key (a Control tap would show "Find My Mouse" or an Office
/// paste menu) — exactly as a Windows+click on a local keyboard would end
/// had the user meant it. A lone Windows-key tap still opens Start.
#[derive(Debug, Default)]
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
struct MetaReleaseMask {
    meta_down: std::cell::Cell<bool>,
    key_since: std::cell::Cell<bool>,
    pointer_since: std::cell::Cell<bool>,
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
impl MetaReleaseMask {
    /// One key edge about to be injected; whether the mask key must be
    /// injected before it.
    fn key_edge(&self, key: HostKey, pressed: bool) -> bool {
        if key == HostKey::Scan(LEFT_META) {
            if pressed {
                self.meta_down.set(true);
                self.key_since.set(false);
                self.pointer_since.set(false);
                return false;
            }
            let mask = self.meta_down.get() && self.pointer_since.get() && !self.key_since.get();
            self.meta_down.set(false);
            return mask;
        }
        if pressed && self.meta_down.get() {
            self.key_since.set(true);
        }
        false
    }

    /// A button press, scroll or drag was injected.
    fn pointer(&self) {
        if self.meta_down.get() {
            self.pointer_since.set(true);
        }
    }
}

/// The layout level a `VkKeyScanExW` shift state names (1 Shift, 2 Control,
/// 4 Alt; Control+Alt is `AltGr`). Kana and other shift states are never
/// typed by key.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const fn layout_level(shift_state: u8) -> arcen_input::text::LayoutLevel {
    use arcen_input::text::LayoutLevel;
    match shift_state {
        0 => LayoutLevel::Base,
        1 => LayoutLevel::Shift,
        6 => LayoutLevel::AltGr,
        7 => LayoutLevel::ShiftAltGr,
        _ => LayoutLevel::Other,
    }
}

/// The set-1 scan code `MapVirtualKeyExW(MAPVK_VK_TO_VSC_EX)` returns:
/// `0xE0xx` is extended; `0xE1` (Pause) and nothing are not typeable.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn scan_from_mapped(value: u32) -> Option<Set1Scancode> {
    let code = u8::try_from(value & 0xFF).ok().filter(|code| *code != 0)?;
    match value >> 8 {
        0 => Some(Set1Scancode {
            code,
            extended: false,
        }),
        0xE0 => Some(Set1Scancode {
            code,
            extended: true,
        }),
        _ => None,
    }
}

/// The thread whose keyboard layout decides committed characters: the
/// foreground window's. `None` when there is no foreground window or its
/// thread is not known — never this process's own (stale) layout.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const fn layout_thread(foreground_window: bool, window_thread: u32) -> Option<u32> {
    if foreground_window && window_thread != 0 {
        Some(window_thread)
    } else {
        None
    }
}

/// How one committed character is typed: by the shared rule when the
/// foreground layout is known, and as Unicode when it is not, since a key
/// on an unverified layout may type another character.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn committed_route(
    character: char,
    probe: Option<arcen_input::text::LayoutProbe>,
    layout_known: bool,
    keys_held: bool,
    input_method_layout: bool,
) -> arcen_input::text::CharRoute {
    if layout_known {
        arcen_input::text::route_char(character, probe, keys_held, input_method_layout)
    } else {
        arcen_input::text::CharRoute::Text
    }
}

/// Whether a keyboard layout handle belongs to an input method that turns
/// keys into other text (Chinese, Japanese, Korean, or a legacy IME handle),
/// so a character must not be typed by its key.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const fn is_input_method_layout(layout: usize) -> bool {
    let primary_language = layout & 0x3FF;
    let device = (layout >> 16) & 0xFFFF;
    matches!(primary_language, 0x04 | 0x11 | 0x12) || device & 0xF000 == 0xE000
}

/// The left modifier keys the compact mask asks to be held. `AltGr` (the
/// right Alt) is a key of its own, never part of the mask: synchronizing
/// never presses or releases it, nor the left Control Windows adds to it.
fn modifier_targets(modifiers: u32) -> [(HostKey, bool); 4] {
    [
        (HostKey::Scan(LEFT_SHIFT), modifiers & MOD_SHIFT != 0),
        (HostKey::Scan(LEFT_CONTROL), modifiers & MOD_CTRL != 0),
        (HostKey::Scan(LEFT_ALT), modifiers & MOD_ALT != 0),
        (HostKey::Scan(LEFT_META), modifiers & MOD_META != 0),
    ]
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn norm_to_abs(v: f64) -> i32 {
    (v.clamp(0.0, 1.0) * 65535.0) as i32
}

/// Temporary legacy single-monitor mapping. Region-authoritative sessions
/// bypass this path and inject the shared adapter's final native coordinates.
fn selected_output_to_virtual_abs(
    x: f64,
    y: f64,
    output: DesktopRect,
    virtual_desktop: DesktopRect,
) -> Result<(i32, i32), String> {
    if output.width <= 0
        || output.height <= 0
        || virtual_desktop.width <= 0
        || virtual_desktop.height <= 0
    {
        return Err("desktop geometry has non-positive dimensions".to_string());
    }

    let output_x =
        output.left as f64 + x.clamp(0.0, 1.0) * f64::from(output.width.saturating_sub(1));
    let output_y =
        output.top as f64 + y.clamp(0.0, 1.0) * f64::from(output.height.saturating_sub(1));
    let virtual_x = (output_x - f64::from(virtual_desktop.left))
        / f64::from(virtual_desktop.width.saturating_sub(1).max(1));
    let virtual_y = (output_y - f64::from(virtual_desktop.top))
        / f64::from(virtual_desktop.height.saturating_sub(1).max(1));
    Ok((norm_to_abs(virtual_x), norm_to_abs(virtual_y)))
}

fn is_identity_dxgi_rotation(rotation: i32) -> bool {
    rotation == 1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RelativeMovePlan {
    dx: i32,
    dy: i32,
    move_flag: bool,
    absolute_flag: bool,
    virtual_desktop_flag: bool,
}

fn relative_move_plan(message: &MouseMoveRelativeMsg) -> RelativeMovePlan {
    RelativeMovePlan {
        dx: message.dx,
        dy: message.dy,
        move_flag: true,
        absolute_flag: false,
        virtual_desktop_flag: false,
    }
}

/// The input that wakes a console display Windows has powered off for idle.
///
/// `SetThreadExecutionState(ES_DISPLAY_REQUIRED)` (capenc's keep-awake) only
/// keeps a display on; it does not turn one back on, and while it is off DWM
/// composes rarely, so a session that starts then captures 4–8 fps until the
/// Deck's first input. Input does wake it. A relative move of zero pixels is
/// input to the power manager but leaves the pointer exactly where it was.
const fn display_wake_plan() -> RelativeMovePlan {
    RelativeMovePlan {
        dx: 0,
        dy: 0,
        move_flag: true,
        absolute_flag: false,
        virtual_desktop_flag: false,
    }
}

const fn prepends_absolute_position(mode: PointerMotionMode) -> bool {
    matches!(mode, PointerMotionMode::Absolute)
}

// ──────────────────────────── Pen pure mapping ─────────────────────────────
//
// Everything in this section is plain data/logic with no `windows` crate
// dependency, so it compiles and is unit-tested on every host platform.
// `windows_impl::PenInjector` (Windows-only, below) is the only place that
// touches the real `POINTER_FLAGS`/`PEN_FLAG_*`/`PEN_MASK_*` Win32 constants;
// it maps every `PenPointerEdge` this state machine produces to the exact
// documented flag combination.

/// Maps normalized `0.0..=1.0` pressure to the Windows Ink documented
/// `0..=1024` pointer pressure range, through the shared vendor-neutral
/// normalisation.
fn pen_pressure_to_windows(pressure: f32) -> u32 {
    let scaled = arcen_input::pen_axes::scale_unit(
        pressure,
        arcen_input::pen_axes::WINDOWS_INK_PRESSURE_RANGE,
    );
    u32::try_from(scaled).unwrap_or(0)
}

/// Maps `PenEventMsg`'s inclusive `0.0..=360.0` rotation (`0` and `360` both
/// denoting the same angle) to Windows Ink's documented `0..=359` range.
fn pen_rotation_to_windows(rotation_degrees: f32) -> u32 {
    arcen_input::pen_axes::whole_rotation_degrees(rotation_degrees)
}

/// Maps `-90.0..=90.0` tilt degrees to Windows Ink's documented
/// `-90..=90` integer range.
fn pen_tilt_to_windows(tilt_degrees: f32) -> i32 {
    arcen_input::pen_axes::whole_tilt_degrees(tilt_degrees)
}

/// Maps a legacy normalized position to a pixel coordinate inside the selected
/// single-monitor output, reusing the exact same validated `DesktopRect` the mouse
/// `SendInput` path uses — but as real screen pixels (`POINTER_INFO.
/// ptPixelLocation`), not the `0..65535` `MOUSEEVENTF_ABSOLUTE` space mouse
/// movement uses. Returns `None` for non-positive output geometry, matching
/// `selected_output_to_virtual_abs`'s guard.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pen_pixel_location(x: f64, y: f64, output: DesktopRect) -> Option<(i32, i32)> {
    if output.width <= 0 || output.height <= 0 {
        return None;
    }
    let px = f64::from(output.left) + x.clamp(0.0, 1.0) * f64::from(output.width.saturating_sub(1));
    let py = f64::from(output.top) + y.clamp(0.0, 1.0) * f64::from(output.height.saturating_sub(1));
    Some((px.round() as i32, py.round() as i32))
}

/// Windows Ink pen flags for one injected frame, as a real digitizer reports
/// them (HID In Range / Invert / Eraser / Barrel Switch):
///
/// - `inverted` (`PEN_FLAG_INVERTED`): the eraser end is in range, hovering
///   or touching.
/// - `eraser` (`PEN_FLAG_ERASER`): the eraser end is touching. Windows treats
///   this flag as the eraser's tip switch, so setting it while hovering turns
///   the hover into a contact (an eraser hover with the flag set arrives as
///   `WM_POINTERDOWN` and clicks).
/// - `barrel` (`PEN_FLAG_BARREL`): either barrel button. Windows Ink has a
///   single barrel switch and reports it as the second pointer button; an
///   injected `POINTER_FLAG_SECONDBUTTON` is ignored for a synthetic pen, so
///   bit 1 would otherwise do nothing.
///
/// Nothing is set on an edge that leaves range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PenInkFlags {
    barrel: bool,
    inverted: bool,
    eraser: bool,
}

fn pen_ink_flags(tool: PenToolMsg, buttons: u16, edge: PenPointerEdge) -> PenInkFlags {
    let in_range = matches!(
        edge,
        PenPointerEdge::Hover
            | PenPointerEdge::ContactDown
            | PenPointerEdge::ContactMove
            | PenPointerEdge::ContactUp
    );
    let in_contact = matches!(
        edge,
        PenPointerEdge::ContactDown | PenPointerEdge::ContactMove
    );
    let eraser_end = matches!(tool, PenToolMsg::Eraser);
    PenInkFlags {
        barrel: in_range && buttons & 0b11 != 0,
        inverted: in_range && eraser_end,
        eraser: in_contact && eraser_end,
    }
}

/// `InjectSyntheticPointerInput` takes `ptPixelLocation` relative to the
/// top-left of the virtual screen, not in desktop coordinates. The two differ
/// whenever a display sits left of or above the primary (a tap at desktop
/// (-901, 1396) on a display left of the primary, passed unconverted, lands
/// at (2559, 1396), the far edge of the primary).
fn pen_injection_point(desktop: (i32, i32), virtual_origin: (i32, i32)) -> (i32, i32) {
    (
        desktop.0.saturating_sub(virtual_origin.0),
        desktop.1.saturating_sub(virtual_origin.1),
    )
}

/// One digitizer's proximity/hover/contact phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PenPhase {
    /// Outside digitizer detection range.
    Out,
    /// In range, tip not touching.
    Hovering,
    /// In range, tip touching.
    Contact,
}

/// Legal Windows Ink pointer transition computed from consecutive
/// `PenEventMsg` samples. `windows_impl::pointer_flags_for` maps each variant
/// to the exact documented `POINTER_FLAG_*` combination; `NoChange` means no
/// `InjectSyntheticPointerInput` call is needed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PenPointerEdge {
    /// Was and remains out of range; nothing to inject.
    NoChange,
    /// Entered or continues hovering in range, tip not touching.
    Hover,
    /// Tip made first contact this frame.
    ContactDown,
    /// Tip remains in contact; position/pressure/tilt updated.
    ContactMove,
    /// Tip lifted but the tool remains in range.
    ContactUp,
    /// Left range directly from hover; no contact was ever active.
    HoverLeave,
    /// Left range and ended contact in the same frame.
    ContactLeave,
}

/// Legal proximity/hover/contact transition state machine for one synthetic
/// pen pointer. Every transition is derived only from the next sample's
/// `in_proximity`/`touching` pair, so the state machine can never desync from
/// what was actually injected. `release` deterministically returns the edge
/// needed to clear any held proximity/contact — used on reset, disconnect,
/// reconnect, and `Drop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PenPointerState {
    phase: PenPhase,
}

impl PenPointerState {
    const fn released() -> Self {
        Self {
            phase: PenPhase::Out,
        }
    }

    /// Advances toward the reported `in_proximity`/`touching` pair. Contact
    /// always implies proximity — real digitizer hardware never reports
    /// touching while out of range, so a peer that violates this convention
    /// is corrected here rather than rejected (numeric-range validation of
    /// `PenEventMsg` itself happens earlier, in `PenEventMsg::validate`).
    fn advance(&mut self, in_proximity: bool, touching: bool) -> PenPointerEdge {
        let in_proximity = in_proximity || touching;
        let next = match (in_proximity, touching) {
            (false, _) => PenPhase::Out,
            (true, false) => PenPhase::Hovering,
            (true, true) => PenPhase::Contact,
        };
        let edge = match (self.phase, next) {
            (PenPhase::Out, PenPhase::Out) => PenPointerEdge::NoChange,
            (PenPhase::Out | PenPhase::Hovering, PenPhase::Hovering) => PenPointerEdge::Hover,
            (PenPhase::Out | PenPhase::Hovering, PenPhase::Contact) => PenPointerEdge::ContactDown,
            (PenPhase::Hovering, PenPhase::Out) => PenPointerEdge::HoverLeave,
            (PenPhase::Contact, PenPhase::Out) => PenPointerEdge::ContactLeave,
            (PenPhase::Contact, PenPhase::Hovering) => PenPointerEdge::ContactUp,
            (PenPhase::Contact, PenPhase::Contact) => PenPointerEdge::ContactMove,
        };
        self.phase = next;
        edge
    }

    /// The legal edge that clears any held proximity/contact, or `None` if
    /// already fully released.
    fn release(&mut self) -> Option<PenPointerEdge> {
        if self.phase == PenPhase::Out {
            None
        } else {
            Some(self.advance(false, false))
        }
    }
}

// ───────────────────────────────── Windows ─────────────────────────────────

#[cfg(windows)]
pub use windows_impl::{
    set_dpi_awareness as initialize_process_dpi_awareness, wake_console_display, Injector,
    PenInjector,
};

#[cfg(not(windows))]
pub fn initialize_process_dpi_awareness() -> &'static str {
    "not-windows"
}

#[cfg(windows)]
mod windows_impl {
    use super::{
        committed_route, is_input_method_layout, layout_level, layout_thread, scan_from_mapped,
    };
    use super::{
        is_identity_dxgi_rotation, modifier_targets, pen_injection_point, pen_ink_flags,
        pen_pixel_location, pen_pressure_to_windows, pen_rotation_to_windows, pen_tilt_to_windows,
        qt_key_to_host_key, selected_output_to_virtual_abs, wheel_delta_from_scroll, DesktopRect,
        HostKey, KeyEventMsg, MappedRegionButton, MappedRegionPen, MappedRegionPoint,
        MappedRegionScroll, MetaReleaseMask, MouseButtonMsg, MouseMoveMsg, MouseMoveRelativeMsg,
        MouseScrollMsg, PenEventMsg, PenPointerEdge, PenPointerState, PenToolMsg, TextCommitMsg,
        INPUT, VK_CAPITAL, VK_MENU_MASK, VK_NUMLOCK, VK_SCROLL,
    };
    use arcen_input::keyboard::scancode::{Set1Scancode, LEFT_CONTROL, LEFT_SHIFT, RIGHT_ALT};
    use arcen_input::text::{CharRoute, LayoutProbe};
    use std::collections::HashSet;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};
    use windows::Win32::UI::Controls::{
        CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
        POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
    };
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
        KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
        MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardLayout, MapVirtualKeyExW, ToUnicodeEx, VkKeyScanExW, HKL, MAPVK_VK_TO_VSC_EX,
    };
    use windows::Win32::UI::Input::Pointer::{
        InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
        POINTER_FLAG_INRANGE, POINTER_FLAG_NONE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
        POINTER_INFO, POINTER_PEN_INFO,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PEN_FLAG_NONE,
        PEN_MASK_PRESSURE, PEN_MASK_ROTATION, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN,
        SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    const WHEEL_DELTA: i32 = 120;

    enum DxgiGeometryError {
        Unavailable(String),
        UnsupportedRotation(i32),
    }

    impl std::fmt::Display for DxgiGeometryError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Unavailable(error) => formatter.write_str(error),
                Self::UnsupportedRotation(rotation) => {
                    write!(formatter, "unsupported DXGI rotation {rotation}")
                }
            }
        }
    }

    fn resolve_pointer_geometry(
        output_index: u32,
        expected_output: DesktopRect,
    ) -> Result<(DesktopRect, DesktopRect), String> {
        // SAFETY: GetSystemMetrics has no pointer arguments or preconditions.
        let virtual_desktop = unsafe {
            DesktopRect {
                left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        };
        if virtual_desktop.width <= 0 || virtual_desktop.height <= 0 {
            return Err("Windows virtual-screen metrics are unavailable".to_string());
        }

        // Use the same desktop-attached DXGI enumeration order as capenc.
        let output = unsafe { dxgi_output_rect(output_index) }.map_err(|error| {
            format!("cannot safely map selected output {output_index}: {error}")
        })?;
        if output != expected_output {
            return Err(format!(
                "selected output geometry changed after display settle: expected \
                 {expected_output:?}, found {output:?}"
            ));
        }

        let virtual_right = virtual_desktop.left.saturating_add(virtual_desktop.width);
        let virtual_bottom = virtual_desktop.top.saturating_add(virtual_desktop.height);
        let output_right = output.left.saturating_add(output.width);
        let output_bottom = output.top.saturating_add(output.height);
        if output.width <= 0
            || output.height <= 0
            || output.left < virtual_desktop.left
            || output.top < virtual_desktop.top
            || output_right > virtual_right
            || output_bottom > virtual_bottom
        {
            return Err(format!(
                "selected output {output_index} is outside Windows virtual-screen geometry"
            ));
        }
        Ok((output, virtual_desktop))
    }

    fn virtual_desktop_geometry() -> Result<DesktopRect, String> {
        // SAFETY: GetSystemMetrics has no pointer arguments or preconditions.
        let desktop = unsafe {
            DesktopRect {
                left: GetSystemMetrics(SM_XVIRTUALSCREEN),
                top: GetSystemMetrics(SM_YVIRTUALSCREEN),
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        };
        if desktop.width <= 0 || desktop.height <= 0 {
            return Err("Windows virtual-screen metrics are unavailable".to_string());
        }
        Ok(desktop)
    }

    fn resolve_region_desktop(expected: DesktopRect) -> Result<DesktopRect, String> {
        let actual = virtual_desktop_geometry()?;
        if actual != expected {
            return Err(format!(
                "committed region desktop changed before input initialization: expected \
                 {expected:?}, found {actual:?}"
            ));
        }
        Ok(actual)
    }

    unsafe fn dxgi_output_rect(output_index: u32) -> Result<DesktopRect, DxgiGeometryError> {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|error| {
            DxgiGeometryError::Unavailable(format!("CreateDXGIFactory1: {error}"))
        })?;
        let mut seen = 0u32;
        let mut adapter_index = 0u32;
        loop {
            let adapter: IDXGIAdapter = match factory.EnumAdapters(adapter_index) {
                Ok(adapter) => adapter,
                Err(_) => break,
            };
            adapter_index += 1;
            let mut local_output = 0u32;
            loop {
                let output = match adapter.EnumOutputs(local_output) {
                    Ok(output) => output,
                    Err(_) => break,
                };
                local_output += 1;
                let desc = output.GetDesc().map_err(|error| {
                    DxgiGeometryError::Unavailable(format!("IDXGIOutput::GetDesc: {error}"))
                })?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                if seen != output_index {
                    seen += 1;
                    continue;
                }
                if !is_identity_dxgi_rotation(desc.Rotation.0) {
                    return Err(DxgiGeometryError::UnsupportedRotation(desc.Rotation.0));
                }
                let rect = desc.DesktopCoordinates;
                return Ok(DesktopRect {
                    left: rect.left,
                    top: rect.top,
                    width: rect.right.saturating_sub(rect.left),
                    height: rect.bottom.saturating_sub(rect.top),
                });
            }
        }
        Err(DxgiGeometryError::Unavailable(
            "DXGI did not expose the requested desktop-attached output".to_string(),
        ))
    }

    /// Sends `display_wake_plan` on this session's input desktop. It carries
    /// the Arcen injection marker, so armed deskside hooks pass it as remote
    /// input rather than treating it as someone at the console. Returns the
    /// number of events Windows accepted (0 on a desktop that refuses input,
    /// such as a locked console's secure desktop).
    pub fn wake_console_display() -> u32 {
        let plan = super::display_wake_plan();
        debug_assert!(plan.move_flag && !plan.absolute_flag && !plan.virtual_desktop_flag);
        let input = Injector::mouse_input(plan.dx, plan.dy, 0, MOUSEEVENTF_MOVE);
        // SAFETY: one correctly-initialised INPUT; cbsize matches its size.
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) }
    }

    pub struct Injector {
        buttons_down: HashSet<i32>,
        keys_down: HashSet<HostKey>,
        meta_mask: MetaReleaseMask,
        output: DesktopRect,
        virtual_desktop: DesktopRect,
    }

    impl Injector {
        pub fn new(output_index: u32, expected_output: DesktopRect) -> Result<Self, String> {
            let dpi = set_dpi_awareness();
            let (output, virtual_desktop) =
                resolve_pointer_geometry(output_index, expected_output)?;
            tracing::info!(
                target: INPUT,
                output_index,
                output_left = output.left,
                output_top = output.top,
                output_width = output.width,
                output_height = output.height,
                virtual_left = virtual_desktop.left,
                virtual_top = virtual_desktop.top,
                virtual_width = virtual_desktop.width,
                virtual_height = virtual_desktop.height,
                dpi,
                "SendInput injector ready"
            );
            Ok(Self {
                buttons_down: HashSet::new(),
                keys_down: HashSet::new(),
                meta_mask: MetaReleaseMask::default(),
                output,
                virtual_desktop,
            })
        }

        pub fn new_region_desktop(expected: DesktopRect) -> Result<Self, String> {
            let dpi = set_dpi_awareness();
            let virtual_desktop = resolve_region_desktop(expected)?;
            tracing::info!(
                target: INPUT,
                virtual_left = virtual_desktop.left,
                virtual_top = virtual_desktop.top,
                virtual_width = virtual_desktop.width,
                virtual_height = virtual_desktop.height,
                dpi,
                "SendInput multi-monitor injector ready"
            );
            Ok(Self {
                buttons_down: HashSet::new(),
                keys_down: HashSet::new(),
                meta_mask: MetaReleaseMask::default(),
                output: virtual_desktop,
                virtual_desktop,
            })
        }

        fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: data as u32,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: crate::deskside::injection_marker(),
                    },
                },
            }
        }

        fn key_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: crate::deskside::injection_marker(),
                    },
                },
            }
        }

        fn send(inputs: &[INPUT]) {
            // SAFETY: `inputs` is a valid slice of correctly-initialized INPUTs;
            // cbsize matches the element size.
            let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
            if sent as usize != inputs.len() {
                tracing::warn!(
                    target: INPUT,
                    requested = inputs.len(),
                    sent,
                    "SendInput dropped events"
                );
            }
        }

        fn pointer_abs(&self, x: f64, y: f64) -> (i32, i32) {
            selected_output_to_virtual_abs(x, y, self.output, self.virtual_desktop)
                .expect("validated pointer geometry")
        }

        /// Absolute move to a normalized (0..1) position over the captured output.
        pub fn move_abs(&self, msg: &MouseMoveMsg) {
            let (ax, ay) = self.pointer_abs(msg.x, msg.y);
            tracing::debug!(
                target: INPUT,
                x = msg.x,
                y = msg.y,
                ax,
                ay,
                sequence = msg.sequence,
                "mouse_move"
            );
            Self::send(&[Self::mouse_input(
                ax,
                ay,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )]);
        }

        /// Emits a shared-region point already mapped to the Windows virtual
        /// axes by `RegionInputAdapter`.
        pub fn move_region(&self, mapped: MappedRegionPoint, sequence: u64) {
            tracing::debug!(
                target: INPUT,
                desktop_x = mapped.desktop.x,
                desktop_y = mapped.desktop.y,
                ax = mapped.send_input.x,
                ay = mapped.send_input.y,
                sequence,
                "region_pointer_move"
            );
            Self::send(&[Self::mouse_input(
                mapped.send_input.x,
                mapped.send_input.y,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )]);
        }

        /// Relative movement uses only `MOUSEEVENTF_MOVE`.
        pub fn move_relative(&self, msg: &MouseMoveRelativeMsg) {
            let plan = super::relative_move_plan(msg);
            debug_assert!(plan.move_flag);
            debug_assert!(!plan.absolute_flag);
            debug_assert!(!plan.virtual_desktop_flag);
            tracing::debug!(
                target: INPUT,
                dx = plan.dx,
                dy = plan.dy,
                sequence = msg.sequence,
                "mouse_move_relative"
            );
            Self::send(&[Self::mouse_input(plan.dx, plan.dy, 0, MOUSEEVENTF_MOVE)]);
        }

        /// Atomic move-then-button as a single SendInput array.
        pub fn button(&mut self, msg: &MouseButtonMsg) {
            if msg.pressed {
                self.meta_mask.pointer();
            }
            let button = i32::from(msg.button);
            let already_down = self.buttons_down.contains(&button);
            if already_down == msg.pressed {
                tracing::debug!(
                    target: INPUT,
                    button,
                    pressed = msg.pressed,
                    sequence = msg.sequence,
                    "duplicate mouse edge dropped"
                );
                return;
            }
            let flag = match (button, msg.pressed) {
                (1, true) => MOUSEEVENTF_LEFTDOWN,
                (1, false) => MOUSEEVENTF_LEFTUP,
                (2, true) => MOUSEEVENTF_MIDDLEDOWN,
                (2, false) => MOUSEEVENTF_MIDDLEUP,
                (3, true) => MOUSEEVENTF_RIGHTDOWN,
                (3, false) => MOUSEEVENTF_RIGHTUP,
                _ => {
                    tracing::debug!(target: INPUT, button, "unknown mouse button — dropped");
                    return;
                }
            };
            if msg.pressed {
                self.buttons_down.insert(button);
            } else {
                self.buttons_down.remove(&button);
            }
            let position = super::prepends_absolute_position(msg.motion_mode)
                .then(|| self.pointer_abs(msg.x, msg.y));
            tracing::debug!(
                target: INPUT,
                x = msg.x,
                y = msg.y,
                ax = position.map(|point| point.0),
                ay = position.map(|point| point.1),
                button,
                pressed = msg.pressed,
                sequence = msg.sequence,
                "mouse_button"
            );
            let edge = Self::mouse_input(0, 0, 0, flag);
            if let Some((ax, ay)) = position {
                Self::send(&[
                    Self::mouse_input(
                        ax,
                        ay,
                        0,
                        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    ),
                    edge,
                ]);
            } else {
                Self::send(&[edge]);
            }
        }

        /// Atomic region move-then-button. The shared state machine has
        /// already required this exact position to be the latest accepted
        /// pointer position.
        pub fn button_region(&mut self, mapped: MappedRegionButton, sequence: u64) {
            if mapped.pressed {
                self.meta_mask.pointer();
            }
            let button = i32::from(mapped.button.protocol_code());
            let already_down = self.buttons_down.contains(&button);
            if already_down == mapped.pressed {
                tracing::debug!(
                    target: INPUT,
                    button,
                    pressed = mapped.pressed,
                    sequence,
                    "duplicate region mouse edge dropped"
                );
                return;
            }
            let flag = match (button, mapped.pressed) {
                (1, true) => MOUSEEVENTF_LEFTDOWN,
                (1, false) => MOUSEEVENTF_LEFTUP,
                (2, true) => MOUSEEVENTF_MIDDLEDOWN,
                (2, false) => MOUSEEVENTF_MIDDLEUP,
                (3, true) => MOUSEEVENTF_RIGHTDOWN,
                (3, false) => MOUSEEVENTF_RIGHTUP,
                _ => unreachable!("RegionInputAdapter emits only supported Windows buttons"),
            };
            if mapped.pressed {
                self.buttons_down.insert(button);
            } else {
                self.buttons_down.remove(&button);
            }
            tracing::debug!(
                target: INPUT,
                desktop_x = mapped.position.desktop.x,
                desktop_y = mapped.position.desktop.y,
                ax = mapped.position.send_input.x,
                ay = mapped.position.send_input.y,
                button,
                pressed = mapped.pressed,
                sequence,
                "region_pointer_button"
            );
            Self::send(&[
                Self::mouse_input(
                    mapped.position.send_input.x,
                    mapped.position.send_input.y,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ),
                Self::mouse_input(0, 0, 0, flag),
            ]);
        }

        pub fn scroll(&self, msg: &MouseScrollMsg) {
            self.meta_mask.pointer();
            let mut inputs = Vec::with_capacity(3);
            if super::prepends_absolute_position(msg.motion_mode) {
                let (ax, ay) = self.pointer_abs(msg.x, msg.y);
                inputs.push(Self::mouse_input(
                    ax,
                    ay,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ));
            }
            if msg.dy != 0.0 {
                inputs.push(Self::mouse_input(
                    0,
                    0,
                    wheel_delta_from_scroll(msg.dy, msg.unit),
                    MOUSEEVENTF_WHEEL,
                ));
            }
            if msg.dx != 0.0 {
                inputs.push(Self::mouse_input(
                    0,
                    0,
                    wheel_delta_from_scroll(msg.dx, msg.unit),
                    MOUSEEVENTF_HWHEEL,
                ));
            }
            Self::send(&inputs);
            tracing::debug!(
                target: INPUT,
                dx = msg.dx,
                dy = msg.dy,
                sequence = msg.sequence,
                "mouse_scroll"
            );
        }

        pub fn scroll_region(&self, mapped: MappedRegionScroll, sequence: u64) {
            self.meta_mask.pointer();
            let mut inputs = Vec::with_capacity(3);
            inputs.push(Self::mouse_input(
                mapped.position.send_input.x,
                mapped.position.send_input.y,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            ));
            if mapped.vertical != 0 {
                inputs.push(Self::mouse_input(0, 0, mapped.vertical, MOUSEEVENTF_WHEEL));
            }
            if mapped.horizontal != 0 {
                inputs.push(Self::mouse_input(
                    0,
                    0,
                    mapped.horizontal,
                    MOUSEEVENTF_HWHEEL,
                ));
            }
            Self::send(&inputs);
            tracing::debug!(
                target: INPUT,
                desktop_x = mapped.position.desktop.x,
                desktop_y = mapped.position.desktop.y,
                horizontal = mapped.horizontal,
                vertical = mapped.vertical,
                sequence,
                "region_pointer_scroll"
            );
        }

        /// Synchronize the compact protocol-v3 modifier mask before a key edge.
        fn sync_modifiers(&mut self, modifiers: u32) {
            for (key, desired_down) in modifier_targets(modifiers) {
                if self.inject_key_edge(key, desired_down) {
                    tracing::debug!(
                        target: INPUT,
                        key = ?key,
                        pressed = desired_down,
                        modifiers,
                        "modifier synchronized"
                    );
                }
            }
        }

        fn sync_lock_state(&self, vk: u16, desired: Option<bool>, name: &str) {
            let Some(desired) = desired else {
                return;
            };
            // SAFETY: GetKeyState is read-only for a valid virtual-key value.
            let current = unsafe { GetKeyState(i32::from(vk)) } & 1 != 0;
            if current != desired {
                Self::send(&[
                    Self::key_input(vk, 0, KEYBD_EVENT_FLAGS(0)),
                    Self::key_input(vk, 0, KEYEVENTF_KEYUP),
                ]);
                tracing::debug!(
                    target: INPUT,
                    lock = name,
                    current,
                    desired,
                    "lock state synchronized"
                );
            }
        }

        /// The `SendInput` record of one key edge.
        fn host_key_input(key: HostKey, pressed: bool) -> INPUT {
            let mut flags = if pressed {
                KEYBD_EVENT_FLAGS(0)
            } else {
                KEYEVENTF_KEYUP
            };
            match key {
                HostKey::Scan(scan) => {
                    flags |= KEYEVENTF_SCANCODE;
                    if scan.extended {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    Self::key_input(0, u16::from(scan.code), flags)
                }
                HostKey::Vk { vk, extended } => {
                    if extended {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    Self::key_input(vk, 0, flags)
                }
            }
        }

        /// Injects the menu mask key when the Windows key is about to go up
        /// after pointer input only.
        fn mask_before(&self, key: HostKey, pressed: bool) {
            if self.meta_mask.key_edge(key, pressed) {
                Self::send(&[
                    Self::key_input(VK_MENU_MASK, 0, KEYBD_EVENT_FLAGS(0)),
                    Self::key_input(VK_MENU_MASK, 0, KEYEVENTF_KEYUP),
                ]);
                tracing::debug!(target: INPUT, "Windows key release masked after pointer input");
            }
        }

        fn inject_key_edge(&mut self, key: HostKey, pressed: bool) -> bool {
            let already_down = self.keys_down.contains(&key);
            if already_down == pressed {
                return false;
            }
            self.mask_before(key, pressed);
            Self::send(&[Self::host_key_input(key, pressed)]);
            if pressed {
                self.keys_down.insert(key);
            } else {
                self.keys_down.remove(&key);
            }
            true
        }

        /// Inject a protocol-v3 Qt key identifier by its position's scan
        /// code, or its Virtual-Key.
        pub fn key_event(&mut self, msg: &KeyEventMsg) {
            let Some(key) = qt_key_to_host_key(msg.scan_code, msg.modifiers) else {
                tracing::debug!(
                    target: INPUT,
                    qt_key = msg.scan_code,
                    modifiers = msg.modifiers,
                    "unmapped Qt key identifier dropped"
                );
                return;
            };
            // A lock-key down edge toggles state itself. Synchronizing that same
            // lock before the edge would toggle twice, so skip it until key-up.
            let lock = key.lock_vk();
            if lock != Some(VK_CAPITAL) {
                self.sync_lock_state(VK_CAPITAL, msg.caps_lock_on, "caps");
            }
            if lock != Some(VK_NUMLOCK) {
                self.sync_lock_state(VK_NUMLOCK, msg.num_lock_on, "num");
            }
            if lock != Some(VK_SCROLL) {
                self.sync_lock_state(VK_SCROLL, msg.scroll_lock_on, "scroll");
            }
            self.sync_modifiers(msg.modifiers);

            if !self.inject_key_edge(key, msg.pressed) {
                tracing::debug!(
                    target: INPUT,
                    qt_key = msg.scan_code,
                    modifiers = msg.modifiers,
                    key = ?key,
                    pressed = msg.pressed,
                    sequence = msg.sequence,
                    "duplicate key edge dropped"
                );
                return;
            }
            if !msg.pressed {
                match lock {
                    Some(VK_CAPITAL) => self.sync_lock_state(VK_CAPITAL, msg.caps_lock_on, "caps"),
                    Some(VK_NUMLOCK) => self.sync_lock_state(VK_NUMLOCK, msg.num_lock_on, "num"),
                    Some(VK_SCROLL) => {
                        self.sync_lock_state(VK_SCROLL, msg.scroll_lock_on, "scroll")
                    }
                    _ => {}
                }
            }
            tracing::debug!(
                target: INPUT,
                qt_key = msg.scan_code,
                modifiers = msg.modifiers,
                key = ?key,
                pressed = msg.pressed,
                sequence = msg.sequence,
                "key_event"
            );
        }

        /// Types committed text, each character by its key on the
        /// foreground window's layout where that is proven, else as
        /// Unicode. Never logs the text.
        pub fn text_commit(&self, msg: &TextCommitMsg) {
            let layout = foreground_layout();
            let input_method =
                layout.is_some_and(|layout| is_input_method_layout(layout.0 as usize));
            let held = !self.keys_down.is_empty();
            // SAFETY: GetKeyState is read-only for a valid virtual-key value.
            let caps = unsafe { GetKeyState(i32::from(VK_CAPITAL)) } & 1 != 0;
            let (mut by_key, mut by_unicode) = (0_usize, 0_usize);
            let mut inputs = Vec::with_capacity(8);
            for character in msg.text.chars() {
                inputs.clear();
                let probe = layout.and_then(|layout| probe_layout(character, layout, caps));
                match committed_route(
                    character,
                    probe.map(|(probe, _)| probe),
                    layout.is_some(),
                    held,
                    input_method,
                ) {
                    CharRoute::Key { shift, altgr } => {
                        let (_, scan) = probe.expect("a key route has a probe");
                        // AltGr as a real AltGr key sends it: left Control
                        // and *right* Alt. Applications tell AltGr from a
                        // Control+Alt shortcut by the right Alt (Windows 11
                        // Notepad took left Control+left Alt+0 for Ctrl+0).
                        let modifiers: &[Set1Scancode] = match (shift, altgr) {
                            (false, false) => &[],
                            (true, false) => &[LEFT_SHIFT],
                            (false, true) => &[LEFT_CONTROL, RIGHT_ALT],
                            (true, true) => &[LEFT_SHIFT, LEFT_CONTROL, RIGHT_ALT],
                        };
                        for modifier in modifiers {
                            inputs.push(Self::host_key_input(HostKey::Scan(*modifier), true));
                        }
                        inputs.push(Self::host_key_input(HostKey::Scan(scan), true));
                        inputs.push(Self::host_key_input(HostKey::Scan(scan), false));
                        for modifier in modifiers.iter().rev() {
                            inputs.push(Self::host_key_input(HostKey::Scan(*modifier), false));
                        }
                        by_key += 1;
                    }
                    CharRoute::Text => {
                        let mut units = [0_u16; 2];
                        for unit in character.encode_utf16(&mut units) {
                            inputs.push(Self::key_input(0, *unit, KEYEVENTF_UNICODE));
                            inputs.push(Self::key_input(
                                0,
                                *unit,
                                KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                            ));
                        }
                        by_unicode += 1;
                    }
                }
                // One character's edges in one call: nothing interleaves.
                Self::send(&inputs);
            }
            tracing::debug!(
                target: INPUT,
                chars = by_key + by_unicode,
                by_key,
                by_unicode,
                held,
                input_method,
                layout_known = layout.is_some(),
                caps,
                "text_commit"
            );
        }

        /// Release every key we believe is held (stuck-key guard): scan code
        /// keys by their scan code, `AltGr` included (Windows releases the
        /// left Control it added with it).
        pub fn reset_modifiers(&mut self) {
            for key in self.keys_down.drain().collect::<Vec<_>>() {
                self.mask_before(key, false);
                Self::send(&[Self::host_key_input(key, false)]);
            }
            tracing::info!(target: INPUT, "reset_modifiers");
        }

        /// Release any held buttons/keys — call on disconnect.
        pub fn close(&mut self) {
            for button in self.buttons_down.drain().collect::<Vec<_>>() {
                let flag = match button {
                    1 => MOUSEEVENTF_LEFTUP,
                    2 => MOUSEEVENTF_MIDDLEUP,
                    3 => MOUSEEVENTF_RIGHTUP,
                    _ => continue,
                };
                Self::send(&[Self::mouse_input(0, 0, 0, flag)]);
            }
            self.reset_modifiers();
        }
    }

    impl Drop for Injector {
        fn drop(&mut self) {
            if self.buttons_down.is_empty() && self.keys_down.is_empty() {
                return;
            }
            tracing::warn!(
                target: INPUT,
                buttons = self.buttons_down.len(),
                keys = self.keys_down.len(),
                "injector dropped with held input; releasing through RAII cleanup"
            );
            self.close();
        }
    }

    /// The keyboard layout of the window that has keyboard focus (layouts
    /// are per thread on Windows); `None` when there is no such window or
    /// its thread is unknown. Never this process's own layout, which says
    /// nothing about the window being typed into.
    fn foreground_layout() -> Option<HKL> {
        // SAFETY: read-only queries of the foreground window and its thread.
        unsafe {
            let window = GetForegroundWindow();
            let foreground = !window.0.is_null();
            let thread = if foreground {
                GetWindowThreadProcessId(window, None)
            } else {
                0
            };
            layout_thread(foreground, thread).map(|thread| GetKeyboardLayout(thread))
        }
    }

    /// Where `layout` types `character`, and what that key types now with
    /// Caps Lock as it is: `None` when the layout has no key for it.
    fn probe_layout(
        character: char,
        layout: HKL,
        caps: bool,
    ) -> Option<(LayoutProbe, Set1Scancode)> {
        const VK_SHIFT: usize = 0x10;
        const VK_CONTROL: usize = 0x11;
        const VK_MENU: usize = 0x12;
        const VK_LSHIFT: usize = 0xA0;
        const VK_LCONTROL: usize = 0xA2;
        const VK_LMENU: usize = 0xA4;
        /// `ToUnicodeEx`: leave the keyboard's dead-key state alone
        /// (Windows 10 1607 and later).
        const DO_NOT_CHANGE_STATE: u32 = 0x4;
        let unit = u16::try_from(u32::from(character)).ok()?;
        // SAFETY: read-only layout queries for a valid layout handle.
        let scanned = unsafe { VkKeyScanExW(unit, layout) };
        if scanned == -1 {
            return None;
        }
        let [vk, shift_state] = scanned.to_le_bytes();
        // SAFETY: as above.
        let mapped = unsafe { MapVirtualKeyExW(u32::from(vk), MAPVK_VK_TO_VSC_EX, layout) };
        let scan = scan_from_mapped(mapped)?;
        let mut state = [0_u8; 256];
        if caps {
            state[usize::from(VK_CAPITAL)] = 0x01;
        }
        if shift_state & 1 != 0 {
            state[VK_SHIFT] = 0x80;
            state[VK_LSHIFT] = 0x80;
        }
        if shift_state & 2 != 0 {
            state[VK_CONTROL] = 0x80;
            state[VK_LCONTROL] = 0x80;
        }
        if shift_state & 4 != 0 {
            state[VK_MENU] = 0x80;
            state[VK_LMENU] = 0x80;
        }
        let mut typed = [0_u16; 8];
        // SAFETY: valid buffers; the flag keeps the kernel's keyboard state
        // (dead keys) untouched.
        let count = unsafe {
            ToUnicodeEx(
                u32::from(vk),
                u32::from(scan.code),
                &state,
                &mut typed,
                DO_NOT_CHANGE_STATE,
                layout,
            )
        };
        let types = (count == 1)
            .then(|| char::from_u32(u32::from(typed[0])))
            .flatten();
        Some((
            LayoutProbe {
                level: layout_level(shift_state),
                types,
            },
            scan,
        ))
    }

    pub fn set_dpi_awareness() -> &'static str {
        // SAFETY: process-wide, set-once; harmless to call.
        match unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
            Ok(()) => "per-monitor-v2",
            Err(_) => "already-set-or-unsupported",
        }
    }

    /// Legal `POINTER_FLAG_*` combination for one state-machine transition,
    /// per the public Windows Ink synthetic-pointer documentation.
    /// `NoChange` is handled by the caller before this is ever reached.
    fn pointer_flags_for(edge: PenPointerEdge) -> POINTER_FLAGS {
        match edge {
            PenPointerEdge::NoChange => POINTER_FLAG_NONE,
            PenPointerEdge::Hover => POINTER_FLAG_INRANGE,
            PenPointerEdge::ContactDown => {
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN
            }
            PenPointerEdge::ContactMove => {
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE
            }
            PenPointerEdge::ContactUp => POINTER_FLAG_INRANGE | POINTER_FLAG_UP,
            // Left proximity without ever touching: no flags at all, matching
            // "outside detection range" (no INRANGE bit set).
            PenPointerEdge::HoverLeave => POINTER_FLAG_NONE,
            // Left proximity and ended contact in the same frame: UP only,
            // deliberately without INRANGE since range was also lost.
            PenPointerEdge::ContactLeave => POINTER_FLAG_UP,
        }
    }

    /// RAII wrapper around one Windows 10 1809+ synthetic `PT_PEN` pointer
    /// device, created via the official `CreateSyntheticPointerDevice` /
    /// `InjectSyntheticPointerInput` / `DestroySyntheticPointerDevice` API.
    /// Device creation doubles as the capability probe: if it fails (older
    /// Windows, API disabled/unavailable), the caller must advertise pen as
    /// Unavailable and keep the mouse fallback, per `Injector`. The device is
    /// created and destroyed only in the interactive user-session agent —
    /// never in a LocalSystem service — same as `Injector`.
    pub struct PenInjector {
        device: HSYNTHETICPOINTERDEVICE,
        output: DesktopRect,
        /// Top-left of the virtual screen, in desktop pixels.
        virtual_origin: (i32, i32),
        state: PenPointerState,
        pointer_id: u32,
        frame_id: u32,
    }

    impl PenInjector {
        pub fn new(output_index: u32, expected_output: DesktopRect) -> Result<Self, String> {
            let (output, virtual_desktop) =
                resolve_pointer_geometry(output_index, expected_output)?;
            // SAFETY: `PT_PEN`/`1`/`POINTER_FEEDBACK_DEFAULT` have no aliasing
            // or lifetime preconditions; this call is itself the honest
            // Windows-version/Windows-Ink-availability probe.
            let device =
                unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
                    .map_err(|error| {
                        format!("CreateSyntheticPointerDevice(PT_PEN) failed: {error}")
                    })?;
            tracing::info!(
                target: INPUT,
                output_index,
                "synthetic PT_PEN pointer device created"
            );
            Ok(Self {
                device,
                output,
                virtual_origin: (virtual_desktop.left, virtual_desktop.top),
                state: PenPointerState::released(),
                pointer_id: 0,
                frame_id: 0,
            })
        }

        pub fn new_region_desktop(expected: DesktopRect) -> Result<Self, String> {
            let output = resolve_region_desktop(expected)?;
            // SAFETY: same validated PT_PEN construction as `new`.
            let device =
                unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
                    .map_err(|error| {
                        format!("CreateSyntheticPointerDevice(PT_PEN) failed: {error}")
                    })?;
            tracing::info!(target: INPUT, "multi-monitor synthetic PT_PEN device created");
            Ok(Self {
                device,
                output,
                // The committed region desktop is the virtual screen.
                virtual_origin: (output.left, output.top),
                state: PenPointerState::released(),
                pointer_id: 0,
                frame_id: 0,
            })
        }

        /// Validates the transition, maps normalized position/pressure/tilt/
        /// rotation to Windows Ink units, and injects — or silently skips
        /// injection if the state machine says nothing legally changed.
        pub fn dispatch(&mut self, msg: &PenEventMsg) {
            let Some((x, y)) = pen_pixel_location(msg.x, msg.y, self.output) else {
                tracing::warn!(
                    target: INPUT,
                    "pen event dropped: selected output geometry unavailable"
                );
                return;
            };
            self.dispatch_sample(
                x,
                y,
                msg.pressure,
                msg.rotation_degrees,
                msg.tilt_x_degrees,
                msg.tilt_y_degrees,
                msg.tool,
                msg.in_proximity,
                msg.touching,
                msg.buttons,
                msg.sequence,
                "pen_event",
            );
        }

        /// Injects one shared-region pen sample at the already mapped native
        /// `ptPixelLocation`.
        pub fn dispatch_region(&mut self, mapped: MappedRegionPen, sequence: u64) {
            let tool = match mapped.sample.tool {
                arcen_input::PenTool::Tip => PenToolMsg::Tip,
                arcen_input::PenTool::Eraser => PenToolMsg::Eraser,
            };
            self.dispatch_sample(
                mapped.position.desktop.x,
                mapped.position.desktop.y,
                mapped.sample.pressure,
                mapped.sample.rotation_degrees,
                mapped.sample.tilt_x_degrees,
                mapped.sample.tilt_y_degrees,
                tool,
                mapped.sample.in_proximity,
                mapped.sample.touching,
                mapped.sample.buttons,
                sequence,
                "region_pen_event",
            );
        }

        #[allow(clippy::too_many_arguments)]
        fn dispatch_sample(
            &mut self,
            x: i32,
            y: i32,
            pressure: f32,
            rotation_degrees: f32,
            tilt_x_degrees: f32,
            tilt_y_degrees: f32,
            tool: PenToolMsg,
            in_proximity: bool,
            touching: bool,
            buttons: u16,
            sequence: u64,
            source: &'static str,
        ) {
            let edge = self.state.advance(in_proximity, touching);
            if edge == PenPointerEdge::NoChange {
                return;
            }
            // Level 3 diagnostic: log every Windows Ink injection for E2E
            // tracing and new-device bring-up. Enable with level=trace.
            tracing::trace!(
                target: INPUT,
                x_px = x,
                y_px = y,
                pressure,
                tilt_x = tilt_x_degrees,
                tilt_y = tilt_y_degrees,
                in_proximity,
                touching,
                sequence,
                source,
                ?edge,
                "pen_inject: InjectSyntheticPointerInput"
            );
            let ink_flags = pen_ink_flags(tool, buttons, edge);
            self.inject(
                edge,
                x,
                y,
                pressure,
                rotation_degrees,
                tilt_x_degrees,
                tilt_y_degrees,
                ink_flags,
            );
        }

        #[allow(clippy::too_many_arguments)]
        fn inject(
            &mut self,
            edge: PenPointerEdge,
            x: i32,
            y: i32,
            pressure: f32,
            rotation_degrees: f32,
            tilt_x_degrees: f32,
            tilt_y_degrees: f32,
            ink_flags: super::PenInkFlags,
        ) {
            self.frame_id = self.frame_id.wrapping_add(1);
            let pointer_flags = pointer_flags_for(edge);
            let mut pen_flags = PEN_FLAG_NONE;
            if ink_flags.inverted {
                pen_flags |= PEN_FLAG_INVERTED;
            }
            if ink_flags.eraser {
                pen_flags |= PEN_FLAG_ERASER;
            }
            if ink_flags.barrel {
                pen_flags |= PEN_FLAG_BARREL;
            }
            let (x, y) = pen_injection_point((x, y), self.virtual_origin);

            let pointer_info = POINTER_INFO {
                pointerType: PT_PEN,
                pointerId: self.pointer_id,
                frameId: self.frame_id,
                pointerFlags: pointer_flags,
                ptPixelLocation: POINT { x, y },
                ptPixelLocationRaw: POINT { x, y },
                ..Default::default()
            };
            let pen_info = POINTER_PEN_INFO {
                pointerInfo: pointer_info,
                penFlags: pen_flags,
                penMask: PEN_MASK_PRESSURE | PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                pressure: pen_pressure_to_windows(pressure),
                rotation: pen_rotation_to_windows(rotation_degrees),
                tiltX: pen_tilt_to_windows(tilt_x_degrees),
                tiltY: pen_tilt_to_windows(tilt_y_degrees),
            };
            let type_info = POINTER_TYPE_INFO {
                r#type: PT_PEN,
                Anonymous: POINTER_TYPE_INFO_0 { penInfo: pen_info },
            };
            // SAFETY: `type_info` is a single, fully-initialized
            // `POINTER_TYPE_INFO` describing a `PT_PEN` sample; the device
            // handle was created above and outlives this call.
            if let Err(error) = unsafe { InjectSyntheticPointerInput(self.device, &[type_info]) } {
                tracing::warn!(target: INPUT, %error, ?edge, "InjectSyntheticPointerInput failed");
            }
        }

        /// Sends the legal terminal edge (if any state is held) so the
        /// digitizer never appears "stuck" in proximity/contact. Used by
        /// `Drop` and by explicit reset/disconnect/reconnect handling.
        pub fn release(&mut self) {
            let Some(edge) = self.state.release() else {
                return;
            };
            let Some((x, y)) = pen_pixel_location(0.0, 0.0, self.output) else {
                return;
            };
            self.inject(
                edge,
                x,
                y,
                0.0,
                0.0,
                0.0,
                0.0,
                pen_ink_flags(PenToolMsg::Tip, 0, edge),
            );
        }
    }

    impl Drop for PenInjector {
        fn drop(&mut self) {
            self.release();
            // SAFETY: `self.device` was created by `CreateSyntheticPointerDevice`
            // in `new` and is destroyed at most once, here, on drop.
            unsafe { DestroySyntheticPointerDevice(self.device) };
        }
    }

    #[cfg(test)]
    mod windows_only_tests {
        use super::*;

        // Regression guard: `pointer_flags_for` is a free function in this
        // module, not an associated function of `PenInjector` — it must be
        // called unqualified (as here and in `PenInjector::inject`), never
        // as `Self::pointer_flags_for`/`PenInjector::pointer_flags_for`. If
        // someone reintroduces the `Self::` typo inside `inject`, this test
        // still compiles fine on its own, but `inject`'s erroneous call
        // fails with E0599 on any Windows build/check — this test instead
        // pins the exact flag combinations so a refactor cannot silently
        // change them while "fixing" the call site.
        #[test]
        fn pointer_flags_for_matches_documented_combinations() {
            assert_eq!(
                pointer_flags_for(PenPointerEdge::NoChange),
                POINTER_FLAG_NONE
            );
            assert_eq!(
                pointer_flags_for(PenPointerEdge::Hover),
                POINTER_FLAG_INRANGE
            );
            assert_eq!(
                pointer_flags_for(PenPointerEdge::ContactDown),
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN
            );
            assert_eq!(
                pointer_flags_for(PenPointerEdge::ContactMove),
                POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE
            );
            assert_eq!(
                pointer_flags_for(PenPointerEdge::ContactUp),
                POINTER_FLAG_INRANGE | POINTER_FLAG_UP
            );
            assert_eq!(
                pointer_flags_for(PenPointerEdge::HoverLeave),
                POINTER_FLAG_NONE
            );
            assert_eq!(
                pointer_flags_for(PenPointerEdge::ContactLeave),
                POINTER_FLAG_UP
            );
        }
    }
}

// ─────────────────────────────── non-Windows ───────────────────────────────

#[cfg(not(windows))]
pub fn wake_console_display() -> u32 {
    0
}

#[cfg(not(windows))]
pub struct Injector;

#[cfg(not(windows))]
#[allow(unused_variables)]
impl Injector {
    pub fn new(_output_index: u32, _expected_output: DesktopRect) -> Result<Self, String> {
        tracing::warn!(target: INPUT, "input injector stub on non-Windows build");
        Ok(Injector)
    }
    pub fn new_region_desktop(_expected: DesktopRect) -> Result<Self, String> {
        tracing::warn!(target: INPUT, "input injector stub on non-Windows build");
        Ok(Injector)
    }
    pub fn move_abs(&self, msg: &MouseMoveMsg) {}
    pub fn move_region(&self, mapped: MappedRegionPoint, sequence: u64) {}
    pub fn move_relative(&self, msg: &MouseMoveRelativeMsg) {}
    pub fn button(&mut self, msg: &MouseButtonMsg) {}
    pub fn button_region(&mut self, mapped: MappedRegionButton, sequence: u64) {}
    pub fn scroll(&self, msg: &MouseScrollMsg) {}
    pub fn scroll_region(&self, mapped: MappedRegionScroll, sequence: u64) {}
    pub fn key_event(&mut self, msg: &KeyEventMsg) {}
    pub fn text_commit(&self, msg: &TextCommitMsg) {}
    pub fn reset_modifiers(&mut self) {}
    pub fn close(&mut self) {}
}

/// Non-Windows stub: always reports the digitizer as unavailable. This is
/// honest, not a placeholder — the real backend also reports `Err` on
/// pre-1809 Windows/API failure via `CreateSyntheticPointerDevice`, so
/// callers already handle "no pen" as the safe, expected outcome.
#[cfg(not(windows))]
pub struct PenInjector;

#[cfg(not(windows))]
#[allow(unused_variables)]
impl PenInjector {
    pub fn new(_output_index: u32, _expected_output: DesktopRect) -> Result<Self, String> {
        Err("synthetic pen pointer devices are only available on Windows".to_string())
    }
    pub fn new_region_desktop(_expected: DesktopRect) -> Result<Self, String> {
        Err("synthetic pen pointer devices are only available on Windows".to_string())
    }
    pub fn dispatch(&mut self, msg: &PenEventMsg) {}
    pub fn dispatch_region(&mut self, mapped: MappedRegionPen, sequence: u64) {}
    pub fn release(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::{
        committed_route, display_wake_plan, is_identity_dxgi_rotation, is_input_method_layout,
        layout_level, layout_thread, modifier_targets, norm_to_abs, pen_injection_point,
        pen_ink_flags, pen_pixel_location, pen_pressure_to_windows, pen_rotation_to_windows,
        pen_tilt_to_windows, prepends_absolute_position, qt_key_to_host_key, qt_key_to_vk,
        qt_key_to_windows, relative_move_plan, scan_from_mapped, selected_output_to_virtual_abs,
        wheel_delta_from_scroll, DesktopRect, HostKey, MetaReleaseMask, PenPointerEdge,
        PenPointerState, MOD_ALT, MOD_CTRL, MOD_KEYPAD, MOD_META, MOD_SHIFT, VK_CAPITAL,
    };
    use arcen_input::keyboard::scancode::{
        Set1Scancode, LEFT_ALT, LEFT_CONTROL, LEFT_META, LEFT_SHIFT, RIGHT_ALT,
    };
    use arcen_protocol::keys::{KEY_ALTGR, KEY_INTL_BACKSLASH};
    use arcen_protocol::messages::{
        MouseMoveRelativeMsg, PenToolMsg, PointerMotionMode, ScrollUnitMsg,
    };

    #[test]
    fn norm_maps_full_range() {
        assert_eq!(norm_to_abs(0.0), 0);
        assert_eq!(norm_to_abs(1.0), 65535);
        assert_eq!(norm_to_abs(0.5), 32767);
    }

    #[test]
    fn norm_clamps_out_of_range() {
        assert_eq!(norm_to_abs(-0.5), 0);
        assert_eq!(norm_to_abs(2.0), 65535);
    }

    #[test]
    fn display_wake_is_a_relative_move_of_zero_pixels() {
        let plan = display_wake_plan();
        assert_eq!((plan.dx, plan.dy), (0, 0));
        assert!(plan.move_flag);
        assert!(!plan.absolute_flag);
        assert!(!plan.virtual_desktop_flag);
    }

    #[test]
    fn relative_sendinput_plan_has_move_only_flags() {
        let plan = relative_move_plan(&MouseMoveRelativeMsg {
            dx: -12,
            dy: 7,
            ..MouseMoveRelativeMsg::default()
        });
        assert_eq!((plan.dx, plan.dy), (-12, 7));
        assert!(plan.move_flag);
        assert!(!plan.absolute_flag);
        assert!(!plan.virtual_desktop_flag);
    }

    #[test]
    fn relative_edges_and_wheels_do_not_prepend_absolute_warp() {
        assert!(prepends_absolute_position(PointerMotionMode::Absolute));
        assert!(!prepends_absolute_position(PointerMotionMode::Relative));
    }

    #[test]
    fn wheel_delta_preserves_horizontal_fractions_for_point_scroll() {
        assert_eq!(wheel_delta_from_scroll(0.5, ScrollUnitMsg::Line), 60);
        assert_eq!(wheel_delta_from_scroll(-0.5, ScrollUnitMsg::Line), -60);
        assert_eq!(wheel_delta_from_scroll(30.4, ScrollUnitMsg::Point), 30);
        assert_eq!(wheel_delta_from_scroll(-0.25, ScrollUnitMsg::Point), -1);
        assert_eq!(wheel_delta_from_scroll(f64::NAN, ScrollUnitMsg::Point), 0);
    }

    #[test]
    fn primary_output_maps_to_full_virtual_desktop() {
        let primary = DesktopRect {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            selected_output_to_virtual_abs(0.0, 0.0, primary, primary).unwrap(),
            (0, 0)
        );
        assert_eq!(
            selected_output_to_virtual_abs(1.0, 1.0, primary, primary).unwrap(),
            (65535, 65535)
        );
    }

    #[test]
    fn positive_secondary_maps_into_virtual_desktop() {
        let virtual_desktop = DesktopRect {
            left: 0,
            top: 0,
            width: 3840,
            height: 1080,
        };
        let secondary = DesktopRect {
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            selected_output_to_virtual_abs(0.0, 0.0, secondary, virtual_desktop).unwrap(),
            (32776, 0)
        );
        assert_eq!(
            selected_output_to_virtual_abs(1.0, 1.0, secondary, virtual_desktop).unwrap(),
            (65535, 65535)
        );
    }

    #[test]
    fn negative_origin_output_maps_from_virtual_zero() {
        let virtual_desktop = DesktopRect {
            left: -1920,
            top: 0,
            width: 3840,
            height: 1080,
        };
        let negative = DesktopRect {
            left: -1920,
            top: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            selected_output_to_virtual_abs(0.0, 0.0, negative, virtual_desktop).unwrap(),
            (0, 0)
        );
        assert_eq!(
            selected_output_to_virtual_abs(1.0, 1.0, negative, virtual_desktop).unwrap(),
            (32758, 65535)
        );
    }

    #[test]
    fn pointer_mapping_clamps_normalized_endpoints() {
        let desktop = DesktopRect {
            left: -100,
            top: -50,
            width: 200,
            height: 100,
        };
        assert_eq!(
            selected_output_to_virtual_abs(-1.0, -1.0, desktop, desktop).unwrap(),
            (0, 0)
        );
        assert_eq!(
            selected_output_to_virtual_abs(2.0, 2.0, desktop, desktop).unwrap(),
            (65535, 65535)
        );
    }

    #[test]
    fn only_identity_dxgi_rotation_is_accepted() {
        assert!(is_identity_dxgi_rotation(1));
        for rotation in [0, 2, 3, 4] {
            assert!(!is_identity_dxgi_rotation(rotation));
        }
    }

    #[test]
    fn protocol_v3_key_field_is_qt_not_evdev() {
        assert_eq!(qt_key_to_vk(0x41, 0), Some(b'A' as u16));
        assert_eq!(qt_key_to_vk(30, 0), None);
    }

    #[test]
    fn maps_qt_navigation_function_and_punctuation_keys() {
        assert_eq!(qt_key_to_vk(0x0100_0012, 0), Some(0x25)); // Left
        assert_eq!(qt_key_to_vk(0x0100_0014, 0), Some(0x27)); // Right
        assert_eq!(qt_key_to_vk(0x0100_0030, 0), Some(0x70)); // F1
        assert_eq!(qt_key_to_vk(0x0100_003B, 0), Some(0x7B)); // F12
        assert_eq!(qt_key_to_vk(0x5B, 0), Some(0xDB)); // [
        assert_eq!(qt_key_to_vk(0x5D, 0), Some(0xDD)); // ]
    }

    #[test]
    fn maps_shifted_qt_symbol_variants_to_base_windows_keys() {
        for (qt_key, vk) in [
            (0x2B, 0xBB), // +
            (0x5F, 0xBD), // _
            (0x3A, 0xBA), // :
            (0x22, 0xDE), // "
            (0x7B, 0xDB), // {
            (0x7C, 0xDC), // |
            (0x7D, 0xDD), // }
            (0x3F, 0xBF), // ?
            (0x21, 0x31), // !
        ] {
            assert_eq!(qt_key_to_vk(qt_key, MOD_SHIFT), Some(vk));
        }
    }

    #[test]
    fn maps_qt_keypad_digits_with_compact_modifier() {
        assert_eq!(qt_key_to_vk(0x30, MOD_KEYPAD), Some(0x60));
        assert_eq!(qt_key_to_vk(0x39, MOD_KEYPAD), Some(0x69));
        assert_eq!(qt_key_to_vk(0x2B, MOD_KEYPAD), Some(0x6B));
    }

    #[test]
    fn preserves_keypad_enter_extended_identity() {
        assert_eq!(qt_key_to_windows(0x0100_0004, 0), Some((0x0D, false)));
        assert_eq!(
            qt_key_to_windows(0x0100_0005, MOD_KEYPAD),
            Some((0x0D, true))
        );
    }

    #[test]
    fn compact_modifier_mask_maps_to_left_modifier_scancodes() {
        assert_eq!(
            modifier_targets(MOD_SHIFT | MOD_CTRL | MOD_META),
            [
                (HostKey::Scan(LEFT_SHIFT), true),
                (HostKey::Scan(LEFT_CONTROL), true),
                (HostKey::Scan(LEFT_ALT), false),
                (HostKey::Scan(LEFT_META), true),
            ]
        );
        // The mask never names AltGr or the Control Windows adds to it.
        for mask in 0..=(MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META) {
            assert!(modifier_targets(mask)
                .iter()
                .all(|(key, _)| *key != HostKey::Scan(RIGHT_ALT)));
        }
    }

    #[test]
    fn a_windows_key_used_only_with_the_pointer_is_masked_on_release() {
        let meta = HostKey::Scan(LEFT_META);
        let mask = MetaReleaseMask::default();
        // Windows+click, then release: masked.
        assert!(!mask.key_edge(meta, true));
        mask.pointer();
        assert!(mask.key_edge(meta, false));
        // A lone tap still opens Start.
        assert!(!mask.key_edge(meta, true));
        assert!(!mask.key_edge(meta, false));
        // Windows+click+E: E already counts as the other key.
        mask.key_edge(meta, true);
        mask.pointer();
        assert!(!mask.key_edge(HostKey::Scan(LEFT_CONTROL), true));
        assert!(!mask.key_edge(meta, false));
        // A click before the Windows key goes down does not count.
        mask.pointer();
        mask.key_edge(meta, true);
        assert!(!mask.key_edge(meta, false));
    }

    #[test]
    fn layout_levels_and_scan_codes_for_committed_characters() {
        use arcen_input::text::LayoutLevel;
        assert_eq!(layout_level(0), LayoutLevel::Base);
        assert_eq!(layout_level(1), LayoutLevel::Shift);
        assert_eq!(layout_level(6), LayoutLevel::AltGr);
        assert_eq!(layout_level(7), LayoutLevel::ShiftAltGr);
        // Control alone, Alt alone, kana shifts: shortcuts, never typed.
        for state in [2, 3, 4, 5, 8, 0x10, 0x20] {
            assert_eq!(layout_level(state), LayoutLevel::Other, "{state:#x}");
        }
        assert_eq!(
            scan_from_mapped(0x1E),
            Some(Set1Scancode {
                code: 0x1E,
                extended: false
            })
        );
        assert_eq!(
            scan_from_mapped(0xE035),
            Some(Set1Scancode {
                code: 0x35,
                extended: true
            })
        );
        assert_eq!(scan_from_mapped(0), None);
        assert_eq!(scan_from_mapped(0xE11D), None);
    }

    #[test]
    fn without_a_foreground_window_every_character_goes_as_unicode() {
        use arcen_input::text::{CharRoute, LayoutLevel, LayoutProbe};
        assert_eq!(layout_thread(false, 0), None);
        assert_eq!(layout_thread(true, 0), None);
        assert_eq!(layout_thread(true, 4242), Some(4242));
        // The Pier thread's own layout would "prove" '@' at Shift+2 (US)
        // while the window being typed into is Norwegian: unknown layout,
        // so Unicode.
        let stale = Some(LayoutProbe {
            level: LayoutLevel::Shift,
            types: Some('@'),
        });
        assert_eq!(
            committed_route('@', stale, false, false, false),
            CharRoute::Text
        );
        assert_eq!(
            committed_route('@', stale, true, false, false),
            CharRoute::Key {
                shift: true,
                altgr: false
            }
        );
    }

    #[test]
    fn input_method_layouts_never_type_by_key() {
        // en-US, nb-NO, de-DE.
        for layout in [0x0409_0409_usize, 0x0414_0414, 0x0407_0407] {
            assert!(!is_input_method_layout(layout), "{layout:#x}");
        }
        // ja-JP, zh-CN, ko-KR, a legacy IME handle.
        for layout in [0x0411_0411_usize, 0x0804_0804, 0x0412_0412, 0xE001_0411] {
            assert!(is_input_method_layout(layout), "{layout:#x}");
        }
    }

    #[test]
    fn keys_inject_by_position_scancode() {
        let scan = |code, extended| Some(HostKey::Scan(Set1Scancode { code, extended }));
        assert_eq!(qt_key_to_host_key(0x41, 0), scan(0x1E, false));
        assert_eq!(qt_key_to_host_key(0x32, MOD_SHIFT), scan(0x03, false));
        assert_eq!(qt_key_to_host_key(0x27, 0), scan(0x28, false));
        assert_eq!(
            qt_key_to_host_key(KEY_ALTGR, 0),
            Some(HostKey::Scan(RIGHT_ALT))
        );
        assert_eq!(qt_key_to_host_key(KEY_INTL_BACKSLASH, 0), scan(0x56, false));
        assert_eq!(qt_key_to_host_key(0x0100_0022, MOD_META), scan(0x5B, true));
        assert_eq!(qt_key_to_host_key(0x0100_0016, 0), scan(0x49, true));
    }

    #[test]
    fn keypad_and_lock_keys_keep_their_virtual_keys() {
        assert_eq!(
            qt_key_to_host_key(0x31, MOD_KEYPAD),
            Some(HostKey::Vk {
                vk: 0x61,
                extended: false
            })
        );
        let caps = qt_key_to_host_key(0x0100_0024, 0).unwrap();
        assert_eq!(caps.lock_vk(), Some(VK_CAPITAL));
        assert_eq!(qt_key_to_host_key(0x41, 0).unwrap().lock_vk(), None);
    }

    #[test]
    fn every_virtual_key_outside_keypad_locks_and_print_has_a_scancode() {
        for qt_key in (0x20..0x80).chain(0x0100_0000..0x0100_0040) {
            let Some((vk, _)) = qt_key_to_windows(qt_key, 0) else {
                continue;
            };
            if matches!(vk, 0x13 | 0x2C | 0x0C | 0x14 | 0x90 | 0x91) {
                continue;
            }
            assert!(
                matches!(qt_key_to_host_key(qt_key, 0), Some(HostKey::Scan(_))),
                "Qt key {qt_key:#x} (VK {vk:#x}) has no scan code"
            );
        }
    }

    #[test]
    fn maps_flame_critical_qt_keys() {
        for qt_key in [
            0x20,
            0x5B,
            0x5D,
            0x57,
            0x45,
            0x52,
            0x0100_0012,
            0x0100_0014,
            0x0100_0038,
        ] {
            assert!(qt_key_to_vk(qt_key, 0).is_some(), "Qt key {qt_key:#x}");
        }
    }

    // ───────────────────────────── pen mapping ─────────────────────────────

    #[test]
    fn pen_pressure_maps_full_range_and_clamps() {
        assert_eq!(pen_pressure_to_windows(0.0), 0);
        assert_eq!(pen_pressure_to_windows(1.0), 1024);
        assert_eq!(pen_pressure_to_windows(0.5), 512);
        assert_eq!(pen_pressure_to_windows(-1.0), 0);
        assert_eq!(pen_pressure_to_windows(2.0), 1024);
    }

    #[test]
    fn pen_rotation_maps_zero_to_below_360_and_wraps_360_to_zero() {
        assert_eq!(pen_rotation_to_windows(0.0), 0);
        assert_eq!(pen_rotation_to_windows(359.4), 359);
        assert_eq!(pen_rotation_to_windows(360.0), 0);
        assert_eq!(pen_rotation_to_windows(-10.0), 0);
        assert_eq!(pen_rotation_to_windows(400.0), 0);
    }

    #[test]
    fn pen_tilt_passes_through_and_clamps() {
        assert_eq!(pen_tilt_to_windows(0.0), 0);
        assert_eq!(pen_tilt_to_windows(-90.0), -90);
        assert_eq!(pen_tilt_to_windows(90.0), 90);
        assert_eq!(pen_tilt_to_windows(-200.0), -90);
        assert_eq!(pen_tilt_to_windows(200.0), 90);
    }

    #[test]
    fn pen_pixel_location_maps_normalized_corners_to_output_pixels() {
        let output = DesktopRect {
            left: 1920,
            top: 0,
            width: 1280,
            height: 1024,
        };
        assert_eq!(pen_pixel_location(0.0, 0.0, output), Some((1920, 0)));
        assert_eq!(pen_pixel_location(1.0, 1.0, output), Some((3199, 1023)));
        assert_eq!(pen_pixel_location(0.5, 0.5, output), Some((2560, 512)));
    }

    #[test]
    fn pen_pixel_location_clamps_out_of_range_normalized_input() {
        let output = DesktopRect {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(pen_pixel_location(-1.0, -1.0, output), Some((0, 0)));
        assert_eq!(pen_pixel_location(2.0, 2.0, output), Some((1919, 1079)));
    }

    #[test]
    fn pen_pixel_location_rejects_non_positive_output_geometry() {
        let degenerate = DesktopRect {
            left: 0,
            top: 0,
            width: 0,
            height: 1080,
        };
        assert_eq!(pen_pixel_location(0.5, 0.5, degenerate), None);
    }

    #[test]
    fn an_eraser_end_is_inverted_in_range_and_erases_only_in_contact() {
        let hover = pen_ink_flags(PenToolMsg::Eraser, 0, PenPointerEdge::Hover);
        assert!(hover.inverted);
        assert!(!hover.eraser, "an eraser flag while hovering is a contact");
        for edge in [PenPointerEdge::ContactDown, PenPointerEdge::ContactMove] {
            let contact = pen_ink_flags(PenToolMsg::Eraser, 0, edge);
            assert!(contact.inverted && contact.eraser, "{edge:?}");
        }
        let lifted = pen_ink_flags(PenToolMsg::Eraser, 0, PenPointerEdge::ContactUp);
        assert!(lifted.inverted && !lifted.eraser);
        for edge in [PenPointerEdge::HoverLeave, PenPointerEdge::ContactLeave] {
            let gone = pen_ink_flags(PenToolMsg::Eraser, 0b11, edge);
            assert!(!gone.inverted && !gone.eraser && !gone.barrel, "{edge:?}");
        }
        let tip = pen_ink_flags(PenToolMsg::Tip, 0, PenPointerEdge::ContactMove);
        assert!(!tip.inverted && !tip.eraser && !tip.barrel);
    }

    #[test]
    fn either_barrel_button_presses_the_windows_ink_barrel() {
        for buttons in [0b01, 0b10, 0b11] {
            for edge in [
                PenPointerEdge::Hover,
                PenPointerEdge::ContactDown,
                PenPointerEdge::ContactMove,
                PenPointerEdge::ContactUp,
            ] {
                assert!(
                    pen_ink_flags(PenToolMsg::Tip, buttons, edge).barrel,
                    "{buttons:#b} {edge:?}"
                );
            }
        }
        assert!(!pen_ink_flags(PenToolMsg::Tip, 0, PenPointerEdge::ContactDown).barrel);
    }

    #[test]
    fn pen_injection_points_are_relative_to_the_virtual_screen() {
        // A Deck display left of and below the primary: virtual screen
        // (-1800, 0) 4360x1962, display (-1800, 832) 1800x1130.
        assert_eq!(pen_injection_point((-901, 1396), (-1800, 0)), (899, 1396));
        assert_eq!(pen_injection_point((-1800, 832), (-1800, 0)), (0, 832));
        assert_eq!(pen_injection_point((1279, 719), (-1800, 0)), (3079, 719));
        // A display above the primary.
        assert_eq!(pen_injection_point((10, -500), (0, -1080)), (10, 580));
        // The usual single-display case is unchanged.
        assert_eq!(pen_injection_point((1799, 1129), (0, 0)), (1799, 1129));
    }

    // ─────────────────────── pen proximity/contact state ───────────────────

    #[test]
    fn pen_state_starts_released() {
        let mut state = PenPointerState::released();
        assert_eq!(state.advance(false, false), PenPointerEdge::NoChange);
    }

    #[test]
    fn pen_state_out_to_hover_is_hover() {
        let mut state = PenPointerState::released();
        assert_eq!(state.advance(true, false), PenPointerEdge::Hover);
    }

    #[test]
    fn pen_state_hover_to_hover_is_hover() {
        let mut state = PenPointerState::released();
        state.advance(true, false);
        assert_eq!(state.advance(true, false), PenPointerEdge::Hover);
    }

    #[test]
    fn pen_state_out_directly_to_contact_is_contact_down() {
        let mut state = PenPointerState::released();
        assert_eq!(state.advance(true, true), PenPointerEdge::ContactDown);
    }

    #[test]
    fn pen_state_hover_to_contact_is_contact_down() {
        let mut state = PenPointerState::released();
        state.advance(true, false);
        assert_eq!(state.advance(true, true), PenPointerEdge::ContactDown);
    }

    #[test]
    fn pen_state_contact_to_contact_is_contact_move() {
        let mut state = PenPointerState::released();
        state.advance(true, true);
        assert_eq!(state.advance(true, true), PenPointerEdge::ContactMove);
    }

    #[test]
    fn pen_state_contact_to_hover_is_contact_up() {
        let mut state = PenPointerState::released();
        state.advance(true, true);
        assert_eq!(state.advance(true, false), PenPointerEdge::ContactUp);
    }

    #[test]
    fn pen_state_hover_to_out_is_hover_leave() {
        let mut state = PenPointerState::released();
        state.advance(true, false);
        assert_eq!(state.advance(false, false), PenPointerEdge::HoverLeave);
    }

    #[test]
    fn pen_state_contact_to_out_is_contact_leave() {
        let mut state = PenPointerState::released();
        state.advance(true, true);
        assert_eq!(state.advance(false, false), PenPointerEdge::ContactLeave);
    }

    #[test]
    fn pen_state_touching_true_forces_in_proximity_even_if_peer_lies() {
        // Defensive normalization: a malformed peer claiming touching=true
        // while in_proximity=false must still legally enter contact, not be
        // silently dropped.
        let mut state = PenPointerState::released();
        assert_eq!(state.advance(false, true), PenPointerEdge::ContactDown);
    }

    #[test]
    fn pen_state_release_from_out_is_none() {
        let mut state = PenPointerState::released();
        assert_eq!(state.release(), None);
    }

    #[test]
    fn pen_state_release_from_hover_sends_hover_leave() {
        let mut state = PenPointerState::released();
        state.advance(true, false);
        assert_eq!(state.release(), Some(PenPointerEdge::HoverLeave));
        // Fully released; a second release is a no-op.
        assert_eq!(state.release(), None);
    }

    #[test]
    fn pen_state_release_from_contact_sends_contact_leave() {
        let mut state = PenPointerState::released();
        state.advance(true, true);
        assert_eq!(state.release(), Some(PenPointerEdge::ContactLeave));
        assert_eq!(state.release(), None);
    }

    #[test]
    fn pen_state_reconnect_after_release_behaves_like_fresh_device() {
        let mut state = PenPointerState::released();
        state.advance(true, true);
        state.release();
        // Reconnect: a fresh contact must again be a legal ContactDown, not
        // some stale/leftover transition.
        assert_eq!(state.advance(true, true), PenPointerEdge::ContactDown);
    }
}
