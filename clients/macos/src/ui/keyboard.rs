// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's keyboard adapter: egui key events to physical Mac keys, and
//! those through the shared keyboard policy
//! ([`arcen_input::keyboard::ShortcutTranslator`]) to the host's key edges.
//! Which host key Command, Control and Option become is decided there, per
//! host platform; this module only reads the Mac.

use std::collections::BTreeSet;

use arcen_input::keyboard::{
    DeckKey, DeckModifiers, HostKeyEdge, KeyboardPolicy, ShortcutTranslator,
};
use arcen_protocol::keys::{KEY_ALTGR, KEY_INTL_BACKSLASH};

use crate::protocol::keymap::{
    MODIFIER_BIT_ALT, MODIFIER_BIT_CTRL, MODIFIER_BIT_KEYPAD, MODIFIER_BIT_META,
    MODIFIER_BIT_SHIFT, MOD_ALT, MOD_CTRL, MOD_KEYPAD, MOD_META, MOD_SHIFT, QT_KEY_ALT,
    QT_KEY_CTRL, QT_KEY_F9, QT_KEY_META, QT_KEY_SHIFT,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyboardAction {
    /// A physical key edge, and the host key edges it became (none while
    /// the policy holds a key back, such as a lone Command for a Windows
    /// host).
    Emit {
        qt_key: u32,
        pressed: bool,
        edges: Vec<HostKeyEdge>,
    },
    /// A clipboard shortcut whose press the text system consumed: its
    /// whole press and release.
    RecoverClipboard {
        qt_key: u32,
        edges: Vec<HostKeyEdge>,
    },
    Reset {
        qt_key: u32,
        modifiers: u32,
        reason: &'static str,
    },
    Suppressed {
        qt_key: u32,
        modifiers: u32,
        reason: &'static str,
    },
    Ignored,
}

#[derive(Debug, Default)]
pub struct KeyboardInput {
    /// Physical keys held, by [`physical_id`].
    held_keys: BTreeSet<u32>,
    translator: ShortcutTranslator,
    frame: u64,
    focused: bool,
    raw_count: u64,
    emitted_count: u64,
    suppressed_count: u64,
}

/// The identifier a physical key is tracked by: its key identifier, with
/// the two Option keys told apart.
const fn physical_id(key: DeckKey) -> u32 {
    match key {
        DeckKey::Command => QT_KEY_META,
        DeckKey::Control => QT_KEY_CTRL,
        DeckKey::Shift => QT_KEY_SHIFT,
        DeckKey::LeftOption => QT_KEY_ALT,
        DeckKey::RightOption => KEY_ALTGR,
        DeckKey::Other(qt_key) => qt_key,
    }
}

impl KeyboardInput {
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.saturating_add(1);
    }

    pub fn update_focus(&mut self, focused: bool) -> bool {
        let lost_focus = self.focused && !focused;
        self.focused = focused;
        lost_focus
    }

    /// Applies a host's keyboard policy; returns the releases of whatever
    /// the old one held on the host.
    pub fn set_policy(&mut self, policy: KeyboardPolicy) -> Vec<HostKeyEdge> {
        self.translator.set_policy(policy)
    }

    #[cfg(test)]
    pub fn handle_event(&mut self, event: &egui::Event) -> KeyboardAction {
        self.handle_event_with_native_key(event, None)
    }

    pub fn handle_event_with_native_key(
        &mut self,
        event: &egui::Event,
        native: Option<NativeKeyMetadata>,
    ) -> KeyboardAction {
        let egui::Event::Key {
            key,
            physical_key,
            pressed,
            repeat,
            modifiers,
        } = event
        else {
            return KeyboardAction::Ignored;
        };

        self.raw_count = self.raw_count.saturating_add(1);
        let keypad = native.is_some_and(|metadata| metadata.keypad);
        let mut qt_modifiers = egui_modifiers_to_qt(*modifiers);
        if keypad {
            qt_modifiers |= MOD_KEYPAD;
        }
        let wire_modifiers = qt_modifiers_to_wire(qt_modifiers);
        let intl_backslash = self.translator.policy().intl_backslash;
        let deck_key = match native {
            Some(metadata) if metadata.keypad => Some(DeckKey::Other(metadata.qt_key)),
            Some(metadata) => mac_grave_or_iso_key(metadata.key_code, iso_keyboard())
                .map(|position| DeckKey::Other(position.qt_key(intl_backslash)))
                .or_else(|| egui_key_to_deck(physical_key.unwrap_or(*key), intl_backslash)),
            None => egui_key_to_deck(physical_key.unwrap_or(*key), intl_backslash),
        };
        let Some(deck_key) = deck_key else {
            self.suppressed_count = self.suppressed_count.saturating_add(1);
            return KeyboardAction::Suppressed {
                qt_key: 0,
                modifiers: wire_modifiers,
                reason: "unmapped",
            };
        };
        let qt_key = physical_id(deck_key);

        if qt_key == QT_KEY_F9 {
            self.suppressed_count = self.suppressed_count.saturating_add(1);
            if *pressed && !*repeat {
                return KeyboardAction::Reset {
                    qt_key,
                    modifiers: wire_modifiers,
                    reason: "panic_f9",
                };
            }

            return KeyboardAction::Suppressed {
                qt_key,
                modifiers: wire_modifiers,
                reason: "local_f9_edge",
            };
        }

        if *repeat {
            self.suppressed_count = self.suppressed_count.saturating_add(1);
            return KeyboardAction::Suppressed {
                qt_key,
                modifiers: wire_modifiers,
                reason: "repeat",
            };
        }

        let held = deck_modifiers(*modifiers);
        let changed = if *pressed {
            self.held_keys.insert(qt_key)
        } else {
            self.held_keys.remove(&qt_key)
        };
        if !changed {
            if !*pressed
                && matches!(
                    deck_key,
                    DeckKey::Command
                        | DeckKey::Control
                        | DeckKey::Shift
                        | DeckKey::LeftOption
                        | DeckKey::RightOption
                )
            {
                // A modifier held from before focus, pressed on the host from
                // the event modifiers rather than from its own key event: its
                // release must still reach the host.
                return KeyboardAction::Emit {
                    qt_key,
                    pressed: false,
                    edges: self.translator.key(deck_key, false, keypad, held),
                };
            }
            let command_held = self.held_keys.contains(&QT_KEY_META);
            if !*pressed && matches!(qt_key, 0x43 | 0x56 | 0x58) && command_held {
                let mut edges = self.translator.key(deck_key, true, keypad, held);
                edges.extend(self.translator.key(deck_key, false, keypad, held));
                return KeyboardAction::RecoverClipboard { qt_key, edges };
            }
            self.suppressed_count = self.suppressed_count.saturating_add(1);
            return KeyboardAction::Suppressed {
                qt_key,
                modifiers: wire_modifiers,
                reason: if *pressed {
                    "duplicate_press"
                } else {
                    "orphan_release"
                },
            };
        }

        KeyboardAction::Emit {
            qt_key,
            pressed: *pressed,
            edges: self.translator.key(deck_key, *pressed, keypad, held),
        }
    }

    pub fn record_delivery(&mut self, sent: bool) {
        if sent {
            self.emitted_count = self.emitted_count.saturating_add(1);
        } else {
            self.suppressed_count = self.suppressed_count.saturating_add(1);
        }
    }

    /// Releases `qt_key` (a key other than a modifier) whose physical
    /// release never arrived; `None` when it is not held.
    pub fn release_lost(&mut self, qt_key: u32, keypad: bool) -> Option<Vec<HostKeyEdge>> {
        if !self.held_keys.remove(&qt_key) {
            return None;
        }
        let held = self.translator.physical();
        Some(
            self.translator
                .key(DeckKey::Other(qt_key), false, keypad, held),
        )
    }

    /// The edges that settle a held-back Command before a pointer action.
    pub fn settle_command_for_pointer(&mut self) -> Vec<HostKeyEdge> {
        self.translator.settle_command_for_pointer()
    }

    /// The client's menu took a Command shortcut (Command+Q).
    pub fn command_chord_consumed(&mut self) {
        self.translator.command_chord_consumed();
    }

    /// A whole Command+`qt_key` the client's menu took. `command_down` is
    /// the keyboard's own state: a Command release the session has not seen
    /// yet is applied first, so Command is pressed around the key exactly
    /// when it is no longer held.
    pub fn tap_with_command(&mut self, qt_key: u32, command_down: bool) -> Vec<HostKeyEdge> {
        let mut edges = Vec::new();
        let tracked = self.held_keys.contains(&QT_KEY_META);
        if command_down != tracked {
            if command_down {
                self.held_keys.insert(QT_KEY_META);
            } else {
                self.held_keys.remove(&QT_KEY_META);
            }
            let mut held = self.translator.physical();
            held.command = command_down;
            edges.extend(
                self.translator
                    .key(DeckKey::Command, command_down, false, held),
            );
        }
        edges.extend(self.translator.tap_with_command(qt_key));
        edges
    }

    /// Releases everything held on the host and forgets every held key.
    pub fn release_all(&mut self) -> Vec<HostKeyEdge> {
        self.held_keys.clear();
        self.translator.release_all()
    }

    /// Forgets every held key without sending anything (the session is
    /// gone), and the host's policy with it.
    pub fn clear_local(&mut self) {
        self.held_keys.clear();
        self.translator = ShortcutTranslator::default();
        self.focused = false;
    }

    pub fn held_count(&self) -> usize {
        self.held_keys.len()
    }

    pub fn frame(&self) -> u64 {
        self.frame
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    pub fn raw_count(&self) -> u64 {
        self.raw_count
    }

    pub fn emitted_count(&self) -> u64 {
        self.emitted_count
    }

    pub fn suppressed_count(&self) -> u64 {
        self.suppressed_count
    }
}

/// The modifier keys an egui event reports held, with the Option keys'
/// sides from the keyboard's own device bits.
fn deck_modifiers(modifiers: egui::Modifiers) -> DeckModifiers {
    DeckModifiers {
        command: modifiers.mac_cmd,
        control: modifiers.ctrl,
        shift: modifiers.shift,
        option: super::option_keys::held_side(modifiers.alt),
    }
}

/// An egui physical key as a Mac key.
fn egui_key_to_deck(key: egui::Key, intl_backslash: bool) -> Option<DeckKey> {
    use egui::Key;
    Some(match key {
        Key::SuperLeft | Key::SuperRight => DeckKey::Command,
        Key::ControlLeft | Key::ControlRight => DeckKey::Control,
        Key::ShiftLeft | Key::ShiftRight => DeckKey::Shift,
        Key::AltLeft => DeckKey::LeftOption,
        Key::AltRight => DeckKey::RightOption,
        Key::IntlBackslash if intl_backslash => DeckKey::Other(KEY_INTL_BACKSLASH),
        other => DeckKey::Other(egui_key_to_qt(other)?),
    })
}

/// The two keys macOS names after an ANSI keyboard's top-left key
/// (`kVK_ANSI_Grave`, 0x32) and an ISO keyboard's `§` key
/// (`kVK_ISO_Section`, 0x0A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraveOrIsoKey {
    /// The key left of 1 (top left).
    Grave,
    /// The ISO key between left Shift and Z.
    IntlBackslash,
}

impl GraveOrIsoKey {
    /// The key identifier sent for it. Without a host that accepts the ISO
    /// key's own identifier it goes as the grave key, as egui reports both.
    pub const fn qt_key(self, intl_backslash: bool) -> u32 {
        match self {
            Self::IntlBackslash if intl_backslash => KEY_INTL_BACKSLASH,
            Self::Grave | Self::IntlBackslash => 0x60,
        }
    }
}

/// Which physical key a macOS virtual key code 0x0A or 0x32 is.
///
/// winit reports both as `Backquote`. On an ISO keyboard macOS reports the
/// top-left key as `kVK_ISO_Section` and the key next to left Shift as
/// `kVK_ANSI_Grave`, the reverse of their names' positions (the swap
/// `FreeRDP` and `TigerVNC` correct for the same way); on an ANSI keyboard
/// `kVK_ANSI_Grave` is the top-left key.
pub const fn mac_grave_or_iso_key(key_code: u16, iso_keyboard: bool) -> Option<GraveOrIsoKey> {
    match (key_code, iso_keyboard) {
        (0x0A, true) | (0x32, false) => Some(GraveOrIsoKey::Grave),
        (0x32, true) | (0x0A, false) => Some(GraveOrIsoKey::IntlBackslash),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeKeyMetadata {
    pub key: egui::Key,
    pub qt_key: u32,
    pub pressed: bool,
    pub keypad: bool,
    /// The macOS virtual key code.
    pub key_code: u16,
}

impl NativeKeyMetadata {
    pub fn requires_synthetic_event(self) -> bool {
        self.keypad && self.qt_key == 0x2A
    }

    /// Keypad keys egui-winit has no physical key for (keypad decimal and
    /// keypad equals): their events carry only the logical key, which the
    /// layout decides (`.` on a US layout, `,` on a Norwegian one).
    pub fn physical_unknown_to_egui(self) -> bool {
        self.keypad && matches!(self.key_code, 65 | 81)
    }

    pub fn matches(self, event: &egui::Event) -> bool {
        let egui::Event::Key {
            key,
            physical_key,
            pressed,
            ..
        } = event
        else {
            return false;
        };
        if *pressed != self.pressed {
            return false;
        }
        if self.physical_unknown_to_egui() {
            return physical_key.is_none();
        }
        physical_key.unwrap_or(*key) == self.key
            && (self.requires_synthetic_event() == physical_key.is_none())
    }
}

#[cfg(target_os = "macos")]
mod native_keypad {
    use super::NativeKeyMetadata;
    use block2::RcBlock;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, Once};

    static INSTALL: Once = Once::new();
    static EVENTS: Mutex<Vec<NativeKeyMetadata>> = Mutex::new(Vec::new());
    /// The keyboard last typed on is ISO, as of its last grave or ISO key.
    static ISO_KEYBOARD: AtomicBool = AtomicBool::new(false);

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn LMGetKbdType() -> u8;
        fn KBGetLayoutType(keyboard_type: i16) -> u32;
    }

    /// `kKeyboardISO`.
    const KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");

    fn keyboard_is_iso() -> bool {
        // SAFETY: plain reads of the current keyboard type, on the main
        // thread the event monitor runs on.
        unsafe { KBGetLayoutType(i16::from(LMGetKbdType())) == KEYBOARD_ISO }
    }

    pub fn iso_keyboard() -> bool {
        ISO_KEYBOARD.load(Ordering::Relaxed)
    }

    pub fn install() {
        INSTALL.call_once(|| {
            let handler = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
                let event_ref = unsafe { event.as_ref() };
                if matches!(event_ref.keyCode(), 0x0A | 0x32) {
                    ISO_KEYBOARD.store(keyboard_is_iso(), Ordering::Relaxed);
                }
                if let Some(metadata) = metadata(
                    event_ref.keyCode(),
                    event_ref.r#type(),
                    event_ref.modifierFlags(),
                ) {
                    match EVENTS.lock() {
                        Ok(mut events) => events.push(metadata),
                        Err(error) => {
                            tracing::warn!(
                                target: crate::logging::target::INPUT,
                                %error,
                                "native keypad metadata queue was poisoned",
                            );
                        }
                    }
                }
                event.as_ptr()
            });
            let monitor = unsafe {
                NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                    NSEventMask::KeyDown | NSEventMask::KeyUp,
                    &handler,
                )
            };
            if let Some(monitor) = monitor {
                std::mem::forget(monitor);
                tracing::info!(
                    target: crate::logging::target::INPUT,
                    "native keypad metadata monitor installed",
                );
            } else {
                tracing::warn!(
                    target: crate::logging::target::INPUT,
                    "native keypad metadata monitor unavailable",
                );
            }
        });
    }

    pub fn drain() -> Vec<NativeKeyMetadata> {
        match EVENTS.lock() {
            Ok(mut events) => std::mem::take(&mut *events),
            Err(error) => {
                tracing::warn!(
                    target: crate::logging::target::INPUT,
                    %error,
                    "recovering poisoned native keypad metadata queue",
                );
                std::mem::take(&mut *error.into_inner())
            }
        }
    }

    fn metadata(
        key_code: u16,
        event_type: NSEventType,
        flags: NSEventModifierFlags,
    ) -> Option<NativeKeyMetadata> {
        let pressed = event_type == NSEventType::KeyDown;
        let keypad = flags.contains(NSEventModifierFlags::NumericPad);
        let (key, qt_key) = match key_code {
            29 => (egui::Key::Num0, 0x30),
            18 => (egui::Key::Num1, 0x31),
            19 => (egui::Key::Num2, 0x32),
            20 => (egui::Key::Num3, 0x33),
            21 => (egui::Key::Num4, 0x34),
            23 => (egui::Key::Num5, 0x35),
            22 => (egui::Key::Num6, 0x36),
            26 => (egui::Key::Num7, 0x37),
            28 => (egui::Key::Num8, 0x38),
            25 => (egui::Key::Num9, 0x39),
            27 => (egui::Key::Minus, 0x2D),
            24 => (egui::Key::Equals, 0x3D),
            47 => (egui::Key::Period, 0x2E),
            44 => (egui::Key::Slash, 0x2F),
            82 => (egui::Key::Num0, 0x30),
            83 => (egui::Key::Num1, 0x31),
            84 => (egui::Key::Num2, 0x32),
            85 => (egui::Key::Num3, 0x33),
            86 => (egui::Key::Num4, 0x34),
            87 => (egui::Key::Num5, 0x35),
            88 => (egui::Key::Num6, 0x36),
            89 => (egui::Key::Num7, 0x37),
            91 => (egui::Key::Num8, 0x38),
            92 => (egui::Key::Num9, 0x39),
            65 => (egui::Key::Period, 0x2E),
            67 => (egui::Key::Num8, 0x2A),
            69 => (egui::Key::Plus, 0x2B),
            75 => (egui::Key::Slash, 0x2F),
            76 => (egui::Key::Enter, 0x0100_0005),
            78 => (egui::Key::Minus, 0x2D),
            81 => (egui::Key::Equals, 0x3D),
            // The top-left and ISO keys, which winit reports alike.
            10 | 50 => (egui::Key::Backtick, 0x60),
            _ => return None,
        };
        Some(NativeKeyMetadata {
            key,
            qt_key,
            pressed,
            keypad,
            key_code,
        })
    }
}

#[cfg(target_os = "macos")]
pub fn install_native_keypad_monitor() {
    native_keypad::install();
}

#[cfg(not(target_os = "macos"))]
pub fn install_native_keypad_monitor() {}

/// Whether the Mac keyboard last typed on is an ISO keyboard.
#[cfg(target_os = "macos")]
pub fn iso_keyboard() -> bool {
    native_keypad::iso_keyboard()
}

#[cfg(not(target_os = "macos"))]
pub fn iso_keyboard() -> bool {
    false
}

#[cfg(target_os = "macos")]
pub fn drain_native_key_metadata() -> Vec<NativeKeyMetadata> {
    native_keypad::drain()
}

#[cfg(not(target_os = "macos"))]
pub fn drain_native_key_metadata() -> Vec<NativeKeyMetadata> {
    Vec::new()
}

pub fn egui_modifiers_to_qt(modifiers: egui::Modifiers) -> u32 {
    let mut qt = 0;
    if modifiers.shift {
        qt |= MOD_SHIFT;
    }
    if modifiers.ctrl {
        qt |= MOD_CTRL;
    }
    if modifiers.alt {
        qt |= MOD_ALT;
    }
    if modifiers.mac_cmd {
        qt |= MOD_META;
    }
    qt
}

pub fn qt_modifiers_to_wire(modifiers: u32) -> u32 {
    let mut wire = 0;
    if modifiers & MOD_SHIFT != 0 {
        wire |= u32::from(MODIFIER_BIT_SHIFT);
    }
    if modifiers & MOD_CTRL != 0 {
        wire |= u32::from(MODIFIER_BIT_CTRL);
    }
    if modifiers & MOD_ALT != 0 {
        wire |= u32::from(MODIFIER_BIT_ALT);
    }
    if modifiers & MOD_META != 0 {
        wire |= u32::from(MODIFIER_BIT_META);
    }
    if modifiers & MOD_KEYPAD != 0 {
        wire |= u32::from(MODIFIER_BIT_KEYPAD);
    }
    wire
}

pub fn egui_key_to_qt(key: egui::Key) -> Option<u32> {
    use egui::Key;

    Some(match key {
        Key::A => 0x41,
        Key::B => 0x42,
        Key::C => 0x43,
        Key::D => 0x44,
        Key::E => 0x45,
        Key::F => 0x46,
        Key::G => 0x47,
        Key::H => 0x48,
        Key::I => 0x49,
        Key::J => 0x4A,
        Key::K => 0x4B,
        Key::L => 0x4C,
        Key::M => 0x4D,
        Key::N => 0x4E,
        Key::O => 0x4F,
        Key::P => 0x50,
        Key::Q => 0x51,
        Key::R => 0x52,
        Key::S => 0x53,
        Key::T => 0x54,
        Key::U => 0x55,
        Key::V => 0x56,
        Key::W => 0x57,
        Key::X => 0x58,
        Key::Y => 0x59,
        Key::Z => 0x5A,
        Key::Num0 => 0x30,
        Key::Num1 | Key::Exclamationmark => 0x31,
        Key::Num2 => 0x32,
        Key::Num3 => 0x33,
        Key::Num4 => 0x34,
        Key::Num5 => 0x35,
        Key::Num6 => 0x36,
        Key::Num7 => 0x37,
        Key::Num8 => 0x38,
        Key::Num9 => 0x39,
        Key::Space => 0x20,
        Key::Minus => 0x2D,
        Key::Equals | Key::Plus => 0x3D,
        Key::OpenBracket | Key::OpenCurlyBracket => 0x5B,
        Key::CloseBracket | Key::CloseCurlyBracket => 0x5D,
        Key::Backslash | Key::Pipe | Key::IntlBackslash => 0x5C,
        Key::Semicolon | Key::Colon => 0x3B,
        Key::Quote => 0x27,
        Key::Backtick => 0x60,
        Key::Comma => 0x2C,
        Key::Period => 0x2E,
        Key::Slash | Key::Questionmark => 0x2F,
        Key::Escape => 0x0100_0000,
        Key::Tab => 0x0100_0001,
        Key::Backspace => 0x0100_0003,
        Key::Enter => 0x0100_0004,
        Key::Insert => 0x0100_0006,
        Key::Delete => 0x0100_0007,
        Key::Home => 0x0100_0010,
        Key::End => 0x0100_0011,
        Key::ArrowLeft => 0x0100_0012,
        Key::ArrowUp => 0x0100_0013,
        Key::ArrowRight => 0x0100_0014,
        Key::ArrowDown => 0x0100_0015,
        Key::PageUp => 0x0100_0016,
        Key::PageDown => 0x0100_0017,
        Key::ShiftLeft | Key::ShiftRight => QT_KEY_SHIFT,
        Key::ControlLeft | Key::ControlRight => QT_KEY_CTRL,
        Key::AltLeft | Key::AltRight => QT_KEY_ALT,
        Key::SuperLeft | Key::SuperRight => QT_KEY_META,
        Key::F1 => 0x0100_0030,
        Key::F2 => 0x0100_0031,
        Key::F3 => 0x0100_0032,
        Key::F4 => 0x0100_0033,
        Key::F5 => 0x0100_0034,
        Key::F6 => 0x0100_0035,
        Key::F7 => 0x0100_0036,
        Key::F8 => 0x0100_0037,
        Key::F9 => QT_KEY_F9,
        Key::F10 => 0x0100_0039,
        Key::F11 => 0x0100_003A,
        Key::F12 => 0x0100_003B,
        Key::F13 => 0x0100_003C,
        Key::F14 => 0x0100_003D,
        Key::F15 => 0x0100_003E,
        Key::F16 => 0x0100_003F,
        Key::F17 => 0x0100_0040,
        Key::F18 => 0x0100_0041,
        Key::F19 => 0x0100_0042,
        Key::F20 => 0x0100_0043,
        Key::F21 => 0x0100_0044,
        Key::F22 => 0x0100_0045,
        Key::F23 => 0x0100_0046,
        Key::F24 => 0x0100_0047,
        Key::F25 => 0x0100_0048,
        Key::F26 => 0x0100_0049,
        Key::F27 => 0x0100_004A,
        Key::F28 => 0x0100_004B,
        Key::F29 => 0x0100_004C,
        Key::F30 => 0x0100_004D,
        Key::F31 => 0x0100_004E,
        Key::F32 => 0x0100_004F,
        Key::F33 => 0x0100_0050,
        Key::F34 => 0x0100_0051,
        Key::F35 => 0x0100_0052,
        Key::Copy | Key::Cut | Key::Paste | Key::BrowserBack => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::keymap::FLAME_CRITICAL_CHORDS;
    use arcen_input::keyboard::{resolve_keyboard_policy, KeyboardSettings};
    use arcen_protocol::messages::HostPlatform;

    fn key_event(
        key: egui::Key,
        physical_key: Option<egui::Key>,
        pressed: bool,
        repeat: bool,
        modifiers: egui::Modifiers,
    ) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key,
            pressed,
            repeat,
            modifiers,
        }
    }

    const CMD: egui::Modifiers = egui::Modifiers {
        alt: false,
        ctrl: false,
        shift: false,
        mac_cmd: true,
        command: true,
    };

    fn edges(action: KeyboardAction) -> Vec<(u32, bool, u32)> {
        match action {
            KeyboardAction::Emit { edges, .. } | KeyboardAction::RecoverClipboard { edges, .. } => {
                edges
                    .into_iter()
                    .map(|edge| (edge.qt_key, edge.pressed, edge.modifiers))
                    .collect()
            }
            other => panic!("not sent: {other:?}"),
        }
    }

    fn windows_input() -> KeyboardInput {
        let mut input = KeyboardInput::default();
        input.set_policy(resolve_keyboard_policy(
            HostPlatform::Windows,
            true,
            KeyboardSettings::default(),
        ));
        input
    }

    #[test]
    fn linux_policy_sends_command_as_control() {
        let mut input = KeyboardInput::default();
        assert_eq!(
            edges(input.handle_event(&key_event(
                egui::Key::SuperLeft,
                Some(egui::Key::SuperLeft),
                true,
                false,
                CMD,
            ))),
            vec![(QT_KEY_CTRL, true, u32::from(MODIFIER_BIT_CTRL))]
        );
        assert_eq!(
            edges(input.handle_event(&key_event(
                egui::Key::A,
                Some(egui::Key::A),
                true,
                false,
                CMD
            ))),
            vec![(0x41, true, u32::from(MODIFIER_BIT_CTRL))]
        );
    }

    #[test]
    fn windows_policy_makes_command_e_windows_e_and_command_c_control_c() {
        let mut input = windows_input();
        let command = key_event(
            egui::Key::SuperLeft,
            Some(egui::Key::SuperLeft),
            true,
            false,
            CMD,
        );
        assert!(edges(input.handle_event(&command)).is_empty());
        assert_eq!(
            edges(input.handle_event(&key_event(
                egui::Key::C,
                Some(egui::Key::C),
                true,
                false,
                CMD
            ))),
            vec![
                (QT_KEY_CTRL, true, u32::from(MODIFIER_BIT_CTRL)),
                (0x43, true, u32::from(MODIFIER_BIT_CTRL))
            ]
        );
        input.release_all();
        assert!(edges(input.handle_event(&command)).is_empty());
        assert_eq!(
            edges(input.handle_event(&key_event(
                egui::Key::E,
                Some(egui::Key::E),
                true,
                false,
                CMD
            ))),
            vec![
                (QT_KEY_META, true, u32::from(MODIFIER_BIT_META)),
                (0x45, true, u32::from(MODIFIER_BIT_META))
            ]
        );
    }

    #[test]
    fn right_option_is_altgr_on_a_windows_host_and_alt_elsewhere() {
        let option = egui::Modifiers {
            alt: true,
            ..egui::Modifiers::NONE
        };
        let right_option = key_event(
            egui::Key::AltRight,
            Some(egui::Key::AltRight),
            true,
            false,
            option,
        );
        assert_eq!(
            edges(windows_input().handle_event(&right_option)),
            vec![(KEY_ALTGR, true, 0)]
        );
        assert_eq!(
            edges(KeyboardInput::default().handle_event(&right_option)),
            vec![(QT_KEY_ALT, true, u32::from(MODIFIER_BIT_ALT))]
        );
    }

    #[test]
    fn mac_iso_quirk_swaps_the_grave_and_iso_keys() {
        assert_eq!(mac_grave_or_iso_key(0x0A, true), Some(GraveOrIsoKey::Grave));
        assert_eq!(
            mac_grave_or_iso_key(0x32, true),
            Some(GraveOrIsoKey::IntlBackslash)
        );
        assert_eq!(
            mac_grave_or_iso_key(0x32, false),
            Some(GraveOrIsoKey::Grave)
        );
        assert_eq!(mac_grave_or_iso_key(0x12, true), None);
        assert_eq!(
            GraveOrIsoKey::IntlBackslash.qt_key(true),
            KEY_INTL_BACKSLASH
        );
        assert_eq!(GraveOrIsoKey::IntlBackslash.qt_key(false), 0x60);
        assert_eq!(GraveOrIsoKey::Grave.qt_key(true), 0x60);
    }

    #[test]
    fn egui_intl_backslash_is_its_own_key_only_for_a_host_that_accepts_it() {
        let iso = key_event(
            egui::Key::IntlBackslash,
            Some(egui::Key::IntlBackslash),
            true,
            false,
            egui::Modifiers::NONE,
        );
        assert_eq!(
            edges(windows_input().handle_event(&iso)),
            vec![(KEY_INTL_BACKSLASH, true, 0)]
        );
        assert_eq!(
            edges(KeyboardInput::default().handle_event(&iso)),
            vec![(0x5C, true, 0)]
        );
    }

    #[test]
    fn physical_key_precedes_logical_key_with_logical_fallback() {
        let mut input = KeyboardInput::default();
        assert!(matches!(
            input.handle_event(&key_event(
                egui::Key::Q,
                Some(egui::Key::A),
                true,
                false,
                egui::Modifiers::NONE,
            )),
            KeyboardAction::Emit { qt_key: 0x41, .. }
        ));
        assert!(matches!(
            input.handle_event(&key_event(
                egui::Key::B,
                None,
                true,
                false,
                egui::Modifiers::NONE,
            )),
            KeyboardAction::Emit { qt_key: 0x42, .. }
        ));
    }

    #[test]
    fn compact_modifier_bits_and_flame_chords_are_stable() {
        let mapped_qt_keys: BTreeSet<_> = egui::Key::ALL
            .iter()
            .filter_map(|key| egui_key_to_qt(*key))
            .collect();
        assert_eq!(qt_modifiers_to_wire(MOD_SHIFT), 0x01);
        assert_eq!(qt_modifiers_to_wire(MOD_CTRL), 0x02);
        assert_eq!(qt_modifiers_to_wire(MOD_ALT), 0x04);
        assert_eq!(qt_modifiers_to_wire(MOD_META), 0x08);
        assert_eq!(qt_modifiers_to_wire(MOD_KEYPAD), 0x10);
        assert_eq!(
            qt_modifiers_to_wire(MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META | MOD_KEYPAD),
            0x1F
        );
        for (qt_key, modifiers, description) in FLAME_CRITICAL_CHORDS {
            assert!(
                mapped_qt_keys.contains(qt_key),
                "{description} must have an egui-to-Qt mapping"
            );
            assert_eq!(qt_modifiers_to_wire(*modifiers) & !0x1F, 0, "{description}");
        }
    }

    #[test]
    fn native_keypad_metadata_sets_compact_keypad_bit() {
        let mut input = KeyboardInput::default();
        assert_eq!(
            edges(input.handle_event_with_native_key(
                &key_event(
                    egui::Key::Num5,
                    Some(egui::Key::Num5),
                    true,
                    false,
                    egui::Modifiers::NONE,
                ),
                Some(NativeKeyMetadata {
                    key: egui::Key::Num5,
                    qt_key: 0x35,
                    pressed: true,
                    keypad: true,
                    key_code: 87,
                }),
            )),
            vec![(0x35, true, u32::from(MODIFIER_BIT_KEYPAD))]
        );
    }

    #[test]
    fn suppresses_repeats_duplicates_and_qt_aliases() {
        let mut input = KeyboardInput::default();
        let press = key_event(
            egui::Key::OpenBracket,
            Some(egui::Key::OpenBracket),
            true,
            false,
            egui::Modifiers::NONE,
        );
        assert!(matches!(
            input.handle_event(&press),
            KeyboardAction::Emit { pressed: true, .. }
        ));
        assert!(matches!(
            input.handle_event(&press),
            KeyboardAction::Suppressed {
                reason: "duplicate_press",
                ..
            }
        ));
        let repeat = key_event(
            egui::Key::OpenBracket,
            Some(egui::Key::OpenBracket),
            true,
            true,
            egui::Modifiers::NONE,
        );
        assert!(matches!(
            input.handle_event(&repeat),
            KeyboardAction::Suppressed {
                reason: "repeat",
                ..
            }
        ));
        let alias_release = key_event(
            egui::Key::OpenCurlyBracket,
            None,
            false,
            false,
            egui::Modifiers::SHIFT,
        );
        assert_eq!(
            edges(input.handle_event(&alias_release)),
            vec![
                (QT_KEY_SHIFT, true, u32::from(MODIFIER_BIT_SHIFT)),
                (0x5B, false, u32::from(MODIFIER_BIT_SHIFT))
            ]
        );
        assert!(matches!(
            input.handle_event(&alias_release),
            KeyboardAction::Suppressed {
                reason: "orphan_release",
                ..
            }
        ));
        assert_eq!(input.raw_count(), 5);
        assert_eq!(input.suppressed_count(), 3);
    }

    #[test]
    fn text_events_do_not_emit_physical_keys() {
        let mut input = KeyboardInput::default();
        assert_eq!(
            input.handle_event(&egui::Event::Text("secret".to_string())),
            KeyboardAction::Ignored
        );
        assert_eq!(input.raw_count(), 0);
        assert_eq!(input.held_count(), 0);
    }

    #[test]
    fn recovers_clipboard_chord_when_empty_paste_press_was_consumed() {
        let mut input = KeyboardInput::default();
        input.handle_event(&key_event(
            egui::Key::SuperLeft,
            Some(egui::Key::SuperLeft),
            true,
            false,
            CMD,
        ));
        assert_eq!(
            edges(input.handle_event(&key_event(
                egui::Key::V,
                Some(egui::Key::V),
                false,
                false,
                CMD
            ))),
            vec![
                (0x56, true, u32::from(MODIFIER_BIT_CTRL)),
                (0x56, false, u32::from(MODIFIER_BIT_CTRL))
            ]
        );
        assert_eq!(input.held_count(), 1);
    }

    #[test]
    fn a_modifier_held_across_a_release_all_is_released_on_the_host() {
        // Command+` moved focus between the Deck's windows: everything was
        // released while Command stayed held. Command+V then presses Command
        // from the event modifiers, and Command's own release must reach the
        // host.
        for (mut input, command) in [
            (KeyboardInput::default(), QT_KEY_CTRL),
            (windows_input(), QT_KEY_CTRL),
        ] {
            let command_bit = u32::from(MODIFIER_BIT_CTRL);
            input.handle_event(&key_event(
                egui::Key::SuperLeft,
                Some(egui::Key::SuperLeft),
                true,
                false,
                CMD,
            ));
            input.release_all();
            assert_eq!(
                edges(input.handle_event(&key_event(
                    egui::Key::V,
                    Some(egui::Key::V),
                    true,
                    false,
                    CMD
                ))),
                vec![(command, true, command_bit), (0x56, true, command_bit)]
            );
            input.handle_event(&key_event(
                egui::Key::V,
                Some(egui::Key::V),
                false,
                false,
                CMD,
            ));
            assert_eq!(
                edges(input.handle_event(&key_event(
                    egui::Key::SuperLeft,
                    Some(egui::Key::SuperLeft),
                    false,
                    false,
                    egui::Modifiers::NONE,
                ))),
                vec![(command, false, 0)]
            );
            assert_eq!(input.held_count(), 0);
        }
    }

    #[test]
    fn unmapped_key_is_safely_suppressed() {
        let mut input = KeyboardInput::default();
        assert!(matches!(
            input.handle_event(&key_event(
                egui::Key::BrowserBack,
                Some(egui::Key::BrowserBack),
                true,
                false,
                egui::Modifiers::NONE,
            )),
            KeyboardAction::Suppressed {
                qt_key: 0,
                reason: "unmapped",
                ..
            }
        ));
        assert_eq!(input.held_count(), 0);
    }

    #[test]
    fn f9_is_local_reset_and_never_held() {
        let mut input = KeyboardInput::default();
        assert!(matches!(
            input.handle_event(&key_event(
                egui::Key::F9,
                Some(egui::Key::F9),
                true,
                false,
                egui::Modifiers::NONE,
            )),
            KeyboardAction::Reset {
                reason: "panic_f9",
                ..
            }
        ));
        assert_eq!(input.held_count(), 0);
    }
}
