// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Windows key messages to PC keys.
//!
//! Windows reports a key with a Virtual-Key code, its set 1 scan code and an
//! extended (`E0`) flag (`WM_KEYDOWN` `lParam`, or `KBDLLHOOKSTRUCT` under
//! the low-level hook). The position comes from the scan code
//! ([`super::pc::pc_key`]); the Virtual-Key settles what the scan code
//! alone cannot: Num Lock's effect on the keypad, Pause against Num Lock,
//! the keys an input method owns, and the Control Windows adds before
//! `AltGr`. These are plain numbers from Microsoft's published tables; the
//! module calls nothing.

use super::pc::{
    ImeKey, KEY_CLEAR, KEY_PAUSE, KEY_PRINT, LockKey, PcKey, PcModifier, keypad_navigation, pc_key,
};
use super::scancode::Set1Scancode;

/// `VK_CANCEL`: Control+Pause (Break).
pub const VK_CANCEL: u16 = 0x03;
/// `VK_CLEAR`: the keypad's 5 with Num Lock off.
pub const VK_CLEAR: u16 = 0x0C;
/// `VK_PAUSE`.
pub const VK_PAUSE: u16 = 0x13;
/// `VK_SNAPSHOT`: Print Screen.
pub const VK_SNAPSHOT: u16 = 0x2C;
/// `VK_NUMLOCK`.
pub const VK_NUMLOCK: u16 = 0x90;
/// `VK_PROCESSKEY`: a key the input method took.
pub const VK_PROCESSKEY: u16 = 0xE5;
/// `VK_PACKET`: a Unicode character injected as a key.
pub const VK_PACKET: u16 = 0xE7;
/// The scan code `KBDLLHOOKSTRUCT` reports for the Control Windows adds
/// before `AltGr` (`0x1D` with bit 9 set).
pub const LL_HOOK_ALTGR_CONTROL_SCAN: u32 = 0x21D;

/// What a Windows key message is, from its Virtual-Key, scan code and
/// extended flag.
///
/// `scan` is the message's scan code (`lParam` bits 16–23, or
/// `KBDLLHOOKSTRUCT::scanCode`); the adapter maps a zero scan code from its
/// Virtual-Key first (`MapVirtualKeyW`), since only the active layout knows
/// which position an `OEM` Virtual-Key is.
#[must_use]
pub fn windows_key(vk: u16, scan: u32, extended: bool) -> PcKey {
    if scan == LL_HOOK_ALTGR_CONTROL_SCAN {
        return PcKey::Synthetic;
    }
    match vk {
        VK_PACKET => return PcKey::CharacterOnly,
        VK_PROCESSKEY => return PcKey::Ime(ImeKey::Processed),
        // VK_KANA/VK_HANGUL, VK_IME_ON.
        0x15 | 0x16 => return PcKey::Ime(ImeKey::KanaHangul),
        // VK_KANJI/VK_HANJA, VK_JUNJA, VK_FINAL, VK_IME_OFF.
        0x17..=0x1A => return PcKey::Ime(ImeKey::KanjiHanja),
        0x1C => return PcKey::Ime(ImeKey::Convert),
        0x1D => return PcKey::Ime(ImeKey::NonConvert),
        // VK_ACCEPT, VK_MODECHANGE; the DBE keys (Eisu, Katakana,
        // Hiragana, Hankaku, Zenkaku, Roman).
        0x1E | 0x1F | 0xF0..=0xF5 => return PcKey::Ime(ImeKey::InputMode),
        VK_NUMLOCK => return PcKey::Lock(LockKey::Num),
        // Control turns the Num Lock key (E0 45) into VK_PAUSE and the
        // Scroll Lock key (46) into VK_CANCEL: the position decides.
        VK_PAUSE | VK_CANCEL => {
            return match (scan, extended) {
                (0x45, true) => PcKey::Lock(LockKey::Num),
                (0x46, false) => PcKey::Lock(LockKey::Scroll),
                _ => PcKey::Position {
                    qt_key: KEY_PAUSE,
                    keypad: false,
                },
            };
        }
        VK_SNAPSHOT => {
            return PcKey::Position {
                qt_key: KEY_PRINT,
                keypad: false,
            };
        }
        // VK_SLEEP; browser, volume, media and launcher keys.
        0x5F | 0xA6..=0xB7 => return PcKey::LocalOnly,
        _ => {}
    }
    let Ok(code) = u8::try_from(scan) else {
        return PcKey::Unmapped;
    };
    let key = pc_key(Set1Scancode { code, extended });
    match key {
        PcKey::Position {
            qt_key,
            keypad: true,
        } if !is_keypad_digit_vk(vk) => match (vk, keypad_navigation(qt_key)) {
            (VK_CLEAR, Some(_)) => PcKey::Position {
                qt_key: KEY_CLEAR,
                keypad: false,
            },
            (_, Some(navigation)) => PcKey::Position {
                qt_key: navigation,
                keypad: false,
            },
            (_, None) => key,
        },
        _ => key,
    }
}

/// `VK_NUMPAD0`–`VK_NUMPAD9`, `VK_MULTIPLY`–`VK_DIVIDE`, or `VK_RETURN`
/// and `VK_OEM_NEC_EQUAL`: a keypad key Windows reports as its keypad self.
const fn is_keypad_digit_vk(vk: u16) -> bool {
    matches!(vk, 0x60..=0x6F | 0x0D | 0x92)
}

/// Reads key messages so that a key's release is always the key its press
/// was.
///
/// Windows reports some keys under another Virtual-Key depending on what
/// else is held at each edge: a keypad 7 pressed as a digit is released as
/// Home if Shift or Num Lock changed in between. The key a press became is
/// kept by position and used for its release, so the host never keeps a key
/// whose release went out as another one.
#[derive(Debug, Clone, Default)]
pub struct WindowsKeyReader {
    pressed: std::collections::BTreeMap<(u32, bool), PcKey>,
}

impl WindowsKeyReader {
    /// One key message; repeats keep what the press was.
    pub fn read(&mut self, vk: u16, scan: u32, extended: bool, pressed: bool) -> PcKey {
        let key = windows_key(vk, scan, extended);
        if scan == 0 || matches!(key, PcKey::Synthetic) {
            return key;
        }
        let position = (scan, extended);
        if pressed {
            *self.pressed.entry(position).or_insert(key)
        } else {
            self.pressed.remove(&position).unwrap_or(key)
        }
    }

    /// Forgets every press (focus loss: Windows will not report their
    /// releases to this window).
    pub fn clear(&mut self) {
        self.pressed.clear();
    }
}

/// One key message after [`windows_key`], with the message time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedKey {
    /// The key.
    pub key: PcKey,
    /// Pressed or released.
    pub pressed: bool,
    /// The message time (`GetMessageTime`, `KBDLLHOOKSTRUCT::time`), in
    /// milliseconds.
    pub time_ms: u32,
}

/// Drops the left Control Windows sends with `AltGr` under a layout that has
/// one.
///
/// Window messages carry no mark for it: it arrives as a left Control press
/// with the same message time as the right Alt press that follows it, and
/// its release the same way. (The low-level hook marks it with
/// [`LL_HOOK_ALTGR_CONTROL_SCAN`].) A left Control press is therefore held
/// until the next key message: dropped if that is the right Alt pressed at
/// the same time, sent first otherwise. [`Self::flush`] sends a held press
/// once the message queue holds no more keys.
#[derive(Debug, Clone, Default)]
pub struct AltGrControlFilter {
    held: Option<TimedKey>,
    added_control_down: bool,
}

impl AltGrControlFilter {
    const LEFT_CONTROL: PcKey = PcKey::Modifier(PcModifier::LeftControl);
    const RIGHT_ALT: PcKey = PcKey::Modifier(PcModifier::RightAlt);

    /// One key message; the messages to act on now, in order.
    pub fn push(&mut self, event: TimedKey) -> Vec<TimedKey> {
        let mut out = Vec::with_capacity(2);
        if let Some(held) = self.held.take() {
            if event.key == Self::RIGHT_ALT && event.pressed && event.time_ms == held.time_ms {
                self.added_control_down = true;
                out.push(event);
                return out;
            }
            out.push(held);
        }
        if event.key == Self::LEFT_CONTROL {
            if event.pressed && !self.added_control_down {
                self.held = Some(event);
                return out;
            }
            if !event.pressed && self.added_control_down {
                self.added_control_down = false;
                return out;
            }
            if event.pressed {
                // A repeat of the added Control while AltGr is held.
                return out;
            }
        }
        out.push(event);
        out
    }

    /// The held Control press, once no key message follows it.
    pub fn flush(&mut self) -> Option<TimedKey> {
        self.held.take()
    }

    /// Forgets everything (focus loss).
    pub fn clear(&mut self) {
        self.held = None;
        self.added_control_down = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::pc::{KEY_DELETE, KEY_HOME};

    const VK_HOME: u16 = 0x24;
    const VK_NUMPAD7: u16 = 0x67;

    fn position(qt_key: u32, keypad: bool) -> PcKey {
        PcKey::Position { qt_key, keypad }
    }

    #[test]
    fn num_lock_decides_what_a_keypad_key_is() {
        assert_eq!(windows_key(VK_NUMPAD7, 0x47, false), position(0x37, true));
        assert_eq!(windows_key(VK_HOME, 0x47, false), position(KEY_HOME, false));
        assert_eq!(
            windows_key(VK_CLEAR, 0x4C, false),
            position(KEY_CLEAR, false)
        );
        assert_eq!(windows_key(0x2E, 0x53, false), position(KEY_DELETE, false));
        assert_eq!(windows_key(0x6E, 0x53, false), position(0x2E, true));
        // The navigation cluster is unaffected.
        assert_eq!(windows_key(VK_HOME, 0x47, true), position(KEY_HOME, false));
        // Keypad operators and Enter have no Num Lock meaning.
        assert_eq!(windows_key(0x6A, 0x37, false), position(0x2A, true));
        assert_eq!(windows_key(0x0D, 0x1C, true), position(0x0100_0005, true));
    }

    #[test]
    fn pause_and_num_lock_share_a_scan_code() {
        assert_eq!(
            windows_key(VK_NUMLOCK, 0x45, true),
            PcKey::Lock(LockKey::Num)
        );
        assert_eq!(
            windows_key(VK_PAUSE, 0x45, false),
            position(KEY_PAUSE, false)
        );
        assert_eq!(
            windows_key(VK_CANCEL, 0x46, true),
            position(KEY_PAUSE, false)
        );
        assert_eq!(
            windows_key(VK_SNAPSHOT, 0x54, false),
            position(KEY_PRINT, false)
        );
    }

    #[test]
    fn control_never_turns_a_lock_key_into_pause() {
        assert_eq!(windows_key(VK_PAUSE, 0x45, true), PcKey::Lock(LockKey::Num));
        assert_eq!(
            windows_key(VK_CANCEL, 0x46, false),
            PcKey::Lock(LockKey::Scroll)
        );
    }

    #[test]
    fn a_release_is_the_key_its_press_was() {
        let mut reader = WindowsKeyReader::default();
        // Keypad 7 as a digit, released after Shift made it Home.
        assert_eq!(
            reader.read(VK_NUMPAD7, 0x47, false, true),
            position(0x37, true)
        );
        assert_eq!(
            reader.read(VK_HOME, 0x47, false, true),
            position(0x37, true)
        );
        assert_eq!(
            reader.read(VK_HOME, 0x47, false, false),
            position(0x37, true)
        );
        // Pause pressed alone, released with Control held (Break).
        assert_eq!(
            reader.read(VK_PAUSE, 0x45, false, true),
            position(KEY_PAUSE, false)
        );
        assert_eq!(
            reader.read(VK_CANCEL, 0x46, true, false),
            position(KEY_PAUSE, false)
        );
        // Scroll Lock pressed alone, released with Control held.
        assert_eq!(
            reader.read(0x91, 0x46, false, true),
            PcKey::Lock(LockKey::Scroll)
        );
        assert_eq!(
            reader.read(VK_CANCEL, 0x46, false, false),
            PcKey::Lock(LockKey::Scroll)
        );
        reader.clear();
        assert_eq!(
            reader.read(VK_HOME, 0x47, false, false),
            position(KEY_HOME, false)
        );
    }

    #[test]
    fn input_method_keys_win_over_their_position() {
        // A Korean layout's right Alt is Hangul, its right Control Hanja.
        assert_eq!(
            windows_key(0x15, 0x38, true),
            PcKey::Ime(ImeKey::KanaHangul)
        );
        assert_eq!(
            windows_key(0x19, 0x1D, true),
            PcKey::Ime(ImeKey::KanjiHanja)
        );
        // Japanese Hankaku/Zenkaku is the grave position.
        assert_eq!(
            windows_key(0xF3, 0x29, false),
            PcKey::Ime(ImeKey::InputMode)
        );
        assert_eq!(windows_key(0x1C, 0x79, false), PcKey::Ime(ImeKey::Convert));
        assert_eq!(
            windows_key(VK_PROCESSKEY, 0x1E, false),
            PcKey::Ime(ImeKey::Processed)
        );
        assert_eq!(windows_key(VK_PACKET, 0, false), PcKey::CharacterOnly);
    }

    #[test]
    fn layout_letters_follow_the_position_not_the_virtual_key() {
        // Under AZERTY the position of Q reports VK_A.
        assert_eq!(windows_key(0x41, 0x10, false), position(0x51, false));
        // The right Alt is the right Alt whatever the layout calls it.
        assert_eq!(
            windows_key(0xA5, 0x38, true),
            PcKey::Modifier(PcModifier::RightAlt)
        );
        assert_eq!(windows_key(0xAD, 0x20, true), PcKey::LocalOnly);
        assert_eq!(windows_key(0x5F, 0x5F, true), PcKey::LocalOnly);
    }

    #[test]
    fn the_hooks_altgr_control_is_synthetic() {
        assert_eq!(
            windows_key(0xA2, LL_HOOK_ALTGR_CONTROL_SCAN, false),
            PcKey::Synthetic
        );
        assert_eq!(
            windows_key(0xA2, 0x1D, false),
            PcKey::Modifier(PcModifier::LeftControl)
        );
    }

    fn timed(key: PcKey, pressed: bool, time_ms: u32) -> TimedKey {
        TimedKey {
            key,
            pressed,
            time_ms,
        }
    }

    const LCTRL: PcKey = PcKey::Modifier(PcModifier::LeftControl);
    const RALT: PcKey = PcKey::Modifier(PcModifier::RightAlt);

    #[test]
    fn altgr_drops_the_control_windows_adds() {
        let mut filter = AltGrControlFilter::default();
        assert!(filter.push(timed(LCTRL, true, 100)).is_empty());
        assert_eq!(
            filter.push(timed(RALT, true, 100)),
            [timed(RALT, true, 100)]
        );
        let two = position(0x32, false);
        assert_eq!(filter.push(timed(two, true, 150)), [timed(two, true, 150)]);
        // Repeats of both while held.
        assert!(filter.push(timed(LCTRL, true, 180)).is_empty());
        assert_eq!(
            filter.push(timed(RALT, true, 180)),
            [timed(RALT, true, 180)]
        );
        assert!(filter.push(timed(LCTRL, false, 200)).is_empty());
        assert_eq!(
            filter.push(timed(RALT, false, 200)),
            [timed(RALT, false, 200)]
        );
        assert_eq!(filter.flush(), None);
    }

    #[test]
    fn a_real_left_control_is_kept() {
        let mut filter = AltGrControlFilter::default();
        assert!(filter.push(timed(LCTRL, true, 100)).is_empty());
        let c = position(0x43, false);
        assert_eq!(
            filter.push(timed(c, true, 140)),
            [timed(LCTRL, true, 100), timed(c, true, 140)]
        );
        assert_eq!(
            filter.push(timed(LCTRL, false, 200)),
            [timed(LCTRL, false, 200)]
        );
        // Control then right Alt later is Control+Alt, not AltGr.
        assert!(filter.push(timed(LCTRL, true, 300)).is_empty());
        assert_eq!(
            filter.push(timed(RALT, true, 320)),
            [timed(LCTRL, true, 300), timed(RALT, true, 320)]
        );
        // A lone press is sent once nothing follows it.
        let mut lone = AltGrControlFilter::default();
        assert!(lone.push(timed(LCTRL, true, 10)).is_empty());
        assert_eq!(lone.flush(), Some(timed(LCTRL, true, 10)));
    }
}
