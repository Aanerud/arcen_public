// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The confirmation transaction for signing another account out.
//!
//! An offer is bound to the connection that asked, the authenticated
//! requester, the exact target session (account, native id, boot and
//! generation), the ownership epoch the facts were read at, a one-use nonce
//! and an expiry. One [`SwitchLedger`] serves one desktop and holds at most
//! one live transaction: a contender is refused while another waits or
//! commits.
//!
//! Confirming revalidates the facts read immediately before: the same
//! session, the same epoch, still no remote client attached. Anything else
//! (an expired, cancelled, replayed or mismatched reply, a returning client,
//! a changed or reused session id, a Pier restart that lost the ledger)
//! rejects and changes nothing. Only a confirmed transaction yields the
//! [`LogoutAuthority`] a Pier needs to ask for the graceful logout.
//!
//! Every stage is bounded. An offer expires; a confirmed logout is let go
//! after [`LOGOUT_COMMIT_DEADLINE`] even if its authority was lost; and once
//! the earlier session ended, the console is reserved for the requester's
//! own sign-in for at most [`ENTRY_DEADLINE`]. Meanwhile [`ConsoleHold`]
//! keeps every other sign-in out ([`SwitchLedger::decide`]): the owner of a
//! session being signed out is told so, anyone else that a change of
//! account is in progress.

use std::collections::VecDeque;
use std::fmt::{self, Debug, Formatter};
use std::time::Duration;

use subtle::ConstantTimeEq;

use super::logout::{
    EntryPermit, LOGOUT_COMPLETION_TIMEOUT, LogoutAuthority, LogoutOutcome, LogoutVerdict,
};
use super::{
    AttachmentEpoch, AuthenticatedPrincipal, ConsoleOccupant, HandoverDecision, HandoverReason,
    HandoverRefusal, NativeSessionIdentity, RemoteAttachment, RemoteClient, SwitchCandidate,
    SwitchRoute, decide_handover,
};
use crate::direct_reconnect::MonotonicMillis;

/// Bytes in a [`SwitchNonce`].
pub const SWITCH_NONCE_BYTES: usize = 32;

/// How long an offer stands by default, in seconds: long enough to read the
/// warning and decide, short enough that the facts it was made on are
/// recent.
pub const DEFAULT_OFFER_LIFETIME_SECS: u32 = 120;

/// The longest an offer may stand, in seconds (the wire's bound too).
pub const MAX_OFFER_LIFETIME_SECS: u32 = 600;

/// Nonces remembered after use, so a replay is told apart from noise.
const REMEMBERED_NONCES: usize = 32;

/// The Pier's own id for one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// Wraps the Pier's id.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A one-use offer nonce: 32 random bytes the Pier draws from the
/// operating system for each offer. Compared in constant time; never shown
/// in `Debug`.
#[derive(Clone)]
pub struct SwitchNonce([u8; SWITCH_NONCE_BYTES]);

impl SwitchNonce {
    /// A nonce from the Pier's random bytes; `None` for all zeros (an
    /// uninitialised buffer is not a nonce).
    #[must_use]
    pub fn from_random(bytes: [u8; SWITCH_NONCE_BYTES]) -> Option<Self> {
        (bytes != [0; SWITCH_NONCE_BYTES]).then_some(Self(bytes))
    }

    /// The nonce as 64 lowercase hexadecimal characters, its wire form.
    #[must_use]
    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(SWITCH_NONCE_BYTES * 2);
        for byte in self.0 {
            text.push(char::from(DIGITS[usize::from(byte >> 4)]));
            text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        text
    }

    /// Parses [`Self::to_hex`]; anything else (uppercase, another length)
    /// is `None`.
    #[must_use]
    pub fn parse_hex(text: &str) -> Option<Self> {
        fn digit(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            }
        }
        let text = text.as_bytes();
        if text.len() != SWITCH_NONCE_BYTES * 2 {
            return None;
        }
        let mut bytes = [0; SWITCH_NONCE_BYTES];
        for (byte, pair) in bytes.iter_mut().zip(text.chunks_exact(2)) {
            *byte = (digit(pair[0])? << 4) | digit(pair[1])?;
        }
        Self::from_random(bytes)
    }

    fn matches(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Debug for SwitchNonce {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("SwitchNonce(<redacted>)")
    }
}

/// What the Pier sends the Deck for an opened offer.
#[derive(Debug, Clone)]
pub struct OfferTicket {
    /// The offer's nonce, sent as its id.
    pub nonce: SwitchNonce,
    /// Seconds until the offer expires.
    pub expires_in_secs: u32,
}

/// Why an offer was not opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenRefusal {
    /// Another change of account is waiting for its decision or its logout.
    InProgress,
    /// The nonce was used before.
    NonceReused,
    /// The lifetime was zero or longer than [`MAX_OFFER_LIFETIME_SECS`].
    Lifetime,
}

impl OpenRefusal {
    /// The refusal a contender is shown:
    /// [`HandoverRefusal::SwitchInProgress`], whose code follows the Deck's
    /// capability ([`HandoverRefusal::console_refusal`]). The others are
    /// the Pier's own faults (it draws a fresh nonce, or uses a valid
    /// lifetime) and are never sent.
    #[must_use]
    pub const fn handover_refusal(self) -> Option<HandoverRefusal> {
        match self {
            Self::InProgress => Some(HandoverRefusal::SwitchInProgress),
            Self::NonceReused | Self::Lifetime => None,
        }
    }

    /// The reason logged.
    #[must_use]
    pub const fn reason(self) -> HandoverReason {
        match self {
            Self::InProgress => HandoverReason::SwitchInProgress,
            Self::NonceReused | Self::Lifetime => HandoverReason::SwitchRejected,
        }
    }
}

/// Why the facts no longer support a confirmed offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// A remote client attached to the target since the offer.
    RemoteAttached(RemoteClient),
    /// Whether a remote client is attached can no longer be proved.
    AttachmentUnknown,
    /// Who holds the desktop can no longer be read.
    OccupantUnknown,
    /// Another session, or the same id in another boot or generation, or
    /// nobody, holds the desktop now.
    TargetChanged,
    /// A client attached and left again since the offer.
    EpochChanged,
}

/// Why a reply was rejected. Nothing changes in any case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyRejection {
    /// No offer is live (none was made, or the Pier restarted since).
    NoOffer,
    /// The nonce was already used.
    Replayed,
    /// The reply came on another connection than the offer's.
    WrongConnection,
    /// The nonce is not the live offer's. The offer is withdrawn.
    Mismatched,
    /// The offer expired. It is withdrawn.
    Expired,
    /// The facts changed. The offer is withdrawn.
    Stale(StaleReason),
}

impl ReplyRejection {
    /// The reason logged.
    #[must_use]
    pub const fn reason(self) -> HandoverReason {
        match self {
            Self::NoOffer | Self::Replayed | Self::WrongConnection | Self::Mismatched => {
                HandoverReason::SwitchRejected
            }
            Self::Expired => HandoverReason::SwitchExpired,
            Self::Stale(_) => HandoverReason::SwitchStale,
        }
    }
}

/// What a returning remote client did to the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientReturn {
    /// Nothing was waiting.
    NoOffer,
    /// An unconfirmed offer was withdrawn ([`HandoverReason::SwitchStale`]).
    OfferWithdrawn,
    /// A confirmed logout is already in flight. The operating system's
    /// logout decides; a Pier does not attach the returning client to a
    /// session it is signing out ([`HandoverRefusal::SigningOut`]).
    LogoutInFlight,
    /// The earlier session ended and the requester is signing in; the
    /// console is theirs until that ends.
    EntryPending,
}

/// A transaction the ledger let go because its time ran out, so the caller
/// can log it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lapse {
    /// An unconfirmed offer expired ([`HandoverReason::SwitchExpired`]).
    OfferExpired,
    /// A confirmed logout reported no outcome within
    /// [`LOGOUT_COMMIT_DEADLINE`] (its [`LogoutAuthority`] was lost, or the
    /// Pier never finished it) ([`HandoverReason::LogoutTimedOut`]).
    LogoutOverdue,
    /// The requester did not finish signing in within
    /// [`ENTRY_DEADLINE`] ([`HandoverReason::NewLoginFailed`]).
    EntryOverdue,
}

impl Lapse {
    /// The reason logged.
    #[must_use]
    pub const fn reason(self) -> HandoverReason {
        match self {
            Self::OfferExpired => HandoverReason::SwitchExpired,
            Self::LogoutOverdue => HandoverReason::LogoutTimedOut,
            Self::EntryOverdue => HandoverReason::NewLoginFailed,
        }
    }
}

/// How long the ledger keeps a confirmed logout before it lets go of it:
/// [`LOGOUT_COMPLETION_TIMEOUT`] and a margin for the Pier to read the
/// session back. A Pier that loses its [`LogoutAuthority`] (a panic, an
/// early return, a cancelled task) therefore holds the desktop no longer.
pub const LOGOUT_COMMIT_DEADLINE: Duration =
    Duration::from_secs(LOGOUT_COMPLETION_TIMEOUT.as_secs() + LOGOUT_READ_BACK_MARGIN.as_secs());

/// The margin in [`LOGOUT_COMMIT_DEADLINE`].
pub const LOGOUT_READ_BACK_MARGIN: Duration = Duration::from_secs(30);

/// How long the console stays reserved for the requester after the
/// earlier session ended, for its sign-in through the platform's own login.
pub const ENTRY_DEADLINE: Duration = Duration::from_secs(120);

/// What the ledger reserves on the desktop now, as seen by one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleHold<'a> {
    /// Nothing.
    Free,
    /// An offer to sign `target` out waits for its answer. Decisions are
    /// unchanged: the owner's return withdraws it, and a contender's offer
    /// is refused when opened.
    Offered(&'a NativeSessionIdentity),
    /// `target` is being signed out.
    SigningOut(&'a NativeSessionIdentity),
    /// The earlier session ended and another connection's requester is
    /// signing in. The permit holder's own connection sees [`Self::Free`].
    Entering(&'a AuthenticatedPrincipal),
}

/// A decision made against the ledger's reservation
/// ([`SwitchLedger::decide`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerDecision {
    /// What the sign-in does.
    pub decision: HandoverDecision,
    /// A transaction the ledger let go first because its time ran out.
    pub lapsed: Option<Lapse>,
}

/// An opened offer.
#[derive(Debug, Clone)]
pub struct Opened {
    /// What the Pier sends the Deck.
    pub ticket: OfferTicket,
    /// A transaction the ledger let go first because its time ran out.
    pub lapsed: Option<Lapse>,
}

struct Transaction {
    connection: ConnectionId,
    requester: AuthenticatedPrincipal,
    target: NativeSessionIdentity,
    epoch: AttachmentEpoch,
    nonce: SwitchNonce,
    expires_at: MonotonicMillis,
}

enum Slot {
    Idle,
    Open(Box<Transaction>),
    Committing {
        connection: ConnectionId,
        target: NativeSessionIdentity,
        nonce: SwitchNonce,
        deadline: MonotonicMillis,
        requester_left: bool,
    },
    Entering {
        connection: ConnectionId,
        requester: AuthenticatedPrincipal,
        deadline: MonotonicMillis,
    },
}

/// The one confirmation transaction of a desktop.
pub struct SwitchLedger {
    slot: Slot,
    used: VecDeque<SwitchNonce>,
}

impl Debug for SwitchLedger {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let state = match &self.slot {
            Slot::Idle => "idle",
            Slot::Open(_) => "open",
            Slot::Committing { .. } => "committing",
            Slot::Entering { .. } => "entering",
        };
        formatter
            .debug_struct("SwitchLedger")
            .field("state", &state)
            .field("used", &self.used.len())
            .finish()
    }
}

impl Default for SwitchLedger {
    fn default() -> Self {
        Self::new()
    }
}

fn after(now: MonotonicMillis, duration: Duration) -> MonotonicMillis {
    MonotonicMillis::new(now.get().saturating_add(duration.as_millis()))
}

impl SwitchLedger {
    /// An idle ledger. A Pier that restarts starts here: every earlier
    /// offer is gone, and a reply naming one is [`ReplyRejection::NoOffer`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slot: Slot::Idle,
            used: VecDeque::new(),
        }
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self.slot, Slot::Idle)
    }

    /// Whether `target` is being signed out right now.
    #[must_use]
    pub fn is_signing_out(&self, target: &NativeSessionIdentity) -> bool {
        matches!(&self.slot, Slot::Committing { target: committing, .. } if committing == target)
    }

    /// What the ledger reserves, as seen by `connection`.
    #[must_use]
    pub fn hold(&self, connection: ConnectionId) -> ConsoleHold<'_> {
        match &self.slot {
            Slot::Idle => ConsoleHold::Free,
            Slot::Open(transaction) => ConsoleHold::Offered(&transaction.target),
            Slot::Committing { target, .. } => ConsoleHold::SigningOut(target),
            Slot::Entering {
                connection: holder, ..
            } if *holder == connection => ConsoleHold::Free,
            Slot::Entering { requester, .. } => ConsoleHold::Entering(requester),
        }
    }

    /// [`decide_handover`] for `requester` on `connection`, against what
    /// the ledger reserves at `now` (after letting go of anything overdue).
    pub fn decide(
        &mut self,
        connection: ConnectionId,
        requester: &AuthenticatedPrincipal,
        occupant: &ConsoleOccupant,
        route: SwitchRoute,
        now: MonotonicMillis,
    ) -> LedgerDecision {
        let lapsed = self.expire(now);
        LedgerDecision {
            decision: decide_handover(requester, occupant, route, self.hold(connection)),
            lapsed,
        }
    }

    /// Opens an offer for `candidate` on `connection`, standing for
    /// `lifetime_secs` from `now`. Anything overdue is let go first
    /// ([`Opened::lapsed`]).
    ///
    /// # Errors
    ///
    /// [`OpenRefusal::InProgress`] while another transaction is live, and
    /// the others for a reused nonce or an invalid lifetime.
    pub fn open(
        &mut self,
        candidate: SwitchCandidate,
        connection: ConnectionId,
        nonce: SwitchNonce,
        now: MonotonicMillis,
        lifetime_secs: u32,
    ) -> Result<Opened, OpenRefusal> {
        if lifetime_secs == 0 || lifetime_secs > MAX_OFFER_LIFETIME_SECS {
            return Err(OpenRefusal::Lifetime);
        }
        if self.was_used(&nonce) {
            return Err(OpenRefusal::NonceReused);
        }
        let lapsed = self.expire(now);
        if !self.is_idle() {
            return Err(OpenRefusal::InProgress);
        }
        let SwitchCandidate {
            requester,
            target,
            epoch,
        } = candidate;
        self.slot = Slot::Open(Box::new(Transaction {
            connection,
            requester,
            target,
            epoch,
            nonce: nonce.clone(),
            expires_at: after(now, Duration::from_secs(u64::from(lifetime_secs))),
        }));
        Ok(Opened {
            ticket: OfferTicket {
                nonce,
                expires_in_secs: lifetime_secs,
            },
            lapsed,
        })
    }

    /// The requester confirmed. `current`: the facts read immediately
    /// before this call. On success the transaction commits until
    /// [`LOGOUT_COMMIT_DEADLINE`] from `now`, and the Pier asks the
    /// operating system for the graceful logout of exactly
    /// [`LogoutAuthority::target`] ([`HandoverReason::LogoutRequested`]).
    ///
    /// # Errors
    ///
    /// A [`ReplyRejection`]; nothing is signed out.
    pub fn confirm(
        &mut self,
        connection: ConnectionId,
        presented: &SwitchNonce,
        now: MonotonicMillis,
        current: &ConsoleOccupant,
    ) -> Result<LogoutAuthority, ReplyRejection> {
        let transaction = self.take_open(connection, presented, now)?;
        if let Err(stale) = revalidate(&transaction, current) {
            self.retire(transaction.nonce);
            return Err(ReplyRejection::Stale(stale));
        }
        let Transaction {
            connection,
            requester,
            target,
            nonce,
            ..
        } = *transaction;
        self.slot = Slot::Committing {
            connection,
            target: target.clone(),
            nonce: nonce.clone(),
            deadline: after(now, LOGOUT_COMMIT_DEADLINE),
            requester_left: false,
        };
        Ok(LogoutAuthority::new(connection, requester, target, nonce))
    }

    /// The requester cancelled. The offer is withdrawn
    /// ([`HandoverReason::SwitchCancelled`]).
    ///
    /// # Errors
    ///
    /// A [`ReplyRejection`] for a reply that names no live offer of this
    /// connection; an expired offer is withdrawn as expired.
    pub fn cancel(
        &mut self,
        connection: ConnectionId,
        presented: &SwitchNonce,
        now: MonotonicMillis,
    ) -> Result<(), ReplyRejection> {
        let transaction = self.take_open(connection, presented, now)?;
        self.retire(transaction.nonce);
        Ok(())
    }

    /// `connection` closed. Its unconfirmed offer is withdrawn
    /// ([`HandoverReason::SwitchCancelled`]), and its reservation for a
    /// sign-in is released ([`HandoverReason::NewLoginFailed`]). A logout
    /// already asked for is not undone: it runs to its end, and
    /// [`Self::finish_logout`] then reserves nothing for the departed
    /// requester ([`LogoutVerdict::CompletedRequesterGone`]).
    pub fn requester_left(&mut self, connection: ConnectionId) -> Option<HandoverReason> {
        if let Slot::Committing {
            connection: holder,
            requester_left,
            ..
        } = &mut self.slot
        {
            if *holder == connection {
                *requester_left = true;
            }
            return None;
        }
        match &self.slot {
            Slot::Open(transaction) if transaction.connection == connection => {
                self.withdraw();
                Some(HandoverReason::SwitchCancelled)
            }
            Slot::Entering {
                connection: holder, ..
            } if *holder == connection => {
                self.slot = Slot::Idle;
                Some(HandoverReason::NewLoginFailed)
            }
            _ => None,
        }
    }

    /// A remote client attached to the desktop (its owner returned, or
    /// another product connected): an unconfirmed offer is stale.
    pub fn remote_client_returned(&mut self) -> ClientReturn {
        match &self.slot {
            Slot::Idle => ClientReturn::NoOffer,
            Slot::Open(_) => {
                self.withdraw();
                ClientReturn::OfferWithdrawn
            }
            Slot::Committing { .. } => ClientReturn::LogoutInFlight,
            Slot::Entering { .. } => ClientReturn::EntryPending,
        }
    }

    /// Lets go of whatever ran out of time at `now`: an unconfirmed offer
    /// past its expiry, a confirmed logout past [`LOGOUT_COMMIT_DEADLINE`]
    /// (its nonce stays spent), a sign-in reservation past
    /// [`ENTRY_DEADLINE`]. Returns what it let go, for the log.
    pub fn expire(&mut self, now: MonotonicMillis) -> Option<Lapse> {
        let lapse = match &self.slot {
            Slot::Open(transaction) if now >= transaction.expires_at => Lapse::OfferExpired,
            Slot::Committing { deadline, .. } if now >= *deadline => Lapse::LogoutOverdue,
            Slot::Entering { deadline, .. } if now >= *deadline => Lapse::EntryOverdue,
            _ => return None,
        };
        match std::mem::replace(&mut self.slot, Slot::Idle) {
            Slot::Open(transaction) => self.retire(transaction.nonce),
            Slot::Committing { nonce, .. } => self.retire(nonce),
            Slot::Idle | Slot::Entering { .. } => {}
        }
        Some(lapse)
    }

    /// The graceful logout `authority` asked for ended with `outcome` at
    /// `now`. Only a completed logout with proof that exactly the target
    /// ended lets the requester in: the console is then reserved for its
    /// sign-in until [`Self::login_finished`] or [`ENTRY_DEADLINE`], unless
    /// its connection closed meanwhile
    /// ([`LogoutVerdict::CompletedRequesterGone`]). Any other outcome leaves
    /// the ledger idle.
    pub fn finish_logout(
        &mut self,
        authority: LogoutAuthority,
        outcome: LogoutOutcome,
        now: MonotonicMillis,
    ) -> LogoutVerdict {
        let ours = matches!(
            &self.slot,
            Slot::Committing { connection, target, nonce, .. }
                if *connection == authority.connection()
                    && target == authority.target()
                    && nonce.matches(authority.nonce())
        );
        if !ours {
            return super::logout::verdict(authority, outcome, false, false);
        }
        let mut present = true;
        if let Slot::Committing {
            nonce,
            requester_left,
            ..
        } = std::mem::replace(&mut self.slot, Slot::Idle)
        {
            self.retire(nonce);
            present = !requester_left;
        }
        let verdict = super::logout::verdict(authority, outcome, true, present);
        if let LogoutVerdict::Completed { permit, .. } = &verdict {
            self.slot = Slot::Entering {
                connection: permit.connection(),
                requester: permit.requester().clone(),
                deadline: after(now, ENTRY_DEADLINE),
            };
        }
        verdict
    }

    /// The permit holder's sign-in ended; `signed_in`: it succeeded. The
    /// reservation is released. Returns
    /// [`HandoverReason::NewLoginFailed`] for a failed sign-in.
    pub fn login_finished(
        &mut self,
        permit: EntryPermit,
        signed_in: bool,
    ) -> Option<HandoverReason> {
        let (holder, account) = permit.into_parts();
        let ours = matches!(
            &self.slot,
            Slot::Entering { connection, requester, .. }
                if *connection == holder && *requester == account
        );
        if ours {
            self.slot = Slot::Idle;
        }
        (!signed_in).then_some(HandoverReason::NewLoginFailed)
    }

    fn take_open(
        &mut self,
        connection: ConnectionId,
        presented: &SwitchNonce,
        now: MonotonicMillis,
    ) -> Result<Box<Transaction>, ReplyRejection> {
        let transaction = match std::mem::replace(&mut self.slot, Slot::Idle) {
            Slot::Open(transaction) if transaction.connection == connection => transaction,
            other => {
                let elsewhere = matches!(other, Slot::Open(_));
                self.slot = other;
                return Err(if elsewhere {
                    ReplyRejection::WrongConnection
                } else if self.was_used(presented) {
                    ReplyRejection::Replayed
                } else {
                    ReplyRejection::NoOffer
                });
            }
        };
        if !transaction.nonce.matches(presented) {
            self.retire(transaction.nonce);
            return Err(if self.was_used(presented) {
                ReplyRejection::Replayed
            } else {
                ReplyRejection::Mismatched
            });
        }
        if now >= transaction.expires_at {
            self.retire(transaction.nonce);
            return Err(ReplyRejection::Expired);
        }
        Ok(transaction)
    }

    fn withdraw(&mut self) {
        if let Slot::Open(transaction) = std::mem::replace(&mut self.slot, Slot::Idle) {
            self.retire(transaction.nonce);
        }
    }

    fn retire(&mut self, nonce: SwitchNonce) {
        if self.used.len() == REMEMBERED_NONCES {
            self.used.pop_front();
        }
        self.used.push_back(nonce);
    }

    fn was_used(&self, nonce: &SwitchNonce) -> bool {
        let live = match &self.slot {
            Slot::Committing { nonce: live, .. } => live.matches(nonce),
            Slot::Open(transaction) => transaction.nonce.matches(nonce),
            Slot::Idle | Slot::Entering { .. } => false,
        };
        live || self.used.iter().any(|used| used.matches(nonce))
    }
}

fn revalidate(transaction: &Transaction, current: &ConsoleOccupant) -> Result<(), StaleReason> {
    let facts = match current {
        ConsoleOccupant::Unknown => return Err(StaleReason::OccupantUnknown),
        ConsoleOccupant::Nobody => return Err(StaleReason::TargetChanged),
        ConsoleOccupant::Session(facts) => facts,
    };
    if facts.identity != transaction.target {
        return Err(StaleReason::TargetChanged);
    }
    match facts.attachment {
        RemoteAttachment::Attached(client) => return Err(StaleReason::RemoteAttached(client)),
        RemoteAttachment::Unknown => return Err(StaleReason::AttachmentUnknown),
        RemoteAttachment::ProvedDetached => {}
    }
    if facts.epoch != transaction.epoch {
        return Err(StaleReason::EpochChanged);
    }
    if facts.identity.principal() == transaction.requester.principal() {
        return Err(StaleReason::TargetChanged);
    }
    Ok(())
}
