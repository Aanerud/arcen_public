// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! How long a Deck may hold the tablet: the component's lease policy.
//!
//! A privileged USB component (the Windows `ArcenUsbComponent` service)
//! hands the physical tablet to one verified Deck. It verifies the Deck
//! **at accept**: the client runs in the active console session, as that
//! session's user. The lease lasts only while that stays true. It is
//! revoked when:
//!
//! - the console session is disconnected, or the console moves to another
//!   session (fast user switching, a remote takeover);
//! - the authorising session logs off;
//! - the authorising Deck process exits (even if another process inherited
//!   its pipe handle);
//! - the component stops.
//!
//! **A screen lock by the same user does not revoke the lease**: the user
//! and session are unchanged, the Deck keeps its window, and a remote
//! session that locks the local console must not lose its tablet. Unlock
//! changes nothing either.
//!
//! This module is the policy and its state, with no operating-system calls,
//! so it is tested everywhere. The platform feeds it session notifications
//! ([`SessionChange::from_wts`]), the console session it observes, and the
//! client's exit; it acts on the [`Revocation`] it gets back by ending the
//! session ([`crate::component_session::SessionRevocation`]), which cancels
//! in-flight transfers and restores the tablet's own driver.

use std::fmt::{Display, Formatter};

/// The console session ID Windows reports when no session is attached to
/// the physical console (`WTSGetActiveConsoleSessionId`).
pub const NO_CONSOLE_SESSION: u32 = 0xFFFF_FFFF;

/// A session notification, as the platform reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionChange {
    /// A session was connected to the physical console.
    ConsoleConnect(u32),
    /// A session was disconnected from the physical console.
    ConsoleDisconnect(u32),
    /// A session was connected remotely.
    RemoteConnect(u32),
    /// A session was disconnected remotely.
    RemoteDisconnect(u32),
    /// A user logged on to a session.
    Logon(u32),
    /// A user logged off a session.
    Logoff(u32),
    /// A session was locked.
    Lock(u32),
    /// A session was unlocked.
    Unlock(u32),
    /// Anything else (remote control status and later additions).
    Other,
}

impl SessionChange {
    /// Maps a `WTS_*` session-change code (`SERVICE_CONTROL_SESSIONCHANGE`'s
    /// event type) and its session ID.
    #[must_use]
    pub const fn from_wts(event: u32, session: u32) -> Self {
        match event {
            1 => Self::ConsoleConnect(session),
            2 => Self::ConsoleDisconnect(session),
            3 => Self::RemoteConnect(session),
            4 => Self::RemoteDisconnect(session),
            5 => Self::Logon(session),
            6 => Self::Logoff(session),
            7 => Self::Lock(session),
            8 => Self::Unlock(session),
            _ => Self::Other,
        }
    }

    /// Whether this may change who sits at the console: everything except
    /// lock, unlock and unknown notifications.
    #[must_use]
    pub const fn moves_console(self) -> bool {
        !matches!(self, Self::Lock(_) | Self::Unlock(_) | Self::Other)
    }
}

/// Why a lease ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revocation {
    /// The authorising session left the physical console.
    ConsoleDisconnected,
    /// Another session now has the physical console.
    ConsoleSwitched {
        /// The session that has it.
        to: u32,
    },
    /// The authorising session was connected remotely instead.
    RemoteTakeover,
    /// The authorising session logged off.
    LoggedOff,
    /// The authorising Deck process exited.
    ClientExited,
    /// The component is stopping.
    ComponentStopping,
}

impl Display for Revocation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConsoleDisconnected => {
                f.write_str("the tablet was released: the session left this PC's console")
            }
            Self::ConsoleSwitched { to } => write!(
                f,
                "the tablet was released: another session ({to}) now has this PC's console"
            ),
            Self::RemoteTakeover => {
                f.write_str("the tablet was released: the session was connected remotely")
            }
            Self::LoggedOff => f.write_str("the tablet was released: the user signed out"),
            Self::ClientExited => {
                f.write_str("the tablet was released: the Deck that was authorised exited")
            }
            Self::ComponentStopping => {
                f.write_str("the tablet was released: the Arcen USB component is stopping")
            }
        }
    }
}

/// One granted lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    /// Unique for the component's lifetime.
    pub id: u64,
    /// The console session that was active, and the client's, at accept.
    pub console_session: u32,
}

/// The component's single lease and the console generation it was granted
/// under.
#[derive(Debug, Default)]
pub struct LeaseAuthority {
    /// Bumped by every notification that may move the console, so a grant
    /// can tell that the console changed while the client was verified.
    generation: u64,
    next_id: u64,
    lease: Option<Lease>,
}

impl LeaseAuthority {
    /// A fresh authority: no lease.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            generation: 0,
            next_id: 0,
            lease: None,
        }
    }

    /// The console generation; read it before verifying a client and hand
    /// it to [`Self::grant`].
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The lease in force, if any.
    #[must_use]
    pub const fn lease(&self) -> Option<Lease> {
        self.lease
    }

    /// Grants the lease for a client verified against `console_session`
    /// while the generation was `observed_generation`.
    ///
    /// # Errors
    ///
    /// The console moved during verification, there is no console session,
    /// or a lease is already held.
    pub fn grant(
        &mut self,
        observed_generation: u64,
        console_session: u32,
    ) -> Result<Lease, String> {
        if observed_generation != self.generation {
            return Err(
                "the console session changed while the Deck was verified; connect again".to_owned(),
            );
        }
        if console_session == NO_CONSOLE_SESSION {
            return Err("no session is attached to this PC's console".to_owned());
        }
        if self.lease.is_some() {
            return Err("the tablet is already leased to another Deck session".to_owned());
        }
        self.next_id += 1;
        let lease = Lease {
            id: self.next_id,
            console_session,
        };
        self.lease = Some(lease);
        Ok(lease)
    }

    /// Applies a session notification; returns the lease it revoked.
    pub fn on_change(&mut self, change: SessionChange) -> Option<(Lease, Revocation)> {
        if change.moves_console() {
            self.generation = self.generation.wrapping_add(1);
        }
        let lease = self.lease?;
        let mine = lease.console_session;
        let revocation = match change {
            SessionChange::ConsoleDisconnect(session) if session == mine => {
                Revocation::ConsoleDisconnected
            }
            SessionChange::ConsoleConnect(session) if session != mine => {
                Revocation::ConsoleSwitched { to: session }
            }
            SessionChange::RemoteConnect(session) if session == mine => Revocation::RemoteTakeover,
            SessionChange::Logoff(session) if session == mine => Revocation::LoggedOff,
            _ => return None,
        };
        self.lease = None;
        Some((lease, revocation))
    }

    /// Applies the console session the platform observes now (a poll that
    /// backs up notifications, and the only signal where none arrive).
    pub fn on_console_observed(&mut self, console_session: u32) -> Option<(Lease, Revocation)> {
        let lease = self.lease?;
        if console_session == lease.console_session {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.lease = None;
        Some((
            lease,
            if console_session == NO_CONSOLE_SESSION {
                Revocation::ConsoleDisconnected
            } else {
                Revocation::ConsoleSwitched {
                    to: console_session,
                }
            },
        ))
    }

    /// Revokes lease `id` for a reason the platform observed itself (the
    /// client exited, the component stops).
    pub fn revoke(&mut self, id: u64, revocation: Revocation) -> Option<(Lease, Revocation)> {
        let lease = self.lease.filter(|lease| lease.id == id)?;
        self.lease = None;
        Some((lease, revocation))
    }

    /// Revokes whatever lease is held (the component stops).
    pub fn revoke_any(&mut self, revocation: Revocation) -> Option<(Lease, Revocation)> {
        let lease = self.lease.take()?;
        Some((lease, revocation))
    }

    /// Ends lease `id` normally (the Deck went away).
    pub fn release(&mut self, id: u64) {
        if self.lease.is_some_and(|lease| lease.id == id) {
            self.lease = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONSOLE: u32 = 1;

    fn leased() -> (LeaseAuthority, Lease) {
        let mut authority = LeaseAuthority::new();
        let lease = authority
            .grant(authority.generation(), CONSOLE)
            .expect("granted");
        (authority, lease)
    }

    #[test]
    fn wts_codes_map_to_changes() {
        assert_eq!(
            SessionChange::from_wts(1, 3),
            SessionChange::ConsoleConnect(3)
        );
        assert_eq!(
            SessionChange::from_wts(2, 3),
            SessionChange::ConsoleDisconnect(3)
        );
        assert_eq!(
            SessionChange::from_wts(3, 3),
            SessionChange::RemoteConnect(3)
        );
        assert_eq!(
            SessionChange::from_wts(4, 3),
            SessionChange::RemoteDisconnect(3)
        );
        assert_eq!(SessionChange::from_wts(5, 3), SessionChange::Logon(3));
        assert_eq!(SessionChange::from_wts(6, 3), SessionChange::Logoff(3));
        assert_eq!(SessionChange::from_wts(7, 3), SessionChange::Lock(3));
        assert_eq!(SessionChange::from_wts(8, 3), SessionChange::Unlock(3));
        assert_eq!(SessionChange::from_wts(9, 3), SessionChange::Other);
        assert_eq!(SessionChange::from_wts(0, 3), SessionChange::Other);
    }

    #[test]
    fn a_console_disconnect_switch_logoff_or_remote_takeover_revokes() {
        for (change, expected) in [
            (
                SessionChange::ConsoleDisconnect(CONSOLE),
                Revocation::ConsoleDisconnected,
            ),
            (
                SessionChange::ConsoleConnect(2),
                Revocation::ConsoleSwitched { to: 2 },
            ),
            (
                SessionChange::RemoteConnect(CONSOLE),
                Revocation::RemoteTakeover,
            ),
            (SessionChange::Logoff(CONSOLE), Revocation::LoggedOff),
        ] {
            let (mut authority, lease) = leased();
            assert_eq!(
                authority.on_change(change),
                Some((lease, expected)),
                "{change:?}"
            );
            assert_eq!(authority.lease(), None, "{change:?} ends the lease");
        }
    }

    #[test]
    fn a_lock_or_unlock_by_the_same_user_keeps_the_lease() {
        let (mut authority, lease) = leased();
        let generation = authority.generation();
        for change in [
            SessionChange::Lock(CONSOLE),
            SessionChange::Unlock(CONSOLE),
            SessionChange::Other,
        ] {
            assert_eq!(authority.on_change(change), None);
        }
        assert_eq!(authority.lease(), Some(lease));
        assert_eq!(
            authority.generation(),
            generation,
            "lock does not move the console"
        );
    }

    #[test]
    fn other_sessions_coming_and_going_keep_the_lease() {
        let (mut authority, lease) = leased();
        for change in [
            SessionChange::ConsoleConnect(CONSOLE),
            SessionChange::ConsoleDisconnect(2),
            SessionChange::RemoteConnect(2),
            SessionChange::RemoteDisconnect(2),
            SessionChange::Logon(2),
            SessionChange::Logoff(2),
            SessionChange::Lock(2),
        ] {
            assert_eq!(authority.on_change(change), None, "{change:?}");
        }
        assert_eq!(authority.lease(), Some(lease));
    }

    #[test]
    fn a_console_change_during_verification_refuses_the_grant() {
        let mut authority = LeaseAuthority::new();
        let observed = authority.generation();
        assert_eq!(authority.on_change(SessionChange::ConsoleConnect(2)), None);
        assert!(authority.grant(observed, CONSOLE).is_err());
        assert!(authority.grant(authority.generation(), CONSOLE).is_ok());
    }

    #[test]
    fn no_console_session_or_a_second_lease_is_refused() {
        let mut authority = LeaseAuthority::new();
        assert!(authority.grant(0, NO_CONSOLE_SESSION).is_err());
        let (mut authority, _) = leased();
        assert!(authority.grant(authority.generation(), CONSOLE).is_err());
    }

    #[test]
    fn the_observed_console_backs_up_the_notifications() {
        let (mut authority, lease) = leased();
        assert_eq!(authority.on_console_observed(CONSOLE), None);
        assert_eq!(
            authority.on_console_observed(4),
            Some((lease, Revocation::ConsoleSwitched { to: 4 }))
        );
        let (mut authority, lease) = leased();
        assert_eq!(
            authority.on_console_observed(NO_CONSOLE_SESSION),
            Some((lease, Revocation::ConsoleDisconnected))
        );
        assert_eq!(
            authority.on_console_observed(7),
            None,
            "nothing left to revoke"
        );
    }

    #[test]
    fn client_exit_and_stop_revoke_only_the_current_lease() {
        let (mut authority, lease) = leased();
        assert_eq!(
            authority.revoke(lease.id + 1, Revocation::ClientExited),
            None
        );
        assert_eq!(
            authority.revoke(lease.id, Revocation::ClientExited),
            Some((lease, Revocation::ClientExited))
        );
        let (mut authority, lease) = leased();
        assert_eq!(
            authority.revoke_any(Revocation::ComponentStopping),
            Some((lease, Revocation::ComponentStopping))
        );
        assert_eq!(authority.revoke_any(Revocation::ComponentStopping), None);
    }

    #[test]
    fn a_normal_release_frees_the_lease_and_ids_never_repeat() {
        let (mut authority, first) = leased();
        authority.release(first.id + 1);
        assert_eq!(
            authority.lease(),
            Some(first),
            "a stale id releases nothing"
        );
        authority.release(first.id);
        assert_eq!(authority.lease(), None);
        let second = authority
            .grant(authority.generation(), CONSOLE)
            .expect("granted");
        assert_ne!(second.id, first.id);
    }

    #[test]
    fn every_revocation_says_the_tablet_was_released() {
        for revocation in [
            Revocation::ConsoleDisconnected,
            Revocation::ConsoleSwitched { to: 2 },
            Revocation::RemoteTakeover,
            Revocation::LoggedOff,
            Revocation::ClientExited,
            Revocation::ComponentStopping,
        ] {
            assert!(
                revocation
                    .to_string()
                    .starts_with("the tablet was released: ")
            );
        }
    }
}
