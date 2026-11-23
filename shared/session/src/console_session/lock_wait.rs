// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Waiting for the lock screen after a Deck's departure (ADR 0025 §4).
//!
//! A hand-back locks the session first ([`super::DETACH_STEPS`]). When the
//! lock is refused, a **private** desk keeps the session hidden and asks
//! again every [`LOCK_RETRY`] for at most [`super::UNLOCKED_GRACE`], and a
//! console another account took over (on consecutive readings) is left to
//! it. A desk that was **not private** asks again the same way rather than
//! giving up after one refusal, but it shows the session anyway, so it
//! does not keep the host from its next Deck: an authenticated Deck that
//! waits for the host's one session slot ends the wait, and the desk is
//! reported unlocked. Every ending that leaves the desk unlocked is reported
//! as an error ([`LockWaitEnd::leaves_desk_unlocked`]).

use std::time::Duration;

use super::desk_privacy::{
    CONSOLE_CHECK, LOCK_RETRY, PrivacyWatch, UnlockedHandBack, WatchVerdict, hand_back_unlocked,
};

/// What one attempt to lock the session did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockAttempt {
    /// The session is locked.
    Locked,
    /// Not a session to lock: the login window, a hand-run host with no
    /// account, or a console another account holds.
    NotOurs,
    /// The lock was asked for and could not be proved.
    NotLocked,
}

/// Whether a hand-back waits for the lock screen after its first attempt:
/// a private desk always does (it also waits out a console reading that is
/// not this account's), any desk whose lock was refused does.
#[must_use]
pub const fn waits_for_lock(private: bool, first: LockAttempt) -> bool {
    private || matches!(first, LockAttempt::NotLocked)
}

/// How a wait for the lock screen ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWaitEnd {
    /// The session locked.
    Locked,
    /// The console left this account (signed out, or another account
    /// switched in): nothing of the session is on screen.
    ConsoleLeft,
    /// The session refused to lock for [`super::UNLOCKED_GRACE`] (its lock
    /// screen is off by policy); the desk gets it back unlocked.
    GraceExpired,
    /// A desk that was not private: an authenticated Deck waits for the
    /// host's session slot, so the wait ends with the desk unlocked.
    YieldedToNewDeck,
}

impl LockWaitEnd {
    /// Whether the desk is left unlocked and signed in: an error the
    /// operator must read.
    #[must_use]
    pub const fn leaves_desk_unlocked(self) -> bool {
        matches!(self, Self::GraceExpired | Self::YieldedToNewDeck)
    }
}

/// What to do after an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWaitVerdict {
    /// Ask for the lock again after this pause.
    RetryAfter(Duration),
    /// Stop waiting.
    Done(LockWaitEnd),
}

/// The wait for one hand-back's lock screen.
#[derive(Debug, Clone, Copy)]
pub struct LockWait {
    private: bool,
    console: PrivacyWatch,
}

impl LockWait {
    /// A wait for a desk that is private (`private`) or not.
    #[must_use]
    pub fn new(private: bool) -> Self {
        Self {
            private,
            console: PrivacyWatch::default(),
        }
    }

    /// Decides what follows `attempt`, `elapsed` after the hand-back's
    /// first refused lock. A console that is not this account's is left
    /// only after consecutive readings: one failed reading is not a switch
    /// of account.
    pub fn next(&mut self, attempt: LockAttempt, elapsed: Duration) -> LockWaitVerdict {
        match attempt {
            LockAttempt::Locked => return LockWaitVerdict::Done(LockWaitEnd::Locked),
            LockAttempt::NotOurs => {
                if self.console.console(false) == WatchVerdict::ConsoleLeft {
                    return LockWaitVerdict::Done(LockWaitEnd::ConsoleLeft);
                }
            }
            LockAttempt::NotLocked => {
                let _ = self.console.console(true);
            }
        }
        if hand_back_unlocked(elapsed) == UnlockedHandBack::GiveBack {
            return LockWaitVerdict::Done(LockWaitEnd::GraceExpired);
        }
        LockWaitVerdict::RetryAfter(if attempt == LockAttempt::NotOurs {
            CONSOLE_CHECK
        } else {
            LOCK_RETRY
        })
    }

    /// An authenticated Deck waits for the host's session slot: a desk that
    /// was not private stops waiting so the Deck is served; a private desk
    /// keeps the session hidden until it locks or the grace ends.
    #[must_use]
    pub const fn on_new_deck(&self) -> Option<LockWaitEnd> {
        if self.private {
            None
        } else {
            Some(LockWaitEnd::YieldedToNewDeck)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console_session::UNLOCKED_GRACE;

    /// Runs a wait over `attempts` (refusals after the last one), with a new
    /// Deck arriving after attempt `deck_after`, the way a host's loop does.
    fn run(
        private: bool,
        attempts: &[LockAttempt],
        deck_after: Option<usize>,
    ) -> (LockWaitEnd, usize) {
        let mut wait = LockWait::new(private);
        let mut elapsed = Duration::ZERO;
        let mut index = 0;
        loop {
            let attempt = attempts
                .get(index)
                .copied()
                .unwrap_or(LockAttempt::NotLocked);
            match wait.next(attempt, elapsed) {
                LockWaitVerdict::Done(end) => return (end, index + 1),
                LockWaitVerdict::RetryAfter(pause) => {
                    if deck_after == Some(index)
                        && let Some(end) = wait.on_new_deck()
                    {
                        return (end, index + 1);
                    }
                    elapsed += pause;
                }
            }
            index += 1;
        }
    }

    #[test]
    fn only_a_private_desk_or_a_refused_lock_waits() {
        assert!(waits_for_lock(true, LockAttempt::Locked));
        assert!(waits_for_lock(true, LockAttempt::NotOurs));
        assert!(waits_for_lock(true, LockAttempt::NotLocked));
        assert!(waits_for_lock(false, LockAttempt::NotLocked));
        assert!(!waits_for_lock(false, LockAttempt::Locked));
        assert!(!waits_for_lock(false, LockAttempt::NotOurs));
    }

    #[test]
    fn a_lock_that_comes_on_the_retry_stops_the_wait() {
        for private in [true, false] {
            let (end, attempts) = run(
                private,
                &[LockAttempt::NotLocked, LockAttempt::Locked],
                None,
            );
            assert_eq!(end, LockWaitEnd::Locked);
            assert_eq!(attempts, 2);
            assert!(!end.leaves_desk_unlocked());
        }
    }

    #[test]
    fn a_lock_that_never_comes_is_given_up_at_the_grace_and_reported() {
        for private in [true, false] {
            let mut wait = LockWait::new(private);
            let mut elapsed = Duration::ZERO;
            let end = loop {
                match wait.next(LockAttempt::NotLocked, elapsed) {
                    LockWaitVerdict::Done(end) => break end,
                    LockWaitVerdict::RetryAfter(pause) => {
                        assert_eq!(pause, LOCK_RETRY);
                        elapsed += pause;
                    }
                }
            };
            assert_eq!(end, LockWaitEnd::GraceExpired);
            assert!(elapsed >= UNLOCKED_GRACE);
            assert!(elapsed < UNLOCKED_GRACE + LOCK_RETRY);
            assert!(end.leaves_desk_unlocked());
        }
    }

    #[test]
    fn a_new_deck_ends_the_wait_only_for_a_desk_that_was_not_private() {
        let (end, attempts) = run(false, &[LockAttempt::NotLocked], Some(0));
        assert_eq!(end, LockWaitEnd::YieldedToNewDeck);
        assert_eq!(attempts, 1);
        assert!(end.leaves_desk_unlocked());

        // A private desk keeps the session hidden and keeps asking.
        assert_eq!(LockWait::new(true).on_new_deck(), None);
        let (end, _) = run(true, &[LockAttempt::NotLocked], Some(0));
        assert_eq!(end, LockWaitEnd::GraceExpired);
        let (end, attempts) = run(
            true,
            &[
                LockAttempt::NotLocked,
                LockAttempt::NotLocked,
                LockAttempt::Locked,
            ],
            Some(0),
        );
        assert_eq!((end, attempts), (LockWaitEnd::Locked, 3));
    }

    #[test]
    fn a_private_desk_leaves_a_console_another_account_holds_after_consecutive_readings() {
        let (end, attempts) = run(
            true,
            &[
                LockAttempt::NotOurs,
                LockAttempt::NotOurs,
                LockAttempt::NotOurs,
            ],
            None,
        );
        assert_eq!((end, attempts), (LockWaitEnd::ConsoleLeft, 3));
        assert!(!end.leaves_desk_unlocked());

        // One failed reading is not a switch of account.
        let (end, _) = run(
            true,
            &[
                LockAttempt::NotOurs,
                LockAttempt::NotLocked,
                LockAttempt::Locked,
            ],
            None,
        );
        assert_eq!(end, LockWaitEnd::Locked);

        let mut wait = LockWait::new(true);
        assert_eq!(
            wait.next(LockAttempt::NotOurs, Duration::ZERO),
            LockWaitVerdict::RetryAfter(CONSOLE_CHECK)
        );
    }
}
