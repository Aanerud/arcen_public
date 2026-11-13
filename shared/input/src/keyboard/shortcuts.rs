// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Physical Mac key edges to host key edges, under a [`KeyboardPolicy`].

use std::collections::BTreeSet;

use arcen_protocol::keys::{
    KEY_ALT, KEY_ALTGR, KEY_CONTROL, KEY_META, KEY_SHIFT, MOD_ALT, MOD_CTRL, MOD_KEYPAD, MOD_META,
    MOD_SHIFT,
};

use super::{KeyboardPolicy, ShortcutMode};
use crate::text::OptionSide;

/// The keys Microsoft's Windows App sends as Control when pressed with
/// Command: C, X, V, A, Z, F and W (copy, cut, paste, select all, undo,
/// find, close).
pub const WINDOWS_APP_CONTROL_KEYS: [u32; 7] = [0x43, 0x58, 0x56, 0x41, 0x5A, 0x46, 0x57];

/// A physical key on the client's keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckKey {
    /// Either Command key.
    Command,
    /// Either Control key.
    Control,
    /// Either Shift key.
    Shift,
    /// The left Option key.
    LeftOption,
    /// The right Option key.
    RightOption,
    /// Any other key, by its key identifier.
    Other(u32),
}

/// The client's modifier keys as held at one key event, from the client's
/// own modifier state rather than from key edges (a modifier pressed before
/// the session had focus has no edge).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeckModifiers {
    /// Command.
    pub command: bool,
    /// Control.
    pub control: bool,
    /// Shift.
    pub shift: bool,
    /// The Option keys.
    pub option: OptionSide,
}

/// One key edge to send to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostKeyEdge {
    /// The key identifier (`key_event.scan_code`).
    pub qt_key: u32,
    /// Pressed or released.
    pub pressed: bool,
    /// The compact wire modifier mask the host holds after this edge.
    pub modifiers: u32,
}

/// What a held Command currently is, in [`ShortcutMode::WindowsApp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandState {
    /// Command is held and nothing has decided what it is yet. `used`
    /// records that another key was pressed meanwhile, so letting go of
    /// Command is not a tap of the Windows key.
    Pending { used: bool },
    /// Command is Control (a Mac edit shortcut is in progress).
    Control,
    /// Command is the Windows key.
    Meta,
}

/// The host modifier keys, in the order modifier presses are sent.
const HOST_MODIFIERS: [(u32, u32); 5] = [
    (KEY_CONTROL, MOD_CTRL),
    (KEY_SHIFT, MOD_SHIFT),
    (KEY_ALT, MOD_ALT),
    (KEY_ALTGR, 0),
    (KEY_META, MOD_META),
];

/// Turns a client's physical key edges into the host's key edges.
///
/// Every host key it presses it also releases: modifier keys follow from
/// the physical modifiers and the policy, and a key is released with the
/// mask it is held under. [`Self::release_all`] releases everything (focus
/// loss, a policy change, reconnecting).
///
/// In [`ShortcutMode::WindowsApp`] the Windows key is held back while
/// Command is held alone: the next key decides. One of
/// [`WINDOWS_APP_CONTROL_KEYS`] is sent with Control (Command+C is Ctrl+C,
/// never Win+Ctrl+C), any other key with the Windows key, and Command let
/// go of alone is a tap of the Windows key (it opens Start).
#[derive(Debug, Clone, Default)]
pub struct ShortcutTranslator {
    policy: KeyboardPolicy,
    physical: DeckModifiers,
    command: Option<CommandState>,
    /// Host modifier keys held (identifiers from `HOST_MODIFIERS`).
    host_modifiers: BTreeSet<u32>,
    /// Other host keys held, with their keypad flag.
    host_keys: BTreeSet<(u32, bool)>,
    /// Keys pressed while Command was resolved to Control or Meta.
    under_command: BTreeSet<(u32, bool)>,
    /// Command became the Windows key for pointer input only: no key has
    /// been pressed with it, so its release alone would open Start.
    meta_for_pointer_only: bool,
}

impl ShortcutTranslator {
    /// A translator with nothing held.
    #[must_use]
    pub fn new(policy: KeyboardPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// The policy in force.
    #[must_use]
    pub const fn policy(&self) -> KeyboardPolicy {
        self.policy
    }

    /// Changes the policy. Everything held is released first, under the
    /// old policy, so no key outlives the policy that pressed it.
    pub fn set_policy(&mut self, policy: KeyboardPolicy) -> Vec<HostKeyEdge> {
        if policy == self.policy {
            return Vec::new();
        }
        let released = self.release_all();
        self.policy = policy;
        released
    }

    /// The client's modifier keys as this translator last saw them.
    #[must_use]
    pub const fn physical(&self) -> DeckModifiers {
        self.physical
    }

    /// Whether any host key is held.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.host_modifiers.is_empty() && self.host_keys.is_empty()
    }

    /// One physical key edge, with the client's modifiers as held at it.
    /// Repeats must not be passed (the caller drops them); a press of a key
    /// already held, or a release of one not held, sends nothing.
    pub fn key(
        &mut self,
        key: DeckKey,
        pressed: bool,
        keypad: bool,
        modifiers: DeckModifiers,
    ) -> Vec<HostKeyEdge> {
        let mut out = Vec::new();
        self.sync_modifiers(key, modifiers, &mut out);
        self.edge(key, pressed, keypad, &mut out);
        out
    }

    /// Records that the client consumed a Command shortcut itself (its
    /// menu's Command+Q): letting go of Command must not then tap the
    /// Windows key.
    pub fn command_chord_consumed(&mut self) {
        if let Some(CommandState::Pending { used }) = &mut self.command {
            *used = true;
        }
    }

    /// Settles a held-back Command before a pointer action (button press,
    /// scroll, drag, gesture): in [`ShortcutMode::WindowsApp`] it becomes the
    /// Windows key now, as Microsoft's Windows App sends Command, so
    /// Command+click is Windows+click and letting go of Command afterwards is
    /// not a Windows-key tap. Returns the edges to send before the pointer
    /// event; nothing in other modes or when Command is not held back.
    pub fn settle_command_for_pointer(&mut self) -> Vec<HostKeyEdge> {
        let mut out = Vec::new();
        if matches!(self.command, Some(CommandState::Pending { .. })) {
            self.command = Some(CommandState::Meta);
            self.meta_for_pointer_only = true;
            self.reconcile(&mut out);
        }
        out
    }

    /// A whole Command+`qt_key`, for a shortcut the client's menu took
    /// before the key reached the session. Command is pressed around the key
    /// only if it is not physically held, so a person still holding it
    /// keeps it held on the host too.
    pub fn tap_with_command(&mut self, qt_key: u32) -> Vec<HostKeyEdge> {
        let mut out = Vec::new();
        let wrap = !self.physical.command;
        if wrap {
            self.edge(DeckKey::Command, true, false, &mut out);
        }
        self.command_chord_consumed();
        self.edge(DeckKey::Other(qt_key), true, false, &mut out);
        self.edge(DeckKey::Other(qt_key), false, false, &mut out);
        if wrap {
            self.edge(DeckKey::Command, false, false, &mut out);
        }
        out
    }

    /// Releases every host key held, other keys before modifiers, and
    /// forgets the physical state (the next event's modifiers restore it). A
    /// Command held back is dropped, not tapped.
    pub fn release_all(&mut self) -> Vec<HostKeyEdge> {
        let mut out = Vec::new();
        for (qt_key, keypad) in std::mem::take(&mut self.host_keys) {
            let modifiers = self.host_mask() | if keypad { MOD_KEYPAD } else { 0 };
            out.push(HostKeyEdge {
                qt_key,
                pressed: false,
                modifiers,
            });
        }
        self.mask_windows_key_release(&mut out);
        self.physical = DeckModifiers::default();
        self.command = None;
        self.under_command.clear();
        self.reconcile(&mut out);
        out
    }

    fn sync_modifiers(&mut self, key: DeckKey, held: DeckModifiers, out: &mut Vec<HostKeyEdge>) {
        if key != DeckKey::Command && held.command != self.physical.command {
            self.edge(DeckKey::Command, held.command, false, out);
        }
        if key != DeckKey::Control && held.control != self.physical.control {
            self.edge(DeckKey::Control, held.control, false, out);
        }
        if key != DeckKey::Shift && held.shift != self.physical.shift {
            self.edge(DeckKey::Shift, held.shift, false, out);
        }
        if matches!(key, DeckKey::LeftOption | DeckKey::RightOption) {
            return;
        }
        let tracked = self.physical.option;
        if held.option == OptionSide::None && tracked != OptionSide::None {
            if matches!(tracked, OptionSide::Left | OptionSide::Both) {
                self.edge(DeckKey::LeftOption, false, false, out);
            }
            if matches!(tracked, OptionSide::Right | OptionSide::Both) {
                self.edge(DeckKey::RightOption, false, false, out);
            }
        } else if held.option != OptionSide::None && tracked == OptionSide::None {
            if matches!(held.option, OptionSide::Left | OptionSide::Both) {
                self.edge(DeckKey::LeftOption, true, false, out);
            }
            if matches!(held.option, OptionSide::Right | OptionSide::Both) {
                self.edge(DeckKey::RightOption, true, false, out);
            }
        }
    }

    fn edge(&mut self, key: DeckKey, pressed: bool, keypad: bool, out: &mut Vec<HostKeyEdge>) {
        let windows_app = self.policy.shortcuts == ShortcutMode::WindowsApp;
        match key {
            DeckKey::Command => {
                if self.physical.command == pressed {
                    return;
                }
                self.physical.command = pressed;
                if !windows_app {
                    self.reconcile(out);
                    return;
                }
                if pressed {
                    self.command = Some(CommandState::Pending { used: false });
                    return;
                }
                let tap = self.command == Some(CommandState::Pending { used: false });
                self.mask_windows_key_release(out);
                self.command = None;
                self.under_command.clear();
                self.reconcile(out);
                if tap {
                    self.press_modifier(KEY_META, out);
                    self.release_modifier(KEY_META, out);
                }
            }
            DeckKey::Control | DeckKey::Shift | DeckKey::LeftOption | DeckKey::RightOption => {
                if !self.set_physical_modifier(key, pressed) {
                    return;
                }
                if pressed {
                    self.command_chord_consumed();
                }
                self.reconcile(out);
            }
            DeckKey::Other(qt_key) => self.other_key(qt_key, pressed, keypad, out),
        }
    }

    fn other_key(&mut self, qt_key: u32, pressed: bool, keypad: bool, out: &mut Vec<HostKeyEdge>) {
        let host_key = (qt_key, keypad);
        if !pressed {
            if self.host_keys.remove(&host_key) {
                self.under_command.remove(&host_key);
                out.push(HostKeyEdge {
                    qt_key,
                    pressed: false,
                    modifiers: self.host_mask() | keypad_bit(keypad),
                });
            }
            return;
        }
        if self.host_keys.contains(&host_key) {
            return;
        }
        if self.policy.shortcuts == ShortcutMode::WindowsApp && self.physical.command {
            let wanted = if !keypad && WINDOWS_APP_CONTROL_KEYS.contains(&qt_key) {
                CommandState::Control
            } else {
                CommandState::Meta
            };
            if matches!(
                self.command,
                Some(CommandState::Control | CommandState::Meta)
            ) && self.command != Some(wanted)
            {
                // Command+C, then Command+E with Command still held: the
                // Control shortcut's keys end before the Windows key starts.
                for held in std::mem::take(&mut self.under_command) {
                    if self.host_keys.remove(&held) {
                        out.push(HostKeyEdge {
                            qt_key: held.0,
                            pressed: false,
                            modifiers: self.host_mask() | keypad_bit(held.1),
                        });
                    }
                }
            }
            self.command = Some(wanted);
            self.meta_for_pointer_only = false;
            self.under_command.insert(host_key);
        }
        self.reconcile(out);
        self.host_keys.insert(host_key);
        out.push(HostKeyEdge {
            qt_key,
            pressed: true,
            modifiers: self.host_mask() | keypad_bit(keypad),
        });
    }

    /// Windows opens Start when the Windows key is released with no other
    /// key between its press and release; a mouse click does not count. When
    /// Command was the Windows key for pointer input only, a Control tap
    /// before its release marks it used, so Command+click never opens Start.
    /// Only for a host that does not mask this itself
    /// (`host_masks_meta_release`): a host's own mask key is invisible, a
    /// Control tap is not (Find My Mouse, Office's paste menu).
    fn mask_windows_key_release(&mut self, out: &mut Vec<HostKeyEdge>) {
        let mask = !self.policy.host_masks_meta_release
            && self.command == Some(CommandState::Meta)
            && self.meta_for_pointer_only
            && self.host_modifiers.contains(&KEY_META)
            && !self.host_modifiers.contains(&KEY_CONTROL);
        self.meta_for_pointer_only = false;
        if mask {
            self.press_modifier(KEY_CONTROL, out);
            self.release_modifier(KEY_CONTROL, out);
        }
    }

    fn set_physical_modifier(&mut self, key: DeckKey, pressed: bool) -> bool {
        let physical = &mut self.physical;
        let slot = match key {
            DeckKey::Control => &mut physical.control,
            DeckKey::Shift => &mut physical.shift,
            DeckKey::LeftOption | DeckKey::RightOption => {
                let (left, right) = option_sides(physical.option);
                let (left, right) = if key == DeckKey::LeftOption {
                    (pressed, right)
                } else {
                    (left, pressed)
                };
                let next = side_of(left, right);
                let changed = next != physical.option;
                physical.option = next;
                return changed;
            }
            DeckKey::Command | DeckKey::Other(_) => return false,
        };
        let changed = *slot != pressed;
        *slot = pressed;
        changed
    }

    /// The host modifier keys the physical state asks for.
    fn wanted_modifiers(&self) -> BTreeSet<u32> {
        let physical = self.physical;
        let (left, right) = option_sides(physical.option);
        let policy = self.policy;
        let command = physical.command;
        let (control, meta) = match policy.shortcuts {
            ShortcutMode::CommandIsMeta => (physical.control, command),
            ShortcutMode::CommandIsControl => (physical.control || command, false),
            ShortcutMode::SwapCommandControl => (command, physical.control),
            ShortcutMode::WindowsApp => (
                physical.control || (command && self.command == Some(CommandState::Control)),
                command && self.command == Some(CommandState::Meta),
            ),
        };
        let alt = (left && !policy.left_option_altgr) || (right && !policy.right_option_altgr);
        let altgr = (left && policy.left_option_altgr) || (right && policy.right_option_altgr);
        [
            (KEY_CONTROL, control),
            (KEY_SHIFT, physical.shift),
            (KEY_ALT, alt),
            (KEY_ALTGR, altgr),
            (KEY_META, meta),
        ]
        .into_iter()
        .filter_map(|(key, held)| held.then_some(key))
        .collect()
    }

    /// Releases host modifiers no longer wanted, then presses new ones —
    /// except that a Meta key going up while another modifier comes down is
    /// released last: the new modifier's press is then the key Windows
    /// needs between the Windows key's press and release, so switching from
    /// Command-as-Windows (after a click) to Command-as-Control never opens
    /// Start.
    fn reconcile(&mut self, out: &mut Vec<HostKeyEdge>) {
        let wanted = self.wanted_modifiers();
        let pressing = HOST_MODIFIERS
            .iter()
            .any(|(key, _)| wanted.contains(key) && !self.host_modifiers.contains(key));
        let meta_last =
            pressing && self.host_modifiers.contains(&KEY_META) && !wanted.contains(&KEY_META);
        for (key, _) in HOST_MODIFIERS.iter().rev() {
            if self.host_modifiers.contains(key)
                && !wanted.contains(key)
                && !(meta_last && *key == KEY_META)
            {
                self.release_modifier(*key, out);
            }
        }
        for (key, _) in HOST_MODIFIERS {
            if wanted.contains(&key) && !self.host_modifiers.contains(&key) {
                self.press_modifier(key, out);
            }
        }
        if meta_last {
            self.release_modifier(KEY_META, out);
        }
    }

    fn press_modifier(&mut self, qt_key: u32, out: &mut Vec<HostKeyEdge>) {
        if self.host_modifiers.insert(qt_key) {
            out.push(HostKeyEdge {
                qt_key,
                pressed: true,
                modifiers: self.host_mask(),
            });
        }
    }

    fn release_modifier(&mut self, qt_key: u32, out: &mut Vec<HostKeyEdge>) {
        if self.host_modifiers.remove(&qt_key) {
            out.push(HostKeyEdge {
                qt_key,
                pressed: false,
                modifiers: self.host_mask(),
            });
        }
    }

    fn host_mask(&self) -> u32 {
        HOST_MODIFIERS
            .iter()
            .filter(|(key, _)| self.host_modifiers.contains(key))
            .fold(0, |mask, (_, bit)| mask | bit)
    }
}

const fn keypad_bit(keypad: bool) -> u32 {
    if keypad { MOD_KEYPAD } else { 0 }
}

const fn option_sides(side: OptionSide) -> (bool, bool) {
    match side {
        OptionSide::None => (false, false),
        OptionSide::Left => (true, false),
        OptionSide::Right => (false, true),
        OptionSide::Both => (true, true),
    }
}

const fn side_of(left: bool, right: bool) -> OptionSide {
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
    use crate::keyboard::{KeyboardSettings, ShortcutPreference, resolve_keyboard_policy};
    use arcen_protocol::messages::HostPlatform;

    const KEY_C: u32 = 0x43;
    const KEY_V: u32 = 0x56;
    const KEY_E: u32 = 0x45;
    const KEY_Z: u32 = 0x5A;
    const KEY_2: u32 = 0x32;
    const KEY_Q: u32 = 0x51;

    fn windows(settings: KeyboardSettings) -> ShortcutTranslator {
        ShortcutTranslator::new(resolve_keyboard_policy(
            HostPlatform::Windows,
            true,
            settings,
        ))
    }

    /// Plays physical edges, each with the modifiers a Mac reports at it,
    /// and returns the host edges as `(key, pressed, mask)`.
    struct Keyboard {
        translator: ShortcutTranslator,
        held: DeckModifiers,
    }

    impl Keyboard {
        fn new(translator: ShortcutTranslator) -> Self {
            Self {
                translator,
                held: DeckModifiers::default(),
            }
        }

        fn press(&mut self, key: DeckKey) -> Vec<(u32, bool, u32)> {
            self.edge(key, true)
        }

        fn release(&mut self, key: DeckKey) -> Vec<(u32, bool, u32)> {
            self.edge(key, false)
        }

        fn edge(&mut self, key: DeckKey, pressed: bool) -> Vec<(u32, bool, u32)> {
            match key {
                DeckKey::Command => self.held.command = pressed,
                DeckKey::Control => self.held.control = pressed,
                DeckKey::Shift => self.held.shift = pressed,
                DeckKey::LeftOption | DeckKey::RightOption => {
                    let (left, right) = option_sides(self.held.option);
                    self.held.option = if key == DeckKey::LeftOption {
                        side_of(pressed, right)
                    } else {
                        side_of(left, pressed)
                    };
                }
                DeckKey::Other(_) => {}
            }
            flatten(self.translator.key(key, pressed, false, self.held))
        }

        fn tap(&mut self, key: u32) -> Vec<(u32, bool, u32)> {
            let mut out = self.press(DeckKey::Other(key));
            out.extend(self.release(DeckKey::Other(key)));
            out
        }
    }

    fn flatten(edges: Vec<HostKeyEdge>) -> Vec<(u32, bool, u32)> {
        edges
            .into_iter()
            .map(|edge| (edge.qt_key, edge.pressed, edge.modifiers))
            .collect()
    }

    #[test]
    fn a_command_tap_alone_is_a_windows_key_tap() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        assert!(keyboard.press(DeckKey::Command).is_empty());
        assert_eq!(
            keyboard.release(DeckKey::Command),
            vec![(KEY_META, true, MOD_META), (KEY_META, false, 0)]
        );
        assert!(keyboard.translator.is_idle());
    }

    #[test]
    fn command_with_another_key_holds_the_windows_key() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        keyboard.press(DeckKey::Command);
        assert_eq!(
            keyboard.press(DeckKey::Other(KEY_E)),
            vec![(KEY_META, true, MOD_META), (KEY_E, true, MOD_META)]
        );
        assert_eq!(
            keyboard.release(DeckKey::Other(KEY_E)),
            vec![(KEY_E, false, MOD_META)]
        );
        assert_eq!(
            keyboard.release(DeckKey::Command),
            vec![(KEY_META, false, 0)]
        );
    }

    #[test]
    fn command_c_is_control_c_without_the_windows_key() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        keyboard.press(DeckKey::Command);
        assert_eq!(
            keyboard.tap(KEY_C),
            vec![
                (KEY_CONTROL, true, MOD_CTRL),
                (KEY_C, true, MOD_CTRL),
                (KEY_C, false, MOD_CTRL)
            ]
        );
        // Command+V with Command still held: Control stays down.
        assert_eq!(
            keyboard.tap(KEY_V),
            vec![(KEY_V, true, MOD_CTRL), (KEY_V, false, MOD_CTRL)]
        );
        assert_eq!(
            keyboard.release(DeckKey::Command),
            vec![(KEY_CONTROL, false, 0)]
        );
        assert!(keyboard.translator.is_idle());
    }

    #[test]
    fn command_shift_z_is_control_shift_z() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        keyboard.press(DeckKey::Command);
        assert_eq!(
            keyboard.press(DeckKey::Shift),
            vec![(KEY_SHIFT, true, MOD_SHIFT)]
        );
        assert_eq!(
            keyboard.tap(KEY_Z),
            vec![
                (KEY_CONTROL, true, MOD_CTRL | MOD_SHIFT),
                (KEY_Z, true, MOD_CTRL | MOD_SHIFT),
                (KEY_Z, false, MOD_CTRL | MOD_SHIFT)
            ]
        );
        keyboard.release(DeckKey::Shift);
        // Shift was pressed: letting go of Command is not a Windows tap.
        assert_eq!(
            keyboard.release(DeckKey::Command),
            vec![(KEY_CONTROL, false, 0)]
        );
    }

    #[test]
    fn switching_shortcut_kind_under_one_command_hold_ends_the_first() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        keyboard.press(DeckKey::Command);
        keyboard.press(DeckKey::Other(KEY_C));
        // E while C is still held: Ctrl+C ends, Win+E starts.
        assert_eq!(
            keyboard.press(DeckKey::Other(KEY_E)),
            vec![
                (KEY_C, false, MOD_CTRL),
                (KEY_CONTROL, false, 0),
                (KEY_META, true, MOD_META),
                (KEY_E, true, MOD_META)
            ]
        );
        // C's physical release has nothing left to release.
        assert!(keyboard.release(DeckKey::Other(KEY_C)).is_empty());
        keyboard.release(DeckKey::Other(KEY_E));
        assert_eq!(
            keyboard.tap(KEY_V),
            vec![
                (KEY_CONTROL, true, MOD_CTRL | MOD_META),
                (KEY_META, false, MOD_CTRL),
                (KEY_V, true, MOD_CTRL),
                (KEY_V, false, MOD_CTRL)
            ]
        );
    }

    #[test]
    fn command_click_is_windows_click_and_never_a_start_tap() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        keyboard.press(DeckKey::Command);
        assert_eq!(
            flatten(keyboard.translator.settle_command_for_pointer()),
            vec![(KEY_META, true, MOD_META)]
        );
        // A second click under the same hold sends nothing more.
        assert!(keyboard.translator.settle_command_for_pointer().is_empty());
        // Released after clicks only: a Control tap first, so Windows does
        // not open Start (a click does not count as another key).
        assert_eq!(
            keyboard.release(DeckKey::Command),
            vec![
                (KEY_CONTROL, true, MOD_CTRL | MOD_META),
                (KEY_CONTROL, false, MOD_META),
                (KEY_META, false, 0)
            ]
        );
        // Focus lost while Command is held for a click: the same mask.
        keyboard.press(DeckKey::Command);
        keyboard.translator.settle_command_for_pointer();
        assert_eq!(
            flatten(keyboard.translator.release_all()),
            vec![
                (KEY_CONTROL, true, MOD_CTRL | MOD_META),
                (KEY_CONTROL, false, MOD_META),
                (KEY_META, false, 0)
            ]
        );
        keyboard.held = DeckModifiers::default();
        // Command+click then Command+E: E is the other key, no mask.
        keyboard.press(DeckKey::Command);
        keyboard.translator.settle_command_for_pointer();
        keyboard.tap(KEY_E);
        assert_eq!(
            keyboard.release(DeckKey::Command),
            vec![(KEY_META, false, 0)]
        );
        // Command+click (or scroll, or gesture) then Command+C: Control goes
        // down before the Windows key comes up, so Windows sees a key
        // between them and does not open Start.
        keyboard.press(DeckKey::Command);
        keyboard.translator.settle_command_for_pointer();
        assert_eq!(
            keyboard.press(DeckKey::Other(KEY_C)),
            vec![
                (KEY_CONTROL, true, MOD_CTRL | MOD_META),
                (KEY_META, false, MOD_CTRL),
                (KEY_C, true, MOD_CTRL)
            ]
        );
        keyboard.release(DeckKey::Other(KEY_C));
        // Command+V next under the same hold: Control stays down.
        assert_eq!(
            keyboard.tap(KEY_V),
            vec![(KEY_V, true, MOD_CTRL), (KEY_V, false, MOD_CTRL)]
        );
        assert_eq!(
            keyboard.release(DeckKey::Command),
            vec![(KEY_CONTROL, false, 0)]
        );
        // Nothing held back, nothing to settle; other modes never hold back.
        let mut idle = windows(KeyboardSettings::default());
        assert!(idle.settle_command_for_pointer().is_empty());
        let mut linux = Keyboard::new(ShortcutTranslator::new(KeyboardPolicy::LEGACY));
        linux.press(DeckKey::Command);
        assert!(linux.translator.settle_command_for_pointer().is_empty());
    }

    #[test]
    fn a_host_that_masks_the_windows_key_gets_no_control_tap() {
        let policy =
            resolve_keyboard_policy(HostPlatform::Windows, true, KeyboardSettings::default())
                .with_host_meta_mask(true);
        let mut keyboard = Keyboard::new(ShortcutTranslator::new(policy));
        keyboard.press(DeckKey::Command);
        assert_eq!(
            flatten(keyboard.translator.settle_command_for_pointer()),
            vec![(KEY_META, true, MOD_META)]
        );
        assert_eq!(
            keyboard.release(DeckKey::Command),
            vec![(KEY_META, false, 0)]
        );
        keyboard.press(DeckKey::Command);
        keyboard.translator.settle_command_for_pointer();
        assert_eq!(
            flatten(keyboard.translator.release_all()),
            vec![(KEY_META, false, 0)]
        );
    }

    #[test]
    fn a_consumed_command_q_is_sent_as_windows_q_without_opening_start() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        keyboard.press(DeckKey::Command);
        // The menu took Q; Command is let go before the hold decides.
        keyboard.translator.command_chord_consumed();
        assert!(keyboard.release(DeckKey::Command).is_empty());
        assert_eq!(
            flatten(keyboard.translator.tap_with_command(KEY_Q)),
            vec![
                (KEY_META, true, MOD_META),
                (KEY_Q, true, MOD_META),
                (KEY_Q, false, MOD_META),
                (KEY_META, false, 0)
            ]
        );
    }

    #[test]
    fn command_q_while_command_is_still_held_keeps_it_held() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        keyboard.press(DeckKey::Command);
        keyboard.translator.command_chord_consumed();
        assert_eq!(
            flatten(keyboard.translator.tap_with_command(KEY_Q)),
            vec![
                (KEY_META, true, MOD_META),
                (KEY_Q, true, MOD_META),
                (KEY_Q, false, MOD_META)
            ]
        );
        assert_eq!(
            keyboard.release(DeckKey::Command),
            vec![(KEY_META, false, 0)]
        );
    }

    #[test]
    fn linux_command_q_tap_matches_the_old_control_q() {
        let mut translator = ShortcutTranslator::new(KeyboardPolicy::LEGACY);
        assert_eq!(
            flatten(translator.tap_with_command(KEY_Q)),
            vec![
                (KEY_CONTROL, true, MOD_CTRL),
                (KEY_Q, true, MOD_CTRL),
                (KEY_Q, false, MOD_CTRL),
                (KEY_CONTROL, false, 0)
            ]
        );
    }

    #[test]
    fn focus_loss_releases_everything_and_drops_a_held_back_command() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        keyboard.press(DeckKey::RightOption);
        keyboard.press(DeckKey::Command);
        keyboard.press(DeckKey::Other(KEY_C));
        assert_eq!(
            flatten(keyboard.translator.release_all()),
            vec![
                (KEY_C, false, MOD_CTRL),
                (KEY_ALTGR, false, MOD_CTRL),
                (KEY_CONTROL, false, 0)
            ]
        );
        assert!(keyboard.translator.is_idle());
        let mut pending = Keyboard::new(windows(KeyboardSettings::default()));
        pending.press(DeckKey::Command);
        assert!(pending.translator.release_all().is_empty());
        // Command is no longer held after focus came back: nothing to tap.
        pending.held = DeckModifiers::default();
        assert_eq!(
            pending.tap(KEY_E),
            vec![(KEY_E, true, 0), (KEY_E, false, 0)]
        );
    }

    #[test]
    fn right_option_is_altgr_without_the_alt_bit() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings::default()));
        assert_eq!(
            keyboard.press(DeckKey::RightOption),
            vec![(KEY_ALTGR, true, 0)]
        );
        assert_eq!(
            keyboard.tap(KEY_2),
            vec![(KEY_2, true, 0), (KEY_2, false, 0)]
        );
        assert_eq!(
            keyboard.release(DeckKey::RightOption),
            vec![(KEY_ALTGR, false, 0)]
        );
        assert_eq!(
            keyboard.press(DeckKey::LeftOption),
            vec![(KEY_ALT, true, MOD_ALT)]
        );
    }

    #[test]
    fn left_option_is_altgr_when_the_connection_asks() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings {
            shortcuts: ShortcutPreference::Automatic,
            left_option_altgr: true,
            typing: None,
        }));
        assert_eq!(
            keyboard.press(DeckKey::LeftOption),
            vec![(KEY_ALTGR, true, 0)]
        );
        // Both Options held still hold one AltGr.
        assert!(keyboard.press(DeckKey::RightOption).is_empty());
        assert!(keyboard.release(DeckKey::LeftOption).is_empty());
        assert_eq!(
            keyboard.release(DeckKey::RightOption),
            vec![(KEY_ALTGR, false, 0)]
        );
    }

    #[test]
    fn swapped_mode_sends_command_as_control_and_control_as_windows() {
        let mut keyboard = Keyboard::new(windows(KeyboardSettings {
            shortcuts: ShortcutPreference::SwapCommandControl,
            left_option_altgr: false,
            typing: None,
        }));
        assert_eq!(
            keyboard.press(DeckKey::Command),
            vec![(KEY_CONTROL, true, MOD_CTRL)]
        );
        assert_eq!(
            keyboard.tap(KEY_E),
            vec![(KEY_E, true, MOD_CTRL), (KEY_E, false, MOD_CTRL)]
        );
        keyboard.release(DeckKey::Command);
        assert_eq!(
            keyboard.press(DeckKey::Control),
            vec![(KEY_META, true, MOD_META)]
        );
    }

    #[test]
    fn linux_and_mac_hosts_keep_their_command_mapping() {
        let mut linux = Keyboard::new(ShortcutTranslator::new(KeyboardPolicy::LEGACY));
        assert_eq!(
            linux.press(DeckKey::Command),
            vec![(KEY_CONTROL, true, MOD_CTRL)]
        );
        assert!(linux.press(DeckKey::Control).is_empty());
        assert!(linux.release(DeckKey::Command).is_empty());
        assert_eq!(
            linux.release(DeckKey::Control),
            vec![(KEY_CONTROL, false, 0)]
        );
        assert_eq!(
            linux.press(DeckKey::RightOption),
            vec![(KEY_ALT, true, MOD_ALT)]
        );
        let mut mac = Keyboard::new(ShortcutTranslator::new(resolve_keyboard_policy(
            HostPlatform::MacOs,
            true,
            KeyboardSettings::default(),
        )));
        assert_eq!(
            mac.press(DeckKey::Command),
            vec![(KEY_META, true, MOD_META)]
        );
    }

    #[test]
    fn modifiers_held_before_focus_are_pressed_from_the_snapshot() {
        let mut translator = ShortcutTranslator::new(KeyboardPolicy::LEGACY);
        let held = DeckModifiers {
            command: true,
            option: OptionSide::Right,
            ..DeckModifiers::default()
        };
        assert_eq!(
            flatten(translator.key(DeckKey::Other(KEY_C), true, false, held)),
            vec![
                (KEY_CONTROL, true, MOD_CTRL),
                (KEY_ALT, true, MOD_CTRL | MOD_ALT),
                (KEY_C, true, MOD_CTRL | MOD_ALT)
            ]
        );
        // The snapshot no longer has them: released before the next key.
        assert_eq!(
            flatten(translator.key(
                DeckKey::Other(KEY_C),
                false,
                false,
                DeckModifiers::default()
            )),
            vec![
                (KEY_CONTROL, false, MOD_ALT),
                (KEY_ALT, false, 0),
                (KEY_C, false, 0)
            ]
        );
    }

    #[test]
    fn a_policy_change_releases_under_the_old_policy() {
        let mut keyboard = Keyboard::new(ShortcutTranslator::new(KeyboardPolicy::LEGACY));
        keyboard.press(DeckKey::Command);
        let released = keyboard.translator.set_policy(resolve_keyboard_policy(
            HostPlatform::Windows,
            true,
            KeyboardSettings::default(),
        ));
        assert_eq!(flatten(released), vec![(KEY_CONTROL, false, 0)]);
        // Command still held: the next key resolves it under the new policy.
        assert_eq!(
            keyboard.press(DeckKey::Other(KEY_E)),
            vec![(KEY_META, true, MOD_META), (KEY_E, true, MOD_META)]
        );
    }

    #[test]
    fn keypad_keys_are_released_as_keypad_keys() {
        let mut translator = ShortcutTranslator::new(KeyboardPolicy::LEGACY);
        translator.key(DeckKey::Other(KEY_2), true, true, DeckModifiers::default());
        assert_eq!(
            flatten(translator.release_all()),
            vec![(KEY_2, false, MOD_KEYPAD)]
        );
    }
}
