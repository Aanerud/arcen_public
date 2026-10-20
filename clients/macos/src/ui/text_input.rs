//! Committed text from the Deck: the macOS input method's commits, and
//! keystrokes the host's layout would type differently, sent as
//! `text_commit`; everything else stays a key position.
//!
//! The decisions are the shared [`arcen_input::text::TextInputPolicy`];
//! this module maps egui's events onto it, enables the input method only
//! while a host types committed text, shows the composition locally (the
//! host never sees partial text), and tells the user what a host could not
//! type.

use std::time::{Duration, Instant};

use arcen_input::text::{
    split_commit, DeckTextAction, DeckTextEvent, HostKeyPositions, HostTextTruth, KeyPhase,
    KeyStroke, OptionKeyMode, OptionSide, ReleaseRecovery, SentPress, TextInputPolicy, TypingMode,
    LOST_RELEASE_GRACE,
};
use arcen_protocol::messages::{
    supports_text_commit_v1, HostKeymapMsg, InputCapabilitiesMsg, InputCapabilityAvailability,
    TextCommitResultMsg, TextRefusalReason,
};

use super::keyboard::egui_key_to_qt;

/// How long a refusal notice stays on screen.
const NOTICE_DURATION: Duration = Duration::from_secs(5);
/// Most refused characters a notice quotes.
const NOTICE_CHARS: usize = 16;

/// What to do with one frame's keyboard events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedInput {
    /// Process the event at this index as a key position, as before.
    Key(usize),
    /// Process the event at this index as a key position without Alt: only
    /// Option keys that compose characters are held.
    KeyWithoutAlt(usize),
    /// Send this committed text.
    Text(String),
}

/// The Deck's committed-text state for the session.
#[derive(Debug)]
pub struct DeckTextInput {
    policy: TextInputPolicy,
    preedit: Option<String>,
    notice: Option<(Instant, String)>,
    /// The key presses sent to the host and not yet released.
    recovery: ReleaseRecovery,
    /// The wire modifiers of the latest key event sent.
    wire_modifiers: u32,
}

impl Default for DeckTextInput {
    fn default() -> Self {
        Self {
            policy: TextInputPolicy::default(),
            preedit: None,
            notice: None,
            recovery: ReleaseRecovery::new(
                LOST_RELEASE_GRACE,
                u32::from(crate::protocol::keymap::MODIFIER_BIT_KEYPAD),
            ),
            wire_modifiers: 0,
        }
    }
}

impl DeckTextInput {
    /// Applies the user's settings.
    pub fn configure(&mut self, mode: TypingMode, option: OptionKeyMode) {
        self.policy.configure(mode, option);
        self.sync_option_keys();
    }

    /// The host's truth from its hello.
    pub fn set_host(&mut self, capabilities: InputCapabilitiesMsg) {
        self.policy.set_host(HostTextTruth {
            text_commit: supports_text_commit_v1(capabilities),
            unicode: capabilities.text_commit_unicode == InputCapabilityAvailability::Available,
        });
        self.preedit = None;
        self.recovery.clear();
        self.sync_option_keys();
        tracing::info!(
            target: crate::logging::target::INPUT,
            host_text_commit = ?capabilities.text_commit,
            host_text_unicode = ?capabilities.text_commit_unicode,
            mode = self.policy.mode().token(),
            option = self.policy.option_mode().token(),
            active = self.policy.active(),
            "committed text negotiated"
        );
    }

    /// Forgets the host (disconnect).
    pub fn reset(&mut self) {
        self.set_host(InputCapabilitiesMsg::default());
        self.notice = None;
    }

    /// Whether committed text is in use, and so the input method may
    /// compose.
    #[must_use]
    pub const fn active(&self) -> bool {
        self.policy.active()
    }

    fn sync_option_keys(&self) {
        let active = self.policy.active();
        let option = self.policy.option_mode();
        super::option_keys::set_alt_sides(
            active && !option.composes(arcen_input::text::OptionSide::Left),
            active && !option.composes(arcen_input::text::OptionSide::Right),
        );
    }

    /// The host's layout notice.
    pub fn on_host_keymap(&mut self, message: &HostKeymapMsg) {
        tracing::info!(
            target: crate::logging::target::INPUT,
            generation = message.generation,
            layouts = ?message.layouts,
            active_layout = ?message.active_layout,
            positions = message.keys.len(),
            "host keyboard layout received"
        );
        self.policy
            .set_host_keymap(Some(HostKeyPositions::from_message(message)));
    }

    /// A host's report on one commit.
    pub fn on_result(&mut self, result: &TextCommitResultMsg) {
        tracing::info!(
            target: crate::logging::target::INPUT,
            sequence = result.sequence,
            delivered = result.delivered_chars,
            refused = result.refused_chars,
            reason = result.reason.token(),
            unverified = result.unverified,
            "host reported committed text"
        );
        if let Some(text) = notice_text(result) {
            self.notice = Some((Instant::now(), text));
        }
    }

    /// Keyboard focus left the session. Every held key has just been
    /// released on the host, so no release is left to recover.
    pub fn focus_lost(&mut self) {
        let _ = self.policy.plan_frame(&[DeckTextEvent::FocusLost]);
        self.preedit = None;
        self.recovery.clear();
    }

    /// The client sent a key event to the host.
    pub fn key_sent(&mut self, qt_key: u32, pressed: bool, modifiers: u32, key_code: Option<u16>) {
        self.wire_modifiers = modifiers;
        if pressed {
            self.recovery.pressed(SentPress {
                key: qt_key,
                modifiers,
                key_code,
            });
        } else {
            self.recovery.released(qt_key);
        }
    }

    /// The releases to send for keys whose release never arrived, as
    /// `(qt_key, wire modifiers)`: while the input method composes, AppKit's
    /// text system swallows key releases (winit sends none outside its
    /// ground state), so a key pressed before a dead key and released during
    /// the composition would stay held on the host and repeat. A key is
    /// released once `is_down` (the keyboard's own state, by virtual key
    /// code) has said it is up for [`LOST_RELEASE_GRACE`], so a release
    /// still queued is never sent twice; it goes as its press went (a
    /// keypad key stays a keypad key).
    pub fn lost_releases(
        &mut self,
        now: Instant,
        is_down: impl Fn(u16) -> bool,
    ) -> Vec<(u32, u32)> {
        if !self.policy.active() {
            return Vec::new();
        }
        self.recovery.lost(now, self.wire_modifiers, is_down)
    }

    /// When to look again for lost releases, if a key is waiting out its
    /// grace.
    #[must_use]
    pub fn release_check_after(&self) -> Option<Duration> {
        (self.policy.active() && self.recovery.waiting()).then(|| self.recovery.grace())
    }

    /// Routes a frame's events: which keys go as positions, which text is
    /// committed.
    pub fn route(&mut self, events: &[egui::Event]) -> Vec<RoutedInput> {
        let mut indices = Vec::with_capacity(events.len());
        let mut mapped = Vec::with_capacity(events.len());
        for (index, event) in events.iter().enumerate() {
            let converted = match event {
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    modifiers,
                } => Some(DeckTextEvent::Key({
                    let physical = physical_key.unwrap_or(*key);
                    KeyStroke {
                        position: egui_key_to_qt(physical),
                        phase: match (*pressed, *repeat) {
                            (false, _) => KeyPhase::Release,
                            (true, true) => KeyPhase::Repeat,
                            (true, false) => KeyPhase::Press,
                        },
                        shift: modifiers.shift,
                        command: modifiers.mac_cmd || modifiers.ctrl || modifiers.command,
                        option: super::option_keys::held_side(modifiers.alt),
                        option_key: match physical {
                            egui::Key::AltLeft => OptionSide::Left,
                            egui::Key::AltRight => OptionSide::Right,
                            _ => OptionSide::None,
                        },
                    }
                })),
                egui::Event::Text(text) => Some(DeckTextEvent::Text(text.clone())),
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                    Some(DeckTextEvent::Preedit(text.clone()))
                }
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                    Some(DeckTextEvent::Commit(text.clone()))
                }
                _ => None,
            };
            if let Some(converted) = converted {
                indices.push(index);
                mapped.push(converted);
            }
        }
        let mut routed = Vec::new();
        for action in self.policy.plan_frame(&mapped) {
            match action {
                DeckTextAction::ForwardKey(at) => routed.push(RoutedInput::Key(indices[at])),
                DeckTextAction::ForwardKeyWithoutAlt(at) => {
                    routed.push(RoutedInput::KeyWithoutAlt(indices[at]));
                }
                DeckTextAction::SuppressKey(at, reason) => {
                    tracing::trace!(
                        target: crate::logging::target::INPUT,
                        index = indices[at],
                        ?reason,
                        "keystroke sent as committed text"
                    );
                }
                DeckTextAction::SendText(text) => {
                    routed.extend(split_commit(&text).into_iter().map(RoutedInput::Text));
                }
                DeckTextAction::Preedit(text) => self.preedit = text,
            }
        }
        routed
    }

    /// The input method's place on screen while committed text is in use:
    /// the bottom centre of the video (the host's caret is not known).
    #[must_use]
    pub fn ime_output(&self, video: egui::Rect) -> Option<egui::output::IMEOutput> {
        if !self.policy.active() {
            return None;
        }
        let anchor = egui::pos2(video.center().x, video.bottom() - 48.0);
        Some(egui::output::IMEOutput {
            rect: video,
            cursor_rect: egui::Rect::from_min_size(anchor, egui::vec2(2.0, 20.0)),
            should_interrupt_composition: false,
        })
    }

    /// Draws the composition and the latest refusal notice over the video.
    pub fn show(&mut self, ctx: &egui::Context, video: egui::Rect) {
        if let Some(preedit) = self.preedit.as_deref().filter(|text| !text.is_empty()) {
            egui::Area::new(egui::Id::new("arcen-text-preedit"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(video.center().x - 120.0, video.bottom() - 72.0))
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_black_alpha(220))
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(preedit)
                                    .size(18.0)
                                    .underline()
                                    .color(egui::Color32::WHITE),
                            );
                        });
                });
        }
        if let Some((shown_at, text)) = &self.notice {
            if shown_at.elapsed() > NOTICE_DURATION {
                self.notice = None;
                return;
            }
            egui::Area::new(egui::Id::new("arcen-text-notice"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 48.0))
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_black_alpha(210))
                        .corner_radius(10.0)
                        .inner_margin(egui::Margin::symmetric(16, 10))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(text)
                                    .size(14.0)
                                    .color(egui::Color32::WHITE),
                            );
                        });
                });
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// What the user is told about a host's report; `None` when nothing went
/// wrong.
#[must_use]
pub fn notice_text(result: &TextCommitResultMsg) -> Option<String> {
    if result.refused_chars == 0 && !result.unverified {
        return None;
    }
    if result.refused_chars == 0 {
        return Some(
            "Typed without checking the remote keyboard layout; some characters may be missing."
                .to_owned(),
        );
    }
    let mut quoted: String = result.refused_text.chars().take(NOTICE_CHARS).collect();
    if result.refused_text.chars().count() > NOTICE_CHARS
        || usize::try_from(result.refused_chars).unwrap_or(usize::MAX)
            > result.refused_text.chars().count()
    {
        quoted.push('…');
    }
    let why = match result.reason {
        TextRefusalReason::NotInSessionLayout => "the remote keyboard layout has no key for it",
        TextRefusalReason::CompositorLacksTextInput => {
            "the remote keyboard layout has no key for it, and the remote desktop cannot take other characters yet"
        }
        TextRefusalReason::KeymapUnknown => "the remote keyboard layout could not be read",
        TextRefusalReason::InvalidText => "it is not typeable text",
        TextRefusalReason::InputUnavailable => "the remote input connection was interrupted",
        TextRefusalReason::HostUnsupported => "this host does not type text",
        TextRefusalReason::RateLimited => "it was typed faster than the host accepts",
        TextRefusalReason::Unknown => "the host could not type it",
    };
    let count = result.refused_chars;
    let noun = if count == 1 {
        "character"
    } else {
        "characters"
    };
    Some(format!("{count} {noun} not typed (“{quoted}”): {why}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> InputCapabilitiesMsg {
        InputCapabilitiesMsg {
            text_commit: InputCapabilityAvailability::Available,
            ..InputCapabilitiesMsg::default()
        }
    }

    fn key(key: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn a_host_without_committed_text_sees_every_key_as_before() {
        let mut input = DeckTextInput::default();
        input.reset();
        assert!(!input.active());
        let events = [
            key(egui::Key::A, true),
            egui::Event::Text("a".to_owned()),
            egui::Event::Ime(egui::ImeEvent::Commit("ü".to_owned())),
            key(egui::Key::A, false),
        ];
        assert_eq!(
            input.route(&events),
            [RoutedInput::Key(0), RoutedInput::Key(3)]
        );
        assert!(input.ime_output(egui::Rect::EVERYTHING).is_none());
    }

    #[test]
    fn commits_go_as_text_and_the_composition_is_shown_locally() {
        let mut input = DeckTextInput::default();
        input.configure(TypingMode::Auto, OptionKeyMode::default());
        input.set_host(host());
        assert!(input.active());
        input.route(&[egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "にほん".to_owned(),
            active_range_chars: None,
        })]);
        assert_eq!(input.preedit.as_deref(), Some("にほん"));
        let routed = input.route(&[
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: String::new(),
                active_range_chars: None,
            }),
            egui::Event::Ime(egui::ImeEvent::Commit("日本".to_owned())),
            key(egui::Key::Enter, false),
        ]);
        assert_eq!(
            routed,
            [RoutedInput::Text("日本".to_owned()), RoutedInput::Key(2)]
        );
        assert_eq!(input.preedit, None);
        assert!(input
            .ime_output(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0)
            ))
            .is_some());
    }

    #[test]
    fn a_composing_right_option_never_reaches_the_host() {
        let mut input = DeckTextInput::default();
        input.configure(TypingMode::Auto, OptionKeyMode::LeftAltRightCharacters);
        input.set_host(host());
        let option = |key, pressed| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: if pressed {
                egui::Modifiers::ALT
            } else {
                egui::Modifiers::NONE
            },
        };
        assert!(input
            .route(&[
                option(egui::Key::AltRight, true),
                option(egui::Key::AltRight, false)
            ])
            .is_empty());
        assert_eq!(
            input.route(&[
                option(egui::Key::AltLeft, true),
                option(egui::Key::AltLeft, false)
            ]),
            [RoutedInput::Key(0), RoutedInput::Key(1)]
        );
    }

    #[test]
    fn refusals_are_explained_with_the_characters() {
        let result = TextCommitResultMsg {
            refused_chars: 1,
            refused_text: "😀".to_owned(),
            reason: TextRefusalReason::CompositorLacksTextInput,
            ..TextCommitResultMsg::default()
        };
        let text = notice_text(&result).unwrap();
        assert!(text.starts_with("1 character not typed (“😀”)"), "{text}");
        assert!(notice_text(&TextCommitResultMsg::default()).is_none());
        let unverified = TextCommitResultMsg {
            unverified: true,
            delivered_chars: 2,
            ..TextCommitResultMsg::default()
        };
        assert!(notice_text(&unverified)
            .unwrap()
            .contains("without checking"));
    }
}
