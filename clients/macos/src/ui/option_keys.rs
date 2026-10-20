//! Which Option keys are held, and Option keys that act as Alt.
//!
//! macOS composes characters with Option (Option+o types ø, Option+u starts
//! a diaeresis). Once the Deck lets its input method compose (to send a
//! host committed text), an Option chord the user means as an Alt shortcut
//! on the host would be composed instead. The Deck's Option-key setting
//! decides per side: an Option key that acts as Alt has its key events
//! rewritten before AppKit's text system sees them, to the characters the
//! key types without modifiers (as winit's `option_as_alt` does), so no
//! composition starts and the keystroke reaches the host as Alt plus the
//! key. The modifier flags are kept, so Alt is still sent.
//!
//! The left and right Option keys are told apart by AppKit's
//! device-dependent modifier bits.
//!
//! The same monitor sees every key release, including those AppKit's text
//! system swallows while the input method composes; it wakes the UI so the
//! Deck can check which keys are still physically down
//! ([`virtual_key_down`]) and release the rest on the host.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use arcen_input::text::OptionSide;

/// `NX_DEVICELALTKEYMASK`.
const DEVICE_LEFT_OPTION: usize = 0x20;
/// `NX_DEVICERALTKEYMASK`.
const DEVICE_RIGHT_OPTION: usize = 0x40;

const LEFT: u8 = 1;
const RIGHT: u8 = 2;

/// The Option keys held, as the last key or flags event reported.
static HELD: AtomicU8 = AtomicU8::new(0);
/// The Option keys whose events are rewritten to act as Alt.
static ALT_SIDES: AtomicU8 = AtomicU8::new(0);
/// Woken on every key release.
static WAKE: OnceLock<egui::Context> = OnceLock::new();

/// The macOS virtual key code of a physical key outside the keypad (keypad
/// keys carry theirs in the native key metadata).
#[must_use]
pub fn virtual_key_code(key: egui::Key) -> Option<u16> {
    use egui::Key;
    Some(match key {
        Key::A => 0,
        Key::S => 1,
        Key::D => 2,
        Key::F => 3,
        Key::H => 4,
        Key::G => 5,
        Key::Z => 6,
        Key::X => 7,
        Key::C => 8,
        Key::V => 9,
        Key::IntlBackslash => 10,
        Key::B => 11,
        Key::Q => 12,
        Key::W => 13,
        Key::E => 14,
        Key::R => 15,
        Key::Y => 16,
        Key::T => 17,
        Key::Num1 => 18,
        Key::Num2 => 19,
        Key::Num3 => 20,
        Key::Num4 => 21,
        Key::Num6 => 22,
        Key::Num5 => 23,
        Key::Equals => 24,
        Key::Num9 => 25,
        Key::Num7 => 26,
        Key::Minus => 27,
        Key::Num8 => 28,
        Key::Num0 => 29,
        Key::CloseBracket => 30,
        Key::O => 31,
        Key::U => 32,
        Key::OpenBracket => 33,
        Key::I => 34,
        Key::P => 35,
        Key::Enter => 36,
        Key::L => 37,
        Key::J => 38,
        Key::Quote => 39,
        Key::K => 40,
        Key::Semicolon => 41,
        Key::Backslash => 42,
        Key::Comma => 43,
        Key::Slash => 44,
        Key::N => 45,
        Key::M => 46,
        Key::Period => 47,
        Key::Tab => 48,
        Key::Space => 49,
        Key::Backtick => 50,
        Key::Backspace => 51,
        Key::Escape => 53,
        Key::SuperRight => 54,
        Key::SuperLeft => 55,
        Key::ShiftLeft => 56,
        Key::AltLeft => 58,
        Key::ControlLeft => 59,
        Key::ShiftRight => 60,
        Key::AltRight => 61,
        Key::ControlRight => 62,
        Key::Plus => 69,
        Key::F17 => 64,
        Key::F18 => 79,
        Key::F19 => 80,
        Key::F20 => 90,
        Key::F5 => 96,
        Key::F6 => 97,
        Key::F7 => 98,
        Key::F3 => 99,
        Key::F8 => 100,
        Key::F9 => 101,
        Key::F11 => 103,
        Key::F13 => 105,
        Key::F16 => 106,
        Key::F14 => 107,
        Key::F10 => 109,
        Key::F12 => 111,
        Key::F15 => 113,
        Key::Insert => 114,
        Key::Home => 115,
        Key::PageUp => 116,
        Key::Delete => 117,
        Key::F4 => 118,
        Key::End => 119,
        Key::F2 => 120,
        Key::PageDown => 121,
        Key::F1 => 122,
        Key::ArrowLeft => 123,
        Key::ArrowRight => 124,
        Key::ArrowDown => 125,
        Key::ArrowUp => 126,
        _ => return None,
    })
}

/// Whether the key with this virtual key code is physically down now, from
/// the keyboard's own state (not from events, which the input method may
/// have swallowed). Off macOS, always down: nothing is released.
#[must_use]
pub fn virtual_key_down(code: u16) -> bool {
    #[cfg(target_os = "macos")]
    {
        native::virtual_key_down(code)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = code;
        true
    }
}

fn side_bits(flags: usize) -> u8 {
    let mut bits = 0;
    if flags & DEVICE_LEFT_OPTION != 0 {
        bits |= LEFT;
    }
    if flags & DEVICE_RIGHT_OPTION != 0 {
        bits |= RIGHT;
    }
    bits
}

const fn side(bits: u8) -> OptionSide {
    match bits & (LEFT | RIGHT) {
        0 => OptionSide::None,
        LEFT => OptionSide::Left,
        RIGHT => OptionSide::Right,
        _ => OptionSide::Both,
    }
}

/// The Option keys held now, given that egui reports Option (`alt`) held.
/// Without device bits (a synthetic event), the left key is assumed.
pub fn held_side(alt: bool) -> OptionSide {
    if !alt {
        return OptionSide::None;
    }
    match side(HELD.load(Ordering::Relaxed)) {
        OptionSide::None => OptionSide::Left,
        held => held,
    }
}

/// Which Option keys act as Alt while the input method composes; `None`
/// when it does not (nothing is rewritten).
pub fn set_alt_sides(left: bool, right: bool) {
    let mut bits = 0;
    if left {
        bits |= LEFT;
    }
    if right {
        bits |= RIGHT;
    }
    ALT_SIDES.store(bits, Ordering::Relaxed);
}

/// Whether a key event with these held Option keys is rewritten: every
/// held Option key acts as Alt.
fn rewrites(held: u8, alt_sides: u8) -> bool {
    held != 0 && held & !alt_sides == 0
}

#[cfg(target_os = "macos")]
mod native {
    use super::{rewrites, side_bits, ALT_SIDES, HELD, WAKE};
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventType};
    use std::ptr::NonNull;
    use std::sync::atomic::Ordering;
    use std::sync::Once;

    static INSTALL: Once = Once::new();

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    }

    /// `kCGEventSourceStateHIDSystemState`: the hardware's own state.
    const HID_SYSTEM_STATE: i32 = 1;

    pub fn virtual_key_down(code: u16) -> bool {
        // SAFETY: a plain query of the keyboard's state.
        unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, code) }
    }

    fn rewritten(event: &NSEvent) -> Option<Retained<NSEvent>> {
        let plain = event.charactersIgnoringModifiers()?;
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            event.r#type(),
            event.locationInWindow(),
            event.modifierFlags(),
            event.timestamp(),
            event.windowNumber(),
            None,
            &plain,
            &plain,
            event.isARepeat(),
            event.keyCode(),
        )
    }

    pub fn install() {
        INSTALL.call_once(|| {
            let handler = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
                // SAFETY: AppKit hands the monitor a live event for the
                // duration of the call.
                let event_ref = unsafe { event.as_ref() };
                let held = side_bits(event_ref.modifierFlags().0);
                HELD.store(held, Ordering::Relaxed);
                let kind = event_ref.r#type();
                if kind == NSEventType::KeyUp {
                    if let Some(wake) = WAKE.get() {
                        wake.request_repaint();
                    }
                }
                if (kind == NSEventType::KeyDown || kind == NSEventType::KeyUp)
                    && rewrites(held, ALT_SIDES.load(Ordering::Relaxed))
                {
                    if let Some(replacement) = rewritten(event_ref) {
                        return Retained::autorelease_ptr(replacement);
                    }
                }
                event.as_ptr()
            });
            // SAFETY: the handler is retained by the monitor for the life of
            // the process; the monitor itself is never removed.
            let monitor = unsafe {
                NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                    NSEventMask::KeyDown | NSEventMask::KeyUp | NSEventMask::FlagsChanged,
                    &handler,
                )
            };
            if let Some(monitor) = monitor {
                std::mem::forget(monitor);
            } else {
                tracing::warn!(
                    target: crate::logging::target::INPUT,
                    "Option key monitor unavailable; both Option keys act as the input method decides",
                );
            }
        });
    }
}

/// Installs the Option-key monitor (once); key releases wake `wake`.
pub fn install(wake: &egui::Context) {
    let _ = WAKE.set(wake.clone());
    #[cfg(target_os = "macos")]
    native::install();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sides_come_from_device_bits() {
        assert_eq!(side(side_bits(0x20 | 0x0008_0000)), OptionSide::Left);
        assert_eq!(side(side_bits(0x40 | 0x0008_0000)), OptionSide::Right);
        assert_eq!(side(side_bits(0x60)), OptionSide::Both);
        assert_eq!(side(side_bits(0)), OptionSide::None);
    }

    #[test]
    fn keys_map_to_their_virtual_key_codes() {
        assert_eq!(virtual_key_code(egui::Key::A), Some(0));
        assert_eq!(virtual_key_code(egui::Key::R), Some(15));
        assert_eq!(virtual_key_code(egui::Key::Quote), Some(39));
        assert_eq!(virtual_key_code(egui::Key::AltRight), Some(61));
        assert_eq!(virtual_key_code(egui::Key::Num5), Some(23));
        assert_eq!(virtual_key_code(egui::Key::Enter), Some(36));
        assert_eq!(virtual_key_code(egui::Key::Copy), None);
    }

    #[test]
    fn only_alt_sides_are_rewritten() {
        assert!(rewrites(LEFT, LEFT));
        assert!(!rewrites(RIGHT, LEFT));
        assert!(!rewrites(LEFT | RIGHT, LEFT));
        assert!(rewrites(LEFT | RIGHT, LEFT | RIGHT));
        assert!(!rewrites(0, LEFT));
    }
}
