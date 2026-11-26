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
//!
//! The administrator's `privacy.desk = "required"` fails closed here too
//! ([`WaitingDesk::Required`], ADR 0025 §4, amended 2026-11-25): a private
//! desk whose session still refuses to lock after the grace is **not**
//! given back. It stays dark with its input blocked, the slot stays held,
//! the refusal is reported once ([`LockWaitVerdict::HoldPrivate`]), and the
//! lock is asked for again every [`HELD_LOCK_RETRY`] until the session
//! locks or the console leaves the account. `"on"` and `"off"` keep the
//! grace.

use std::time::Duration;

use super::desk_privacy::{
    CONSOLE_CHECK, DeskPrivacyPlan, LOCK_RETRY, PrivacyWatch, UnlockedHandBack, WatchVerdict,
    hand_back_unlocked,
};

/// How often a private desk held under `privacy.desk = "required"` asks a
/// session that has refused to lock for [`super::UNLOCKED_GRACE`] again:
/// slower than [`LOCK_RETRY`] (a lock screen turned off by policy will not
/// come back by itself), and never given up, so the session locks once an
/// administrator turns its lock screen back on.
pub const HELD_LOCK_RETRY: Duration = Duration::from_secs(30);

/// The desk a hand-back waits with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingDesk {
    /// Not private: it shows the session anyway.
    Live,
    /// Private, and given back unlocked after [`super::UNLOCKED_GRACE`]
    /// (`privacy.desk` `"on"`: the Deck's user asked for it).
    Private,
    /// Private because the administrator requires it (`privacy.desk =
    /// "required"`): never given back unlocked.
    Required,
}

impl WaitingDesk {
    /// The desk of an attachment served under `plan`, whose desk the host
    /// made private (`private`). A required plan whose desk is not private
    /// (nothing at the desk shows the session) has nothing to hold.
    #[must_use]
    pub const fn new(private: bool, plan: Option<DeskPrivacyPlan>) -> Self {
        match (private, plan) {
            (false, _) => Self::Live,
            (true, Some(plan)) if plan.fails_closed() => Self::Required,
            (true, _) => Self::Private,
        }
    }

    /// Whether the desk is kept private while the wait runs.
    #[must_use]
    pub const fn private(self) -> bool {
        !matches!(self, Self::Live)
    }
}

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
    /// [`WaitingDesk::Required`]: the session has refused to lock for
    /// [`super::UNLOCKED_GRACE`]. The desk stays private and the slot held;
    /// ask again after `retry_after` ([`HELD_LOCK_RETRY`]). `first`: the
    /// first such verdict, when the host reports the refusal as an error
    /// (and tells a Deck that asks for the slot why it is held).
    HoldPrivate { retry_after: Duration, first: bool },
    /// Stop waiting.
    Done(LockWaitEnd),
}

/// The wait for one hand-back's lock screen.
#[derive(Debug, Clone, Copy)]
pub struct LockWait {
    desk: WaitingDesk,
    console: PrivacyWatch,
    held: bool,
}

impl LockWait {
    /// A wait for a desk that is private (`private`) or not, given back
    /// after the grace either way.
    #[must_use]
    pub fn new(private: bool) -> Self {
        Self::for_desk(if private {
            WaitingDesk::Private
        } else {
            WaitingDesk::Live
        })
    }

    /// A wait for `desk`.
    #[must_use]
    pub fn for_desk(desk: WaitingDesk) -> Self {
        Self {
            desk,
            console: PrivacyWatch::default(),
            held: false,
        }
    }

    /// Whether the desk is being held private past the grace
    /// ([`LockWaitVerdict::HoldPrivate`] was given).
    #[must_use]
    pub const fn held(&self) -> bool {
        self.held
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
            if self.desk == WaitingDesk::Required {
                let first = !self.held;
                self.held = true;
                return LockWaitVerdict::HoldPrivate {
                    retry_after: if attempt == LockAttempt::NotOurs {
                        CONSOLE_CHECK
                    } else {
                        HELD_LOCK_RETRY
                    },
                    first,
                };
            }
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
    /// keeps the session hidden until it locks or the grace ends (a
    /// required one, until it locks).
    #[must_use]
    pub const fn on_new_deck(&self) -> Option<LockWaitEnd> {
        if self.desk.private() {
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
                LockWaitVerdict::HoldPrivate { .. } => {
                    panic!("only a required desk is held past the grace")
                }
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
                    LockWaitVerdict::HoldPrivate { .. } => panic!("on and off keep the grace"),
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

    #[test]
    fn only_a_desk_private_by_requirement_is_held() {
        use super::super::desk_privacy::DeskPrivacyPlan;
        let required = Some(DeskPrivacyPlan::Required);
        assert_eq!(WaitingDesk::new(true, required), WaitingDesk::Required);
        assert_eq!(
            WaitingDesk::new(true, Some(DeskPrivacyPlan::BestEffort)),
            WaitingDesk::Private
        );
        assert_eq!(WaitingDesk::new(true, None), WaitingDesk::Private);
        // Nothing at the desk was made private: nothing to hold.
        assert_eq!(WaitingDesk::new(false, required), WaitingDesk::Live);
        assert_eq!(
            WaitingDesk::new(false, Some(DeskPrivacyPlan::OperatorOff)),
            WaitingDesk::Live
        );
    }

    /// Asks a required desk's wait about `attempts` (refusals after the last
    /// one) every pause it names, as a host's loop does, until it ends or
    /// `limit` has passed; returns how it ended, the `first` flags of its
    /// holds, and their pauses.
    fn run_required(
        attempts: &[LockAttempt],
        limit: Duration,
    ) -> (Option<LockWaitEnd>, Vec<bool>, Vec<Duration>) {
        let mut wait = LockWait::for_desk(WaitingDesk::Required);
        let mut elapsed = Duration::ZERO;
        let (mut firsts, mut pauses) = (Vec::new(), Vec::new());
        for index in 0.. {
            if elapsed > limit {
                return (None, firsts, pauses);
            }
            let attempt = attempts
                .get(index)
                .copied()
                .unwrap_or(LockAttempt::NotLocked);
            match wait.next(attempt, elapsed) {
                LockWaitVerdict::Done(end) => return (Some(end), firsts, pauses),
                LockWaitVerdict::RetryAfter(pause) => elapsed += pause,
                LockWaitVerdict::HoldPrivate { retry_after, first } => {
                    assert!(elapsed >= UNLOCKED_GRACE);
                    assert!(wait.held());
                    // A newcomer never ends a held wait.
                    assert_eq!(wait.on_new_deck(), None);
                    firsts.push(first);
                    pauses.push(retry_after);
                    elapsed += retry_after;
                }
            }
        }
        unreachable!()
    }

    #[test]
    fn a_required_desk_is_never_given_back_unlocked() {
        // A lock screen off by policy: an hour of refusals, still held.
        let (end, firsts, pauses) = run_required(&[], Duration::from_secs(3_600));
        assert_eq!(end, None);
        assert_eq!(firsts.first(), Some(&true));
        assert_eq!(firsts.iter().filter(|first| **first).count(), 1);
        assert!(pauses.iter().all(|pause| *pause == HELD_LOCK_RETRY));
        // Bounded: about two asks a minute once held.
        let asks_after_grace = pauses.len();
        assert!(
            (110..=120).contains(&asks_after_grace),
            "{asks_after_grace}"
        );
    }

    #[test]
    fn a_held_required_desk_ends_when_its_session_locks_or_the_console_leaves() {
        let refusals = |count: usize| vec![LockAttempt::NotLocked; count];
        // Locked on the first ask after the grace was held.
        // The ask at the grace is the first one held.
        let held_until =
            usize::try_from(UNLOCKED_GRACE.as_secs() / LOCK_RETRY.as_secs()).expect("a few asks");
        let mut attempts = refusals(held_until + 3);
        attempts.push(LockAttempt::Locked);
        let (end, firsts, _) = run_required(&attempts, Duration::from_secs(3_600));
        assert_eq!(end, Some(LockWaitEnd::Locked));
        assert_eq!(firsts, [true, false, false]);
        assert!(!LockWaitEnd::Locked.leaves_desk_unlocked());

        // Another account took the console while it was held.
        let mut attempts = refusals(held_until + 2);
        attempts.extend([LockAttempt::NotOurs; 3]);
        let (end, _, pauses) = run_required(&attempts, Duration::from_secs(3_600));
        assert_eq!(end, Some(LockWaitEnd::ConsoleLeft));
        assert_eq!(pauses.last(), Some(&CONSOLE_CHECK));
    }

    #[test]
    fn a_required_desk_locked_within_the_grace_is_not_held() {
        let (end, firsts, _) = run_required(
            &[LockAttempt::NotLocked, LockAttempt::Locked],
            Duration::from_secs(3_600),
        );
        assert_eq!(end, Some(LockWaitEnd::Locked));
        assert!(firsts.is_empty());
    }
}
