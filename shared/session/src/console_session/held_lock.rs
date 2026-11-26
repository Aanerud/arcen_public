// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A joined console session held for its Deck's resume is locked at once
//! (ADR 0025 §4; GAP-044, amended 2026-11-25).
//!
//! Every departure locks the session, and a lost transport is no exception:
//! the session is locked as soon as its Deck's transport is lost, while the
//! Pier holds the attachment for the Deck's resume. What a resume then does
//! depends on who can unlock the session again ([`UnlockAuthority`]): a Pier
//! that may unlock it after its own sign-in proved the owner's password
//! (Linux, through logind) resumes the Deck into it; on a host where only
//! the owner's password given to the operating system's own sign-in unlocks
//! it (Windows), a credential-free resume cannot, so the returning Deck is
//! asked to sign in again ([`HeldResume::SignInAgain`]). That sign-in
//! unlocks the same session and takes the held desktop over; the hold is
//! kept for it.
//!
//! While held, a session that is still the Deck's, locked or not, keeps the
//! hold ([`HeldReading`]): a lock is the hold's own, or one at the desk, and
//! either secures it. A held session agent stops proving the desk's input
//! hooks while the session is locked ([`DeskInputProof`]): the lock screen
//! owns the desk's keyboard and mouse then, and no synthetic input reaches a
//! locked session.
//!
//! When the hold ends, a session that is already locked is handed back in
//! the lock-first order ([`super::hand_back_order`]).

/// One step of holding a joined session for its Deck's resume once the
/// transport is lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldStep {
    /// The session agent stops streaming and stays, idle, keeping the desk
    /// as private as the attachment had it.
    DetachAgent,
    /// Lock the session: a departure locks, a lost transport too.
    LockSession,
    /// Watch the held session ([`HeldReading`]) until the Deck resumes, the
    /// resume window ends, or the session changes.
    Watch,
}

/// The order a lost transport's hold runs in: nothing more is captured
/// before the lock, and the lock comes before the hold is watched, so the
/// session is never held unlocked.
pub const HOLD_STEPS: [HoldStep; 3] = [
    HoldStep::DetachAgent,
    HoldStep::LockSession,
    HoldStep::Watch,
];

/// Who can unlock a console session Arcen locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockAuthority {
    /// The Pier itself, after its own sign-in proved the owner's password
    /// (logind's unlock, as root).
    Pier,
    /// Only the owner's password, given to the operating system's own
    /// sign-in (the lock screen, or the Pier's credential provider during a
    /// fresh sign-in): there is no unlock without the credential, and a
    /// resume carries none.
    OwnersPassword,
}

/// What the held session is, read while the Deck is away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldReading {
    /// Still the Deck's session (same account, same session): `locked`
    /// says whether it is secured, behind its lock screen or taken off the
    /// console to the sign-in screen when it would not lock.
    Ours { locked: bool },
    /// Signed out, or switched to another account: the hold ends
    /// ([`super::Departure::SessionChanged`]).
    Changed,
}

impl HeldReading {
    /// Whether the hold continues: a session that is still the Deck's,
    /// locked or not.
    #[must_use]
    pub const fn keeps_hold(self) -> bool {
        matches!(self, Self::Ours { .. })
    }
}

/// What a resume of a held session does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldResume {
    /// Attach the returning Deck (the Pier unlocks the session first when
    /// it was locked).
    Attach,
    /// Refuse this credential-free resume and keep holding: the Deck asks
    /// its user to sign in again, and that sign-in unlocks the same session
    /// and takes the held desktop over.
    SignInAgain,
    /// The session is no longer the Deck's: refuse, and end the hold.
    Refuse,
}

/// What a resume does with the held session read as `reading`, on a host
/// whose locked sessions `authority` can unlock.
#[must_use]
pub const fn held_resume(reading: HeldReading, authority: UnlockAuthority) -> HeldResume {
    match (reading, authority) {
        (HeldReading::Changed, _) => HeldResume::Refuse,
        (HeldReading::Ours { locked: true }, UnlockAuthority::OwnersPassword) => {
            HeldResume::SignInAgain
        }
        (HeldReading::Ours { .. }, _) => HeldResume::Attach,
    }
}

/// Whether a held session agent proves that the desk's input hooks still
/// hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskInputProof {
    /// Prove them: the session is unlocked, and only the hooks keep the
    /// desk's keyboard and mouse out of it.
    Probe,
    /// Not while the session is locked: its lock screen takes the desk's
    /// input, and a synthetic probe cannot reach a locked session.
    LockScreen,
}

impl DeskInputProof {
    /// The proof for a held session whose lock screen is up (`locked`), or
    /// whose state could not be read (`None`, proved as if unlocked).
    #[must_use]
    pub const fn while_held(locked: Option<bool>) -> Self {
        match locked {
            Some(true) => Self::LockScreen,
            Some(false) | None => Self::Probe,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locked_held_session_asks_a_returning_deck_to_sign_in_where_only_the_password_unlocks() {
        let locked = HeldReading::Ours { locked: true };
        let unlocked = HeldReading::Ours { locked: false };
        assert_eq!(
            held_resume(locked, UnlockAuthority::OwnersPassword),
            HeldResume::SignInAgain
        );
        // A Pier that may unlock it resumes the Deck into it.
        assert_eq!(
            held_resume(locked, UnlockAuthority::Pier),
            HeldResume::Attach
        );
        // Unlocked at the desk by its owner, or by an earlier sign-in.
        for authority in [UnlockAuthority::OwnersPassword, UnlockAuthority::Pier] {
            assert_eq!(held_resume(unlocked, authority), HeldResume::Attach);
            assert_eq!(
                held_resume(HeldReading::Changed, authority),
                HeldResume::Refuse
            );
        }
    }

    #[test]
    fn a_held_session_is_locked_before_it_is_watched() {
        let position = |step| HOLD_STEPS.iter().position(|s| *s == step).unwrap();
        assert!(position(HoldStep::DetachAgent) < position(HoldStep::LockSession));
        assert!(position(HoldStep::LockSession) < position(HoldStep::Watch));
    }

    #[test]
    fn the_hold_keeps_a_session_that_is_still_the_decks_locked_or_not() {
        assert!(HeldReading::Ours { locked: true }.keeps_hold());
        assert!(HeldReading::Ours { locked: false }.keeps_hold());
        assert!(!HeldReading::Changed.keeps_hold());
    }

    #[test]
    fn a_held_agent_proves_the_desk_input_only_while_the_session_is_unlocked() {
        assert_eq!(
            DeskInputProof::while_held(Some(true)),
            DeskInputProof::LockScreen
        );
        assert_eq!(
            DeskInputProof::while_held(Some(false)),
            DeskInputProof::Probe
        );
        // An unread state is never taken as a lock screen.
        assert_eq!(DeskInputProof::while_held(None), DeskInputProof::Probe);
    }
}
