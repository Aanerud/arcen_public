// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A PC keyboard on a Deck: its keyboard policy, its key edges to host key
//! edges, and which chords the Deck's own system keeps.
//!
//! A PC keyboard's Control, Windows, Alt and `AltGr` keys go to a Windows or
//! Linux host as themselves. A Mac host's shortcut key is Command, so its
//! Command is the PC's Control by default, as a Mac Deck's Command+C is a
//! Windows host's Control+C: the shortcut key of the keyboard in front of
//! someone is the shortcut key of the desktop they work on, whichever Deck
//! they use. The edges are made by the same [`ShortcutTranslator`] a Mac
//! Deck uses (a Windows key is its Command, Control its Control, the left
//! Alt its left Option and the right Alt its right Option), so both Decks
//! release, re-press and reset keys the same way.

use std::collections::BTreeSet;

use arcen_protocol::messages::HostPlatform;

use super::deck_shortcuts::{DeckShortcut, PcModifierState};
use super::pc::{KEY_DELETE, LockKey, PcKey, PcModifier, iso_key_identifier};
use super::send_keys::SendKey;
use super::switch_chord::KeyboardSwitchChord;
use super::{
    DeckKey, DeckModifiers, HostKeyEdge, KeyboardPolicy, KeyboardSettings, ShortcutMode,
    ShortcutPreference, ShortcutTranslator,
};
use crate::text::{KeyPhase, KeyStroke, OptionKeyMode, OptionKeyPreference, OptionSide};
use arcen_protocol::keys::{KEY_ALT, KEY_CONTROL, KEY_INTL_BACKSLASH, MOD_ALT, MOD_CTRL};

/// Which PC key is a Mac host's Command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MacHostCommandKey {
    /// Control is Command and the Windows key is Control, so Ctrl+C copies
    /// on a Mac host as on every other.
    #[default]
    Control,
    /// The Windows key is Command, by position, as macOS treats a PC
    /// keyboard plugged into a Mac.
    Windows,
}

impl MacHostCommandKey {
    /// Both, in menu order.
    pub const ALL: [Self; 2] = [Self::Control, Self::Windows];

    /// The settings token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Windows => "windows",
        }
    }

    /// The setting a token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.token() == token)
    }
}

/// What the right Alt (`AltGr`) is on a PC keyboard (`PARITY.md` Q-4, the
/// PC meaning of the Mac's Option-keys setting).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AltGrKey {
    /// It types the characters the Deck's layout puts on it, and is `AltGr`
    /// on the host; Alt where the layout has no `AltGr`.
    #[default]
    TypesCharacters,
    /// It is Alt, on the Deck and on the host.
    IsAlt,
}

impl AltGrKey {
    /// Both, in menu order.
    pub const ALL: [Self; 2] = [Self::TypesCharacters, Self::IsAlt];

    /// The settings token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::TypesCharacters => "types_characters",
            Self::IsAlt => "is_alt",
        }
    }

    /// The setting a token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.token() == token)
    }

    /// The shared Option-keys preference this is for the text policy: the
    /// right side composes characters only when the Deck's own layout has
    /// `AltGr` and the setting lets it; the left Alt is always Alt.
    #[must_use]
    pub const fn option_preference(self, layout_has_altgr: bool) -> OptionKeyPreference {
        OptionKeyPreference::Chosen(match (self, layout_has_altgr) {
            (Self::TypesCharacters, true) => OptionKeyMode::LeftAltRightCharacters,
            (Self::TypesCharacters, false) | (Self::IsAlt, _) => OptionKeyMode::BothAlt,
        })
    }
}

/// A connection's PC keyboard settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PcKeyboardSettings {
    /// Which PC key is a Mac host's Command.
    pub mac_host_command: MacHostCommandKey,
    /// What the right Alt is.
    pub altgr: AltGrKey,
}

impl PcKeyboardSettings {
    /// The PC meaning of the settings both Decks store: a connection's
    /// Command/Control choice (its "Keys as labelled" is
    /// [`ShortcutPreference::WindowsApp`]; a Mac-chosen swap or "Command is
    /// Control" works as Automatic) and the Option-keys setting (only "both
    /// Alt" makes `AltGr` plain Alt).
    #[must_use]
    pub const fn from_settings(
        keyboard: &KeyboardSettings,
        option_keys: OptionKeyPreference,
    ) -> Self {
        Self {
            mac_host_command: match keyboard.shortcuts {
                ShortcutPreference::WindowsApp => MacHostCommandKey::Windows,
                ShortcutPreference::Automatic
                | ShortcutPreference::SwapCommandControl
                | ShortcutPreference::CommandIsControl => MacHostCommandKey::Control,
            },
            altgr: match option_keys {
                OptionKeyPreference::Chosen(OptionKeyMode::BothAlt) => AltGrKey::IsAlt,
                OptionKeyPreference::Automatic
                | OptionKeyPreference::Chosen(
                    OptionKeyMode::LeftAltRightCharacters | OptionKeyMode::BothCharacters,
                ) => AltGrKey::TypesCharacters,
            },
        }
    }
}

/// The keyboard policy for a PC keyboard and a host.
///
/// Windows and Linux hosts get every key as itself: Control, the Windows
/// (Super) key, Alt, and the right Alt as `AltGr` wherever the host accepts
/// `extended_keys` (a PC keyboard has an `AltGr` key; a Mac's right Option
/// is `AltGr` only for a Windows host) and the setting keeps it `AltGr`. A
/// Mac host gets Control as Command unless the setting says otherwise, and
/// the right Alt as Option.
#[must_use]
pub fn resolve_pc_keyboard_policy(
    platform: HostPlatform,
    extended_keys: bool,
    settings: PcKeyboardSettings,
) -> KeyboardPolicy {
    let altgr = extended_keys && settings.altgr == AltGrKey::TypesCharacters;
    let (shortcuts, altgr) = match (platform, settings.mac_host_command) {
        (HostPlatform::MacOs, MacHostCommandKey::Control) => {
            (ShortcutMode::SwapCommandControl, false)
        }
        (HostPlatform::MacOs, MacHostCommandKey::Windows) => (ShortcutMode::CommandIsMeta, false),
        (HostPlatform::Windows | HostPlatform::Linux | HostPlatform::Unknown, _) => {
            (ShortcutMode::CommandIsMeta, altgr)
        }
    };
    KeyboardPolicy {
        shortcuts,
        right_option_altgr: altgr,
        left_option_altgr: false,
        intl_backslash: extended_keys,
        host_masks_meta_release: false,
        pc_print_keys: false,
    }
}

/// A chord the Deck's own operating system acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemChord {
    /// The Windows key, alone or with another key.
    WindowsKey,
    /// Alt+Tab (and Shift+Alt+Tab).
    AltTab,
    /// Alt+Esc.
    AltEscape,
    /// Ctrl+Esc.
    ControlEscape,
    /// Ctrl+Shift+Esc (Task Manager).
    TaskManager,
    /// Alt+Space (the window menu).
    WindowMenu,
    /// Print Screen.
    PrintScreen,
    /// Windows+L: Windows locks before any application sees it.
    LockWorkstation,
    /// Ctrl+Alt+Del: the secure attention sequence, which no application
    /// sees.
    SecureAttention,
}

impl SystemChord {
    /// Whether a keyboard grab can take it from the system. Windows locks
    /// on Windows+L and answers Ctrl+Alt+Del whatever a hook does.
    #[must_use]
    pub const fn grabbable(self) -> bool {
        !matches!(self, Self::LockWorkstation | Self::SecureAttention)
    }

    /// The system chord a key press is, with `held` the modifiers held at
    /// it (the key itself included when it is a modifier).
    #[must_use]
    pub fn of(key: PcKey, held: PcModifierState) -> Option<Self> {
        let qt_key = match key {
            PcKey::Modifier(PcModifier::LeftWindows | PcModifier::RightWindows) => {
                return Some(Self::WindowsKey);
            }
            PcKey::Position { qt_key, .. } => qt_key,
            _ => return None,
        };
        let alt = held.alt || held.altgr;
        Some(match qt_key {
            0x4C if held.windows => Self::LockWorkstation,
            _ if held.windows => Self::WindowsKey,
            KEY_DELETE if held.control && alt => Self::SecureAttention,
            0x0100_0001 if alt => Self::AltTab,
            0x0100_0000 if held.control && held.shift => Self::TaskManager,
            0x0100_0000 if alt => Self::AltEscape,
            0x0100_0000 if held.control => Self::ControlEscape,
            0x20 if alt => Self::WindowMenu,
            super::pc::KEY_PRINT => Self::PrintScreen,
            _ => return None,
        })
    }
}

/// Whether the low-level keyboard hook takes a key press for the session
/// under the keyboard grab: the Windows keys and the system shortcuts
/// Windows lets a hook take ([`SystemChord::grabbable`]). Every other key
/// stays on the window's own path, where Windows' layout and input method
/// still turn it into characters (`AltGr`, dead keys, IME), so the grab
/// changes only what the system would have acted on.
#[must_use]
pub fn grab_takes(key: PcKey, held: PcModifierState) -> bool {
    match key {
        PcKey::Modifier(PcModifier::LeftWindows | PcModifier::RightWindows) => true,
        PcKey::Position { .. } => SystemChord::of(key, held).is_some_and(SystemChord::grabbable),
        PcKey::Modifier(_)
        | PcKey::Lock(_)
        | PcKey::Ime(_)
        | PcKey::CharacterOnly
        | PcKey::LocalOnly
        | PcKey::Synthetic
        | PcKey::Unmapped => false,
    }
}

/// Where the low-level keyboard hook sends one key edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookRoute {
    /// On to the system (`CallNextHookEx`): the Deck's computer has it.
    System,
    /// Taken for the session (or a Deck chord); the system never sees it.
    Session,
    /// Taken, and the system holds a Windows key whose release would now
    /// open Start: inject the unassigned mask key (`VK 0xE8`) first, as the
    /// Windows Pier does for a pointer-only Windows key.
    SessionMaskingWindowsKey,
}

/// The low-level keyboard hook's routing, installed while the session window
/// has focus, so a key's release always goes where its press went.
///
/// With the grab on, the session takes what [`grab_takes`] allows. With it
/// off, the system keeps everything except the Deck's own chords
/// ([`DeckShortcut`], whose Ctrl+Windows chords Windows would otherwise act
/// on). A key whose press the system saw (it was down when routing began,
/// or was passed on, like Windows+L) has its release passed on too, so
/// Windows never keeps a key down on the Deck's computer.
///
/// The hooks installed before the Deck's see a taken key first; one that
/// blocks it ([`HookRouter::blocked_below`]) keeps it from the session.
#[derive(Debug, Clone, Default)]
pub struct HookRouter {
    system_keys: BTreeSet<PcKey>,
    session_keys: BTreeSet<PcKey>,
    held: BTreeSet<PcModifier>,
    switch: KeyboardSwitchChord,
    /// The press the last [`HookRouter::route`] newly gave the session.
    fresh: Option<PcKey>,
}

impl HookRouter {
    /// The keyboard switch's chord (Settings ▸ Input ▸ Keyboard switch).
    pub const fn set_switch_chord(&mut self, switch: KeyboardSwitchChord) {
        self.switch = switch;
    }

    /// Routing begins (the session window got focus, or the grab changed)
    /// with `down` held, as the system saw them.
    pub fn begin(&mut self, down: impl IntoIterator<Item = PcKey>) {
        self.system_keys = down.into_iter().collect();
        self.session_keys.clear();
        self.held = self
            .system_keys
            .iter()
            .filter_map(|key| match key {
                PcKey::Modifier(modifier) => Some(*modifier),
                _ => None,
            })
            .collect();
    }

    /// Where this edge goes, with the grab on or off.
    pub fn route(&mut self, key: PcKey, pressed: bool, grabbed: bool) -> HookRoute {
        self.fresh = None;
        let before = self.modifiers();
        if let PcKey::Modifier(modifier) = key {
            if pressed {
                self.held.insert(modifier);
            } else {
                self.held.remove(&modifier);
            }
        }
        if !pressed {
            if self.session_keys.remove(&key) {
                return HookRoute::Session;
            }
            // The system saw the press, or nobody routed it (it was down
            // before routing began): the window's own path has it.
            self.system_keys.remove(&key);
            return HookRoute::System;
        }
        // A repeat goes where its press went, whatever changed since.
        if self.system_keys.contains(&key) {
            return HookRoute::System;
        }
        if self.session_keys.contains(&key) {
            return HookRoute::Session;
        }
        // The Control Windows adds before AltGr goes with the AltGr, which
        // the window's own path reads.
        if key == PcKey::Synthetic {
            self.system_keys.insert(key);
            return HookRoute::System;
        }
        let deck_chord = matches!(key, PcKey::Position { qt_key, .. }
            if DeckShortcut::for_pc_key_with(qt_key, before, self.switch).is_some());
        let takes = deck_chord || (grabbed && grab_takes(key, self.modifiers()));
        if !takes {
            self.system_keys.insert(key);
            return HookRoute::System;
        }
        self.session_keys.insert(key);
        self.fresh = Some(key);
        let system_windows = self
            .system_keys
            .contains(&PcKey::Modifier(PcModifier::LeftWindows))
            || self
                .system_keys
                .contains(&PcKey::Modifier(PcModifier::RightWindows));
        if system_windows {
            HookRoute::SessionMaskingWindowsKey
        } else {
            HookRoute::Session
        }
    }

    /// A hook installed before the Deck's blocked the edge
    /// [`route`](Self::route) just gave the session (a Windows Pier keeping
    /// its desk private while the Deck runs in its session, a kiosk lock);
    /// whether the session still gets it.
    ///
    /// The Deck is never a way around such a hook: a press it blocked is
    /// not the session's, and that key's release follows it down the chain.
    /// A repeat it blocked is dropped. A release still goes to the session,
    /// whose press the host holds, so no key stays down there.
    pub fn blocked_below(&mut self, key: PcKey, pressed: bool) -> bool {
        if !pressed {
            return true;
        }
        if self.fresh.take() == Some(key) {
            self.session_keys.remove(&key);
        }
        false
    }

    /// Routing ends (focus lost): the keys the system still holds are its
    /// own.
    pub fn end(&mut self) {
        self.system_keys.clear();
        self.session_keys.clear();
        self.held.clear();
        self.fresh = None;
    }

    fn modifiers(&self) -> PcModifierState {
        let held = |modifier| self.held.contains(&modifier);
        PcModifierState {
            control: held(PcModifier::LeftControl) || held(PcModifier::RightControl),
            shift: held(PcModifier::LeftShift) || held(PcModifier::RightShift),
            alt: held(PcModifier::LeftAlt),
            altgr: held(PcModifier::RightAlt),
            windows: held(PcModifier::LeftWindows) || held(PcModifier::RightWindows),
        }
    }
}

/// One key edge from a PC keyboard, after the platform's own filtering
/// (`AltGr`'s added Control removed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcKeyEvent {
    /// The key.
    pub key: PcKey,
    /// Pressed or released.
    pub pressed: bool,
    /// An automatic repeat of a held key.
    pub repeat: bool,
}

/// What to do with one PC key edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcKeyAction {
    /// Send these host key edges (none while nothing changes on the host).
    Host(Vec<HostKeyEdge>),
    /// The Deck's own action; the key's release stays on the Deck.
    Shortcut(DeckShortcut),
    /// The Deck's operating system or input method has it.
    Local,
    /// The key's characters go as committed text, the key itself not.
    Text,
    /// Not sent.
    Suppressed {
        /// Why, for the input log.
        reason: &'static str,
    },
}

/// A key held on the PC keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HeldKey {
    Modifier(PcModifier),
    Position { qt_key: u32, keypad: bool },
}

/// A PC keyboard's key edges to the host's key edges.
///
/// Physical modifiers are tracked from the keyboard's own edges, because
/// under the keyboard grab the system's key state never sees the keys the
/// hook takes. Repeats never reach the host (it repeats keys itself), and a
/// key the Deck took for a shortcut keeps its release too.
#[derive(Debug, Default)]
pub struct PcKeyboardInput {
    translator: ShortcutTranslator,
    held: BTreeSet<HeldKey>,
    deck_keys: BTreeSet<u32>,
    /// Alt keys the text policy kept from the host because they compose
    /// characters (`SuppressReason::ComposingOption`).
    composing: BTreeSet<PcModifier>,
    grabbed: bool,
    /// This computer has the keyboard ([`super::KeyboardCapture`]): keys
    /// are only watched for the escape chord.
    released: bool,
    /// The keyboard switch's chord.
    switch: KeyboardSwitchChord,
    raw_count: u64,
    suppressed_count: u64,
}

impl PcKeyboardInput {
    /// Input with nothing held, under `policy`.
    #[must_use]
    pub fn new(policy: KeyboardPolicy) -> Self {
        Self {
            translator: ShortcutTranslator::new(policy),
            ..Self::default()
        }
    }

    /// Applies a host's keyboard policy; the releases of whatever the old
    /// one held on the host.
    pub fn set_policy(&mut self, policy: KeyboardPolicy) -> Vec<HostKeyEdge> {
        self.translator.set_policy(policy)
    }

    /// The policy in force.
    #[must_use]
    pub const fn policy(&self) -> KeyboardPolicy {
        self.translator.policy()
    }

    /// The keyboard switch's chord (Settings ▸ Input ▸ Keyboard switch).
    pub const fn set_switch_chord(&mut self, switch: KeyboardSwitchChord) {
        self.switch = switch;
    }

    /// The modifier keys held on the PC keyboard.
    #[must_use]
    pub fn modifiers(&self) -> PcModifierState {
        let held = |modifier| {
            self.held.contains(&HeldKey::Modifier(modifier)) || self.composing.contains(&modifier)
        };
        PcModifierState {
            control: held(PcModifier::LeftControl) || held(PcModifier::RightControl),
            shift: held(PcModifier::LeftShift) || held(PcModifier::RightShift),
            alt: held(PcModifier::LeftAlt),
            altgr: held(PcModifier::RightAlt),
            windows: held(PcModifier::LeftWindows) || held(PcModifier::RightWindows),
        }
    }

    /// An Alt key the text policy keeps from the host because it composes
    /// characters (its press and release both): tracked on the Deck, never
    /// held on the host, so keys forwarded meanwhile go without Alt.
    pub fn compose_option(&mut self, modifier: PcModifier, pressed: bool) {
        if pressed {
            self.composing.insert(modifier);
        } else {
            self.composing.remove(&modifier);
        }
    }

    /// Records modifiers already held when the session got focus (from the
    /// system's key state), without sending anything: the next key sent
    /// carries them to the host, as a Mac Deck's does.
    pub fn seed_modifiers(&mut self, held: impl IntoIterator<Item = PcModifier>) {
        self.held.extend(held.into_iter().map(HeldKey::Modifier));
    }

    /// The keys held, as the keyboard reported them (for
    /// [`HookRouter::begin`]).
    #[must_use]
    pub fn held_keys(&self) -> Vec<PcKey> {
        self.held
            .iter()
            .map(|held| match *held {
                HeldKey::Modifier(modifier) => PcKey::Modifier(modifier),
                HeldKey::Position { qt_key, keypad } => {
                    match [LockKey::Caps, LockKey::Scroll]
                        .into_iter()
                        .find(|lock| lock.qt_key() == qt_key)
                    {
                        Some(lock) if !keypad => PcKey::Lock(lock),
                        _ => PcKey::Position { qt_key, keypad },
                    }
                }
            })
            .collect()
    }

    /// Whether the Deck holds a key it took for a shortcut (a held Alt+F4).
    #[must_use]
    pub fn holds_deck_key(&self, qt_key: u32) -> bool {
        self.deck_keys.contains(&qt_key)
    }

    /// The keystroke the text policy sees for `event`, with the modifiers
    /// as held after it.
    #[must_use]
    pub fn text_stroke(&self, event: PcKeyEvent) -> KeyStroke {
        let mut held = self.modifiers();
        if let PcKey::Modifier(modifier) = event.key {
            let slot = match modifier {
                PcModifier::LeftControl | PcModifier::RightControl => &mut held.control,
                PcModifier::LeftShift | PcModifier::RightShift => &mut held.shift,
                PcModifier::LeftAlt => &mut held.alt,
                PcModifier::RightAlt => &mut held.altgr,
                PcModifier::LeftWindows | PcModifier::RightWindows => &mut held.windows,
            };
            *slot = event.pressed;
        }
        KeyStroke {
            position: match event.key {
                PcKey::Position { qt_key, .. } => Some(self.wire_key(qt_key)),
                _ => None,
            },
            phase: match (event.pressed, event.repeat) {
                (false, _) => KeyPhase::Release,
                (true, true) => KeyPhase::Repeat,
                (true, false) => KeyPhase::Press,
            },
            shift: held.shift,
            command: held.control || held.windows,
            option: option_side(held.alt, held.altgr),
            option_key: match event.key {
                PcKey::Modifier(PcModifier::LeftAlt) => OptionSide::Left,
                PcKey::Modifier(PcModifier::RightAlt) => OptionSide::Right,
                _ => OptionSide::None,
            },
        }
    }

    /// Whether the keyboard grab is on. Without it the Windows key belongs
    /// to the Deck's computer: it is tracked for the Deck's own chords but
    /// never reaches the host. Turning the grab off releases a Windows key
    /// the host holds.
    pub fn set_grab(&mut self, grabbed: bool) -> Vec<HostKeyEdge> {
        if grabbed == self.grabbed {
            return Vec::new();
        }
        self.grabbed = grabbed;
        if grabbed {
            return Vec::new();
        }
        let modifiers = self.deck_modifiers();
        self.translator
            .key(DeckKey::Command, false, false, modifiers)
    }

    /// This computer has the keyboard (`true`), or the host again. Giving it
    /// to this computer releases every key held on the host; while it has
    /// it, every key is this computer's and only the escape chord
    /// ([`DeckShortcut::ReleaseKeyboard`]) is reported. Giving it back
    /// forgets what was held meanwhile: the host never saw those presses
    /// (the platform seeds the modifiers still down).
    pub fn set_released(&mut self, released: bool) -> Vec<HostKeyEdge> {
        if released == self.released {
            return Vec::new();
        }
        let edges = if released {
            self.held.clear();
            self.composing.clear();
            self.deck_keys.clear();
            self.translator.release_all_for_switch()
        } else {
            self.held.clear();
            self.composing.clear();
            Vec::new()
        };
        self.released = released;
        edges
    }

    /// Whether this computer has the keyboard.
    #[must_use]
    pub const fn released(&self) -> bool {
        self.released
    }

    /// A Send Key menu item: every key held on the host is released, then
    /// the key is pressed and released with no modifier (Ctrl+Alt+Del as
    /// [`Self::secure_attention`]). Keys still physically held are pressed
    /// again on the host with the next key.
    pub fn send_key(&mut self, key: SendKey) -> Vec<HostKeyEdge> {
        if key == SendKey::ControlAltDelete {
            return self.secure_attention();
        }
        let mut edges = self.translator.release_all();
        edges.extend(key.edges());
        edges
    }

    /// A key edge while this computer has the keyboard: modifiers are
    /// tracked for the escape chord, nothing reaches the host.
    fn local_key(&mut self, held: HeldKey, event: PcKeyEvent) -> PcKeyAction {
        match held {
            HeldKey::Modifier(_) if !event.repeat => {
                if event.pressed {
                    self.held.insert(held);
                } else {
                    self.held.remove(&held);
                }
            }
            HeldKey::Position { qt_key, .. } => {
                if !event.pressed && self.deck_keys.remove(&qt_key) {
                    return self.suppress("deck_shortcut");
                }
                if event.pressed
                    && !event.repeat
                    && DeckShortcut::for_pc_key_with(qt_key, self.modifiers(), self.switch)
                        == Some(DeckShortcut::ReleaseKeyboard)
                {
                    self.deck_keys.insert(qt_key);
                    return PcKeyAction::Shortcut(DeckShortcut::ReleaseKeyboard);
                }
            }
            HeldKey::Modifier(_) => {}
        }
        PcKeyAction::Local
    }

    /// One key edge.
    pub fn key(&mut self, event: PcKeyEvent) -> PcKeyAction {
        self.raw_count = self.raw_count.saturating_add(1);
        if self.released && matches!(event.key, PcKey::CharacterOnly | PcKey::Synthetic) {
            return PcKeyAction::Local;
        }
        if let PcKey::Modifier(modifier @ (PcModifier::LeftWindows | PcModifier::RightWindows)) =
            event.key
        {
            if !self.grabbed && !event.repeat {
                if event.pressed {
                    self.held.insert(HeldKey::Modifier(modifier));
                } else {
                    self.held.remove(&HeldKey::Modifier(modifier));
                }
                return PcKeyAction::Local;
            }
        }
        let held = match event.key {
            PcKey::Modifier(modifier) => HeldKey::Modifier(modifier),
            PcKey::Position { qt_key, keypad } => HeldKey::Position { qt_key, keypad },
            PcKey::Lock(LockKey::Num) | PcKey::Ime(_) | PcKey::LocalOnly | PcKey::Unmapped => {
                return PcKeyAction::Local;
            }
            PcKey::Lock(lock) => HeldKey::Position {
                qt_key: lock.qt_key(),
                keypad: false,
            },
            PcKey::CharacterOnly => return PcKeyAction::Text,
            PcKey::Synthetic => return self.suppress("synthetic"),
        };
        if self.released {
            return self.local_key(held, event);
        }
        if let HeldKey::Position { qt_key, .. } = held {
            if !event.pressed && self.deck_keys.remove(&qt_key) {
                return self.suppress("deck_shortcut");
            }
            if event.pressed && self.deck_keys.contains(&qt_key) {
                return self.suppress("deck_shortcut");
            }
            if event.pressed && !event.repeat {
                if let Some(action) =
                    DeckShortcut::for_pc_key_with(qt_key, self.modifiers(), self.switch)
                {
                    self.deck_keys.insert(qt_key);
                    return PcKeyAction::Shortcut(action);
                }
            }
        }
        if event.repeat {
            return self.suppress("repeat");
        }
        let changed = if event.pressed {
            self.held.insert(held)
        } else {
            self.held.remove(&held)
        };
        let deck_key = match held {
            HeldKey::Modifier(modifier) => deck_modifier_key(modifier),
            HeldKey::Position { qt_key, .. } => DeckKey::Other(self.wire_key(qt_key)),
        };
        let keypad = matches!(held, HeldKey::Position { keypad: true, .. });
        if !changed && !matches!(held, HeldKey::Modifier(_)) {
            return self.suppress(if event.pressed {
                "duplicate_press"
            } else {
                "orphan_release"
            });
        }
        let modifiers = self.deck_modifiers();
        // Both Control keys are one Control on the host: letting go of one
        // while the other is held releases nothing there.
        let pressed = match deck_key {
            DeckKey::Control => modifiers.control,
            DeckKey::Shift => modifiers.shift,
            DeckKey::Command => modifiers.command,
            DeckKey::LeftOption | DeckKey::RightOption | DeckKey::Other(_) => event.pressed,
        };
        PcKeyAction::Host(self.translator.key(deck_key, pressed, keypad, modifiers))
    }

    /// A whole press and release of `qt_key` on the host under the
    /// modifiers held there (a tapped Alt+F4 that was not held to quit).
    pub fn tap(&mut self, qt_key: u32) -> Vec<HostKeyEdge> {
        let modifiers = self.deck_modifiers();
        let key = DeckKey::Other(self.wire_key(qt_key));
        let mut edges = self.translator.key(key, true, false, modifiers);
        edges.extend(self.translator.key(key, false, false, modifiers));
        edges
    }

    /// Ctrl+Alt+Del for the host: everything held there is released first,
    /// then the chord is pressed and released on its own. The keys still
    /// physically held are pressed again on the host with the next key.
    pub fn secure_attention(&mut self) -> Vec<HostKeyEdge> {
        let mut edges = self.translator.release_all();
        let chord = [
            (KEY_CONTROL, MOD_CTRL),
            (KEY_ALT, MOD_CTRL | MOD_ALT),
            (KEY_DELETE, MOD_CTRL | MOD_ALT),
        ];
        for (qt_key, modifiers) in chord {
            edges.push(HostKeyEdge {
                qt_key,
                pressed: true,
                modifiers,
            });
        }
        for (qt_key, modifiers) in chord.into_iter().rev() {
            let modifiers = match qt_key {
                KEY_CONTROL => 0,
                KEY_ALT => MOD_CTRL,
                _ => modifiers,
            };
            edges.push(HostKeyEdge {
                qt_key,
                pressed: false,
                modifiers,
            });
        }
        edges
    }

    /// Releases everything held on the host and forgets every held key
    /// (focus loss, Release Stuck Keys, the keyboard going to this computer,
    /// a paste taking the keyboard), as a Mac Deck does.
    pub fn release_all(&mut self) -> Vec<HostKeyEdge> {
        self.held.clear();
        self.composing.clear();
        self.deck_keys.clear();
        self.translator.release_all()
    }

    /// Forgets everything without sending anything (the session is gone).
    pub fn clear_local(&mut self) {
        self.held.clear();
        self.composing.clear();
        self.deck_keys.clear();
        self.translator = ShortcutTranslator::default();
    }

    /// Key edges seen.
    #[must_use]
    pub const fn raw_count(&self) -> u64 {
        self.raw_count
    }

    /// Key edges not sent.
    #[must_use]
    pub const fn suppressed_count(&self) -> u64 {
        self.suppressed_count
    }

    /// Keys held, modifiers included.
    #[must_use]
    pub fn held_count(&self) -> usize {
        self.held.len()
    }

    fn suppress(&mut self, reason: &'static str) -> PcKeyAction {
        self.suppressed_count = self.suppressed_count.saturating_add(1);
        PcKeyAction::Suppressed { reason }
    }

    fn wire_key(&self, qt_key: u32) -> u32 {
        if qt_key == KEY_INTL_BACKSLASH {
            iso_key_identifier(self.translator.policy().intl_backslash)
        } else {
            qt_key
        }
    }

    fn deck_modifiers(&self) -> DeckModifiers {
        let mut held = self.modifiers();
        held.alt &= !self.composing.contains(&PcModifier::LeftAlt);
        held.altgr &= !self.composing.contains(&PcModifier::RightAlt);
        DeckModifiers {
            command: held.windows && self.grabbed,
            control: held.control,
            shift: held.shift,
            option: option_side(held.alt, held.altgr),
        }
    }
}

/// The Mac key the shortcut translator knows a PC modifier as.
const fn deck_modifier_key(modifier: PcModifier) -> DeckKey {
    match modifier {
        PcModifier::LeftControl | PcModifier::RightControl => DeckKey::Control,
        PcModifier::LeftShift | PcModifier::RightShift => DeckKey::Shift,
        PcModifier::LeftAlt => DeckKey::LeftOption,
        PcModifier::RightAlt => DeckKey::RightOption,
        PcModifier::LeftWindows | PcModifier::RightWindows => DeckKey::Command,
    }
}

const fn option_side(left: bool, right: bool) -> OptionSide {
    match (left, right) {
        (false, false) => OptionSide::None,
        (true, false) => OptionSide::Left,
        (false, true) => OptionSide::Right,
        (true, true) => OptionSide::Both,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::pc::KEY_F4;
    use crate::keyboard::{KeyboardSettings, resolve_keyboard_policy};
    use arcen_protocol::keys::{KEY_ALTGR, KEY_META, KEY_SHIFT, MOD_KEYPAD, MOD_META};

    const KEY_C: u32 = 0x43;

    fn mac_command(mac_host_command: MacHostCommandKey) -> PcKeyboardSettings {
        PcKeyboardSettings {
            mac_host_command,
            ..PcKeyboardSettings::default()
        }
    }

    fn press(key: PcKey) -> PcKeyEvent {
        PcKeyEvent {
            key,
            pressed: true,
            repeat: false,
        }
    }

    fn release(key: PcKey) -> PcKeyEvent {
        PcKeyEvent {
            key,
            pressed: false,
            repeat: false,
        }
    }

    const fn position(qt_key: u32) -> PcKey {
        PcKey::Position {
            qt_key,
            keypad: false,
        }
    }

    const fn modifier(modifier: PcModifier) -> PcKey {
        PcKey::Modifier(modifier)
    }

    fn host_edges(input: &mut PcKeyboardInput, events: &[PcKeyEvent]) -> Vec<HostKeyEdge> {
        events
            .iter()
            .flat_map(|event| match input.key(*event) {
                PcKeyAction::Host(edges) => edges,
                other => panic!("{event:?} was {other:?}"),
            })
            .collect()
    }

    fn edge(qt_key: u32, pressed: bool, modifiers: u32) -> HostKeyEdge {
        HostKeyEdge {
            qt_key,
            pressed,
            modifiers,
        }
    }

    /// Command+C on a Mac keyboard, through the Mac Deck's own policy.
    fn mac_copy(platform: HostPlatform) -> Vec<HostKeyEdge> {
        let policy = resolve_keyboard_policy(platform, true, KeyboardSettings::default());
        let mut translator = ShortcutTranslator::new(policy);
        let command = DeckModifiers {
            command: true,
            ..DeckModifiers::default()
        };
        let mut edges = translator.key(DeckKey::Command, true, false, command);
        edges.extend(translator.key(DeckKey::Other(KEY_C), true, false, command));
        edges.extend(translator.key(DeckKey::Other(KEY_C), false, false, command));
        edges.extend(translator.key(DeckKey::Command, false, false, DeckModifiers::default()));
        edges
    }

    /// Control+C on a PC keyboard.
    fn pc_copy(platform: HostPlatform) -> Vec<HostKeyEdge> {
        let policy = resolve_pc_keyboard_policy(platform, true, PcKeyboardSettings::default());
        let mut input = PcKeyboardInput::new(policy);
        let control = modifier(PcModifier::LeftControl);
        host_edges(
            &mut input,
            &[
                press(control),
                press(position(KEY_C)),
                release(position(KEY_C)),
                release(control),
            ],
        )
    }

    #[test]
    fn copy_reaches_every_host_alike_from_either_deck() {
        for platform in [
            HostPlatform::Windows,
            HostPlatform::Linux,
            HostPlatform::MacOs,
        ] {
            assert_eq!(pc_copy(platform), mac_copy(platform), "{platform:?}");
        }
        assert_eq!(
            pc_copy(HostPlatform::MacOs),
            [
                edge(KEY_META, true, MOD_META),
                edge(KEY_C, true, MOD_META),
                edge(KEY_C, false, MOD_META),
                edge(KEY_META, false, 0),
            ]
        );
        assert_eq!(
            pc_copy(HostPlatform::Windows)[0],
            edge(KEY_CONTROL, true, MOD_CTRL)
        );
    }

    #[test]
    fn shared_settings_map_to_the_pc_meaning() {
        let with = |shortcuts| KeyboardSettings {
            shortcuts,
            ..KeyboardSettings::default()
        };
        for (shortcuts, command) in [
            (ShortcutPreference::Automatic, MacHostCommandKey::Control),
            (
                ShortcutPreference::SwapCommandControl,
                MacHostCommandKey::Control,
            ),
            (
                ShortcutPreference::CommandIsControl,
                MacHostCommandKey::Control,
            ),
            (ShortcutPreference::WindowsApp, MacHostCommandKey::Windows),
        ] {
            let settings =
                PcKeyboardSettings::from_settings(&with(shortcuts), OptionKeyPreference::Automatic);
            assert_eq!(settings.mac_host_command, command, "{shortcuts:?}");
        }
        for (option_keys, altgr) in [
            (OptionKeyPreference::Automatic, AltGrKey::TypesCharacters),
            (OptionKeyMode::BothAlt.into(), AltGrKey::IsAlt),
            (
                OptionKeyMode::LeftAltRightCharacters.into(),
                AltGrKey::TypesCharacters,
            ),
            (
                OptionKeyMode::BothCharacters.into(),
                AltGrKey::TypesCharacters,
            ),
        ] {
            let settings =
                PcKeyboardSettings::from_settings(&KeyboardSettings::default(), option_keys);
            assert_eq!(settings.altgr, altgr, "{option_keys:?}");
        }
        assert_eq!(
            PcKeyboardSettings::from_settings(
                &KeyboardSettings::default(),
                OptionKeyPreference::Automatic
            ),
            PcKeyboardSettings::default()
        );
    }

    #[test]
    fn keys_pass_as_themselves_to_windows_and_linux_hosts() {
        for platform in [HostPlatform::Windows, HostPlatform::Linux] {
            let policy =
                resolve_pc_keyboard_policy(platform, true, mac_command(MacHostCommandKey::Control));
            assert_eq!(policy.shortcuts, ShortcutMode::CommandIsMeta);
            assert!(policy.right_option_altgr && !policy.left_option_altgr);
            let mut input = PcKeyboardInput::new(policy);
            input.set_grab(true);
            let windows = modifier(PcModifier::LeftWindows);
            assert_eq!(
                host_edges(&mut input, &[press(windows), release(windows)]),
                [edge(KEY_META, true, MOD_META), edge(KEY_META, false, 0)]
            );
            let altgr = modifier(PcModifier::RightAlt);
            assert_eq!(
                host_edges(&mut input, &[press(altgr), release(altgr)]),
                [edge(KEY_ALTGR, true, 0), edge(KEY_ALTGR, false, 0)]
            );
        }
        let without = resolve_pc_keyboard_policy(
            HostPlatform::Windows,
            false,
            mac_command(MacHostCommandKey::Control),
        );
        assert!(!without.right_option_altgr && !without.intl_backslash);
    }

    #[test]
    fn a_mac_host_gets_control_as_command_unless_asked_otherwise() {
        let swap = resolve_pc_keyboard_policy(
            HostPlatform::MacOs,
            true,
            mac_command(MacHostCommandKey::Control),
        );
        assert_eq!(swap.shortcuts, ShortcutMode::SwapCommandControl);
        assert!(!swap.right_option_altgr);
        let mut input = PcKeyboardInput::new(swap);
        input.set_grab(true);
        let windows = modifier(PcModifier::LeftWindows);
        assert_eq!(
            host_edges(&mut input, &[press(windows)]),
            [edge(KEY_CONTROL, true, MOD_CTRL)]
        );
        let labelled = resolve_pc_keyboard_policy(
            HostPlatform::MacOs,
            true,
            mac_command(MacHostCommandKey::Windows),
        );
        assert_eq!(labelled.shortcuts, ShortcutMode::CommandIsMeta);
        let mut input = PcKeyboardInput::new(labelled);
        input.set_grab(true);
        assert_eq!(
            host_edges(&mut input, &[press(windows)]),
            [edge(KEY_META, true, MOD_META)]
        );
        // Either right Alt is Option on a Mac host.
        let altgr = modifier(PcModifier::RightAlt);
        assert_eq!(
            host_edges(&mut input, &[press(altgr)]),
            [edge(KEY_ALT, true, MOD_META | MOD_ALT)]
        );
        for setting in MacHostCommandKey::ALL {
            assert_eq!(
                MacHostCommandKey::from_token(setting.token()),
                Some(setting)
            );
        }
    }

    #[test]
    fn both_control_keys_are_one_control_on_the_host() {
        let policy = resolve_pc_keyboard_policy(
            HostPlatform::Windows,
            true,
            mac_command(MacHostCommandKey::Control),
        );
        let mut input = PcKeyboardInput::new(policy);
        let left = modifier(PcModifier::LeftControl);
        let right = modifier(PcModifier::RightControl);
        assert_eq!(
            host_edges(&mut input, &[press(left), press(right), release(left)]),
            [edge(KEY_CONTROL, true, MOD_CTRL)]
        );
        assert_eq!(
            host_edges(&mut input, &[release(right)]),
            [edge(KEY_CONTROL, false, 0)]
        );
        assert_eq!(input.held_count(), 0);
    }

    #[test]
    fn keypad_and_iso_keys_keep_their_position() {
        let policy = resolve_pc_keyboard_policy(
            HostPlatform::Linux,
            true,
            mac_command(MacHostCommandKey::Control),
        );
        let mut input = PcKeyboardInput::new(policy);
        let seven = PcKey::Position {
            qt_key: 0x37,
            keypad: true,
        };
        assert_eq!(
            host_edges(&mut input, &[press(seven), release(seven)]),
            [edge(0x37, true, MOD_KEYPAD), edge(0x37, false, MOD_KEYPAD)]
        );
        let iso = position(KEY_INTL_BACKSLASH);
        assert_eq!(
            host_edges(&mut input, &[press(iso)])[0].qt_key,
            KEY_INTL_BACKSLASH
        );
        let mac = resolve_pc_keyboard_policy(
            HostPlatform::MacOs,
            false,
            mac_command(MacHostCommandKey::Control),
        );
        let mut input = PcKeyboardInput::new(mac);
        assert_eq!(host_edges(&mut input, &[press(iso)])[0].qt_key, 0x60);
    }

    #[test]
    fn repeats_never_reach_the_host() {
        let mut input = PcKeyboardInput::default();
        let a = position(0x41);
        assert_eq!(host_edges(&mut input, &[press(a)]).len(), 1);
        let repeat = PcKeyEvent {
            repeat: true,
            ..press(a)
        };
        assert_eq!(
            input.key(repeat),
            PcKeyAction::Suppressed { reason: "repeat" }
        );
        assert_eq!(
            input.key(press(a)),
            PcKeyAction::Suppressed {
                reason: "duplicate_press"
            }
        );
    }

    #[test]
    fn f9_reaches_the_host_and_deck_chords_keep_their_release() {
        let mut input = PcKeyboardInput::default();
        let f9 = position(crate::keyboard::pc::KEY_F9);
        assert_eq!(
            host_edges(&mut input, &[press(f9), release(f9)]),
            [
                edge(crate::keyboard::pc::KEY_F9, true, 0),
                edge(crate::keyboard::pc::KEY_F9, false, 0)
            ]
        );
        let control = modifier(PcModifier::LeftControl);
        let alt = modifier(PcModifier::LeftAlt);
        host_edges(&mut input, &[press(control), press(alt)]);
        let home = position(crate::keyboard::pc::KEY_HOME);
        assert_eq!(
            input.key(press(home)),
            PcKeyAction::Shortcut(DeckShortcut::ReleaseKeyboard)
        );
        assert_eq!(
            input.key(release(home)),
            PcKeyAction::Suppressed {
                reason: "deck_shortcut"
            }
        );
    }

    #[test]
    fn a_chosen_switch_chord_replaces_control_alt_home() {
        let mut input = PcKeyboardInput::default();
        input.set_switch_chord(KeyboardSwitchChord::ALT_F1);
        let alt = modifier(PcModifier::LeftAlt);
        let f1 = position(crate::keyboard::pc::KEY_F1);
        host_edges(&mut input, &[press(alt)]);
        assert_eq!(
            input.key(press(f1)),
            PcKeyAction::Shortcut(DeckShortcut::ReleaseKeyboard)
        );
        // Only Alt reached the host: a Control tap masks its release, so the
        // host never sees Alt pressed and let go alone.
        let edges = input.set_released(true);
        assert_eq!(
            edges.first().map(|edge| (edge.qt_key, edge.pressed)),
            Some((KEY_CONTROL, true))
        );
        assert!(
            edges
                .iter()
                .any(|edge| edge.qt_key == KEY_ALT && !edge.pressed)
        );
        assert_ne!(
            input.key(release(f1)),
            PcKeyAction::Shortcut(DeckShortcut::ReleaseKeyboard)
        );
        // While this computer has the keyboard, only the chosen chord comes
        // back.
        let control = modifier(PcModifier::LeftControl);
        let home = position(crate::keyboard::pc::KEY_HOME);
        assert_eq!(input.key(press(control)), PcKeyAction::Local);
        assert_eq!(input.key(press(home)), PcKeyAction::Local);
        assert_eq!(input.key(release(home)), PcKeyAction::Local);
        assert_eq!(input.key(release(control)), PcKeyAction::Local);
        assert_eq!(input.key(press(alt)), PcKeyAction::Local);
        assert_eq!(
            input.key(press(f1)),
            PcKeyAction::Shortcut(DeckShortcut::ReleaseKeyboard)
        );
        let mut router = HookRouter::default();
        router.set_switch_chord(KeyboardSwitchChord::ALT_F1);
        router.begin([]);
        assert_eq!(router.route(alt, true, false), HookRoute::System);
        assert_eq!(router.route(f1, true, false), HookRoute::Session);
    }

    #[test]
    fn released_keys_stay_local_except_the_escape_chord() {
        let policy =
            resolve_pc_keyboard_policy(HostPlatform::Linux, true, PcKeyboardSettings::default());
        let mut input = PcKeyboardInput::new(policy);
        let control = modifier(PcModifier::LeftControl);
        let alt = modifier(PcModifier::LeftAlt);
        let a = position(0x41);
        host_edges(&mut input, &[press(control), press(a)]);
        // Leaving releases what the host holds, once.
        assert_eq!(
            input.set_released(true),
            [edge(0x41, false, MOD_CTRL), edge(KEY_CONTROL, false, 0)]
        );
        assert!(input.set_released(true).is_empty());
        assert!(input.released());
        assert_eq!(input.key(release(a)), PcKeyAction::Local);
        assert_eq!(input.key(press(position(0x42))), PcKeyAction::Local);
        // Leaving forgot what was held; the platform seeds the modifiers
        // still down, or they are pressed again.
        assert_eq!(input.key(release(control)), PcKeyAction::Local);
        assert_eq!(input.key(press(control)), PcKeyAction::Local);
        assert_eq!(input.key(press(alt)), PcKeyAction::Local);
        let home = position(crate::keyboard::pc::KEY_HOME);
        assert_eq!(
            input.key(press(home)),
            PcKeyAction::Shortcut(DeckShortcut::ReleaseKeyboard)
        );
        assert_eq!(
            input.key(release(home)),
            PcKeyAction::Suppressed {
                reason: "deck_shortcut"
            }
        );
        assert!(input.set_released(false).is_empty());
        assert!(!input.released());
        assert_eq!(input.held_count(), 0);
        let b = position(0x42);
        assert_eq!(host_edges(&mut input, &[press(b)]), [edge(0x42, true, 0)]);
    }

    #[test]
    fn a_sent_key_releases_what_the_host_holds_first() {
        let policy =
            resolve_pc_keyboard_policy(HostPlatform::Linux, true, PcKeyboardSettings::default());
        let mut input = PcKeyboardInput::new(policy);
        host_edges(&mut input, &[press(modifier(PcModifier::LeftShift))]);
        let print = crate::keyboard::pc::KEY_PRINT;
        assert_eq!(
            input.send_key(SendKey::PrintScreen),
            [
                edge(arcen_protocol::keys::KEY_SHIFT, false, 0),
                edge(print, true, 0),
                edge(print, false, 0)
            ]
        );
        assert_eq!(
            input.send_key(SendKey::ControlAltDelete),
            input.secure_attention()
        );
    }

    #[test]
    fn the_windows_key_reaches_the_host_only_under_the_grab() {
        let policy =
            resolve_pc_keyboard_policy(HostPlatform::Windows, true, PcKeyboardSettings::default());
        let mut input = PcKeyboardInput::new(policy);
        let windows = modifier(PcModifier::LeftWindows);
        assert_eq!(input.key(press(windows)), PcKeyAction::Local);
        assert!(input.modifiers().windows);
        assert_eq!(input.key(release(windows)), PcKeyAction::Local);
        assert!(input.set_grab(true).is_empty());
        assert_eq!(
            host_edges(&mut input, &[press(windows)]),
            [edge(KEY_META, true, MOD_META)]
        );
        assert_eq!(input.set_grab(false), [edge(KEY_META, false, 0)]);
        assert_eq!(input.key(release(windows)), PcKeyAction::Local);
        assert!(input.release_all().is_empty());
    }

    #[test]
    fn a_composing_altgr_never_reaches_the_host() {
        let policy =
            resolve_pc_keyboard_policy(HostPlatform::Linux, true, PcKeyboardSettings::default());
        let mut input = PcKeyboardInput::new(policy);
        input.compose_option(PcModifier::RightAlt, true);
        assert!(input.modifiers().altgr);
        let e = position(0x45);
        assert_eq!(
            host_edges(&mut input, &[press(e), release(e)]),
            [edge(0x45, true, 0), edge(0x45, false, 0)]
        );
        // The Deck still sees it held (no Deck chord matches meanwhile).
        input.compose_option(PcModifier::RightAlt, false);
        assert!(!input.modifiers().altgr);
    }

    #[test]
    fn a_tapped_alt_f4_goes_to_the_host() {
        let policy = resolve_pc_keyboard_policy(
            HostPlatform::Windows,
            true,
            mac_command(MacHostCommandKey::Control),
        );
        let mut input = PcKeyboardInput::new(policy);
        let alt = modifier(PcModifier::LeftAlt);
        assert_eq!(
            host_edges(&mut input, &[press(alt)]),
            [edge(KEY_ALT, true, MOD_ALT)]
        );
        assert_eq!(
            input.key(press(position(KEY_F4))),
            PcKeyAction::Shortcut(DeckShortcut::Quit)
        );
        assert!(input.holds_deck_key(KEY_F4));
        assert_eq!(
            input.tap(KEY_F4),
            [edge(KEY_F4, true, MOD_ALT), edge(KEY_F4, false, MOD_ALT)]
        );
        input.key(release(position(KEY_F4)));
        assert!(!input.holds_deck_key(KEY_F4));
    }

    #[test]
    fn focus_loss_releases_every_held_key() {
        let mut input = PcKeyboardInput::default();
        let shift = modifier(PcModifier::LeftShift);
        host_edges(&mut input, &[press(shift), press(position(0x41))]);
        assert_eq!(
            input.release_all(),
            [
                edge(0x41, false, arcen_protocol::keys::MOD_SHIFT),
                edge(KEY_SHIFT, false, 0)
            ]
        );
        assert_eq!(input.held_count(), 0);
        assert_eq!(
            input.key(release(position(0x41))),
            PcKeyAction::Suppressed {
                reason: "orphan_release"
            }
        );
    }

    #[test]
    fn secure_attention_is_a_clean_chord() {
        let mut input = PcKeyboardInput::default();
        let control = modifier(PcModifier::LeftControl);
        let alt = modifier(PcModifier::LeftAlt);
        host_edges(&mut input, &[press(control), press(alt)]);
        let edges = input.secure_attention();
        assert_eq!(
            edges,
            [
                edge(KEY_ALT, false, MOD_CTRL),
                edge(KEY_CONTROL, false, 0),
                edge(KEY_CONTROL, true, MOD_CTRL),
                edge(KEY_ALT, true, MOD_CTRL | MOD_ALT),
                edge(KEY_DELETE, true, MOD_CTRL | MOD_ALT),
                edge(KEY_DELETE, false, MOD_CTRL | MOD_ALT),
                edge(KEY_ALT, false, MOD_CTRL),
                edge(KEY_CONTROL, false, 0),
            ]
        );
        // Still held on the keyboard: the next key carries them again.
        assert_eq!(
            host_edges(&mut input, &[press(position(0x41))])[..2],
            [
                edge(KEY_CONTROL, true, MOD_CTRL),
                edge(KEY_ALT, true, MOD_CTRL | MOD_ALT)
            ]
        );
    }

    #[test]
    fn the_grab_takes_what_windows_lets_it() {
        let none = PcModifierState::default();
        let windows = PcModifierState {
            windows: true,
            ..none
        };
        let alt = PcModifierState { alt: true, ..none };
        let control_alt = PcModifierState {
            control: true,
            alt: true,
            ..none
        };
        assert!(grab_takes(modifier(PcModifier::LeftWindows), windows));
        assert!(grab_takes(position(0x0100_0001), alt));
        assert!(!grab_takes(position(0x41), none));
        assert!(!grab_takes(modifier(PcModifier::LeftAlt), none));
        assert!(!grab_takes(modifier(PcModifier::RightAlt), none));
        assert!(!grab_takes(position(0x4C), windows));
        assert!(!grab_takes(position(KEY_DELETE), control_alt));
        assert!(!grab_takes(PcKey::Lock(LockKey::Num), none));
        assert!(!grab_takes(PcKey::Lock(LockKey::Caps), none));
        assert!(!grab_takes(PcKey::LocalOnly, none));
        assert_eq!(
            SystemChord::of(
                position(0x0100_0000),
                PcModifierState {
                    control: true,
                    shift: true,
                    ..none
                }
            ),
            Some(SystemChord::TaskManager)
        );
    }

    #[test]
    fn a_grab_sends_each_release_where_its_press_went() {
        let mut input = PcKeyboardInput::default();
        let control = modifier(PcModifier::LeftControl);
        let alt = modifier(PcModifier::LeftAlt);
        host_edges(&mut input, &[press(control), press(alt)]);
        // The grab starts with Control and Alt down.
        let mut router = HookRouter::default();
        router.begin(input.held_keys());
        // Ordinary keys stay on the window's path, where the layout types.
        let a = position(0x41);
        assert_eq!(router.route(a, true, true), HookRoute::System);
        assert_eq!(router.route(a, false, true), HookRoute::System);
        // Alt+Tab: the Tab is the session's, with its release.
        let tab = position(0x0100_0001);
        assert_eq!(router.route(tab, true, true), HookRoute::Session);
        assert_eq!(router.route(alt, false, true), HookRoute::System);
        assert_eq!(router.route(tab, false, true), HookRoute::Session);
        assert_eq!(router.route(control, false, true), HookRoute::System);
        // Windows+L: the system locks, so the L release is its; the Windows
        // key, taken, comes back to the session.
        let windows = modifier(PcModifier::LeftWindows);
        assert_eq!(router.route(windows, true, true), HookRoute::Session);
        assert_eq!(router.route(position(0x4C), true, true), HookRoute::System);
        assert_eq!(router.route(position(0x4C), false, true), HookRoute::System);
        assert_eq!(router.route(windows, false, true), HookRoute::Session);
        // A release nobody routed (down before routing began) is the window's.
        assert_eq!(router.route(position(0x42), false, true), HookRoute::System);
        assert_eq!(
            router.route(PcKey::Lock(LockKey::Num), false, true),
            HookRoute::System
        );
    }

    #[test]
    fn a_key_a_hook_below_blocks_is_not_the_sessions() {
        let mut router = HookRouter::default();
        router.begin([]);
        let windows = modifier(PcModifier::LeftWindows);
        // Blocked below: the press is dropped and its release follows it
        // down the chain, so nothing reaches the session.
        assert_eq!(router.route(windows, true, true), HookRoute::Session);
        assert!(!router.blocked_below(windows, true));
        assert_eq!(router.route(windows, false, true), HookRoute::System);
        // Blocked again on a repeat: still nothing for the session.
        assert_eq!(router.route(windows, true, true), HookRoute::Session);
        assert!(!router.blocked_below(windows, true));
        assert_eq!(router.route(windows, true, true), HookRoute::Session);
        assert!(!router.blocked_below(windows, true));
        assert_eq!(router.route(windows, false, true), HookRoute::System);
        // The block begins while the session holds the key: a blocked
        // repeat is dropped, but the release still reaches the session, so
        // the host does not keep the key down.
        let tab = position(0x0100_0001);
        router.route(modifier(PcModifier::LeftAlt), true, true);
        assert_eq!(router.route(tab, true, true), HookRoute::Session);
        assert_eq!(router.route(tab, true, true), HookRoute::Session);
        assert!(!router.blocked_below(tab, true));
        assert_eq!(router.route(tab, false, true), HookRoute::Session);
        assert!(router.blocked_below(tab, false));
        // The key is free again: its next press is the session's.
        assert_eq!(router.route(tab, true, true), HookRoute::Session);
        assert_eq!(router.route(tab, false, true), HookRoute::Session);
    }

    #[test]
    fn without_a_grab_only_deck_chords_are_taken() {
        let mut router = HookRouter::default();
        router.begin([]);
        let control = modifier(PcModifier::LeftControl);
        let windows = modifier(PcModifier::LeftWindows);
        assert_eq!(router.route(windows, true, false), HookRoute::System);
        assert_eq!(router.route(position(0x45), true, false), HookRoute::System);
        assert_eq!(
            router.route(position(0x45), false, false),
            HookRoute::System
        );
        // Ctrl+Windows+K is the host's now (no Deck chord has the Windows
        // key).
        assert_eq!(router.route(control, true, false), HookRoute::System);
        let k = position(0x4B);
        assert_eq!(router.route(k, true, false), HookRoute::System);
        assert_eq!(router.route(k, false, false), HookRoute::System);
        assert_eq!(router.route(control, false, false), HookRoute::System);
        assert_eq!(router.route(windows, false, false), HookRoute::System);
        // A Windows key down before routing began is the system's: a key
        // the grab takes meanwhile masks it, so Start stays shut.
        let mut masked = HookRouter::default();
        masked.begin([windows]);
        assert_eq!(
            masked.route(position(0x0100_0001), true, true),
            HookRoute::SessionMaskingWindowsKey
        );
        assert_eq!(
            masked.route(position(0x0100_0001), false, true),
            HookRoute::Session
        );
        // Control let go while a taken key repeats: the repeat stays with
        // its press.
        router.route(control, true, false);
        router.route(modifier(PcModifier::LeftAlt), true, false);
        let home = position(crate::keyboard::pc::KEY_HOME);
        assert_eq!(router.route(home, true, false), HookRoute::Session);
        assert_eq!(router.route(control, false, false), HookRoute::System);
        assert_eq!(router.route(home, true, false), HookRoute::Session);
        assert_eq!(router.route(home, false, false), HookRoute::Session);
        router.route(modifier(PcModifier::LeftAlt), false, false);
        // Ctrl+Alt+End with no Windows key: taken, nothing to mask.
        router.route(control, true, false);
        router.route(modifier(PcModifier::LeftAlt), true, false);
        assert_eq!(
            router.route(position(crate::keyboard::pc::KEY_END), true, false),
            HookRoute::Session
        );
    }

    #[test]
    fn altgrs_added_control_goes_with_the_altgr() {
        let altgr = modifier(PcModifier::RightAlt);
        for grabbed in [true, false] {
            let mut router = HookRouter::default();
            assert_eq!(
                router.route(PcKey::Synthetic, true, grabbed),
                HookRoute::System
            );
            assert_eq!(router.route(altgr, true, grabbed), HookRoute::System);
            assert_eq!(
                router.route(PcKey::Synthetic, false, grabbed),
                HookRoute::System
            );
            assert_eq!(router.route(altgr, false, grabbed), HookRoute::System);
        }
    }

    #[test]
    fn held_keys_are_reported_as_the_keyboard_named_them() {
        let mut input = PcKeyboardInput::default();
        let caps = PcKey::Lock(LockKey::Caps);
        let iso = position(KEY_INTL_BACKSLASH);
        host_edges(&mut input, &[press(caps), press(iso)]);
        let mut held = input.held_keys();
        held.sort();
        let mut expected = vec![caps, iso];
        expected.sort();
        assert_eq!(held, expected);
    }

    #[test]
    fn altgr_is_a_composing_option_not_a_shortcut() {
        let mut input = PcKeyboardInput::default();
        let altgr = modifier(PcModifier::RightAlt);
        let stroke = input.text_stroke(press(altgr));
        assert_eq!(stroke.option_key, OptionSide::Right);
        assert_eq!(stroke.option, OptionSide::Right);
        assert!(!stroke.command);
        host_edges(&mut input, &[press(altgr)]);
        let two = input.text_stroke(press(position(0x32)));
        assert_eq!(
            (two.position, two.command, two.option),
            (Some(0x32), false, OptionSide::Right)
        );
        assert_eq!(
            AltGrKey::TypesCharacters.option_preference(true),
            OptionKeyPreference::Chosen(OptionKeyMode::LeftAltRightCharacters)
        );
        for (altgr, layout) in [(AltGrKey::TypesCharacters, false), (AltGrKey::IsAlt, true)] {
            assert_eq!(
                altgr.option_preference(layout),
                OptionKeyPreference::Chosen(OptionKeyMode::BothAlt)
            );
        }
        let is_alt = PcKeyboardSettings {
            altgr: AltGrKey::IsAlt,
            ..PcKeyboardSettings::default()
        };
        assert!(
            !resolve_pc_keyboard_policy(HostPlatform::Windows, true, is_alt).right_option_altgr
        );
        for key in AltGrKey::ALL {
            assert_eq!(AltGrKey::from_token(key.token()), Some(key));
        }
    }
}
