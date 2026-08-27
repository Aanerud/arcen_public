use std::collections::BTreeSet;

use crate::protocol::keymap::{
    swap_cmd_ctrl_for_linux_dest, MODIFIER_BIT_ALT, MODIFIER_BIT_CTRL, MODIFIER_BIT_KEYPAD,
    MODIFIER_BIT_META, MODIFIER_BIT_SHIFT, MOD_ALT, MOD_CTRL, MOD_KEYPAD, MOD_META, MOD_SHIFT,
    QT_KEY_ALT, QT_KEY_CTRL, QT_KEY_F9, QT_KEY_META, QT_KEY_SHIFT,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardAction {
    Emit {
        qt_key: u32,
        pressed: bool,
        modifiers: u32,
    },
    RecoverClipboard {
        qt_key: u32,
        modifiers: u32,
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
    held_qt_keys: BTreeSet<u32>,
    frame: u64,
    focused: bool,
    raw_count: u64,
    emitted_count: u64,
    suppressed_count: u64,
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

    #[cfg(test)]
    pub fn handle_event(&mut self, event: &egui::Event, swap_cmd_ctrl: bool) -> KeyboardAction {
        self.handle_event_with_native_key(event, swap_cmd_ctrl, None)
    }

    pub fn handle_event_with_native_key(
        &mut self,
        event: &egui::Event,
        swap_cmd_ctrl: bool,
        native_qt_key: Option<u32>,
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
        let mut qt_modifiers = egui_modifiers_to_qt(*modifiers);
        if native_qt_key.is_some() {
            qt_modifiers |= MOD_KEYPAD;
        }
        let Some(qt_key) = native_qt_key.or_else(|| egui_key_to_qt(physical_key.unwrap_or(*key)))
        else {
            self.suppressed_count = self.suppressed_count.saturating_add(1);
            return KeyboardAction::Suppressed {
                qt_key: 0,
                modifiers: qt_modifiers_to_wire(qt_modifiers),
                reason: "unmapped",
            };
        };
        let (qt_key, qt_modifiers) = if swap_cmd_ctrl {
            swap_cmd_ctrl_for_linux_dest(qt_key, qt_modifiers)
        } else {
            (qt_key, qt_modifiers)
        };
        let wire_modifiers = qt_modifiers_to_wire(qt_modifiers);

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

        let changed = if *pressed {
            self.held_qt_keys.insert(qt_key)
        } else {
            self.held_qt_keys.remove(&qt_key)
        };
        if !changed {
            let command_held = (wire_modifiers & u32::from(MODIFIER_BIT_CTRL) != 0
                && self.held_qt_keys.contains(&QT_KEY_CTRL))
                || (wire_modifiers & u32::from(MODIFIER_BIT_META) != 0
                    && self.held_qt_keys.contains(&QT_KEY_META));
            if !*pressed && matches!(qt_key, 0x43 | 0x56 | 0x58) && command_held {
                return KeyboardAction::RecoverClipboard {
                    qt_key,
                    modifiers: wire_modifiers,
                };
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
            modifiers: wire_modifiers,
        }
    }

    pub fn record_delivery(&mut self, sent: bool) {
        if sent {
            self.emitted_count = self.emitted_count.saturating_add(1);
        } else {
            self.suppressed_count = self.suppressed_count.saturating_add(1);
        }
    }

    pub fn drain_held(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.held_qt_keys).into_iter().collect()
    }

    pub fn clear_local(&mut self) {
        self.held_qt_keys.clear();
        self.focused = false;
    }

    pub fn held_count(&self) -> usize {
        self.held_qt_keys.len()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeKeyMetadata {
    pub key: egui::Key,
    pub qt_key: u32,
    pub pressed: bool,
    pub keypad: bool,
}

impl NativeKeyMetadata {
    pub fn requires_synthetic_event(self) -> bool {
        self.keypad && self.qt_key == 0x2A
    }

    pub fn matches(self, event: &egui::Event) -> bool {
        matches!(
            event,
            egui::Event::Key {
                key,
                physical_key,
                pressed,
                ..
            } if physical_key.unwrap_or(*key) == self.key
                && *pressed == self.pressed
                && (self.requires_synthetic_event() == physical_key.is_none())
        )
    }
}

#[cfg(target_os = "macos")]
mod native_keypad {
    use super::NativeKeyMetadata;
    use block2::RcBlock;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
    use std::ptr::NonNull;
    use std::sync::{Mutex, Once};

    static INSTALL: Once = Once::new();
    static EVENTS: Mutex<Vec<NativeKeyMetadata>> = Mutex::new(Vec::new());

    pub fn install() {
        INSTALL.call_once(|| {
            let handler = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
                let event_ref = unsafe { event.as_ref() };
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
            _ => return None,
        };
        Some(NativeKeyMetadata {
            key,
            qt_key,
            pressed,
            keypad,
        })
    }
}

#[cfg(target_os = "macos")]
pub fn install_native_keypad_monitor() {
    native_keypad::install();
}

#[cfg(not(target_os = "macos"))]
pub fn install_native_keypad_monitor() {}

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
    use crate::protocol::keymap::{
        FLAME_CRITICAL_CHORDS, MOD_ALT, MOD_CTRL, MOD_KEYPAD, MOD_META, MOD_SHIFT,
    };

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

    #[test]
    fn cmd_modifier_and_cmd_a_transform_in_qt_domain() {
        let mut input = KeyboardInput::default();
        let cmd = egui::Modifiers {
            mac_cmd: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        assert_eq!(
            input.handle_event(
                &key_event(
                    egui::Key::SuperLeft,
                    Some(egui::Key::SuperLeft),
                    true,
                    false,
                    cmd,
                ),
                true,
            ),
            KeyboardAction::Emit {
                qt_key: QT_KEY_CTRL,
                pressed: true,
                modifiers: u32::from(MODIFIER_BIT_CTRL),
            }
        );
        assert_eq!(
            input.handle_event(
                &key_event(egui::Key::A, Some(egui::Key::A), true, false, cmd),
                true,
            ),
            KeyboardAction::Emit {
                qt_key: 0x41,
                pressed: true,
                modifiers: u32::from(MODIFIER_BIT_CTRL),
            }
        );
    }

    #[test]
    fn physical_key_precedes_logical_key_with_logical_fallback() {
        let mut input = KeyboardInput::default();
        assert!(matches!(
            input.handle_event(
                &key_event(
                    egui::Key::Q,
                    Some(egui::Key::A),
                    true,
                    false,
                    egui::Modifiers::NONE,
                ),
                false,
            ),
            KeyboardAction::Emit { qt_key: 0x41, .. }
        ));
        assert!(matches!(
            input.handle_event(
                &key_event(egui::Key::B, None, true, false, egui::Modifiers::NONE,),
                false,
            ),
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
            input.handle_event_with_native_key(
                &key_event(
                    egui::Key::Num5,
                    Some(egui::Key::Num5),
                    true,
                    false,
                    egui::Modifiers::NONE,
                ),
                false,
                Some(0x35),
            ),
            KeyboardAction::Emit {
                qt_key: 0x35,
                pressed: true,
                modifiers: u32::from(MODIFIER_BIT_KEYPAD),
            }
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
            input.handle_event(&press, false),
            KeyboardAction::Emit { pressed: true, .. }
        ));
        assert!(matches!(
            input.handle_event(&press, false),
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
            input.handle_event(&repeat, false),
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
            input.handle_event(&alias_release, false),
            KeyboardAction::Emit {
                qt_key: 0x5B,
                pressed: false,
                modifiers: u32::from(MODIFIER_BIT_SHIFT),
            }
        );
        assert!(matches!(
            input.handle_event(&alias_release, false),
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
            input.handle_event(&egui::Event::Text("secret".to_string()), true),
            KeyboardAction::Ignored
        );
        assert_eq!(input.raw_count(), 0);
        assert_eq!(input.held_count(), 0);
    }

    #[test]
    fn recovers_clipboard_chord_when_empty_paste_press_was_consumed() {
        let mut input = KeyboardInput::default();
        let cmd = egui::Modifiers {
            mac_cmd: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        assert!(matches!(
            input.handle_event(
                &key_event(
                    egui::Key::SuperLeft,
                    Some(egui::Key::SuperLeft),
                    true,
                    false,
                    cmd,
                ),
                true,
            ),
            KeyboardAction::Emit {
                qt_key: QT_KEY_CTRL,
                ..
            }
        ));
        assert_eq!(
            input.handle_event(
                &key_event(egui::Key::V, Some(egui::Key::V), false, false, cmd,),
                true,
            ),
            KeyboardAction::RecoverClipboard {
                qt_key: 0x56,
                modifiers: u32::from(MODIFIER_BIT_CTRL),
            }
        );
        assert_eq!(input.held_count(), 1);
    }

    #[test]
    fn unmapped_key_is_safely_suppressed() {
        let mut input = KeyboardInput::default();
        assert!(matches!(
            input.handle_event(
                &key_event(
                    egui::Key::BrowserBack,
                    Some(egui::Key::BrowserBack),
                    true,
                    false,
                    egui::Modifiers::NONE,
                ),
                false,
            ),
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
            input.handle_event(
                &key_event(
                    egui::Key::F9,
                    Some(egui::Key::F9),
                    true,
                    false,
                    egui::Modifiers::NONE,
                ),
                false,
            ),
            KeyboardAction::Reset {
                reason: "panic_f9",
                ..
            }
        ));
        assert_eq!(input.held_count(), 0);
    }
}
