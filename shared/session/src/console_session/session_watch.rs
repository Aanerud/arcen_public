// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Watching a joined console session while its launcher holds it
//! (ADR 0025 §4).
//!
//! A launcher that joined the account's session asks logind about it
//! every few seconds, and leaves when the user has logged out. A look
//! logind did not answer (`loginctl` failed or was slow) is not a logout:
//! ending the attachment on it would take a live session away from its
//! Deck. But the launcher must not keep the desk handed over forever on no
//! evidence either. After [`UNANSWERED_LOOKS_BEFORE_HAND_BACK`] unanswered
//! looks in a row it leaves exactly as it does when the Deck leaves: the
//! session is locked and the console handed back (kept private while it
//! cannot be locked, [`super::DETACH_STEPS`]). The session itself is never
//! ended; only its user, an administrator or a shutdown ends it.

use super::{ConsoleSessionFact, SessionLookup};

/// Unanswered looks in a row after which the launcher stops holding the
/// console. The launcher looks every two seconds and each look is bounded
/// (three seconds), so the console is handed back within about 10 to 25
/// seconds of logind going silent, while a single slow answer never ends
/// a live attachment.
pub const UNANSWERED_LOOKS_BEFORE_HAND_BACK: u32 = 5;

/// What the launcher does after one look at its joined session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStep {
    /// The session is still the account's console desktop, or logind did
    /// not answer this time: keep holding it and look again.
    Hold,
    /// The session ended (logind no longer has it), or it is no longer the
    /// account's console desktop (closing, moved off the console seat).
    Ended,
    /// logind has not answered for [`UNANSWERED_LOOKS_BEFORE_HAND_BACK`]
    /// looks: whether the session is still the account's is unproven, so
    /// it is locked and the console handed back. Never ended.
    HandBack,
}

impl WatchStep {
    /// Stable token for the launcher's log.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Ended => "session_ended",
            Self::HandBack => "session_unproven",
        }
    }
}

/// The launcher's watch over the session it joined for `uid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWatch {
    uid: u32,
    unanswered: u32,
}

impl SessionWatch {
    /// A watch over a session of `uid`.
    #[must_use]
    pub const fn new(uid: u32) -> Self {
        Self { uid, unanswered: 0 }
    }

    /// Unanswered looks in a row so far.
    #[must_use]
    pub const fn unanswered(&self) -> u32 {
        self.unanswered
    }

    /// One look at the joined session, as logind answered it. Whether its
    /// X server answers (`display_alive`) is not consulted: a session ends
    /// when logind says so.
    pub fn observe(&mut self, lookup: SessionLookup<'_>) -> WatchStep {
        match lookup {
            SessionLookup::Known { fact, .. } => {
                self.unanswered = 0;
                if self.still_held(fact) {
                    WatchStep::Hold
                } else {
                    WatchStep::Ended
                }
            }
            SessionLookup::Unknown => {
                self.unanswered = 0;
                WatchStep::Ended
            }
            SessionLookup::Failed => {
                self.unanswered = self.unanswered.saturating_add(1);
                if self.unanswered >= UNANSWERED_LOOKS_BEFORE_HAND_BACK {
                    WatchStep::HandBack
                } else {
                    WatchStep::Hold
                }
            }
        }
    }

    fn still_held(&self, fact: &ConsoleSessionFact) -> bool {
        fact.uid == self.uid && fact.is_console_desktop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console_session::CONSOLE_SEAT;

    const TIM: u32 = 1001;

    fn joined(uid: u32, state: &str) -> ConsoleSessionFact {
        ConsoleSessionFact {
            id: "7".to_owned(),
            uid,
            user: format!("u{uid}"),
            seat: CONSOLE_SEAT.to_owned(),
            vtnr: 2,
            tty: "tty2".to_owned(),
            class: "user".to_owned(),
            session_type: "x11".to_owned(),
            state: state.to_owned(),
            active: true,
            service: "gdm-password".to_owned(),
            ..ConsoleSessionFact::default()
        }
    }

    fn known(fact: &ConsoleSessionFact) -> SessionLookup<'_> {
        SessionLookup::Known {
            fact,
            display_alive: true,
        }
    }

    #[test]
    fn one_unanswered_look_keeps_a_live_attachment() {
        let mut watch = SessionWatch::new(TIM);
        let live = joined(TIM, "active");
        assert_eq!(watch.observe(known(&live)), WatchStep::Hold);
        assert_eq!(watch.observe(SessionLookup::Failed), WatchStep::Hold);
        assert_eq!(watch.observe(known(&live)), WatchStep::Hold);
        assert_eq!(watch.unanswered(), 0);
    }

    #[test]
    fn a_logout_ends_the_attachment() {
        let mut watch = SessionWatch::new(TIM);
        assert_eq!(watch.observe(SessionLookup::Unknown), WatchStep::Ended);
        // Closing, or no longer the account's console desktop.
        let mut watch = SessionWatch::new(TIM);
        assert_eq!(
            watch.observe(known(&joined(TIM, "closing"))),
            WatchStep::Ended
        );
        assert_eq!(
            watch.observe(known(&joined(2002, "active"))),
            WatchStep::Ended
        );
        let mut off_seat = joined(TIM, "active");
        off_seat.seat = String::new();
        assert_eq!(watch.observe(known(&off_seat)), WatchStep::Ended);
        // An unanswered look before the logout changes nothing.
        let mut watch = SessionWatch::new(TIM);
        assert_eq!(watch.observe(SessionLookup::Failed), WatchStep::Hold);
        assert_eq!(watch.observe(SessionLookup::Unknown), WatchStep::Ended);
    }

    #[test]
    fn sustained_silence_hands_the_console_back_and_never_ends_the_session() {
        let mut watch = SessionWatch::new(TIM);
        for look in 1..UNANSWERED_LOOKS_BEFORE_HAND_BACK {
            assert_eq!(
                watch.observe(SessionLookup::Failed),
                WatchStep::Hold,
                "look {look}"
            );
        }
        assert_eq!(watch.observe(SessionLookup::Failed), WatchStep::HandBack);
        assert_eq!(watch.observe(SessionLookup::Failed), WatchStep::HandBack);
    }

    #[test]
    fn only_unanswered_looks_in_a_row_count() {
        let mut watch = SessionWatch::new(TIM);
        let live = joined(TIM, "active");
        for _ in 0..3 {
            for _ in 1..UNANSWERED_LOOKS_BEFORE_HAND_BACK {
                assert_eq!(watch.observe(SessionLookup::Failed), WatchStep::Hold);
            }
            assert_eq!(watch.observe(known(&live)), WatchStep::Hold);
        }
    }
}
