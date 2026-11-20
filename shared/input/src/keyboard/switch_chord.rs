// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The keyboard switch's chord: the keys that flip the keyboard between the
//! remote computer and this computer ([`super::KeyboardCapture::toggle`]).
//!
//! One chord for both Decks, written the PC way (Ctrl, Alt, Shift, Win) and
//! read on a Mac with the usual transliteration: Control → Control, Alt →
//! Option, Win → Command. The default is Ctrl+Alt+Home (Ctrl+Option+Home on
//! a Mac). A user may pick a preset or type another chord; [`check`] decides
//! whether it can work and warns when it takes a chord a professional
//! application uses ([`super::PRO_APP_CHORDS`]).
//!
//! [`check`]: KeyboardSwitchChord::check

use arcen_protocol::keys::{
    KEY_ESCAPE, KEY_Q, KEY_RETURN, KEY_TAB, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT,
};

use super::deck_shortcuts::{DeckShortcut, MacChord, PcChord, PcModifierState};
use super::pc::{KEY_DELETE, KEY_END, KEY_ENTER, KEY_F1, KEY_HOME, KEY_PAUSE, KEY_PRINT};
use super::pro_app_chords::PRO_APP_CHORDS;

const KEY_BACKSPACE: u32 = 0x0100_0003;
const KEY_INSERT: u32 = 0x0100_0006;
const KEY_LEFT: u32 = 0x0100_0012;
const KEY_UP: u32 = 0x0100_0013;
const KEY_RIGHT: u32 = 0x0100_0014;
const KEY_DOWN: u32 = 0x0100_0015;
const KEY_PAGE_UP: u32 = 0x0100_0016;
const KEY_PAGE_DOWN: u32 = 0x0100_0017;
const KEY_F24: u32 = KEY_F1 + 23;
const KEY_SPACE: u32 = 0x20;
const KEY_GRAVE: u32 = 0x60;

/// The keys that flip the keyboard between the remote computer and this
/// computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per modifier key of a chord"
)]
pub struct KeyboardSwitchChord {
    /// Either Control (Control on a Mac).
    pub control: bool,
    /// The left Alt (either Option on a Mac).
    pub alt: bool,
    /// Either Shift.
    pub shift: bool,
    /// Either Windows key (Command on a Mac).
    pub windows: bool,
    /// The key identifier (a Qt key code, upper-case for letters).
    pub key: u32,
}

impl Default for KeyboardSwitchChord {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The chords Settings offers by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardSwitchPreset {
    /// Ctrl+Alt+Home (Ctrl+Option+Home on a Mac), the default.
    ControlAltHome,
    /// Ctrl+Alt+Shift+Z.
    ControlAltShiftZ,
    /// Alt+F1 (Option+F1 on a Mac).
    AltF1,
    /// Any other chord the user typed.
    Custom,
}

impl KeyboardSwitchPreset {
    /// Every choice, in menu order.
    pub const ALL: [Self; 4] = [
        Self::ControlAltHome,
        Self::ControlAltShiftZ,
        Self::AltF1,
        Self::Custom,
    ];

    /// The preset's chord (`None` for a custom one).
    #[must_use]
    pub const fn chord(self) -> Option<KeyboardSwitchChord> {
        match self {
            Self::ControlAltHome => Some(KeyboardSwitchChord::DEFAULT),
            Self::ControlAltShiftZ => Some(KeyboardSwitchChord::CONTROL_ALT_SHIFT_Z),
            Self::AltF1 => Some(KeyboardSwitchChord::ALT_F1),
            Self::Custom => None,
        }
    }

    /// A note Settings shows under the choice, if any.
    #[must_use]
    pub const fn note(self) -> Option<&'static str> {
        match self {
            Self::AltF1 => Some(
                "GNOME uses Alt+F1 to open its Activities overview; with this chord a GNOME \
                 desktop never sees it.",
            ),
            Self::ControlAltHome | Self::ControlAltShiftZ | Self::Custom => None,
        }
    }
}

/// Why a chord cannot be the keyboard switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchChordRejection {
    /// The text names no key, or a key Arcen does not know.
    Unreadable,
    /// No Ctrl, Alt or Win (Command): a plain key, or Shift and a key, is
    /// typing.
    NeedsModifier,
    /// The operating system keeps it and never hands it to an application
    /// (Win+L, Ctrl+Alt+Del, and on a Mac ⌥⌘Esc and ⌃⌘Q).
    NeverDelivered,
    /// Another Arcen Deck shortcut already uses it.
    DeckShortcut(DeckShortcut),
    /// macOS acts on it before any window sees it (⌘Tab, ⌘Space, Mission
    /// Control, Spaces, the screenshot keys, ⌥⌘D), so a Mac could never
    /// use it as the switch.
    MacSystemShortcut,
}

impl SwitchChordRejection {
    /// What Settings says.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Unreadable => {
                "Type a chord such as Ctrl+Alt+Home: modifiers and one key, joined with +."
            }
            Self::NeedsModifier => {
                "Add Ctrl, Alt or Win (Command): a plain key or Shift and a key is typing."
            }
            Self::NeverDelivered => {
                "The operating system keeps this chord for itself and never passes it on."
            }
            Self::DeckShortcut(_) => "Arcen Deck already uses this chord for another command.",
            Self::MacSystemShortcut => {
                "macOS acts on this chord before Arcen Deck can see it, so it cannot work on a Mac."
            }
        }
    }
}

/// Whether a chord can be the keyboard switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchChordCheck {
    /// Fine.
    Ok,
    /// It works, but a host application loses this chord: the description
    /// from [`super::PRO_APP_CHORDS`].
    ProAppCollision(&'static str),
    /// It cannot work.
    Rejected(SwitchChordRejection),
}

impl KeyboardSwitchChord {
    /// Ctrl+Alt+Home, Ctrl+Option+Home on a Mac.
    pub const DEFAULT: Self = Self::new(true, true, false, false, KEY_HOME);
    /// Ctrl+Alt+Shift+Z.
    pub const CONTROL_ALT_SHIFT_Z: Self = Self::new(true, true, true, false, 0x5A);
    /// Alt+F1.
    pub const ALT_F1: Self = Self::new(false, true, false, false, KEY_F1);

    /// A chord of these modifiers and `key`.
    #[must_use]
    #[allow(
        clippy::fn_params_excessive_bools,
        reason = "one flag per modifier key, in the order the chord is spelled"
    )]
    pub const fn new(control: bool, alt: bool, shift: bool, windows: bool, key: u32) -> Self {
        Self {
            control,
            alt,
            shift,
            windows,
            key,
        }
    }

    /// The preset this chord is, or [`KeyboardSwitchPreset::Custom`].
    #[must_use]
    pub fn preset(self) -> KeyboardSwitchPreset {
        KeyboardSwitchPreset::ALL
            .into_iter()
            .find(|preset| preset.chord() == Some(self))
            .unwrap_or(KeyboardSwitchPreset::Custom)
    }

    /// The chord on a PC keyboard.
    #[must_use]
    pub const fn pc_chord(self) -> PcChord {
        PcChord {
            control: self.control,
            alt: self.alt,
            shift: self.shift,
            windows: self.windows,
            key: self.key,
        }
    }

    /// The chord on a Mac keyboard.
    #[must_use]
    pub const fn mac_chord(self) -> MacChord {
        MacChord {
            control: self.control,
            option: self.alt,
            shift: self.shift,
            command: self.windows,
            key: self.key,
        }
    }

    /// Whether a key press on a PC keyboard, with `held` held, is this
    /// chord. `AltGr` held never is: it types characters.
    #[must_use]
    pub const fn matches_pc(self, qt_key: u32, held: PcModifierState) -> bool {
        !held.altgr
            && qt_key == self.key
            && held.control == self.control
            && held.alt == self.alt
            && held.shift == self.shift
            && held.windows == self.windows
    }

    /// Whether a chord pressed on a Mac keyboard is this chord.
    #[must_use]
    pub const fn matches_mac(self, pressed: MacChord) -> bool {
        pressed.key == self.key
            && pressed.control == self.control
            && pressed.option == self.alt
            && pressed.shift == self.shift
            && pressed.command == self.windows
    }

    /// The modifiers as `MOD_*` bits.
    #[must_use]
    pub const fn modifier_bits(self) -> u32 {
        (if self.control { MOD_CTRL } else { 0 })
            | (if self.alt { MOD_ALT } else { 0 })
            | (if self.shift { MOD_SHIFT } else { 0 })
            | (if self.windows { MOD_META } else { 0 })
    }

    /// Reads a chord: modifiers and one key joined with `+` (or `-`), in
    /// any order and case. Ctrl/Control, Alt/Option, Shift and
    /// Win/Windows/Super/Cmd/Command are modifiers; keys are letters,
    /// digits, F1–F24 and named keys (Home, End, Insert, Delete, Page Up,
    /// Page Down, Pause/Break, Space, Tab, Esc, Enter, Backspace, the
    /// arrows). `None` when it names no single key.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut chord = Self::new(false, false, false, false, 0);
        let mut key = None;
        let normalised: String = text
            .trim()
            .chars()
            .map(|c| match c {
                '⌃' => "Ctrl+".to_owned(),
                '⌥' => "Alt+".to_owned(),
                '⇧' => "Shift+".to_owned(),
                '⌘' => "Win+".to_owned(),
                other => other.to_string(),
            })
            .collect();
        for part in normalised.split(['+', '-']) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "ctl" => chord.control = true,
                "alt" | "option" | "opt" => chord.alt = true,
                "shift" => chord.shift = true,
                "win" | "windows" | "super" | "meta" | "cmd" | "command" => chord.windows = true,
                _ => {
                    if key.is_some() {
                        return None;
                    }
                    key = Some(key_from_name(part)?);
                }
            }
        }
        chord.key = key?;
        Some(chord)
    }

    /// As Settings stores it, and as a PC spells it: "Ctrl+Alt+Home".
    #[must_use]
    pub fn pc_text(self) -> String {
        let mut parts = Vec::new();
        if self.control {
            parts.push("Ctrl".to_owned());
        }
        if self.alt {
            parts.push("Alt".to_owned());
        }
        if self.shift {
            parts.push("Shift".to_owned());
        }
        if self.windows {
            parts.push("Win".to_owned());
        }
        parts.push(key_text(self.key));
        parts.join("+")
    }

    /// As a Mac's menus spell it: "Control-Option-Home".
    #[must_use]
    pub fn mac_text(self) -> String {
        let mut parts = Vec::new();
        if self.control {
            parts.push("Control".to_owned());
        }
        if self.alt {
            parts.push("Option".to_owned());
        }
        if self.shift {
            parts.push("Shift".to_owned());
        }
        if self.windows {
            parts.push("Command".to_owned());
        }
        parts.push(key_text(self.key));
        parts.join("-")
    }

    /// Whether this chord can be the keyboard switch on both Decks, and
    /// whether it takes a chord a professional application uses.
    #[must_use]
    pub fn check(self) -> SwitchChordCheck {
        if key_text(self.key).is_empty() || is_modifier_key(self.key) {
            return SwitchChordCheck::Rejected(SwitchChordRejection::Unreadable);
        }
        if !(self.control || self.alt || self.windows) {
            return SwitchChordCheck::Rejected(SwitchChordRejection::NeedsModifier);
        }
        if self.never_delivered() {
            return SwitchChordCheck::Rejected(SwitchChordRejection::NeverDelivered);
        }
        let pc = self.pc_chord();
        let mac = self.mac_chord();
        for action in DeckShortcut::ALL {
            if action == DeckShortcut::ReleaseKeyboard {
                continue;
            }
            if action.pc_chords().contains(&pc) || action.mac_chord() == Some(mac) {
                return SwitchChordCheck::Rejected(SwitchChordRejection::DeckShortcut(action));
            }
        }
        if self.mac_system_shortcut() {
            return SwitchChordCheck::Rejected(SwitchChordRejection::MacSystemShortcut);
        }
        let bits = self.modifier_bits();
        PRO_APP_CHORDS
            .iter()
            .find(|(key, held, _)| *key == self.key && *held == bits)
            .map_or(SwitchChordCheck::Ok, |(_, _, description)| {
                SwitchChordCheck::ProAppCollision(description)
            })
    }

    /// The chord if it can be the switch, otherwise the default.
    #[must_use]
    pub fn or_default(self) -> Self {
        if matches!(self.check(), SwitchChordCheck::Rejected(_)) {
            Self::DEFAULT
        } else {
            self
        }
    }

    /// Win+L (and anything with Win+L), Ctrl+Alt+Del, and a Mac's ⌥⌘Esc
    /// (Force Quit) and ⌃⌘Q (Lock Screen).
    const fn never_delivered(self) -> bool {
        let lock = self.windows && self.key == 0x4C;
        let secure_attention = self.control && self.alt && self.key == KEY_DELETE;
        let force_quit = self.alt && self.windows && !self.control && self.key == KEY_ESCAPE;
        let mac_lock = self.control && self.windows && !self.alt && self.key == KEY_Q;
        lock || secure_attention || force_quit || mac_lock
    }

    /// The Mac system shortcuts a macOS Deck's event tap takes for the
    /// remote computer ([`super::MacSystemChord`]), in PC terms.
    const fn mac_system_shortcut(self) -> bool {
        let (c, a, s, w) = (self.control, self.alt, self.shift, self.windows);
        match self.key {
            KEY_TAB | KEY_GRAVE => w && !c && !a,
            KEY_SPACE => w && !c && !s,
            KEY_UP | KEY_DOWN | KEY_LEFT | KEY_RIGHT => c && !w && !a && !s,
            0x33..=0x35 => w && s && !a,
            0x44 => w && a && !c && !s,
            _ => false,
        }
    }
}

const fn is_modifier_key(key: u32) -> bool {
    matches!(key, 0x0100_0020..=0x0100_0023 | 0x0100_1103)
}

fn key_from_name(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "home" => KEY_HOME,
        "end" => KEY_END,
        "insert" | "ins" => KEY_INSERT,
        "delete" | "del" => KEY_DELETE,
        "pageup" | "page up" | "pgup" => KEY_PAGE_UP,
        "pagedown" | "page down" | "pgdn" => KEY_PAGE_DOWN,
        "pause" | "break" => KEY_PAUSE,
        "print" | "printscreen" | "print screen" | "prtsc" => KEY_PRINT,
        "space" => KEY_SPACE,
        "tab" => KEY_TAB,
        "esc" | "escape" => KEY_ESCAPE,
        "enter" | "return" => KEY_RETURN,
        "backspace" => KEY_BACKSPACE,
        "left" => KEY_LEFT,
        "right" => KEY_RIGHT,
        "up" => KEY_UP,
        "down" => KEY_DOWN,
        _ => {
            if let Some(number) = lower.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
                return (1..=24).contains(&number).then(|| KEY_F1 + number - 1);
            }
            let mut chars = name.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return None;
            };
            return match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' => Some(u32::from(c.to_ascii_uppercase())),
                '`' => Some(KEY_GRAVE),
                _ => None,
            };
        }
    };
    Some(named)
}

fn key_text(key: u32) -> String {
    match key {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(key).map_or_else(String::new, String::from),
        KEY_GRAVE => "`".to_owned(),
        KEY_F1..=KEY_F24 => format!("F{}", key - KEY_F1 + 1),
        KEY_HOME => "Home".to_owned(),
        KEY_END => "End".to_owned(),
        KEY_INSERT => "Insert".to_owned(),
        KEY_DELETE => "Delete".to_owned(),
        KEY_PAGE_UP => "Page Up".to_owned(),
        KEY_PAGE_DOWN => "Page Down".to_owned(),
        KEY_PAUSE => "Break".to_owned(),
        KEY_PRINT => "Print Screen".to_owned(),
        KEY_SPACE => "Space".to_owned(),
        KEY_TAB => "Tab".to_owned(),
        KEY_ESCAPE => "Esc".to_owned(),
        KEY_RETURN | KEY_ENTER => "Enter".to_owned(),
        KEY_BACKSPACE => "Backspace".to_owned(),
        KEY_LEFT => "Left".to_owned(),
        KEY_RIGHT => "Right".to_owned(),
        KEY_UP => "Up".to_owned(),
        KEY_DOWN => "Down".to_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_presets_read_back_and_the_default_is_control_alt_home() {
        assert_eq!(KeyboardSwitchChord::default().pc_text(), "Ctrl+Alt+Home");
        assert_eq!(
            KeyboardSwitchChord::default().mac_text(),
            "Control-Option-Home"
        );
        for preset in KeyboardSwitchPreset::ALL {
            let Some(chord) = preset.chord() else {
                continue;
            };
            assert_eq!(KeyboardSwitchChord::parse(&chord.pc_text()), Some(chord));
            assert_eq!(KeyboardSwitchChord::parse(&chord.mac_text()), Some(chord));
            assert_eq!(chord.preset(), preset);
            assert_ne!(
                chord.check(),
                SwitchChordCheck::Rejected(SwitchChordRejection::Unreadable)
            );
            assert!(!matches!(chord.check(), SwitchChordCheck::Rejected(_)));
        }
        assert!(
            KeyboardSwitchPreset::AltF1
                .note()
                .unwrap()
                .contains("GNOME")
        );
    }

    #[test]
    fn parsing_takes_any_spelling_and_refuses_two_keys() {
        assert_eq!(
            KeyboardSwitchChord::parse(" ctrl + ALT + shift + z "),
            Some(KeyboardSwitchChord::CONTROL_ALT_SHIFT_Z)
        );
        assert_eq!(
            KeyboardSwitchChord::parse("⌃⌥Home"),
            Some(KeyboardSwitchChord::DEFAULT)
        );
        assert_eq!(
            KeyboardSwitchChord::parse("Cmd+F12"),
            Some(KeyboardSwitchChord::new(
                false,
                false,
                false,
                true,
                KEY_F1 + 11
            ))
        );
        assert_eq!(KeyboardSwitchChord::parse("Ctrl+A+B"), None);
        assert_eq!(KeyboardSwitchChord::parse("Ctrl+Alt"), None);
        assert_eq!(KeyboardSwitchChord::parse("Ctrl+F25"), None);
        assert_eq!(KeyboardSwitchChord::parse(""), None);
    }

    #[test]
    fn chords_the_system_keeps_and_plain_typing_are_refused() {
        use SwitchChordRejection as R;
        for (text, rejection) in [
            ("Win+L", R::NeverDelivered),
            ("Ctrl+Win+L", R::NeverDelivered),
            ("Ctrl+Alt+Del", R::NeverDelivered),
            ("Ctrl+Alt+Shift+Delete", R::NeverDelivered),
            ("Alt+Cmd+Esc", R::NeverDelivered),
            ("Ctrl+Cmd+Q", R::NeverDelivered),
            ("A", R::NeedsModifier),
            ("Shift+A", R::NeedsModifier),
            ("F9", R::NeedsModifier),
            (
                "Ctrl+Alt+End",
                R::DeckShortcut(DeckShortcut::SendSecureAttention),
            ),
            ("Alt+F4", R::DeckShortcut(DeckShortcut::Quit)),
            ("Cmd+Q", R::DeckShortcut(DeckShortcut::Quit)),
            ("Cmd+Tab", R::MacSystemShortcut),
            ("Win+Space", R::MacSystemShortcut),
            ("Ctrl+Left", R::MacSystemShortcut),
            ("Cmd+Shift+4", R::MacSystemShortcut),
        ] {
            let chord = KeyboardSwitchChord::parse(text).expect(text);
            assert_eq!(
                chord.check(),
                SwitchChordCheck::Rejected(rejection),
                "{text}"
            );
            assert_eq!(chord.or_default(), KeyboardSwitchChord::DEFAULT, "{text}");
            assert!(!rejection.message().is_empty());
        }
    }

    #[test]
    fn a_chord_a_professional_application_uses_is_allowed_with_a_warning() {
        let chord = KeyboardSwitchChord::parse("Ctrl+Alt+Shift+K").unwrap();
        assert!(matches!(
            chord.check(),
            SwitchChordCheck::ProAppCollision(description) if description.contains("Flame")
        ));
        assert_eq!(chord.or_default(), chord);
        assert_eq!(
            KeyboardSwitchChord::parse("Ctrl+Alt+K").unwrap().check(),
            SwitchChordCheck::Ok
        );
    }

    #[test]
    fn matching_follows_the_chord_on_both_keyboards() {
        let chord = KeyboardSwitchChord::CONTROL_ALT_SHIFT_Z;
        let held = PcModifierState {
            control: true,
            alt: true,
            shift: true,
            ..PcModifierState::default()
        };
        assert!(chord.matches_pc(0x5A, held));
        assert!(!chord.matches_pc(
            0x5A,
            PcModifierState {
                altgr: true,
                ..held
            }
        ));
        assert!(!chord.matches_pc(
            0x5A,
            PcModifierState {
                shift: false,
                ..held
            }
        ));
        assert!(chord.matches_mac(MacChord {
            shift: true,
            ..MacChord::new(true, true, false, 0x5A)
        }));
        assert!(!chord.matches_mac(MacChord::new(true, true, false, 0x5A)));
    }
}
