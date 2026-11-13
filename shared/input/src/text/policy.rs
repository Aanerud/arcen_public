// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The client's choice, per keystroke, between sending the key position and
//! sending the characters the client's own input method produced.
//!
//! Key positions keep shortcuts, games and held keys working, and the host's
//! layout decides the character. Committed text keeps the characters the
//! user sees on their own keyboard. The policy sends positions unless the
//! characters would come out differently:
//!
//! - input-method and dead-key commits are always text (the keystrokes that
//!   composed them never reach the host);
//! - a plain keystroke is text when the host told the client its layout
//!   types something else at that position ([`TypingMode::Auto`]), or always
//!   ([`TypingMode::Characters`]);
//! - an Option keystroke is text or an Alt shortcut by the side of the
//!   Option key ([`OptionKeyMode`]);
//! - Command and Control chords are always positions.
//!
//! A keystroke sent as text has its press, repeats and release kept from the
//! host, so the host never sees half a key. An Option key that composes is
//! not Alt on the host at all: its own press and release are kept from the
//! host, and keys forwarded while only composing Option keys are held go
//! without Alt (a bare Alt tap would focus a host application's menu bar).
//! Without a host that types committed text the policy changes nothing.

use std::collections::{BTreeSet, HashMap};

use arcen_protocol::messages::{HostKeymapMsg, HostPlatform, MAX_TEXT_COMMIT_BYTES};

/// How plain keystrokes reach the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TypingMode {
    /// Key positions only; the host's layout decides every character. Input
    /// method composition stays off.
    Keys,
    /// Positions, except where the host's layout would type a different
    /// character, and every composed commit.
    #[default]
    Auto,
    /// Every typed character as text.
    Characters,
}

impl TypingMode {
    /// The settings token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Keys => "keys",
            Self::Auto => "auto",
            Self::Characters => "characters",
        }
    }

    /// The mode a settings token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "keys" => Some(Self::Keys),
            "auto" => Some(Self::Auto),
            "characters" => Some(Self::Characters),
            _ => None,
        }
    }
}

/// A connection's keyboard mode, chosen per connection and switchable during
/// a session (Microsoft's Windows App calls the two "scancode" and
/// "Unicode").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KeyboardMode {
    /// Keys go as key positions and the host's layout decides each
    /// character, as the Typing setting describes (Auto or Keys only).
    #[default]
    Physical,
    /// Every typed character goes as the character this Mac's layout typed
    /// (dead keys, Option characters, input methods included); shortcuts,
    /// navigation and function keys stay keys.
    Characters,
}

impl KeyboardMode {
    /// Both, in menu order.
    pub const ALL: [Self; 2] = [Self::Physical, Self::Characters];

    /// The settings token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Physical => "physical",
            Self::Characters => "characters",
        }
    }

    /// The mode a settings token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "physical" => Some(Self::Physical),
            "characters" => Some(Self::Characters),
            _ => None,
        }
    }

    /// The mode the application-wide typing setting amounts to.
    #[must_use]
    pub const fn of_typing(typing: TypingMode) -> Self {
        match typing {
            TypingMode::Characters => Self::Characters,
            TypingMode::Keys | TypingMode::Auto => Self::Physical,
        }
    }

    /// The typing mode in force under this keyboard mode, given the
    /// application-wide typing setting: Characters always types characters;
    /// Physical keeps the setting's own choice of key positions, and is
    /// Auto when the setting itself is "always characters".
    #[must_use]
    pub const fn typing_mode(self, setting: TypingMode) -> TypingMode {
        match (self, setting) {
            (Self::Characters, _) => TypingMode::Characters,
            (Self::Physical, TypingMode::Characters) => TypingMode::Auto,
            (Self::Physical, setting) => setting,
        }
    }
}

/// What each Option key does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OptionKeyMode {
    /// Both Option keys are Alt.
    BothAlt,
    /// Left Option is Alt (shortcuts); right Option types the Mac's
    /// special characters.
    #[default]
    LeftAltRightCharacters,
    /// Both Option keys type the Mac's special characters.
    BothCharacters,
}

impl OptionKeyMode {
    /// The settings token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::BothAlt => "both_alt",
            Self::LeftAltRightCharacters => "left_alt_right_characters",
            Self::BothCharacters => "both_characters",
        }
    }

    /// The mode a settings token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "both_alt" => Some(Self::BothAlt),
            "left_alt_right_characters" => Some(Self::LeftAltRightCharacters),
            "both_characters" => Some(Self::BothCharacters),
            _ => None,
        }
    }

    /// Whether an Option key on `side` composes characters (rather than
    /// acting as Alt).
    #[must_use]
    pub const fn composes(self, side: OptionSide) -> bool {
        matches!(
            (self, side),
            (
                Self::BothCharacters,
                OptionSide::Left | OptionSide::Right | OptionSide::Both
            ) | (Self::LeftAltRightCharacters, OptionSide::Right)
        )
    }
}

/// The Option-keys setting: [`Automatic`](Self::Automatic) unless the user
/// chose a mode.
///
/// Automatic keeps a Mac host's Option keys as Option ([`OptionKeyMode::BothAlt`]),
/// which is how a Mac host received them before it typed committed text, so
/// a Mac-to-Mac session does not change under someone's fingers; other hosts
/// get [`OptionKeyMode::LeftAltRightCharacters`]. A chosen mode applies to
/// every host. Characters mode and Paste as Keystrokes send text either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OptionKeyPreference {
    /// Decided by the host's platform.
    #[default]
    Automatic,
    /// The user's choice.
    Chosen(OptionKeyMode),
}

impl From<OptionKeyMode> for OptionKeyPreference {
    fn from(mode: OptionKeyMode) -> Self {
        Self::Chosen(mode)
    }
}

impl OptionKeyPreference {
    /// The mode in force for a host on `platform`.
    #[must_use]
    pub const fn resolve(self, platform: HostPlatform) -> OptionKeyMode {
        match (self, platform) {
            (Self::Chosen(mode), _) => mode,
            (Self::Automatic, HostPlatform::MacOs) => OptionKeyMode::BothAlt,
            (Self::Automatic, _) => OptionKeyMode::LeftAltRightCharacters,
        }
    }

    /// The settings token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Chosen(mode) => mode.token(),
        }
    }

    /// The preference a settings token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "automatic" => Some(Self::Automatic),
            _ => OptionKeyMode::from_token(token).map(Self::Chosen),
        }
    }

    /// The preference stored settings hold: `preference` (written by builds
    /// that know [`Automatic`](Self::Automatic), alongside `legacy_mode` as
    /// it resolves for an unknown host), and the older `legacy_mode`.
    ///
    /// A `legacy_mode` that disagrees with `preference` was written later by
    /// an older build (a downgrade, which keeps keys it does not know), so it
    /// is the user's latest choice. Without `preference`, older builds stored
    /// their default with every save, so a stored
    /// [`OptionKeyMode::LeftAltRightCharacters`] says nothing about a choice
    /// and reads as Automatic (which is that mode on every host those builds
    /// typed text into); the other two modes were chosen.
    #[must_use]
    pub fn from_settings(preference: Option<&str>, legacy_mode: Option<&str>) -> Self {
        let legacy = legacy_mode.and_then(OptionKeyMode::from_token);
        if let Some(preference) = preference.and_then(Self::from_token) {
            return match legacy {
                Some(mode) if mode != preference.resolve(HostPlatform::Unknown) => {
                    Self::Chosen(mode)
                }
                _ => preference,
            };
        }
        match legacy {
            Some(mode @ (OptionKeyMode::BothAlt | OptionKeyMode::BothCharacters)) => {
                Self::Chosen(mode)
            }
            Some(OptionKeyMode::LeftAltRightCharacters) | None => Self::Automatic,
        }
    }
}

/// Which Option keys are held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OptionSide {
    /// None.
    #[default]
    None,
    /// The left one.
    Left,
    /// The right one.
    Right,
    /// Both.
    Both,
}

/// A keystroke's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPhase {
    /// Pressed.
    Press,
    /// Auto-repeated while held.
    Repeat,
    /// Released.
    Release,
}

/// One physical keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStroke {
    /// The key position as a Qt key identifier, when it has one.
    pub position: Option<u32>,
    /// Press, repeat or release.
    pub phase: KeyPhase,
    /// Shift held.
    pub shift: bool,
    /// Command or Control held.
    pub command: bool,
    /// Option keys held.
    pub option: OptionSide,
    /// The Option key this keystroke is, if it is one.
    pub option_key: OptionSide,
}

/// One input event of a frame, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckTextEvent {
    /// A keystroke.
    Key(KeyStroke),
    /// The text a keystroke typed (it follows that keystroke's press).
    Text(String),
    /// The input method's composition changed; empty when it ended.
    Preedit(String),
    /// The input method committed text.
    Commit(String),
    /// Keyboard focus left the session.
    FocusLost,
}

/// Why a keystroke is kept from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressReason {
    /// Its characters were sent as text instead.
    SentAsText,
    /// The release (or repeat) of a keystroke sent as text.
    TextKeystroke,
    /// An Option key that composes characters; the host has no use for it.
    ComposingOption,
}

/// What to do for a frame's events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckTextAction {
    /// Send the key event at this index of the frame, as a key position.
    ForwardKey(usize),
    /// Send the key event at this index as a key position, without Alt: the
    /// Option keys held compose characters.
    ForwardKeyWithoutAlt(usize),
    /// Do not send the key event at this index.
    SuppressKey(usize, SuppressReason),
    /// Send this committed text.
    SendText(String),
    /// Show this composition (`None`: hide it).
    Preedit(Option<String>),
}

/// What the host types committed text with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostTextTruth {
    /// The host types `text_commit`.
    pub text_commit: bool,
    /// The host types characters beyond its session's layout.
    pub unicode: bool,
}

/// What the host's layout types at each key position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostKeyPositions {
    generation: u64,
    positions: HashMap<u32, (Option<String>, Option<String>)>,
    active_layout: Option<String>,
}

impl HostKeyPositions {
    /// From the host's notice.
    #[must_use]
    pub fn from_message(message: &HostKeymapMsg) -> Self {
        Self {
            generation: message.generation,
            positions: message
                .keys
                .iter()
                .map(|key| (key.qt_key, (key.base.clone(), key.shift.clone())))
                .collect(),
            active_layout: message.active_layout.clone(),
        }
    }

    /// The notice's generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The active layout's name, if the host knows it.
    #[must_use]
    pub fn active_layout(&self) -> Option<&str> {
        self.active_layout.as_deref()
    }

    /// Whether the notice describes any key (a host that could not read its
    /// layout sends one that describes none).
    #[must_use]
    pub fn describes_keys(&self) -> bool {
        !self.positions.is_empty()
    }

    /// Whether the host's layout types `text` at `position` with `shift`.
    /// `None` when the position is not described.
    #[must_use]
    pub fn types_same(&self, position: u32, shift: bool, text: &str) -> Option<bool> {
        let (base, shifted) = self.positions.get(&position)?;
        let expected = if shift { shifted } else { base };
        Some(expected.as_deref() == Some(text))
    }
}

/// The per-session keystroke policy.
#[derive(Debug, Clone, Default)]
pub struct TextInputPolicy {
    mode: TypingMode,
    option: OptionKeyPreference,
    /// The host's platform; `None` until a hello says it.
    platform: Option<HostPlatform>,
    host: HostTextTruth,
    keymap: Option<HostKeyPositions>,
    text_keys: BTreeSet<u32>,
    /// Composing Option keys (left, right) whose press was kept from the
    /// host, so their release is too.
    held_composing_options: [bool; 2],
    preedit: bool,
}

impl TextInputPolicy {
    /// A policy with these settings, for a host that has not said what it
    /// types.
    #[must_use]
    pub fn new(mode: TypingMode, option: impl Into<OptionKeyPreference>) -> Self {
        Self {
            mode,
            option: option.into(),
            ..Self::default()
        }
    }

    /// Changes the settings.
    pub fn configure(&mut self, mode: TypingMode, option: impl Into<OptionKeyPreference>) {
        self.mode = mode;
        self.option = option.into();
    }

    /// The host's platform, from its hello: what an automatic Option-keys
    /// setting resolves against.
    pub const fn set_host_platform(&mut self, platform: HostPlatform) {
        self.platform = Some(platform);
    }

    /// The host's truth, from its hello. Forgets the previous host's
    /// layout and keystrokes.
    pub fn set_host(&mut self, host: HostTextTruth) {
        self.host = host;
        self.keymap = None;
        self.text_keys.clear();
        self.held_composing_options = [false; 2];
        self.preedit = false;
    }

    /// The host's layout positions.
    pub fn set_host_keymap(&mut self, keymap: Option<HostKeyPositions>) {
        self.keymap = keymap;
    }

    /// The host's layout positions, if known.
    #[must_use]
    pub const fn host_keymap(&self) -> Option<&HostKeyPositions> {
        self.keymap.as_ref()
    }

    /// The host's truth.
    #[must_use]
    pub const fn host(&self) -> HostTextTruth {
        self.host
    }

    /// The typing mode.
    #[must_use]
    pub const fn mode(&self) -> TypingMode {
        self.mode
    }

    /// The Option-key mode in force for this host.
    #[must_use]
    pub const fn option_mode(&self) -> OptionKeyMode {
        self.option.resolve(match self.platform {
            Some(platform) => platform,
            None => HostPlatform::Unknown,
        })
    }

    /// The Option-keys setting.
    #[must_use]
    pub const fn option_preference(&self) -> OptionKeyPreference {
        self.option
    }

    /// Whether committed text is in use: the host types it and the mode
    /// is not [`TypingMode::Keys`]. Only then may the client let its input
    /// method compose.
    #[must_use]
    pub const fn active(&self) -> bool {
        self.host.text_commit && !matches!(self.mode, TypingMode::Keys)
    }

    /// Whether a composition is showing.
    #[must_use]
    pub const fn composing(&self) -> bool {
        self.preedit
    }

    /// Decides a frame's events.
    pub fn plan_frame(&mut self, events: &[DeckTextEvent]) -> Vec<DeckTextAction> {
        let mut actions = Vec::with_capacity(events.len());
        if !self.active() {
            for (index, event) in events.iter().enumerate() {
                if matches!(event, DeckTextEvent::Key(_)) {
                    actions.push(DeckTextAction::ForwardKey(index));
                }
            }
            return actions;
        }
        let mut index = 0;
        while index < events.len() {
            match &events[index] {
                DeckTextEvent::Key(stroke) => {
                    let text = match events.get(index + 1) {
                        Some(DeckTextEvent::Text(text)) if stroke.phase != KeyPhase::Release => {
                            Some(text.as_str())
                        }
                        _ => None,
                    };
                    actions.push(self.keystroke(index, stroke, text));
                    if let Some(text) = text {
                        if matches!(actions.last(), Some(DeckTextAction::SuppressKey(_, _))) {
                            actions.push(DeckTextAction::SendText(text.to_owned()));
                        }
                        index += 1;
                    }
                }
                DeckTextEvent::Text(text) => {
                    // Text that no keystroke of this frame typed: what the
                    // user entered, by whatever means.
                    if !text.is_empty() {
                        actions.push(DeckTextAction::SendText(text.clone()));
                    }
                }
                DeckTextEvent::Commit(text) => {
                    if self.preedit {
                        self.preedit = false;
                        actions.push(DeckTextAction::Preedit(None));
                    }
                    if !text.is_empty() {
                        actions.push(DeckTextAction::SendText(text.clone()));
                    }
                }
                DeckTextEvent::Preedit(text) => {
                    self.preedit = !text.is_empty();
                    actions.push(DeckTextAction::Preedit(self.preedit.then(|| text.clone())));
                }
                DeckTextEvent::FocusLost => {
                    self.text_keys.clear();
                    self.held_composing_options = [false; 2];
                    if self.preedit {
                        self.preedit = false;
                        actions.push(DeckTextAction::Preedit(None));
                    }
                }
            }
            index += 1;
        }
        actions
    }

    fn keystroke(
        &mut self,
        index: usize,
        stroke: &KeyStroke,
        text: Option<&str>,
    ) -> DeckTextAction {
        if stroke.option_key != OptionSide::None {
            return self.option_keystroke(index, stroke);
        }
        match self.plain_keystroke(index, stroke, text) {
            DeckTextAction::ForwardKey(index) if self.option_mode().composes(stroke.option) => {
                DeckTextAction::ForwardKeyWithoutAlt(index)
            }
            action => action,
        }
    }

    /// An Option key itself: kept from the host when it composes, tracked
    /// per side so a release matches its press across a settings change.
    fn option_keystroke(&mut self, index: usize, stroke: &KeyStroke) -> DeckTextAction {
        let slot = match stroke.option_key {
            OptionSide::Left => 0,
            OptionSide::Right => 1,
            OptionSide::None | OptionSide::Both => return DeckTextAction::ForwardKey(index),
        };
        let kept = match stroke.phase {
            KeyPhase::Press => {
                self.held_composing_options[slot] = self.option_mode().composes(stroke.option_key);
                self.held_composing_options[slot]
            }
            KeyPhase::Repeat => self.held_composing_options[slot],
            KeyPhase::Release => std::mem::take(&mut self.held_composing_options[slot]),
        };
        if kept {
            DeckTextAction::SuppressKey(index, SuppressReason::ComposingOption)
        } else {
            DeckTextAction::ForwardKey(index)
        }
    }

    fn plain_keystroke(
        &mut self,
        index: usize,
        stroke: &KeyStroke,
        text: Option<&str>,
    ) -> DeckTextAction {
        let Some(position) = stroke.position else {
            return DeckTextAction::ForwardKey(index);
        };
        if stroke.phase == KeyPhase::Press {
            // A new press: if this key was sent as text before, its release
            // was lost (an input method swallows releases while it
            // composes), and must not swallow this press's release.
            self.text_keys.remove(&position);
        }
        if stroke.phase == KeyPhase::Release {
            return if self.text_keys.remove(&position) {
                DeckTextAction::SuppressKey(index, SuppressReason::TextKeystroke)
            } else {
                DeckTextAction::ForwardKey(index)
            };
        }
        if stroke.phase == KeyPhase::Repeat && self.text_keys.contains(&position) {
            // The host repeats nothing it never saw pressed: the repeat's
            // text is sent again by the caller when there is one.
            return if text.is_some() {
                DeckTextAction::SuppressKey(index, SuppressReason::SentAsText)
            } else {
                DeckTextAction::SuppressKey(index, SuppressReason::TextKeystroke)
            };
        }
        let Some(text) = text else {
            return DeckTextAction::ForwardKey(index);
        };
        if stroke.command || text.is_empty() {
            return DeckTextAction::ForwardKey(index);
        }
        let as_text = if stroke.option == OptionSide::None {
            match self.mode {
                TypingMode::Keys => false,
                TypingMode::Characters => true,
                TypingMode::Auto => self
                    .keymap
                    .as_ref()
                    .and_then(|keymap| keymap.types_same(position, stroke.shift, text))
                    .is_some_and(|same| !same),
            }
        } else {
            self.option_mode().composes(stroke.option)
        };
        if as_text {
            self.text_keys.insert(position);
            DeckTextAction::SuppressKey(index, SuppressReason::SentAsText)
        } else {
            DeckTextAction::ForwardKey(index)
        }
    }
}

/// Splits `text` into commits of at most [`MAX_TEXT_COMMIT_BYTES`], on
/// character boundaries.
#[must_use]
pub fn split_commit(text: &str) -> Vec<String> {
    super::plan::utf8_chunks(text, MAX_TEXT_COMMIT_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::HostKeyCharsMsg;

    const QUOTE: u32 = 0x27;
    const KEY_A: u32 = 0x41;

    fn stroke(position: u32, pressed: bool) -> KeyStroke {
        KeyStroke {
            position: Some(position),
            phase: if pressed {
                KeyPhase::Press
            } else {
                KeyPhase::Release
            },
            shift: false,
            command: false,
            option: OptionSide::None,
            option_key: OptionSide::None,
        }
    }

    fn host() -> HostTextTruth {
        HostTextTruth {
            text_commit: true,
            unicode: false,
        }
    }

    fn us_keymap() -> HostKeyPositions {
        HostKeyPositions::from_message(&HostKeymapMsg {
            generation: 1,
            active_layout: Some("us".to_owned()),
            keys: vec![
                HostKeyCharsMsg {
                    qt_key: QUOTE,
                    base: Some("'".to_owned()),
                    shift: Some("\"".to_owned()),
                },
                HostKeyCharsMsg {
                    qt_key: KEY_A,
                    base: Some("a".to_owned()),
                    shift: Some("A".to_owned()),
                },
            ],
            ..HostKeymapMsg::default()
        })
    }

    fn active_policy(mode: TypingMode) -> TextInputPolicy {
        let mut policy = TextInputPolicy::new(mode, OptionKeyMode::default());
        policy.set_host(host());
        policy.set_host_keymap(Some(us_keymap()));
        policy
    }

    #[test]
    fn without_host_support_every_key_goes_as_before() {
        let mut policy = TextInputPolicy::new(TypingMode::Characters, OptionKeyMode::default());
        assert!(!policy.active());
        let actions = policy.plan_frame(&[
            DeckTextEvent::Key(stroke(QUOTE, true)),
            DeckTextEvent::Text("æ".to_owned()),
            DeckTextEvent::Commit("ü".to_owned()),
            DeckTextEvent::Key(stroke(QUOTE, false)),
        ]);
        assert_eq!(
            actions,
            [DeckTextAction::ForwardKey(0), DeckTextAction::ForwardKey(3)]
        );
        let mut keys = active_policy(TypingMode::Keys);
        assert!(!keys.active());
        assert_eq!(
            keys.plan_frame(&[DeckTextEvent::Key(stroke(QUOTE, true))]),
            [DeckTextAction::ForwardKey(0)]
        );
    }

    #[test]
    fn auto_sends_a_position_the_host_types_differently_as_text() {
        let mut policy = active_policy(TypingMode::Auto);
        let actions = policy.plan_frame(&[
            DeckTextEvent::Key(stroke(QUOTE, true)),
            DeckTextEvent::Text("æ".to_owned()),
            DeckTextEvent::Key(stroke(KEY_A, true)),
            DeckTextEvent::Text("a".to_owned()),
        ]);
        assert_eq!(
            actions,
            [
                DeckTextAction::SuppressKey(0, SuppressReason::SentAsText),
                DeckTextAction::SendText("æ".to_owned()),
                DeckTextAction::ForwardKey(2),
            ]
        );
        // The text keystroke's release never reaches the host; the plain
        // one's does.
        let release = policy.plan_frame(&[
            DeckTextEvent::Key(stroke(QUOTE, false)),
            DeckTextEvent::Key(stroke(KEY_A, false)),
        ]);
        assert_eq!(
            release,
            [
                DeckTextAction::SuppressKey(0, SuppressReason::TextKeystroke),
                DeckTextAction::ForwardKey(1),
            ]
        );
    }

    #[test]
    fn auto_without_a_host_layout_keeps_positions() {
        let mut policy = active_policy(TypingMode::Auto);
        policy.set_host_keymap(None);
        assert_eq!(
            policy.plan_frame(&[
                DeckTextEvent::Key(stroke(QUOTE, true)),
                DeckTextEvent::Text("æ".to_owned()),
            ]),
            [DeckTextAction::ForwardKey(0)]
        );
    }

    #[test]
    fn repeats_of_a_text_keystroke_repeat_its_text() {
        let mut policy = active_policy(TypingMode::Characters);
        policy.plan_frame(&[
            DeckTextEvent::Key(stroke(KEY_A, true)),
            DeckTextEvent::Text("a".to_owned()),
        ]);
        let repeat = KeyStroke {
            phase: KeyPhase::Repeat,
            ..stroke(KEY_A, true)
        };
        assert_eq!(
            policy.plan_frame(&[
                DeckTextEvent::Key(repeat),
                DeckTextEvent::Text("a".to_owned()),
            ]),
            [
                DeckTextAction::SuppressKey(0, SuppressReason::SentAsText),
                DeckTextAction::SendText("a".to_owned()),
            ]
        );
    }

    #[test]
    fn commands_and_composition_follow_their_rules() {
        let mut policy = active_policy(TypingMode::Characters);
        let command = KeyStroke {
            command: true,
            ..stroke(KEY_A, true)
        };
        assert_eq!(
            policy.plan_frame(&[DeckTextEvent::Key(command)]),
            [DeckTextAction::ForwardKey(0)]
        );
        // A dead key, then its result: the keystrokes never surface (the
        // input method consumed them), the commit does.
        assert_eq!(
            policy.plan_frame(&[DeckTextEvent::Preedit("¨".to_owned())]),
            [DeckTextAction::Preedit(Some("¨".to_owned()))]
        );
        assert!(policy.composing());
        assert_eq!(
            policy.plan_frame(&[
                DeckTextEvent::Preedit(String::new()),
                DeckTextEvent::Commit("ü".to_owned()),
            ]),
            [
                DeckTextAction::Preedit(None),
                DeckTextAction::SendText("ü".to_owned()),
            ]
        );
        assert!(!policy.composing());
    }

    #[test]
    fn option_keys_follow_their_side() {
        let mut policy = active_policy(TypingMode::Auto);
        let left = KeyStroke {
            option: OptionSide::Left,
            ..stroke(KEY_A, true)
        };
        let right = KeyStroke {
            option: OptionSide::Right,
            ..stroke(QUOTE, true)
        };
        assert_eq!(
            policy.plan_frame(&[
                DeckTextEvent::Key(left),
                DeckTextEvent::Text("a".to_owned()),
                DeckTextEvent::Key(right),
                DeckTextEvent::Text("æ".to_owned()),
            ]),
            [
                DeckTextAction::ForwardKey(0),
                DeckTextAction::SuppressKey(2, SuppressReason::SentAsText),
                DeckTextAction::SendText("æ".to_owned()),
            ]
        );
        assert!(!OptionKeyMode::BothAlt.composes(OptionSide::Right));
        assert!(OptionKeyMode::BothCharacters.composes(OptionSide::Left));
        assert!(!OptionKeyMode::LeftAltRightCharacters.composes(OptionSide::Both));
    }

    #[test]
    fn an_automatic_option_setting_keeps_option_on_a_mac_host() {
        const ALT: u32 = 0x0100_0023;
        let right_option = |pressed| KeyStroke {
            option_key: OptionSide::Right,
            option: if pressed {
                OptionSide::Right
            } else {
                OptionSide::None
            },
            ..stroke(ALT, pressed)
        };
        let frame = [
            DeckTextEvent::Key(right_option(true)),
            DeckTextEvent::Key(right_option(false)),
            DeckTextEvent::Commit("日本".to_owned()),
        ];
        let mut mac = TextInputPolicy::new(TypingMode::Auto, OptionKeyPreference::Automatic);
        mac.set_host(host());
        mac.set_host_platform(HostPlatform::MacOs);
        assert_eq!(mac.option_mode(), OptionKeyMode::BothAlt);
        // Option reaches a Mac host as Option, as before it typed text; an
        // input method's commit still goes as text.
        assert_eq!(
            mac.plan_frame(&frame),
            [
                DeckTextAction::ForwardKey(0),
                DeckTextAction::ForwardKey(1),
                DeckTextAction::SendText("日本".to_owned()),
            ]
        );
        let mut windows = TextInputPolicy::new(TypingMode::Auto, OptionKeyPreference::Automatic);
        windows.set_host(host());
        windows.set_host_platform(HostPlatform::Windows);
        assert_eq!(windows.option_mode(), OptionKeyMode::LeftAltRightCharacters);
        assert_eq!(
            windows.plan_frame(&frame)[0],
            DeckTextAction::SuppressKey(0, SuppressReason::ComposingOption)
        );
        // A chosen mode applies to a Mac host too.
        mac.configure(TypingMode::Auto, OptionKeyMode::LeftAltRightCharacters);
        assert_eq!(mac.option_mode(), OptionKeyMode::LeftAltRightCharacters);
        // A host that has not said what it is gets the general default.
        let unknown = TextInputPolicy::new(TypingMode::Auto, OptionKeyPreference::Automatic);
        assert_eq!(unknown.option_mode(), OptionKeyMode::LeftAltRightCharacters);
    }

    #[test]
    fn option_settings_written_by_older_builds_stay_automatic_unless_chosen() {
        use OptionKeyPreference::{Automatic, Chosen};
        // Older builds saved their default with every save.
        assert_eq!(
            OptionKeyPreference::from_settings(None, Some("left_alt_right_characters")),
            Automatic
        );
        assert_eq!(OptionKeyPreference::from_settings(None, None), Automatic);
        assert_eq!(
            OptionKeyPreference::from_settings(None, Some("both_characters")),
            Chosen(OptionKeyMode::BothCharacters)
        );
        // As this build saves them: the preference, explicit default
        // included, and its resolution for older builds.
        assert_eq!(
            OptionKeyPreference::from_settings(
                Some("automatic"),
                Some("left_alt_right_characters")
            ),
            Automatic
        );
        assert_eq!(
            OptionKeyPreference::from_settings(
                Some("left_alt_right_characters"),
                Some("left_alt_right_characters")
            ),
            Chosen(OptionKeyMode::LeftAltRightCharacters)
        );
        assert_eq!(
            OptionKeyPreference::from_settings(Some("both_alt"), None),
            Chosen(OptionKeyMode::BothAlt)
        );
        // A choice made in an older build after a downgrade (it rewrites only
        // the legacy key and keeps this one) survives the upgrade, either way.
        assert_eq!(
            OptionKeyPreference::from_settings(Some("automatic"), Some("both_characters")),
            Chosen(OptionKeyMode::BothCharacters)
        );
        assert_eq!(
            OptionKeyPreference::from_settings(Some("both_alt"), Some("left_alt_right_characters")),
            Chosen(OptionKeyMode::LeftAltRightCharacters)
        );
        for preference in [
            Automatic,
            Chosen(OptionKeyMode::BothAlt),
            Chosen(OptionKeyMode::LeftAltRightCharacters),
            Chosen(OptionKeyMode::BothCharacters),
        ] {
            assert_eq!(
                OptionKeyPreference::from_token(preference.token()),
                Some(preference)
            );
        }
    }

    #[test]
    fn a_composing_option_key_is_not_alt_on_the_host() {
        const ALT: u32 = 0x0100_0023;
        const LEFT_ARROW: u32 = 0x0100_0012;
        let mut policy = active_policy(TypingMode::Auto);
        let option_key = |side, pressed| KeyStroke {
            option_key: side,
            option: if pressed { side } else { OptionSide::None },
            ..stroke(ALT, pressed)
        };
        // Right Option composes: neither its press nor its release reaches
        // the host, and a key forwarded while it is held goes without Alt.
        let arrow = KeyStroke {
            option: OptionSide::Right,
            ..stroke(LEFT_ARROW, true)
        };
        assert_eq!(
            policy.plan_frame(&[
                DeckTextEvent::Key(option_key(OptionSide::Right, true)),
                DeckTextEvent::Key(arrow),
                DeckTextEvent::Key(option_key(OptionSide::Right, false)),
            ]),
            [
                DeckTextAction::SuppressKey(0, SuppressReason::ComposingOption),
                DeckTextAction::ForwardKeyWithoutAlt(1),
                DeckTextAction::SuppressKey(2, SuppressReason::ComposingOption),
            ]
        );
        // Left Option is Alt: sent, and keys keep Alt.
        let shortcut = KeyStroke {
            option: OptionSide::Left,
            ..stroke(LEFT_ARROW, true)
        };
        assert_eq!(
            policy.plan_frame(&[
                DeckTextEvent::Key(option_key(OptionSide::Left, true)),
                DeckTextEvent::Key(shortcut),
                DeckTextEvent::Key(option_key(OptionSide::Left, false)),
            ]),
            [
                DeckTextAction::ForwardKey(0),
                DeckTextAction::ForwardKey(1),
                DeckTextAction::ForwardKey(2),
            ]
        );
        // A side's release matches its press, even across a settings change.
        policy.plan_frame(&[DeckTextEvent::Key(option_key(OptionSide::Right, true))]);
        policy.configure(TypingMode::Auto, OptionKeyMode::BothAlt);
        assert_eq!(
            policy.plan_frame(&[DeckTextEvent::Key(option_key(OptionSide::Right, false))]),
            [DeckTextAction::SuppressKey(
                0,
                SuppressReason::ComposingOption
            )]
        );
    }

    #[test]
    fn a_release_lost_during_a_composition_does_not_swallow_the_next_one() {
        let mut policy = active_policy(TypingMode::Characters);
        // 'a' is sent as text; a dead key starts composing; the input
        // method swallows 'a''s release.
        policy.plan_frame(&[
            DeckTextEvent::Key(stroke(KEY_A, true)),
            DeckTextEvent::Text("a".to_owned()),
            DeckTextEvent::Preedit("¨".to_owned()),
        ]);
        policy.plan_frame(&[
            DeckTextEvent::Preedit(String::new()),
            DeckTextEvent::Commit("ü".to_owned()),
        ]);
        // Command+A goes to the host as a key: its release must too.
        let shortcut = KeyStroke {
            command: true,
            ..stroke(KEY_A, true)
        };
        assert_eq!(
            policy.plan_frame(&[
                DeckTextEvent::Key(shortcut),
                DeckTextEvent::Key(stroke(KEY_A, false)),
            ]),
            [DeckTextAction::ForwardKey(0), DeckTextAction::ForwardKey(1)]
        );
    }

    #[test]
    fn focus_loss_forgets_text_keystrokes_and_the_composition() {
        let mut policy = active_policy(TypingMode::Characters);
        policy.plan_frame(&[
            DeckTextEvent::Key(stroke(KEY_A, true)),
            DeckTextEvent::Text("a".to_owned()),
            DeckTextEvent::Preedit("に".to_owned()),
        ]);
        assert_eq!(
            policy.plan_frame(&[DeckTextEvent::FocusLost]),
            [DeckTextAction::Preedit(None)]
        );
        assert_eq!(
            policy.plan_frame(&[DeckTextEvent::Key(stroke(KEY_A, false))]),
            [DeckTextAction::ForwardKey(0)]
        );
    }

    #[test]
    fn settings_tokens_round_trip() {
        for mode in [TypingMode::Keys, TypingMode::Auto, TypingMode::Characters] {
            assert_eq!(TypingMode::from_token(mode.token()), Some(mode));
        }
        for mode in [
            OptionKeyMode::BothAlt,
            OptionKeyMode::LeftAltRightCharacters,
            OptionKeyMode::BothCharacters,
        ] {
            assert_eq!(OptionKeyMode::from_token(mode.token()), Some(mode));
        }
        assert_eq!(TypingMode::default(), TypingMode::Auto);
        assert_eq!(
            OptionKeyMode::default(),
            OptionKeyMode::LeftAltRightCharacters
        );
    }

    #[test]
    fn long_commits_are_split() {
        let text = "日".repeat(400);
        let commits = split_commit(&text);
        assert!(
            commits
                .iter()
                .all(|commit| commit.len() <= MAX_TEXT_COMMIT_BYTES)
        );
        assert_eq!(commits.concat(), text);
    }

    #[test]
    fn the_keyboard_mode_overrides_the_typing_setting_only_where_it_must() {
        use TypingMode::{Auto, Characters, Keys};
        assert_eq!(KeyboardMode::Characters.typing_mode(Keys), Characters);
        assert_eq!(KeyboardMode::Characters.typing_mode(Auto), Characters);
        assert_eq!(KeyboardMode::Physical.typing_mode(Keys), Keys);
        assert_eq!(KeyboardMode::Physical.typing_mode(Auto), Auto);
        assert_eq!(KeyboardMode::Physical.typing_mode(Characters), Auto);
        assert_eq!(
            KeyboardMode::of_typing(Characters),
            KeyboardMode::Characters
        );
        assert_eq!(KeyboardMode::of_typing(Keys), KeyboardMode::Physical);
        for mode in KeyboardMode::ALL {
            assert_eq!(KeyboardMode::from_token(mode.token()), Some(mode));
        }
        assert_eq!(KeyboardMode::from_token("unicode"), None);
    }
}
