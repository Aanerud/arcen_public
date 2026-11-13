// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Paste as Keystrokes in the Deck: the menu command's state, its
//! confirmation and progress, and the host's reports on what it typed.
//!
//! What is typed, how fast, and when it waits are the shared
//! [`arcen_input::text::paste`] decisions; this module only holds a paste
//! for the session, draws it, and keeps its text out of logs and notices
//! (it may be a password: only counts are ever logged or shown).

use std::time::{Duration, Instant};

use arcen_input::text::{
    prepare_paste, HostTextTruth, PastePhase, PastePoll, PasteRefusal, PasteSettings, PasteText,
    PasteTyping,
};
use arcen_protocol::messages::{TextCommitResultMsg, TextRefusalReason};

/// How long the outcome stays on screen.
const NOTICE_DURATION: Duration = Duration::from_secs(5);

/// What a paste request led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteRequest {
    /// Typing starts: release every key held on the host first.
    Started,
    /// The user is asked first.
    AskFirst,
    /// Nothing is typed; the user has been told why.
    Refused,
    /// A paste is already going on.
    Busy,
}

/// What the user did with a long paste's confirmation this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    /// Nothing.
    None,
    /// Type it: typing starts.
    Confirmed,
    /// Do not type it.
    Declined,
}

/// Why a paste ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The user pressed Esc or chose Stop Typing.
    User,
    /// The session ended or the window lost the session.
    SessionEnded,
}

/// The session's paste, if any.
#[derive(Debug, Default)]
pub struct PasteKeys {
    settings: PasteSettings,
    pending: Option<PasteText>,
    /// The host said it cannot read its own keyboard layout, so the paste
    /// waiting for confirmation may come out differently.
    pending_unchecked: bool,
    typing: Option<PasteTyping>,
    /// Sequence numbers of this paste's commits, first and last.
    sequences: Option<(u64, u64)>,
    refused: u64,
    refusal: Option<TextRefusalReason>,
    /// The host typed some of it without being able to check its layout.
    unverified: bool,
    notice: Option<(Instant, String)>,
}

impl PasteKeys {
    /// Applies the user's settings to the next paste.
    pub fn configure(&mut self, settings: PasteSettings) {
        self.settings = settings.clamped();
    }

    /// The settings in force.
    #[must_use]
    pub const fn settings(&self) -> PasteSettings {
        self.settings
    }

    /// Whether a paste is being typed or waits for confirmation: the
    /// keyboard belongs to it until it ends.
    #[must_use]
    pub fn captures_keyboard(&self) -> bool {
        self.pending.is_some() || self.typing.as_ref().is_some_and(PasteTyping::active)
    }

    /// Whether a paste is being typed.
    #[must_use]
    pub fn typing(&self) -> bool {
        self.typing.as_ref().is_some_and(PasteTyping::active)
    }

    /// The menu command: types `text` (the clipboard's plain text) into a
    /// host with `host`'s text routes, unless `allowed` (the host's
    /// clipboard policy) says no. `layout_unchecked`: the host said it
    /// could not read its keyboard layout, so it would type without proof;
    /// the user is asked first, whatever the length.
    pub fn request(
        &mut self,
        text: Option<&str>,
        host: HostTextTruth,
        allowed: bool,
        layout_unchecked: bool,
        now: Instant,
    ) -> PasteRequest {
        if self.captures_keyboard() {
            return PasteRequest::Busy;
        }
        if !allowed {
            self.tell(
                now,
                "This host's administrator does not allow text from this Mac's clipboard, \
                 typed or pasted."
                    .to_owned(),
            );
            return PasteRequest::Refused;
        }
        let prepared = match prepare_paste(text.unwrap_or_default(), host) {
            Ok(prepared) => prepared,
            Err(refusal) => {
                tracing::info!(
                    target: crate::logging::target::INPUT,
                    refusal = refusal_token(refusal),
                    "paste as keystrokes refused"
                );
                self.tell(now, refusal.message());
                return PasteRequest::Refused;
            }
        };
        let summary = prepared.summary();
        tracing::info!(
            target: crate::logging::target::INPUT,
            items = summary.items,
            keys = summary.keys,
            dropped = summary.dropped,
            trailing_breaks = summary.trailing_breaks,
            chars_per_second = self.settings.chars_per_second,
            confirm = prepared.needs_confirmation(self.settings),
            layout_unchecked,
            "paste as keystrokes requested"
        );
        if layout_unchecked || prepared.needs_confirmation(self.settings) {
            self.pending = Some(prepared);
            self.pending_unchecked = layout_unchecked;
            return PasteRequest::AskFirst;
        }
        self.begin(prepared);
        PasteRequest::Started
    }

    /// The user confirmed a long paste. `true` when typing starts.
    pub fn confirm(&mut self) -> bool {
        let Some(prepared) = self.pending.take() else {
            return false;
        };
        self.begin(prepared);
        true
    }

    fn begin(&mut self, prepared: PasteText) {
        self.notice = None;
        self.sequences = None;
        self.refused = 0;
        self.refusal = None;
        self.unverified = false;
        self.typing = Some(PasteTyping::new(prepared, self.settings));
    }

    /// Ends the paste between two characters, or drops one waiting for
    /// confirmation.
    pub fn stop(&mut self, reason: StopReason, now: Instant) {
        if self.pending.take().is_some() {
            tracing::info!(
                target: crate::logging::target::INPUT,
                "paste as keystrokes not confirmed"
            );
            return;
        }
        let Some(typing) = self.typing.as_mut().filter(|typing| typing.active()) else {
            return;
        };
        typing.cancel();
        let (typed, total) = typing.progress();
        tracing::info!(
            target: crate::logging::target::INPUT,
            typed,
            total,
            ?reason,
            "paste as keystrokes stopped"
        );
        if reason == StopReason::User {
            self.tell(now, format!("Typing stopped after {typed} of {total}."));
        }
        self.typing = None;
    }

    /// What to send now. `modifiers_held`: a modifier key is physically
    /// held, so typing waits.
    pub fn poll(&mut self, now: Instant, modifiers_held: bool) -> PastePoll {
        let Some(typing) = self.typing.as_mut() else {
            return PastePoll::default();
        };
        let poll = typing.poll(now, modifiers_held);
        if typing.phase() == PastePhase::Finished {
            let (typed, _) = typing.progress();
            tracing::info!(
                target: crate::logging::target::INPUT,
                typed,
                refused = self.refused,
                "paste as keystrokes finished"
            );
            self.typing = None;
            self.tell(now, self.outcome(Some(typed)));
        }
        poll
    }

    /// Records the sequence of a commit sent for the paste.
    pub fn sent_commit(&mut self, sequence: u64) {
        self.sequences = Some(match self.sequences {
            Some((first, _)) => (first, sequence),
            None => (sequence, sequence),
        });
    }

    /// A host's report on a commit. `true` when it was one of this paste's
    /// commits: it is counted here and never quoted (the text may be a
    /// password), so the caller must not show it.
    pub fn absorb_result(&mut self, result: &TextCommitResultMsg, now: Instant) -> bool {
        let Some((first, last)) = self.sequences else {
            return false;
        };
        if result.sequence < first || result.sequence > last || result.sequence == 0 {
            return false;
        }
        let news = result.refused_chars > 0 || (result.unverified && !self.unverified);
        if result.refused_chars > 0 {
            self.refused += u64::from(result.refused_chars);
            self.refusal = Some(result.reason);
        }
        self.unverified |= result.unverified;
        if news && !self.typing() {
            // A late report after the outcome was shown.
            self.tell(now, self.outcome(None));
        }
        true
    }

    fn outcome(&self, typed: Option<usize>) -> String {
        let mut text = if self.refused == 0 {
            typed.map_or_else(
                || "The clipboard was typed.".to_owned(),
                |typed| format!("Typed {typed} characters from the clipboard."),
            )
        } else {
            let why = self
                .refusal
                .map_or("the host could not type them", refusal_reason);
            format!("{} characters were not typed: {why}.", self.refused)
        };
        if self.unverified {
            text.push_str(
                " The host could not check its keyboard layout, so some characters may have \
                 come out differently: check before submitting.",
            );
        }
        text
    }

    fn tell(&mut self, now: Instant, text: String) {
        self.notice = Some((now, text));
    }

    /// Draws the confirmation, the progress and the outcome, and says what
    /// the user did with a confirmation this frame.
    pub fn show(&mut self, ctx: &egui::Context) -> Confirmation {
        let mut outcome = Confirmation::None;
        if let Some(pending) = &self.pending {
            let summary = pending.summary();
            let duration = pending.duration(self.settings);
            let mut confirm = false;
            let mut decline = false;
            egui::Window::new("Paste as Keystrokes")
                .id(egui::Id::new("arcen-paste-confirm"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Type {} characters into the remote computer? At {} characters a \
                         second this takes about {}.",
                        summary.items,
                        self.settings.chars_per_second,
                        format_duration(duration)
                    ));
                    if self.pending_unchecked {
                        ui.label(
                            egui::RichText::new(
                                "The remote computer could not read its own keyboard layout, \
                                 so some characters may come out differently. Check the result \
                                 before you submit it.",
                            )
                            .color(egui::Color32::from_rgb(200, 120, 0)),
                        );
                    }
                    ui.label("Press Esc at any time to stop.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Type").clicked() {
                            confirm = true;
                        }
                        if ui.button("Cancel").clicked() {
                            decline = true;
                        }
                    });
                });
            if confirm && self.confirm() {
                outcome = Confirmation::Confirmed;
            } else if decline {
                self.stop(StopReason::User, Instant::now());
                outcome = Confirmation::Declined;
            }
        }
        if let Some(typing) = self.typing.as_ref().filter(|typing| typing.active()) {
            let (typed, total) = typing.progress();
            let (title, detail) = if typing.phase() == PastePhase::WaitingForRelease {
                (
                    "Release the modifier keys to start typing".to_owned(),
                    format!("{total} characters from the clipboard · Esc stops"),
                )
            } else {
                (
                    format!("Typing the clipboard: {typed} of {total}"),
                    format!(
                        "About {} left · Esc stops",
                        format_duration(typing.remaining())
                    ),
                )
            };
            overlay(ctx, "arcen-paste-progress", &title, Some(&detail));
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if let Some((shown_at, text)) = &self.notice {
            if shown_at.elapsed() > NOTICE_DURATION {
                self.notice = None;
            } else {
                overlay(ctx, "arcen-paste-notice", text, None);
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
        outcome
    }
}

fn overlay(ctx: &egui::Context, id: &str, title: &str, detail: Option<&str>) {
    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 96.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(210))
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(16, 10))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(title)
                            .size(15.0)
                            .color(egui::Color32::WHITE),
                    );
                    if let Some(detail) = detail {
                        ui.label(
                            egui::RichText::new(detail)
                                .size(12.0)
                                .color(egui::Color32::from_gray(200)),
                        );
                    }
                });
        });
}

/// "45 s", "3 min 20 s".
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64().ceil() as u64;
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds.is_multiple_of(60) {
        format!("{} min", seconds / 60)
    } else {
        format!("{} min {} s", seconds / 60, seconds % 60)
    }
}

const fn refusal_token(refusal: PasteRefusal) -> &'static str {
    match refusal {
        PasteRefusal::Empty => "empty",
        PasteRefusal::TooLong { .. } => "too_long",
        PasteRefusal::NoTextRoute => "no_text_route",
    }
}

const fn refusal_reason(reason: TextRefusalReason) -> &'static str {
    match reason {
        TextRefusalReason::NotInSessionLayout => "the remote keyboard layout has no key for them",
        TextRefusalReason::CompositorLacksTextInput => {
            "the remote keyboard layout has no key for them, and the remote desktop cannot take \
             other characters"
        }
        TextRefusalReason::KeymapUnknown => "the remote keyboard layout could not be read",
        TextRefusalReason::InvalidText => "they are not typeable text",
        TextRefusalReason::InputUnavailable => "the remote input connection was interrupted",
        TextRefusalReason::HostUnsupported => "this host does not type text",
        TextRefusalReason::RateLimited => "they were typed faster than the host accepts",
        TextRefusalReason::Unknown => "the host could not type them",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: HostTextTruth = HostTextTruth {
        text_commit: true,
        unicode: true,
    };

    #[test]
    fn a_short_paste_starts_and_a_long_one_asks_first() {
        let mut paste = PasteKeys::default();
        let now = Instant::now();
        assert_eq!(
            paste.request(Some("hunter2"), HOST, true, false, now),
            PasteRequest::Started
        );
        assert!(paste.captures_keyboard());
        assert_eq!(
            paste.request(Some("again"), HOST, true, false, now),
            PasteRequest::Busy
        );
        paste.stop(StopReason::User, now);
        assert!(!paste.captures_keyboard());

        paste.configure(PasteSettings {
            confirm_above: 3,
            ..PasteSettings::default()
        });
        assert_eq!(
            paste.request(Some("four"), HOST, true, false, now),
            PasteRequest::AskFirst
        );
        assert!(paste.captures_keyboard() && !paste.typing());
        assert!(paste.confirm());
        assert!(paste.typing());
    }

    #[test]
    fn a_host_that_cannot_read_its_layout_is_typed_into_only_after_asking() {
        let mut paste = PasteKeys::default();
        let now = Instant::now();
        assert_eq!(
            paste.request(Some("pw"), HOST, true, true, now),
            PasteRequest::AskFirst
        );
        assert!(paste.pending_unchecked && !paste.typing());
        paste.stop(StopReason::User, now);
        assert!(!paste.captures_keyboard());
    }

    #[test]
    fn refusals_are_told_and_never_type() {
        let mut paste = PasteKeys::default();
        let now = Instant::now();
        assert_eq!(
            paste.request(Some("x"), HostTextTruth::default(), true, false, now),
            PasteRequest::Refused
        );
        assert_eq!(
            paste.request(None, HOST, true, false, now),
            PasteRequest::Refused
        );
        assert_eq!(
            paste.request(Some("x"), HOST, false, false, now),
            PasteRequest::Refused
        );
        assert!(!paste.captures_keyboard());
        assert!(paste.notice.is_some());
    }

    #[test]
    fn the_hosts_reports_on_paste_commits_are_counted_never_quoted() {
        let mut paste = PasteKeys::default();
        let now = Instant::now();
        assert_eq!(
            paste.request(Some("pässwörd"), HOST, true, false, now),
            PasteRequest::Started
        );
        paste.sent_commit(41);
        paste.sent_commit(42);
        let refused = TextCommitResultMsg {
            sequence: 42,
            refused_chars: 2,
            refused_text: "äö".to_owned(),
            reason: TextRefusalReason::NotInSessionLayout,
            ..TextCommitResultMsg::default()
        };
        assert!(paste.absorb_result(&refused, now));
        let other = TextCommitResultMsg {
            sequence: 43,
            ..refused.clone()
        };
        assert!(!paste.absorb_result(&other, now));
        let outcome = paste.outcome(Some(8));
        assert!(outcome.starts_with("2 characters were not typed"));
        assert!(!outcome.contains('ä') && !outcome.contains('ö'));
        assert!(!outcome.contains("check before submitting"));
        let unverified = TextCommitResultMsg {
            sequence: 41,
            unverified: true,
            ..TextCommitResultMsg::default()
        };
        assert!(paste.absorb_result(&unverified, now));
        assert!(paste.outcome(Some(8)).ends_with("check before submitting."));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(Duration::from_millis(400)), "1 s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45 s");
        assert_eq!(format_duration(Duration::from_secs(120)), "2 min");
        assert_eq!(format_duration(Duration::from_secs(200)), "3 min 20 s");
    }
}
