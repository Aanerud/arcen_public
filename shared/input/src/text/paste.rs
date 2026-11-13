// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Paste as keystrokes: the client's clipboard text typed into the host, for
//! the places clipboard sharing cannot reach (a sign-in or elevation screen,
//! a virtual machine console, a firmware-style application).
//!
//! The client turns the text into a bounded list of characters and the two
//! keys text cannot carry (Return and Tab), then types it at a steady,
//! configurable rate:
//!
//! - **Bounded.** Up to [`PasteSettings::confirm_above`] characters are typed
//!   at once; more ask the user first, and nothing beyond
//!   [`MAX_PASTE_CHARS`] is typed at all.
//! - **Paced.** Characters go one at a time, at least
//!   `1 / chars_per_second` apart, with no catch-up burst after a stall, so
//!   a slow application does not drop keys.
//! - **Cancellable** at any point, between two characters.
//! - **Never under a held modifier.** Typing waits while a modifier key is
//!   physically held, so no character becomes a shortcut.
//! - **Truthful.** Characters go as committed text, which the host types
//!   with its own keyboard layout where that layout has the character and by
//!   its text route otherwise; a host without a text route is refused rather
//!   than sent key positions a layout the client cannot see would mistype.
//!
//! The text itself is never logged by anything here; callers log counts.

use std::time::{Duration, Instant};

use arcen_protocol::keys::{KEY_RETURN, KEY_TAB};
use arcen_protocol::messages::MAX_TEXT_COMMIT_BYTES;

use super::policy::HostTextTruth;

/// Characters typed without asking first, by default.
pub const DEFAULT_CONFIRM_ABOVE_CHARS: usize = 2_000;
/// The most characters one paste types, confirmed or not.
pub const MAX_PASTE_CHARS: usize = 100_000;
/// The default typing rate, in characters per second: what Windows 11's
/// Notepad took without dropping a key in the lab (it dropped some at 50).
pub const DEFAULT_CHARS_PER_SECOND: u32 = 25;
/// The slowest configurable rate.
pub const MIN_CHARS_PER_SECOND: u32 = 5;
/// The fastest configurable rate.
pub const MAX_CHARS_PER_SECOND: u32 = 500;
/// The rates a client offers, slowest first.
pub const RATE_CHOICES: [u32; 5] = [10, 25, 50, 100, 250];
/// The confirmation thresholds a client offers.
pub const CONFIRM_CHOICES: [usize; 4] = [500, 2_000, 10_000, MAX_PASTE_CHARS];
/// Most characters in one committed-text message.
const MAX_RUN_CHARS: usize = 64;
/// The slowest display frame a client polls at: faster rates send what one
/// frame owes at once.
const FRAME: Duration = Duration::from_millis(17);

/// A key the paste presses and releases on its own: text cannot carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteKey {
    /// A line break.
    Return,
    /// A tab.
    Tab,
}

impl PasteKey {
    /// The key position sent for it.
    #[must_use]
    pub const fn qt_key(self) -> u32 {
        match self {
            Self::Return => KEY_RETURN,
            Self::Tab => KEY_TAB,
        }
    }
}

/// One thing to send, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteUnit {
    /// Committed text (one or more characters, within one message's limit).
    Text(String),
    /// A key press and release, with no modifier.
    Key(PasteKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Char(char),
    Key(PasteKey),
}

/// How pastes are typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteSettings {
    /// Pastes longer than this many characters ask first.
    pub confirm_above: usize,
    /// The typing rate.
    pub chars_per_second: u32,
}

impl Default for PasteSettings {
    fn default() -> Self {
        Self {
            confirm_above: DEFAULT_CONFIRM_ABOVE_CHARS,
            chars_per_second: DEFAULT_CHARS_PER_SECOND,
        }
    }
}

impl PasteSettings {
    /// The settings within their limits.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            confirm_above: self.confirm_above.clamp(1, MAX_PASTE_CHARS),
            chars_per_second: self
                .chars_per_second
                .clamp(MIN_CHARS_PER_SECOND, MAX_CHARS_PER_SECOND),
        }
    }
}

/// Why a paste is not typed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteRefusal {
    /// Nothing typeable.
    Empty,
    /// More than [`MAX_PASTE_CHARS`].
    TooLong {
        /// Its length, in characters.
        characters: usize,
    },
    /// The host types no committed text, so characters cannot be typed
    /// without guessing its keyboard layout.
    NoTextRoute,
}

impl PasteRefusal {
    /// What the user is told.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Empty => "The clipboard holds no text to type.".to_owned(),
            Self::TooLong { characters } => format!(
                "The clipboard holds {characters} characters; Paste as Keystrokes types at most \
                 {MAX_PASTE_CHARS}."
            ),
            Self::NoTextRoute => "This host cannot type text sent from the Deck, so Paste as \
                 Keystrokes is not available. Sending key positions instead could type \
                 different characters on its keyboard layout."
                .to_owned(),
        }
    }
}

/// What preparing a paste left out, and why.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PasteSummary {
    /// Characters and keys to type.
    pub items: usize,
    /// Of those, Return and Tab keys.
    pub keys: usize,
    /// Control and noncharacter code points left out.
    pub dropped: usize,
    /// Line breaks at the very end left out, so a copied password never
    /// submits a form by itself.
    pub trailing_breaks: usize,
}

/// Clipboard text made ready to type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteText {
    items: Vec<Item>,
    summary: PasteSummary,
}

impl PasteText {
    /// What it holds.
    #[must_use]
    pub const fn summary(&self) -> PasteSummary {
        self.summary
    }

    /// Whether the user is asked before it is typed.
    #[must_use]
    pub fn needs_confirmation(&self, settings: PasteSettings) -> bool {
        self.items.len() > settings.clamped().confirm_above
    }

    /// How long typing it takes at `settings`' rate.
    #[must_use]
    pub fn duration(&self, settings: PasteSettings) -> Duration {
        let rate = settings.clamped().chars_per_second;
        typing_time(self.items.len(), rate)
    }
}

impl Drop for PasteText {
    fn drop(&mut self) {
        wipe(&mut self.items);
    }
}

/// Overwrites the characters (they may be a password) before the memory is
/// freed, as far as safe code can.
fn wipe(items: &mut [Item]) {
    items.fill(Item::Char('\0'));
    std::hint::black_box(items);
}

/// Prepares `text` for a host with `host`'s text routes.
///
/// Line breaks (`\r\n`, `\r`, `\n`, U+0085, U+2028, U+2029) become Return,
/// tabs Tab; other control characters, noncharacters and a leading byte
/// order mark are left out, and so are line breaks at the very end.
///
/// # Errors
///
/// [`PasteRefusal`] when nothing is left, the text is longer than
/// [`MAX_PASTE_CHARS`], or it has characters and the host types no text.
pub fn prepare_paste(text: &str, host: HostTextTruth) -> Result<PasteText, PasteRefusal> {
    let mut items = Vec::new();
    let mut summary = PasteSummary::default();
    let mut chars = text.chars().peekable();
    let mut truncated = false;
    if chars.peek() == Some(&'\u{FEFF}') {
        chars.next();
        summary.dropped += 1;
    }
    while let Some(character) = chars.next() {
        if items.len() > MAX_PASTE_CHARS {
            truncated = true;
            break;
        }
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                items.push(Item::Key(PasteKey::Return));
            }
            '\n' | '\u{85}' | '\u{2028}' | '\u{2029}' => items.push(Item::Key(PasteKey::Return)),
            '\t' => items.push(Item::Key(PasteKey::Tab)),
            _ if typeable(character) => items.push(Item::Char(character)),
            _ => summary.dropped += 1,
        }
    }
    while items.last() == Some(&Item::Key(PasteKey::Return)) {
        items.pop();
        summary.trailing_breaks += 1;
    }
    if truncated || items.len() > MAX_PASTE_CHARS {
        return Err(PasteRefusal::TooLong {
            characters: text.chars().count(),
        });
    }
    if items.is_empty() {
        return Err(PasteRefusal::Empty);
    }
    if !host.text_commit && items.iter().any(|item| matches!(item, Item::Char(_))) {
        return Err(PasteRefusal::NoTextRoute);
    }
    summary.items = items.len();
    summary.keys = items
        .iter()
        .filter(|item| matches!(item, Item::Key(_)))
        .count();
    Ok(PasteText { items, summary })
}

/// How long `items` take at `rate` per second (a paste holds at most
/// [`MAX_PASTE_CHARS`] items, well within `u32`).
fn typing_time(items: usize, rate: u32) -> Duration {
    let items = u32::try_from(items).unwrap_or(u32::MAX);
    Duration::from_secs_f64(f64::from(items) / f64::from(rate))
}

/// Whether a committed-text message may carry `character`.
fn typeable(character: char) -> bool {
    let code = u32::from(character);
    !character.is_control() && !(0xFDD0..=0xFDEF).contains(&code) && code & 0xFFFE != 0xFFFE
}

/// Where a paste is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PastePhase {
    /// Waiting for every modifier key to be let go.
    WaitingForRelease,
    /// Typing.
    Typing,
    /// Everything typed.
    Finished,
    /// Stopped by the user (or the session) before the end.
    Cancelled,
}

/// What to send now, and when to look again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PastePoll {
    /// Units due now, in order.
    pub units: Vec<PasteUnit>,
    /// When the next unit is due, while the paste goes on.
    pub wake_after: Option<Duration>,
}

/// A paste being typed.
#[derive(Debug, Clone)]
pub struct PasteTyping {
    items: Vec<Item>,
    next: usize,
    rate: u32,
    phase: PastePhase,
    tokens: f64,
    last_poll: Option<Instant>,
}

impl PasteTyping {
    /// Starts typing `text` at `settings`' rate. Nothing is typed until a
    /// poll finds no modifier held.
    #[must_use]
    pub fn new(mut text: PasteText, settings: PasteSettings) -> Self {
        Self {
            items: std::mem::take(&mut text.items),
            next: 0,
            rate: settings.clamped().chars_per_second,
            phase: PastePhase::WaitingForRelease,
            tokens: 0.0,
            last_poll: None,
        }
    }

    /// Where the paste is.
    #[must_use]
    pub const fn phase(&self) -> PastePhase {
        self.phase
    }

    /// Whether it is still going on.
    #[must_use]
    pub const fn active(&self) -> bool {
        matches!(
            self.phase,
            PastePhase::WaitingForRelease | PastePhase::Typing
        )
    }

    /// Items typed and items in all.
    #[must_use]
    pub fn progress(&self) -> (usize, usize) {
        (self.next, self.items.len())
    }

    /// The time the rest takes.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        typing_time(self.items.len() - self.next, self.rate)
    }

    /// Stops between two characters; nothing more is typed.
    pub fn cancel(&mut self) {
        if self.active() {
            self.phase = PastePhase::Cancelled;
        }
    }

    /// The units due at `now`. `modifiers_held` is whether any modifier key
    /// is physically held: typing waits until none is, and starts again at
    /// its rate without catching up.
    pub fn poll(&mut self, now: Instant, modifiers_held: bool) -> PastePoll {
        if !self.active() {
            return PastePoll::default();
        }
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.rate));
        if modifiers_held {
            self.phase = PastePhase::WaitingForRelease;
            self.tokens = 0.0;
            self.last_poll = None;
            return PastePoll {
                units: Vec::new(),
                wake_after: Some(interval.max(Duration::from_millis(20))),
            };
        }
        if self.phase == PastePhase::WaitingForRelease {
            self.phase = PastePhase::Typing;
            // The first item goes at once.
            self.tokens = 1.0;
        } else if let Some(last) = self.last_poll {
            // At most one display frame's worth is ever banked (one
            // character at the default rate): a stall never turns into a
            // burst, and at rates a client's frame rate can pace, two
            // characters are never closer together than the rate allows.
            let elapsed = now.saturating_duration_since(last).as_secs_f64();
            let bank = (FRAME.as_secs_f64() * f64::from(self.rate)).max(1.0);
            self.tokens = (self.tokens + elapsed * f64::from(self.rate)).min(bank);
        }
        self.last_poll = Some(now);
        let mut units: Vec<PasteUnit> = Vec::new();
        while self.tokens >= 1.0 && self.next < self.items.len() {
            self.tokens -= 1.0;
            match self.items[self.next] {
                Item::Key(key) => units.push(PasteUnit::Key(key)),
                Item::Char(character) => match units.last_mut() {
                    Some(PasteUnit::Text(run))
                        if run.chars().count() < MAX_RUN_CHARS
                            && run.len() + character.len_utf8() <= MAX_TEXT_COMMIT_BYTES =>
                    {
                        run.push(character);
                    }
                    _ => units.push(PasteUnit::Text(character.to_string())),
                },
            }
            self.next += 1;
        }
        if self.next == self.items.len() {
            self.phase = PastePhase::Finished;
            return PastePoll {
                units,
                wake_after: None,
            };
        }
        let missing = (1.0 - self.tokens).max(0.0);
        PastePoll {
            units,
            wake_after: Some(Duration::from_secs_f64(missing / f64::from(self.rate))),
        }
    }
}

impl Drop for PasteTyping {
    fn drop(&mut self) {
        wipe(&mut self.items);
    }
}

/// The layout levels a host may type a character at by key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutLevel {
    /// No modifier.
    Base,
    /// Shift.
    Shift,
    /// `AltGr` (Control+Alt on Windows).
    AltGr,
    /// Shift and `AltGr`.
    ShiftAltGr,
    /// Some other combination (Control or Alt alone, a kana shift): never
    /// typed by key, since it is a shortcut rather than a character.
    Other,
}

impl LayoutLevel {
    const fn modifiers(self) -> Option<(bool, bool)> {
        match self {
            Self::Base => Some((false, false)),
            Self::Shift => Some((true, false)),
            Self::AltGr => Some((false, true)),
            Self::ShiftAltGr => Some((true, true)),
            Self::Other => None,
        }
    }
}

/// What a host learnt about one character from its own keyboard layout,
/// right before typing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutProbe {
    /// The level the layout types the character at.
    pub level: LayoutLevel,
    /// What pressing that key at that level types now, with the session's
    /// lock keys as they are; `None` for a dead key or nothing.
    pub types: Option<char>,
}

/// How a host types one committed character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharRoute {
    /// By its key on the host's own layout, with the level's modifiers
    /// pressed around it and released after it.
    Key {
        /// Shift is pressed around the key.
        shift: bool,
        /// `AltGr` (Control+Alt on Windows) is pressed around the key.
        altgr: bool,
    },
    /// By the host's text route (Unicode injection or an input method).
    Text,
}

/// Decides how a host types `character`: by key only when its own layout
/// provably types exactly that character (at the base, Shift, `AltGr` or
/// Shift+`AltGr` level, with the lock keys as they are), no key is held on
/// the host (a held modifier would turn the key into a shortcut, and Shift
/// around it would release the user's own), and the layout is not one an
/// input method converts keys for. Everything else goes by the host's text
/// route, which never depends on the layout.
#[must_use]
pub fn route_char(
    character: char,
    probe: Option<LayoutProbe>,
    host_keys_held: bool,
    input_method_layout: bool,
) -> CharRoute {
    if host_keys_held || input_method_layout {
        return CharRoute::Text;
    }
    match probe {
        Some(LayoutProbe {
            level,
            types: Some(typed),
        }) if typed == character && !character.is_control() => match level.modifiers() {
            Some((shift, altgr)) => CharRoute::Key { shift, altgr },
            None => CharRoute::Text,
        },
        _ => CharRoute::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT_HOST: HostTextTruth = HostTextTruth {
        text_commit: true,
        unicode: true,
    };

    fn type_all(typing: &mut PasteTyping, start: Instant) -> Vec<PasteUnit> {
        let mut units = Vec::new();
        let mut now = start;
        for _ in 0..100_000 {
            let poll = typing.poll(now, false);
            units.extend(poll.units);
            let Some(after) = poll.wake_after else {
                break;
            };
            now += after.max(Duration::from_millis(1));
        }
        units
    }

    #[test]
    fn line_breaks_and_tabs_are_keys_and_trailing_breaks_are_not_typed() {
        let text = prepare_paste("a\r\nb\rc\nd\te\u{2028}f\n\r\n", TEXT_HOST).unwrap();
        let summary = text.summary();
        assert_eq!(summary.trailing_breaks, 2);
        assert_eq!(summary.keys, 5);
        let mut typing = PasteTyping::new(
            text,
            PasteSettings {
                chars_per_second: 500,
                ..PasteSettings::default()
            },
        );
        let units = type_all(&mut typing, Instant::now());
        let flat: Vec<String> = units
            .iter()
            .map(|unit| match unit {
                PasteUnit::Text(text) => text.clone(),
                PasteUnit::Key(PasteKey::Return) => "⏎".to_owned(),
                PasteUnit::Key(PasteKey::Tab) => "⇥".to_owned(),
            })
            .collect();
        assert_eq!(flat.concat(), "a⏎b⏎c⏎d⇥e⏎f");
        assert_eq!(typing.phase(), PastePhase::Finished);
    }

    #[test]
    fn control_characters_and_a_byte_order_mark_are_left_out() {
        let text = prepare_paste("\u{FEFF}p\u{7}w\u{FFFF}\u{1b}d", TEXT_HOST).unwrap();
        assert_eq!(text.summary().dropped, 4);
        assert_eq!(text.summary().items, 3);
    }

    #[test]
    fn a_host_without_text_types_line_breaks_only() {
        let none = HostTextTruth::default();
        assert_eq!(
            prepare_paste("pa$$word", none),
            Err(PasteRefusal::NoTextRoute)
        );
        assert!(prepare_paste("\t\t\nx", TEXT_HOST).is_ok());
        assert_eq!(prepare_paste("\t\n\t", none).unwrap().summary().keys, 3);
        assert_eq!(prepare_paste("\n\r\n", TEXT_HOST), Err(PasteRefusal::Empty));
        assert_eq!(prepare_paste("", TEXT_HOST), Err(PasteRefusal::Empty));
    }

    #[test]
    fn long_pastes_ask_first_and_the_ceiling_is_absolute() {
        let settings = PasteSettings::default();
        let short = prepare_paste(&"x".repeat(2_000), TEXT_HOST).unwrap();
        assert!(!short.needs_confirmation(settings));
        let long = prepare_paste(&"x".repeat(2_001), TEXT_HOST).unwrap();
        assert!(long.needs_confirmation(settings));
        assert_eq!(long.duration(settings), Duration::from_secs_f64(80.04));
        assert_eq!(
            prepare_paste(&"x".repeat(MAX_PASTE_CHARS + 1), TEXT_HOST),
            Err(PasteRefusal::TooLong {
                characters: MAX_PASTE_CHARS + 1
            })
        );
        assert!(prepare_paste(&"x".repeat(MAX_PASTE_CHARS), TEXT_HOST).is_ok());
    }

    #[test]
    fn typing_never_exceeds_its_rate_even_after_a_stall() {
        let text = prepare_paste(&"y".repeat(400), TEXT_HOST).unwrap();
        let mut typing = PasteTyping::new(
            text,
            PasteSettings {
                chars_per_second: 100,
                ..PasteSettings::default()
            },
        );
        let start = Instant::now();
        let first = typing.poll(start, false);
        assert_eq!(first.units, vec![PasteUnit::Text("y".to_owned())]);
        // A two-second stall banks one character, never a burst.
        let after_stall = typing.poll(start + Duration::from_secs(2), false);
        let typed: usize = after_stall
            .units
            .iter()
            .map(|unit| match unit {
                PasteUnit::Text(text) => text.chars().count(),
                PasteUnit::Key(_) => 1,
            })
            .sum();
        assert_eq!(typed, 1, "typed {typed} at once after a stall");
        // Polling every millisecond for one second types about 100.
        let mut now = start + Duration::from_secs(2);
        let before = typing.progress().0;
        for _ in 0..1000 {
            now += Duration::from_millis(1);
            let _ = typing.poll(now, false);
        }
        let in_one_second = typing.progress().0 - before;
        assert!((99..=101).contains(&in_one_second), "{in_one_second}");
    }

    #[test]
    fn a_held_modifier_holds_typing_and_cancel_stops_it() {
        let text = prepare_paste("secret", TEXT_HOST).unwrap();
        let mut typing = PasteTyping::new(text, PasteSettings::default());
        let start = Instant::now();
        let held = typing.poll(start, true);
        assert!(held.units.is_empty());
        assert_eq!(typing.phase(), PastePhase::WaitingForRelease);
        assert!(held.wake_after.is_some());
        let first = typing.poll(start + Duration::from_millis(30), false);
        assert_eq!(first.units.len(), 1);
        assert_eq!(typing.phase(), PastePhase::Typing);
        // Held again mid-paste: nothing until released, no catch-up after.
        assert!(
            typing
                .poll(start + Duration::from_secs(1), true)
                .units
                .is_empty()
        );
        let resumed = typing.poll(start + Duration::from_secs(3), false);
        assert_eq!(resumed.units, vec![PasteUnit::Text("e".to_owned())]);
        typing.cancel();
        assert_eq!(typing.phase(), PastePhase::Cancelled);
        assert!(
            typing
                .poll(start + Duration::from_secs(9), false)
                .units
                .is_empty()
        );
        assert_eq!(typing.progress(), (2, 6));
    }

    #[test]
    fn runs_respect_the_message_limits() {
        let text = prepare_paste(&"é".repeat(600), TEXT_HOST).unwrap();
        let mut typing = PasteTyping::new(
            text,
            PasteSettings {
                chars_per_second: MAX_CHARS_PER_SECOND,
                ..PasteSettings::default()
            },
        );
        for unit in type_all(&mut typing, Instant::now()) {
            let PasteUnit::Text(run) = unit else {
                panic!("no keys expected");
            };
            assert!(run.chars().count() <= MAX_RUN_CHARS);
            assert!(arcen_protocol::messages::validate_commit_text(&run).is_ok());
        }
        assert_eq!(typing.progress(), (600, 600));
    }

    #[test]
    fn settings_are_clamped() {
        let clamped = PasteSettings {
            confirm_above: 0,
            chars_per_second: 1,
        }
        .clamped();
        assert_eq!(clamped.confirm_above, 1);
        assert_eq!(clamped.chars_per_second, MIN_CHARS_PER_SECOND);
        assert_eq!(
            PasteSettings {
                confirm_above: usize::MAX,
                chars_per_second: u32::MAX,
            }
            .clamped(),
            PasteSettings {
                confirm_above: MAX_PASTE_CHARS,
                chars_per_second: MAX_CHARS_PER_SECOND,
            }
        );
    }

    #[test]
    fn a_character_goes_by_key_only_when_the_layout_proves_it() {
        let base = |types| {
            Some(LayoutProbe {
                level: LayoutLevel::Base,
                types,
            })
        };
        assert_eq!(
            route_char('a', base(Some('a')), false, false),
            CharRoute::Key {
                shift: false,
                altgr: false
            }
        );
        assert_eq!(
            route_char(
                'A',
                Some(LayoutProbe {
                    level: LayoutLevel::Shift,
                    types: Some('A'),
                }),
                false,
                false
            ),
            CharRoute::Key {
                shift: true,
                altgr: false
            }
        );
        // Caps Lock on: the key types 'A', not 'a'.
        assert_eq!(
            route_char('a', base(Some('A')), false, false),
            CharRoute::Text
        );
        // AltGr on a layout that has it (nb-NO '@').
        assert_eq!(
            route_char(
                '@',
                Some(LayoutProbe {
                    level: LayoutLevel::AltGr,
                    types: Some('@'),
                }),
                false,
                false
            ),
            CharRoute::Key {
                shift: false,
                altgr: true
            }
        );
        // A dead key, Control or Alt alone, an absent character, held
        // keys, an IME layout.
        assert_eq!(route_char('^', base(None), false, false), CharRoute::Text);
        assert_eq!(
            route_char(
                'x',
                Some(LayoutProbe {
                    level: LayoutLevel::Other,
                    types: Some('x'),
                }),
                false,
                false
            ),
            CharRoute::Text
        );
        assert_eq!(route_char('æ', None, false, false), CharRoute::Text);
        assert_eq!(
            route_char('a', base(Some('a')), true, false),
            CharRoute::Text
        );
        assert_eq!(
            route_char('a', base(Some('a')), false, true),
            CharRoute::Text
        );
    }
}
