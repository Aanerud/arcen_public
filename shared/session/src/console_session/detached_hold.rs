// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A joined console while its Deck is away (ADR 0025 §4).
//!
//! When the Deck leaves, the desktop is the user's again at once: the
//! hand-back ([`super::DETACH_STEPS`]) locks the session, then gives the
//! desk its monitors, its sound (the session's audio lease ends, so the
//! default sink and every mute are what they were, or what the user chose
//! since) and its input back. Arcen's input devices go with the attachment,
//! and a guest console laid out to the Deck keeps that layout, as the
//! other product leaves its own ([`super::guest_layout`]).
//!
//! What may stay is the Pier's hold for the Deck's resume — after a lost
//! transport only, never after the Deck closed — and nothing of it is the
//! desk's: an idle launcher and session agent, no audio lease, no device.
//! The hold lasts at most the resume window ([`crate::direct_reconnect`])
//! and ends at once ([`DetachedHold::observe`])
//! when another remote-desktop product's client takes the desktop, that
//! product starts a desktop of its own, or the person at the desk unlocks
//! the session or signs in.

use super::{ConsoleSessionFact, SessionRead, other_product_desktops, other_product_started};

/// What happens to a desktop when its Deck has left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterDeckLeft {
    /// A desktop Arcen created is kept for the next sign-in of its account.
    KeepDesktop,
    /// A joined console is held for the Deck's resume (bounded, see
    /// [`DetachedHold`]); its audio lease is already given back.
    HoldForResume,
    /// A joined console is handed back and Arcen leaves it now; the session
    /// continues (disconnect locks, never ends).
    LeaveConsole,
}

/// What happens when the Deck of a desktop leaves: `console` for a joined
/// console session, `resume_held` when the Pier holds the attachment for
/// the Deck's resume (a lost transport of an attachment that negotiated
/// resume).
#[must_use]
pub const fn after_deck_left(console: bool, resume_held: bool) -> AfterDeckLeft {
    match (console, resume_held) {
        (false, _) => AfterDeckLeft::KeepDesktop,
        (true, true) => AfterDeckLeft::HoldForResume,
        (true, false) => AfterDeckLeft::LeaveConsole,
    }
}

/// Why a console held for its Deck's resume is let go before the window
/// ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldEnd {
    /// Another remote-desktop product's client is using the desktop (a
    /// guest console, [`super::another_products_client_input`]).
    OtherProductClient,
    /// Another remote-desktop product started a desktop of its own on the
    /// console ([`super::other_product_started`]).
    OtherProductStarted,
    /// The person at the desk unlocked the session, or signed in.
    DeskTookOver,
}

impl HoldEnd {
    /// The reason class logged when the hold ends.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::OtherProductClient => "other_product_client",
            Self::OtherProductStarted => "other_product_started",
            Self::DeskTookOver => "desk_took_over",
        }
    }
}

/// The hold of one joined console while its Deck may still resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedHold {
    joined: String,
    guest: bool,
    /// The console as it was when the Deck left (or at the first complete
    /// look after it).
    before: Option<Baseline>,
    /// The hand-back was seen done: the joined session locked, or in the
    /// background behind a login screen.
    given_back: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Baseline {
    /// Other products' desktops already there.
    foreign: Vec<String>,
    /// Other console desktops in use (in front and unlocked).
    in_use: Vec<String>,
}

impl Baseline {
    /// From a look that lists the joined session; any other (logind did
    /// not answer, or not about every session) proves nothing.
    fn of(sessions: &[ConsoleSessionFact], joined: &str) -> Option<Self> {
        sessions
            .iter()
            .any(|session| session.id == joined)
            .then(|| Self {
                foreign: other_product_desktops(sessions, joined)
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                in_use: desks_in_use(sessions, joined)
                    .map(|session| session.id.clone())
                    .collect(),
            })
    }
}

impl DetachedHold {
    /// The hold of `joined` (`guest`: another product's desktop), as the
    /// console looked when its Deck left. A look that does not list the
    /// joined session is no baseline: the first one that does becomes it.
    #[must_use]
    pub fn new(joined: &str, guest: bool, sessions: &[ConsoleSessionFact]) -> Self {
        Self {
            joined: joined.to_owned(),
            guest,
            before: Baseline::of(sessions, joined),
            given_back: false,
        }
    }

    /// [`Self::new`] from a read of logind's sessions: an unreadable one is
    /// no baseline (the first complete look that lists the joined session
    /// becomes it).
    #[must_use]
    pub fn from_read(joined: &str, guest: bool, read: &SessionRead) -> Self {
        Self {
            joined: joined.to_owned(),
            guest,
            before: read
                .as_ref()
                .ok()
                .and_then(|sessions| Baseline::of(sessions, joined)),
            given_back: false,
        }
    }

    /// Whether [`Self::observe`] needs to know if another product's client
    /// is using the desktop (only a guest console's own product counts).
    #[must_use]
    pub const fn watches_client(&self) -> bool {
        self.guest
    }

    /// One look at the console while it is held: `sessions` as logind lists
    /// them (a look without the joined session — logind did not answer, or
    /// not about every session — concludes nothing from sessions),
    /// `another_products_client` whether a client of another product is
    /// using the desktop (an unanswered check counts as one). `Some` ends
    /// the hold now.
    ///
    /// The desk is judged only after the hand-back was seen done (the
    /// session locked, or behind a login screen): a session that is in
    /// front and unlocked again after that, or another desktop that is
    /// in front and unlocked and was not when the Deck left, is the desk's.
    pub fn observe(
        &mut self,
        sessions: &[ConsoleSessionFact],
        another_products_client: bool,
    ) -> Option<HoldEnd> {
        if self.guest && another_products_client {
            return Some(HoldEnd::OtherProductClient);
        }
        let joined = sessions.iter().find(|session| session.id == self.joined)?;
        let Some(before) = self.before.as_ref() else {
            self.before = Baseline::of(sessions, &self.joined);
            return None;
        };
        if !self.guest && other_product_started(sessions, &self.joined, &before.foreign) {
            return Some(HoldEnd::OtherProductStarted);
        }
        if joined.locked || !joined.active {
            self.given_back = true;
        } else if self.given_back {
            return Some(HoldEnd::DeskTookOver);
        }
        let signed_in_at_desk = desks_in_use(sessions, &self.joined)
            .any(|session| !before.in_use.contains(&session.id));
        signed_in_at_desk.then_some(HoldEnd::DeskTookOver)
    }

    /// [`Self::observe`] from a read of logind's sessions: an unreadable
    /// one concludes nothing from sessions (only a guest console's own
    /// product's client still ends the hold).
    pub fn observe_read(
        &mut self,
        read: &SessionRead,
        another_products_client: bool,
    ) -> Option<HoldEnd> {
        match read {
            Ok(sessions) => self.observe(sessions, another_products_client),
            Err(_) => {
                (self.guest && another_products_client).then_some(HoldEnd::OtherProductClient)
            }
        }
    }
}

/// Console desktops other than `joined` in front and unlocked.
fn desks_in_use<'a>(
    sessions: &'a [ConsoleSessionFact],
    joined: &'a str,
) -> impl Iterator<Item = &'a ConsoleSessionFact> {
    sessions.iter().filter(move |session| {
        session.id != joined && session.is_console_desktop() && session.active && !session.locked
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console_session::CONSOLE_SEAT;
    use crate::direct_reconnect::{
        DirectReconnect, MonotonicMillis, ReconnectEvent, ReconnectPolicy, ReconnectState,
    };

    const TIM: u32 = 1001;
    const DESK: u32 = 1002;

    fn desktop(id: &str, uid: u32, active: bool, locked: bool) -> ConsoleSessionFact {
        ConsoleSessionFact {
            id: id.to_owned(),
            uid,
            user: format!("u{uid}"),
            seat: CONSOLE_SEAT.to_owned(),
            vtnr: 2,
            tty: String::new(),
            class: "user".to_owned(),
            session_type: "x11".to_owned(),
            state: if active { "active" } else { "online" }.to_owned(),
            active,
            remote: false,
            service: "gdm-password".to_owned(),
            locked,
            leader: 0,
            desktop: String::new(),
        }
    }

    fn product(id: &str, uid: u32, active: bool, locked: bool) -> ConsoleSessionFact {
        let mut session = desktop(id, uid, active, locked);
        session.service = "remote-product-session".to_owned();
        session
    }

    fn greeter(active: bool) -> ConsoleSessionFact {
        let mut session = desktop("c1", 42, active, false);
        session.class = "greeter".to_owned();
        session.service = "gdm-launch-environment".to_owned();
        session
    }

    #[test]
    fn only_a_joined_console_with_a_resume_hold_stays_after_its_deck_leaves() {
        assert_eq!(after_deck_left(true, false), AfterDeckLeft::LeaveConsole);
        assert_eq!(after_deck_left(true, true), AfterDeckLeft::HoldForResume);
        assert_eq!(after_deck_left(false, false), AfterDeckLeft::KeepDesktop);
        assert_eq!(after_deck_left(false, true), AfterDeckLeft::KeepDesktop);
    }

    #[test]
    fn the_hold_ends_with_the_resume_window() {
        let mut reconnect = DirectReconnect::new(ReconnectPolicy::new(180).expect("policy"));
        let lost = reconnect.apply(ReconnectEvent::UnexpectedLoss, MonotonicMillis::new(1_000));
        assert!(lost.hold_restore_leases && !lost.restore_leases);
        let ReconnectState::Detached {
            deadline,
            timer_generation,
        } = reconnect.state()
        else {
            panic!("held for the resume");
        };
        assert_eq!(deadline.get(), 181_000);
        let expired = reconnect.apply(
            ReconnectEvent::DeadlineReached { timer_generation },
            deadline,
        );
        assert!(expired.restore_leases);
        assert!(!matches!(
            reconnect.state(),
            ReconnectState::Detached { .. } | ReconnectState::Resuming { .. }
        ));
    }

    #[test]
    fn a_guest_hold_ends_when_the_products_client_connects() {
        let sessions = [product("81", TIM, true, true)];
        let mut hold = DetachedHold::new("81", true, &sessions);
        assert!(hold.watches_client());
        assert_eq!(hold.observe(&sessions, false), None);
        assert_eq!(
            hold.observe(&sessions, true),
            Some(HoldEnd::OtherProductClient)
        );
    }

    #[test]
    fn a_guest_hold_ignores_its_own_products_desktop() {
        let sessions = [product("81", TIM, true, true)];
        let mut hold = DetachedHold::new("81", true, &[]);
        assert_eq!(hold.observe(&sessions, false), None);
    }

    #[test]
    fn a_display_manager_hold_ends_when_another_product_starts_a_desktop() {
        let mine = desktop("40", TIM, true, true);
        let mut hold = DetachedHold::new("40", false, std::slice::from_ref(&mine));
        assert!(!hold.watches_client());
        // Only a guest console's own product's client counts.
        assert_eq!(hold.observe(std::slice::from_ref(&mine), true), None);
        let started = [mine, product("81", TIM, false, false)];
        assert_eq!(
            hold.observe(&started, false),
            Some(HoldEnd::OtherProductStarted)
        );
    }

    #[test]
    fn a_product_desktop_already_there_when_the_deck_left_is_not_news() {
        let sessions = [
            desktop("40", TIM, true, true),
            product("81", DESK, false, true),
        ];
        let mut hold = DetachedHold::new("40", false, &sessions);
        assert_eq!(hold.observe(&sessions, false), None);
    }

    #[test]
    fn unlocking_at_the_desk_after_the_hand_back_ends_the_hold() {
        // The hand-back has not locked it yet: nothing is concluded.
        let mut hold = DetachedHold::new("40", false, &[desktop("40", TIM, true, false)]);
        assert_eq!(
            hold.observe(&[desktop("40", TIM, true, false)], false),
            None
        );
        assert_eq!(hold.observe(&[desktop("40", TIM, true, true)], false), None);
        assert_eq!(
            hold.observe(&[desktop("40", TIM, true, false)], false),
            Some(HoldEnd::DeskTookOver)
        );
    }

    #[test]
    fn signing_in_again_behind_the_login_screen_ends_the_hold() {
        let mut hold = DetachedHold::new("40", false, &[desktop("40", TIM, true, false)]);
        // A session that would not lock went behind a login screen.
        let behind = [greeter(true), desktop("40", TIM, false, false)];
        assert_eq!(hold.observe(&behind, false), None);
        // The login screen brought it back to the desk.
        let back = [greeter(false), desktop("40", TIM, true, false)];
        assert_eq!(hold.observe(&back, false), Some(HoldEnd::DeskTookOver));
    }

    #[test]
    fn another_account_signing_in_at_the_desk_ends_the_hold() {
        let left = [desktop("40", TIM, true, true)];
        let mut hold = DetachedHold::new("40", false, &left);
        assert_eq!(hold.observe(&left, false), None);
        // A login screen in front of the locked session is nobody yet.
        let switching = [desktop("40", TIM, false, true), greeter(true)];
        assert_eq!(hold.observe(&switching, false), None);
        let signed_in = [
            desktop("40", TIM, false, true),
            desktop("41", DESK, true, false),
        ];
        assert_eq!(hold.observe(&signed_in, false), Some(HoldEnd::DeskTookOver));
    }

    #[test]
    fn a_desk_already_in_use_when_the_deck_left_is_not_a_takeover() {
        // A guest desktop on another VT beside a desk that was in use.
        let sessions = [
            product("81", TIM, false, true),
            desktop("41", DESK, true, false),
        ];
        let mut hold = DetachedHold::new("81", true, &sessions);
        assert_eq!(hold.observe(&sessions, false), None);
    }

    #[test]
    fn nothing_is_concluded_when_logind_does_not_answer() {
        let mut hold = DetachedHold::new("40", false, &[desktop("40", TIM, true, true)]);
        assert_eq!(hold.observe(&[desktop("40", TIM, true, true)], false), None);
        assert_eq!(hold.observe(&[], false), None);
        // A look without the joined session proves nothing either.
        let partial = [product("81", DESK, true, false)];
        assert_eq!(hold.observe(&partial, false), None);
    }

    #[test]
    fn an_unanswered_first_look_never_makes_old_desktops_news() {
        // logind did not answer when the Deck left.
        let mut hold = DetachedHold::new("40", false, &[]);
        let console = [
            desktop("40", TIM, true, true),
            product("81", DESK, false, true),
            desktop("41", DESK, true, false),
        ];
        assert_eq!(hold.observe(&console, false), None, "becomes the baseline");
        assert_eq!(hold.observe(&console, false), None);
        let mut guest = DetachedHold::new("81", true, &[desktop("41", DESK, true, false)]);
        let console = [
            product("81", TIM, false, true),
            desktop("41", DESK, true, false),
        ];
        assert_eq!(guest.observe(&console, false), None);
        assert_eq!(guest.observe(&console, false), None);
        // The product's client needs no baseline.
        assert_eq!(guest.observe(&[], true), Some(HoldEnd::OtherProductClient));
    }
}
