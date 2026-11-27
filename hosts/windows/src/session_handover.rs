// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows Pier's facts for `arcen_session::session_handover` (ADR 0025
//! §13): who holds the desktop (SID, WTS session, logon, boot), whether a
//! remote client is attached to it, and the one switch ledger the broker's
//! connections share. The decisions are the shared crate's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use arcen_session::direct_reconnect::MonotonicMillis;
use arcen_session::session_handover::consent::{
    ClientReturn, ConnectionId, ConsoleHold, SwitchLedger,
};
use arcen_session::session_handover::{
    AttachmentEpoch, BootId, ConsoleOccupant, DeskState, NativePrincipal, NativeSessionId,
    NativeSessionIdentity, RemoteAttachment, RemoteClient, SessionFacts, SwitchRoute,
};

use crate::windows_session::HolderReading;

/// Whether this Pier can sign an account out for a confirmed change of
/// account: the ordinary sign-out asked from inside the target session
/// (`arcen-pier sign-out-session`, `ExitWindowsEx(EWX_LOGOFF)` without a
/// force flag), and the exact WTS session read back gone. A Deck is offered
/// the change only while this holds; otherwise another account is refused
/// as before (`PierCannotSignOut`).
///
/// Off: Windows carries out that sign-out only for an unlocked session. A
/// locked or disconnected one (every session a Deck left: disconnect locks)
/// stays signed in and the request is dropped without an error (Windows 11,
/// measured on the lab, 2026-11-26). The only other routes force the
/// sign-out (`EWX_FORCE`, `WTSLogoffSession`), which Arcen never does.
pub(crate) const GRACEFUL_SIGN_OUT: bool = false;

/// Whether this connection can carry a confirmed change of account.
pub(crate) const fn route(deck_account_switch_v1: bool) -> SwitchRoute {
    SwitchRoute::from_capabilities(deck_account_switch_v1, GRACEFUL_SIGN_OUT)
}

/// The shared state of the desktop this Pier serves: the switch ledger, the
/// attachment epoch and the connection counter. One per service; a restart
/// forgets every offer.
pub(crate) struct HandoverDesk {
    ledger: Mutex<SwitchLedger>,
    epoch: AtomicU64,
    connections: AtomicU64,
    started: std::time::Instant,
}

impl Default for HandoverDesk {
    fn default() -> Self {
        Self::new()
    }
}

impl HandoverDesk {
    pub(crate) fn new() -> Self {
        Self {
            ledger: Mutex::new(SwitchLedger::new()),
            epoch: AtomicU64::new(0),
            connections: AtomicU64::new(0),
            started: std::time::Instant::now(),
        }
    }

    /// A new connection's id.
    pub(crate) fn connection(&self) -> ConnectionId {
        ConnectionId::new(self.connections.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub(crate) fn now(&self) -> MonotonicMillis {
        MonotonicMillis::new(self.started.elapsed().as_millis())
    }

    pub(crate) fn epoch(&self) -> AttachmentEpoch {
        AttachmentEpoch::new(self.epoch.load(Ordering::SeqCst))
    }

    /// An Arcen Deck attaches to `session`: refused (`Err`) while a
    /// confirmed change of account signs that session out, or signs out a
    /// session that could not be named; otherwise the epoch moves on and an
    /// open offer is withdrawn. One ledger hold, so a confirmation sees
    /// either this attachment or none.
    pub(crate) fn attach(
        &self,
        session: Option<&NativeSessionIdentity>,
    ) -> Result<ClientReturn, ClientReturn> {
        self.with_ledger(|ledger| {
            let signing_out = match session {
                Some(session) => ledger.is_signing_out(session),
                None => matches!(
                    ledger.hold(ConnectionId::new(0)),
                    ConsoleHold::SigningOut(_)
                ),
            };
            if signing_out {
                return Err(ClientReturn::LogoutInFlight);
            }
            self.epoch.fetch_add(1, Ordering::SeqCst);
            Ok(ledger.remote_client_returned())
        })
    }

    /// An Arcen Deck left a session: the epoch moves on.
    pub(crate) fn detached(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn with_ledger<R>(&self, action: impl FnOnce(&mut SwitchLedger) -> R) -> R {
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        action(&mut ledger)
    }
}

/// The account a Windows SID names.
pub(crate) fn principal(sid: &str) -> Option<NativePrincipal> {
    NativePrincipal::new(sid.to_ascii_uppercase()).ok()
}

/// The native identity of WTS session `session_id` signed in by `logon_id`
/// of `sid`, in `boot`: the logon id is the generation, new for every
/// sign-in within a boot even when Windows reuses the session id.
pub(crate) fn session_identity(
    sid: &str,
    session_id: u32,
    logon_id: u64,
    boot: &str,
) -> Option<NativeSessionIdentity> {
    Some(NativeSessionIdentity::new(
        principal(sid)?,
        NativeSessionId::new(format!("wts:{session_id}")).ok()?,
        BootId::new(boot).ok()?,
        logon_id,
    ))
}

/// Who holds the desktop, in the shared vocabulary. `arcen_attached`: an
/// Arcen Deck's agent serves that account now (attached, or held for its
/// resume). Another product's client is the WTS fact `foreign` (a session
/// off the console that Windows reports `Active` or `Connected`); anything
/// unreadable, the boot included, is unknown.
///
/// Nothing attached is never claimed: a product that serves the console
/// session itself (its own agent capturing the console, as a remote-access
/// server process does) leaves no trace in WTS, and this Pier has no proof
/// for those routes yet, so an idle-looking session is
/// [`RemoteAttachment::Unknown`]. Its own account still joins it (keeping
/// the layout, as the shared rule says); another account is refused.
pub(crate) fn occupant(
    reading: &HolderReading,
    boot: Option<&str>,
    epoch: AttachmentEpoch,
    arcen_attached: bool,
) -> ConsoleOccupant {
    match reading {
        HolderReading::Nobody => ConsoleOccupant::Nobody,
        HolderReading::Unknown => ConsoleOccupant::Unknown,
        HolderReading::Session {
            session_id,
            owner,
            foreign,
            locked,
            ..
        } => {
            let Some(identity) = boot
                .and_then(|boot| session_identity(&owner.sid, *session_id, owner.logon_id, boot))
            else {
                return ConsoleOccupant::Unknown;
            };
            let attachment = if arcen_attached {
                RemoteAttachment::Attached(RemoteClient::Arcen)
            } else if *foreign {
                RemoteAttachment::Attached(RemoteClient::Foreign)
            } else {
                RemoteAttachment::Unknown
            };
            ConsoleOccupant::Session(SessionFacts {
                identity,
                attachment,
                epoch,
                desk: match locked {
                    Some(true) => DeskState::Locked,
                    Some(false) => DeskState::Unlocked,
                    None => DeskState::Unknown,
                },
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows_session::SessionOwner;
    use arcen_session::console_session::guest_layout::{GuestLayoutPolicy, ShownBecause};
    use arcen_session::session_handover::consent::{SwitchNonce, DEFAULT_OFFER_LIFETIME_SECS};
    use arcen_session::session_handover::{
        AuthenticatedPrincipal, HandoverDecision, HandoverRefusal,
    };

    const ARTIST: &str = "S-1-5-21-1-2-3-1001";
    const EDITOR: &str = "S-1-5-21-1-2-3-1002";

    fn held(sid: &str, own: bool, foreign: bool) -> HolderReading {
        HolderReading::Session {
            session_id: 3,
            owner: SessionOwner {
                sid: sid.to_string(),
                logon_id: 0x1_0000_0042,
            },
            own,
            foreign,
            locked: Some(true),
        }
    }

    fn requester(sid: &str) -> AuthenticatedPrincipal {
        AuthenticatedPrincipal::after_authentication(principal(sid).unwrap())
    }

    fn decide(
        desk: &HandoverDesk,
        connection: ConnectionId,
        requester_sid: &str,
        occupant: &ConsoleOccupant,
        route: SwitchRoute,
    ) -> HandoverDecision {
        desk.with_ledger(|ledger| {
            ledger
                .decide(
                    connection,
                    &requester(requester_sid),
                    occupant,
                    route,
                    desk.now(),
                )
                .decision
        })
    }

    fn read(desk: &HandoverDesk, reading: &HolderReading, arcen: bool) -> ConsoleOccupant {
        occupant(reading, Some("boot-a"), desk.epoch(), arcen)
    }

    #[test]
    fn the_own_session_is_joined_whatever_is_attached() {
        let desk = HandoverDesk::new();
        let connection = desk.connection();
        for (foreign, because) in [
            (true, ShownBecause::ClientAttached),
            (false, ShownBecause::ClientUnknown),
        ] {
            let occupant = read(&desk, &held(ARTIST, true, foreign), false);
            assert!(matches!(
                decide(&desk, connection, ARTIST, &occupant, route(true)),
                HandoverDecision::JoinOwnSession {
                    layout: GuestLayoutPolicy::Shown(shown),
                    ..
                } if shown == because
            ));
        }
    }

    #[test]
    fn another_account_is_refused_and_never_offered() {
        let desk = HandoverDesk::new();
        let connection = desk.connection();
        let idle = read(&desk, &held(EDITOR, false, false), false);
        // Nothing attached is never proved on Windows, so even a Pier that
        // could sign out would not offer.
        assert_eq!(
            decide(
                &desk,
                connection,
                ARTIST,
                &idle,
                SwitchRoute::from_capabilities(true, true)
            ),
            HandoverDecision::Refuse(HandoverRefusal::AttachmentUnknown)
        );
        assert_eq!(
            decide(
                &desk,
                connection,
                ARTIST,
                &read(&desk, &held(EDITOR, false, false), true),
                route(true)
            ),
            HandoverDecision::Refuse(HandoverRefusal::Busy(RemoteClient::Arcen))
        );
        assert_eq!(
            decide(
                &desk,
                connection,
                ARTIST,
                &read(&desk, &held(EDITOR, false, true), false),
                route(true)
            ),
            HandoverDecision::Refuse(HandoverRefusal::Busy(RemoteClient::Foreign))
        );
        assert_eq!(route(true), SwitchRoute::PierCannotSignOut);
    }

    #[test]
    fn an_unreadable_boot_or_holder_is_unknown_never_nobody() {
        let epoch = AttachmentEpoch::new(0);
        assert_eq!(
            occupant(&held(EDITOR, false, false), None, epoch, false),
            ConsoleOccupant::Unknown
        );
        assert_eq!(
            occupant(&HolderReading::Unknown, Some("boot-a"), epoch, false),
            ConsoleOccupant::Unknown
        );
    }

    #[test]
    fn a_new_logon_in_a_reused_session_id_is_another_session() {
        let first = session_identity(EDITOR, 3, 1, "boot-a").unwrap();
        assert_ne!(first, session_identity(EDITOR, 3, 2, "boot-a").unwrap());
        assert_ne!(first, session_identity(EDITOR, 3, 1, "boot-b").unwrap());
        assert_eq!(
            first.principal(),
            session_identity(&EDITOR.to_ascii_lowercase(), 4, 9, "boot-a")
                .unwrap()
                .principal()
        );
    }

    /// An offer against `EDITOR`'s session, as a Pier that proved nothing
    /// attached would open it (the ledger's own behaviour).
    fn open_offer(desk: &HandoverDesk, connection: ConnectionId) -> (SwitchNonce, ConsoleOccupant) {
        let mut occupant = read(desk, &held(EDITOR, false, false), false);
        if let ConsoleOccupant::Session(facts) = &mut occupant {
            facts.attachment = RemoteAttachment::ProvedDetached;
        }
        let HandoverDecision::OfferSwitch(candidate) = decide(
            desk,
            connection,
            ARTIST,
            &occupant,
            SwitchRoute::from_capabilities(true, true),
        ) else {
            panic!("expected an offer");
        };
        let nonce = SwitchNonce::from_random([9; 32]).unwrap();
        desk.with_ledger(|ledger| {
            ledger
                .open(
                    candidate,
                    connection,
                    nonce.clone(),
                    desk.now(),
                    DEFAULT_OFFER_LIFETIME_SECS,
                )
                .unwrap()
        });
        (nonce, occupant)
    }

    #[test]
    fn an_arcen_attachment_moves_the_epoch_and_withdraws_an_open_offer() {
        let desk = HandoverDesk::new();
        let connection = desk.connection();
        let (nonce, occupant) = open_offer(&desk, connection);
        let before = desk.epoch();
        assert_eq!(desk.attach(None), Ok(ClientReturn::OfferWithdrawn));
        assert!(desk.epoch() > before);
        desk.detached();
        assert!(desk.with_ledger(|ledger| ledger.is_idle()));
        assert!(desk
            .with_ledger(|ledger| ledger.confirm(connection, &nonce, desk.now(), &occupant))
            .is_err());
    }

    #[test]
    fn nobody_attaches_to_a_session_being_signed_out() {
        let desk = HandoverDesk::new();
        let connection = desk.connection();
        let (nonce, occupant) = open_offer(&desk, connection);
        let ConsoleOccupant::Session(facts) = &occupant else {
            panic!("expected a session");
        };
        let target = facts.identity.clone();
        let _authority = desk
            .with_ledger(|ledger| ledger.confirm(connection, &nonce, desk.now(), &occupant))
            .unwrap();
        let before = desk.epoch();
        assert_eq!(
            desk.attach(Some(&target)),
            Err(ClientReturn::LogoutInFlight)
        );
        assert_eq!(desk.attach(None), Err(ClientReturn::LogoutInFlight));
        assert_eq!(desk.epoch(), before);
        let other = session_identity(ARTIST, 5, 7, "boot-a").unwrap();
        assert_eq!(desk.attach(Some(&other)), Ok(ClientReturn::LogoutInFlight));
    }
}
