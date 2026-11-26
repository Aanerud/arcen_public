// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Shared Pier admission: the pre-sign-in limits and deadlines every Pier
//! applies, capacity-one session admission and the bounded reconnect hold.

use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Maximum reconnect hold retained by one local session admission.
pub const MAX_RECONNECT_HOLD_SECONDS: u64 = 2 * 60 * 60;

/// Connections a Pier holds at once before they have signed in.
///
/// Each costs a socket, a TLS or QUIC handshake and a bounded inbound message
/// buffer; a connection past this is refused before any of that is spent.
pub const PREAUTH_CAPACITY: usize = 16;

/// How long a Deck has to answer the sign-in prompt (`auth_request`).
///
/// A Deck shows its sign-in sheet only when the prompt arrives, so this is a
/// person's time to type a user name and password, or fetch them from a
/// password manager. Sixty seconds is what the macOS Pier was qualified with
/// against real Decks (Linux and Windows allowed thirty), and it is the same
/// for every Pier because typing a password does not depend on the host's
/// operating system. The cost is that an idle connection holds one of
/// [`PREAUTH_CAPACITY`] places for up to this long.
pub const AUTH_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// [`AUTH_RESPONSE_TIMEOUT`] when the Pier shows an operator disclaimer that
/// must be read and accepted before signing in.
pub const INTERACTIVE_AUTH_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Connections one source may hold at once before they have signed in, so
/// one machine cannot take every one of the [`PREAUTH_CAPACITY`] places.
///
/// Two, not one: a Deck that reconnects while its previous connection is
/// still timing out needs a second place. A source is an address, and an
/// IPv6 address counts as its /64 ([`source_network`]).
pub const PREAUTH_PER_SOURCE: usize = 2;

/// How long a Deck has to send each message of the handshake that is not a
/// person's answer: the `client_hello` and the quality settings. The Deck
/// sends them at once, so this bounds a peer that connects and says nothing.
pub const HANDSHAKE_MESSAGE_TIMEOUT: Duration = Duration::from_secs(15);

/// The whole application handshake, for a Pier that bounds it as one wait:
/// the sign-in prompt plus a [`HANDSHAKE_MESSAGE_TIMEOUT`] for the messages
/// on either side of it.
pub const APPLICATION_HANDSHAKE_TIMEOUT: Duration = AUTH_RESPONSE_TIMEOUT
    .saturating_add(HANDSHAKE_MESSAGE_TIMEOUT)
    .saturating_add(HANDSHAKE_MESSAGE_TIMEOUT);

/// How long a Pier waits for the operating system to check a password.
///
/// The check may consult a directory server on every Pier (PAM through
/// `sssd` or `pam_krb5`, `LogonUserW`, Open Directory), and a directory that
/// does not answer is the slow case on each: Windows' Kerberos client waits
/// `KdcWaitTime` (10 s) for each of `KdcSendRetries` (3) attempts before it
/// falls back to cached credentials, while `sssd` gives up on a KDC after
/// `krb5_auth_timeout` (6 s) and goes offline. One bound covers both; a
/// check that outlives it is answered as a failure without being counted
/// against the person asking.
pub const CREDENTIAL_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// The network a connection comes from, as admission and the sign-in
/// throttle count it: an IPv4-mapped IPv6 address is its IPv4 address, and
/// any other IPv6 address is its /64, because one host is normally given a
/// whole /64 and could otherwise take a fresh place with every address.
#[must_use]
pub fn source_network(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            let mut segments = v6.segments();
            segments[4..].fill(0);
            IpAddr::V6(Ipv6Addr::from(segments))
        }
        address => address.to_canonical(),
    }
}

/// Why a connection was refused a pre-sign-in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreauthRefusal {
    /// Every place is held.
    Full,
    /// This source already holds its [`PREAUTH_PER_SOURCE`] places.
    SourceBusy,
    /// The connection must prove its address first (a QUIC Retry), see
    /// [`PreauthAdmission::try_enter_unproved`].
    AddressUnproved,
}

impl PreauthRefusal {
    /// Stable code for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "preauth_full",
            Self::SourceBusy => "preauth_source_busy",
            Self::AddressUnproved => "preauth_address_unproved",
        }
    }
}

impl Display for PreauthRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Full => "pre-authentication capacity exhausted",
            Self::SourceBusy => "too many connections from this address are signing in",
            Self::AddressUnproved => "the connection has not proved its address",
        })
    }
}

impl std::error::Error for PreauthRefusal {}

/// The places connections hold between arriving and signing in: at most
/// `capacity` in all and `per_source` from one [`source_network`].
#[derive(Debug)]
pub struct PreauthAdmission {
    capacity: usize,
    per_source: usize,
    held: Mutex<HashMap<IpAddr, usize>>,
}

impl Default for PreauthAdmission {
    /// [`PREAUTH_CAPACITY`] places, [`PREAUTH_PER_SOURCE`] per source.
    fn default() -> Self {
        Self::new(PREAUTH_CAPACITY, PREAUTH_PER_SOURCE)
    }
}

impl PreauthAdmission {
    /// Places for `capacity` connections, `per_source` from one source.
    #[must_use]
    pub fn new(capacity: usize, per_source: usize) -> Self {
        Self {
            capacity,
            per_source,
            held: Mutex::new(HashMap::new()),
        }
    }

    /// Takes a place for a connection from `peer`, held until the returned
    /// place is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`PreauthRefusal::Full`] when every place is held and
    /// [`PreauthRefusal::SourceBusy`] when `peer`'s source holds its share.
    pub fn try_enter(self: &Arc<Self>, peer: IpAddr) -> Result<PreauthPlace, PreauthRefusal> {
        self.enter(peer, true)
    }

    /// [`Self::try_enter`] for a connection whose source address may be
    /// forged: a QUIC Initial that `address_validated` says carries no Retry
    /// token.
    ///
    /// Such a connection must prove its address first when its source
    /// already holds a place or half the places are held. Without this, a
    /// forger could hold another machine's places, or every place, with
    /// handshakes it never completes. With it, a forged source holds at most
    /// one place and forged sources at most half; a real Deck answers the
    /// Retry and takes the place left for it. A Deck's first connection to an
    /// idle Pier costs no extra round trip. The check and the place are taken
    /// under one lock, so a burst of connections cannot all pass the check.
    ///
    /// # Errors
    ///
    /// Returns [`PreauthRefusal::AddressUnproved`] when the caller must send a
    /// stateless Retry instead, otherwise as [`Self::try_enter`].
    pub fn try_enter_unproved(
        self: &Arc<Self>,
        peer: IpAddr,
        address_validated: bool,
    ) -> Result<PreauthPlace, PreauthRefusal> {
        self.enter(peer, address_validated)
    }

    fn enter(
        self: &Arc<Self>,
        peer: IpAddr,
        address_validated: bool,
    ) -> Result<PreauthPlace, PreauthRefusal> {
        let source = source_network(peer);
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let total = held.values().sum::<usize>();
        let from_source = held.get(&source).copied().unwrap_or(0);
        if !address_validated && (from_source > 0 || total * 2 >= self.capacity) {
            return Err(PreauthRefusal::AddressUnproved);
        }
        if total >= self.capacity {
            return Err(PreauthRefusal::Full);
        }
        if from_source >= self.per_source {
            return Err(PreauthRefusal::SourceBusy);
        }
        *held.entry(source).or_insert(0) += 1;
        Ok(PreauthPlace {
            admission: Arc::clone(self),
            source,
        })
    }

    /// Places held now.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .sum()
    }

    fn release(&self, source: IpAddr) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = held.get_mut(&source) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.remove(&source);
            }
        }
    }
}

/// One connection's pre-sign-in place, given back when dropped.
pub struct PreauthPlace {
    admission: Arc<PreauthAdmission>,
    source: IpAddr,
}

impl Debug for PreauthPlace {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreauthPlace")
            .finish_non_exhaustive()
    }
}

impl Drop for PreauthPlace {
    fn drop(&mut self) {
        self.admission.release(self.source);
    }
}

/// Validated reconnect-hold policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectHoldPolicy {
    max_hold_seconds: u64,
}

impl ReconnectHoldPolicy {
    /// Creates a policy no longer than the supported two-hour maximum.
    ///
    /// # Errors
    ///
    /// Returns [`SessionAdmissionError::ReconnectHoldOutOfRange`] when the
    /// requested hold exceeds two hours.
    pub const fn new(max_hold_seconds: u64) -> Result<Self, SessionAdmissionError> {
        if max_hold_seconds > MAX_RECONNECT_HOLD_SECONDS {
            return Err(SessionAdmissionError::ReconnectHoldOutOfRange);
        }
        Ok(Self { max_hold_seconds })
    }
}

/// Exclusive capacity-one admission lease.
pub struct SessionAdmissionLease {
    token: u64,
    reconnect_until: Option<u64>,
}

impl Debug for SessionAdmissionLease {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionAdmissionLease")
            .field("token", &"<redacted>")
            .field("reconnect_hold", &self.reconnect_until.is_some())
            .finish()
    }
}

impl SessionAdmissionLease {
    /// Returns whether this lease currently retains a reconnect hold.
    #[must_use]
    pub const fn is_reconnect_hold(&self) -> bool {
        self.reconnect_until.is_some()
    }
}

/// Failure while admitting, resuming, or releasing a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAdmissionError {
    SessionAlreadyActive,
    ForeignLease,
    ReconnectHoldExpired,
    ReconnectHoldOutOfRange,
    LockPoisoned,
}

impl Display for SessionAdmissionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SessionAlreadyActive => "a session is already active",
            Self::ForeignLease => "session admission lease does not belong to this gate",
            Self::ReconnectHoldExpired => "direct reconnect hold expired",
            Self::ReconnectHoldOutOfRange => "direct reconnect hold exceeds 7200 seconds",
            Self::LockPoisoned => "session admission gate lock poisoned",
        })
    }
}

impl std::error::Error for SessionAdmissionError {}

/// How long an authenticated Deck waits for a session that is only waiting
/// for its lock screen to give the slot up
/// ([`SessionAdmissionRuntime::request_yield`]): the holder notices within a
/// fraction of a second, but may be inside one lock attempt (bounded at five
/// seconds) and then hands its displays and sound back.
pub const YIELD_WAIT: Duration = Duration::from_secs(15);

/// Whether the session holding the slot will give it up to a newcomer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Yield {
    /// No: it is serving, or a private desk waits for its lock.
    Holding,
    /// A desk that was not private waits for its lock and gives the slot up
    /// to an authenticated Deck.
    Offered,
    /// A Deck asked for the slot.
    Requested,
    /// No: a private desk under `privacy.desk = "required"` is held because
    /// its session refuses to lock
    /// (`console_session::lock_wait::LockWaitVerdict::HoldPrivate`); a Deck
    /// that finds the slot taken is told so.
    LockRefused,
}

#[derive(Debug)]
struct SessionAdmissionGate {
    active_token: Option<u64>,
    next_token: u64,
    yielding: Yield,
}

impl SessionAdmissionGate {
    const fn new() -> Self {
        Self {
            active_token: None,
            next_token: 1,
            yielding: Yield::Holding,
        }
    }

    fn admit_new(&mut self) -> Result<SessionAdmissionLease, SessionAdmissionError> {
        if self.active_token.is_some() {
            return Err(SessionAdmissionError::SessionAlreadyActive);
        }
        let token = self.next_token;
        self.next_token = self.next_token.saturating_add(1);
        self.active_token = Some(token);
        self.yielding = Yield::Holding;
        Ok(SessionAdmissionLease {
            token,
            reconnect_until: None,
        })
    }

    fn validate_lease(&self, lease: &SessionAdmissionLease) -> Result<(), SessionAdmissionError> {
        if self.active_token == Some(lease.token) {
            Ok(())
        } else {
            Err(SessionAdmissionError::ForeignLease)
        }
    }

    fn hold_for_reconnect(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
        reconnect_until: u64,
        policy: ReconnectHoldPolicy,
    ) -> Result<(), SessionAdmissionError> {
        self.validate_lease(lease)?;
        let Some(hold_seconds) = reconnect_until.checked_sub(now_epoch_seconds) else {
            return Err(SessionAdmissionError::ReconnectHoldExpired);
        };
        if hold_seconds > policy.max_hold_seconds {
            return Err(SessionAdmissionError::ReconnectHoldOutOfRange);
        }
        lease.reconnect_until = Some(reconnect_until);
        Ok(())
    }

    fn resume(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
    ) -> Result<(), SessionAdmissionError> {
        self.validate_lease(lease)?;
        match lease.reconnect_until {
            Some(until) if now_epoch_seconds <= until => {
                lease.reconnect_until = None;
                Ok(())
            }
            Some(_) | None => Err(SessionAdmissionError::ReconnectHoldExpired),
        }
    }

    fn complete(&mut self, lease: &SessionAdmissionLease) -> Result<(), SessionAdmissionError> {
        if self.active_token == Some(lease.token) {
            self.active_token = None;
            self.yielding = Yield::Holding;
            Ok(())
        } else {
            Err(SessionAdmissionError::ForeignLease)
        }
    }

    fn set_yield(
        &mut self,
        lease: &SessionAdmissionLease,
        offered: bool,
    ) -> Result<(), SessionAdmissionError> {
        self.validate_lease(lease)?;
        self.yielding = if offered {
            Yield::Offered
        } else {
            Yield::Holding
        };
        Ok(())
    }

    fn set_lock_refused(
        &mut self,
        lease: &SessionAdmissionLease,
        refused: bool,
    ) -> Result<(), SessionAdmissionError> {
        self.validate_lease(lease)?;
        self.yielding = if refused {
            Yield::LockRefused
        } else {
            Yield::Holding
        };
        Ok(())
    }

    fn request_yield(&mut self) -> bool {
        match self.yielding {
            Yield::Holding | Yield::LockRefused => false,
            Yield::Offered | Yield::Requested => {
                self.yielding = Yield::Requested;
                true
            }
        }
    }

    fn yield_requested(&self, lease: &SessionAdmissionLease) -> bool {
        self.active_token == Some(lease.token) && self.yielding == Yield::Requested
    }
}

/// Thread-safe shared capacity-one admission gate.
#[derive(Debug)]
pub struct SessionAdmissionRuntime {
    gate: Mutex<SessionAdmissionGate>,
}

impl SessionAdmissionRuntime {
    /// Creates an empty admission gate.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            gate: Mutex::new(SessionAdmissionGate::new()),
        })
    }

    /// Admits a new session when no session or reconnect hold is active.
    ///
    /// # Errors
    ///
    /// Returns [`SessionAdmissionError::SessionAlreadyActive`] when the gate
    /// is occupied, or [`SessionAdmissionError::LockPoisoned`] if its mutex
    /// cannot be acquired.
    pub fn admit_new(&self) -> Result<SessionAdmissionLease, SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .admit_new()
    }

    /// Retains the lease for a bounded direct reconnect window.
    ///
    /// # Errors
    ///
    /// Returns an admission error when the lease is foreign, the deadline is
    /// expired, or the requested hold exceeds the supported maximum.
    pub fn hold_for_reconnect(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
        reconnect_until: u64,
    ) -> Result<(), SessionAdmissionError> {
        let policy = ReconnectHoldPolicy::new(MAX_RECONNECT_HOLD_SECONDS)?;
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .hold_for_reconnect(lease, now_epoch_seconds, reconnect_until, policy)
    }

    /// Resumes a held lease before its deadline.
    ///
    /// # Errors
    ///
    /// Returns [`SessionAdmissionError::ReconnectHoldExpired`] when no valid
    /// hold remains, or [`SessionAdmissionError::ForeignLease`] for another
    /// gate's lease.
    pub fn resume(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
    ) -> Result<(), SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .resume(lease, now_epoch_seconds)
    }

    /// Releases the session slot.
    ///
    /// # Errors
    ///
    /// Returns [`SessionAdmissionError::ForeignLease`] when the lease belongs
    /// to another gate, or [`SessionAdmissionError::LockPoisoned`] if the
    /// gate mutex cannot be acquired.
    pub fn complete(&self, lease: &SessionAdmissionLease) -> Result<(), SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .complete(lease)
    }

    /// The session holding `lease` has ended and only waits for its lock
    /// screen on a desk that was not private (`offered`), so it gives the
    /// slot up to an authenticated Deck that asks
    /// ([`Self::request_yield`]); `false` withdraws the offer.
    ///
    /// # Errors
    ///
    /// [`SessionAdmissionError::ForeignLease`] for a lease that does not
    /// hold the slot, or [`SessionAdmissionError::LockPoisoned`].
    pub fn offer_yield(
        &self,
        lease: &SessionAdmissionLease,
        offered: bool,
    ) -> Result<(), SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .set_yield(lease, offered)
    }

    /// An authenticated Deck that found the slot taken asks the holder to
    /// give it up. `true` when the holder offered to: the Deck then waits up
    /// to [`YIELD_WAIT`] for [`Self::admit_new`] to succeed. `false` (a
    /// session is being served, or a private desk waits for its lock): the
    /// host is busy.
    #[must_use]
    pub fn request_yield(&self) -> bool {
        self.gate.lock().is_ok_and(|mut gate| gate.request_yield())
    }

    /// Whether a Deck asked the holder of `lease` for the slot.
    #[must_use]
    pub fn yield_requested(&self, lease: &SessionAdmissionLease) -> bool {
        self.gate
            .lock()
            .is_ok_and(|gate| gate.yield_requested(lease))
    }

    /// The session holding `lease` has ended, and its private desk is held
    /// because the session refuses to lock under `privacy.desk =
    /// "required"` (`refused`); it never gives the slot up for it. `false`
    /// clears it. A Deck that finds the slot taken reads it with
    /// [`Self::holder_lock_refused`] to say why the host is busy.
    ///
    /// # Errors
    ///
    /// [`SessionAdmissionError::ForeignLease`] for a lease that does not
    /// hold the slot, or [`SessionAdmissionError::LockPoisoned`].
    pub fn set_lock_refused(
        &self,
        lease: &SessionAdmissionLease,
        refused: bool,
    ) -> Result<(), SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .set_lock_refused(lease, refused)
    }

    /// Whether the slot is held by a session whose private desk waits for a
    /// lock it refuses ([`Self::set_lock_refused`]).
    #[must_use]
    pub fn holder_lock_refused(&self) -> bool {
        self.gate
            .lock()
            .is_ok_and(|gate| gate.active_token.is_some() && gate.yielding == Yield::LockRefused)
    }
}

/// Why a fresh sign-in that authenticated could not take over the desktop a
/// disconnected Deck left waiting for its resume. The Deck is told why in
/// words that neither blame the password nor name the desktop's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachedTakeoverRefusal {
    /// The waiting desktop belongs to another user.
    AnotherUsersDesktop,
    /// The waiting desktop did not answer the hand-over in time.
    OwnerDidNotAnswer,
}

impl DetachedTakeoverRefusal {
    /// What the Deck shows (a close reason: at most 120 bytes).
    #[must_use]
    pub const fn deck_reason(self) -> &'static str {
        match self {
            Self::AnotherUsersDesktop => {
                "Another user's remote desktop is waiting for its owner to reconnect; try again later."
            }
            Self::OwnerDidNotAnswer => {
                "The remote desktop on this host is still finishing its last connection; try again in a moment."
            }
        }
    }

    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::AnotherUsersDesktop => "another_users_desktop",
            Self::OwnerDidNotAnswer => "owner_did_not_answer",
        }
    }
}

/// Why a Pier refused an authenticated newcomer because another session
/// holds its desktop: the host is busy, the credentials were fine.
///
/// A Deck reads these words back ([`Self::from_reason`]) to say "Another
/// session is already using ‹host›" rather than a closed connection or a
/// failed sign-in, so a Pier sends [`Self::deck_reason`] as it is. The words
/// are the ones each Pier sent before they were shared, so a Deck recognises
/// those Piers too; never reword one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopBusyRefusal {
    /// Another Deck holds the Linux Pier's capacity-one session slot.
    AnotherDeckAttached,
    /// Another session owns the account's persistent Linux desktop.
    PersistentSessionOwned,
    /// Another authenticated session owns the Windows display until it
    /// disconnects and its resume window expires.
    WindowsDisplayOwned,
    /// A live Deck signed in as the same user holds the Windows display.
    WindowsSameUserAttached,
}

impl DesktopBusyRefusal {
    /// Every busy refusal.
    pub const ALL: [Self; 4] = [
        Self::AnotherDeckAttached,
        Self::PersistentSessionOwned,
        Self::WindowsDisplayOwned,
        Self::WindowsSameUserAttached,
    ];

    /// What the Pier tells the Deck. The Linux reasons travel as a close
    /// reason (at most 120 bytes); the Windows ones in an `auth_result`.
    #[must_use]
    pub const fn deck_reason(self) -> &'static str {
        match self {
            Self::AnotherDeckAttached => {
                "shared display is busy: another Deck is attached to this desktop"
            }
            Self::PersistentSessionOwned => {
                "another session owns this desktop; retry after it disconnects"
            }
            Self::WindowsDisplayOwned => {
                "The Windows display is already owned by another authenticated session. It is \
                 released when that session disconnects and its resume window \
                 (auth.reconnect_window_secs) expires."
            }
            Self::WindowsSameUserAttached => {
                "The Windows display is in use by another Deck signed in as this user. \
                 Disconnect it first."
            }
        }
    }

    /// The busy refusal a host's reason states, wherever a Deck's transport
    /// placed it in the text. `None` for any other reason.
    #[must_use]
    pub fn from_reason(reason: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|refusal| reason.contains(refusal.deck_reason()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> IpAddr {
        text.parse().expect("address")
    }

    #[test]
    fn only_a_holder_that_offered_gives_its_slot_to_an_authenticated_deck() {
        let runtime = SessionAdmissionRuntime::new();
        let lease = runtime.admit_new().expect("first session");
        // A session being served, or a private desk waiting for its lock:
        // busy.
        assert!(!runtime.request_yield());
        assert!(!runtime.yield_requested(&lease));

        runtime.offer_yield(&lease, true).expect("offered");
        assert!(!runtime.yield_requested(&lease));
        assert!(runtime.request_yield());
        assert!(runtime.yield_requested(&lease));
        // The slot is given up only when the holder completes.
        assert_eq!(
            runtime.admit_new().err(),
            Some(SessionAdmissionError::SessionAlreadyActive)
        );
        runtime.complete(&lease).expect("completed");
        let next = runtime.admit_new().expect("the waiting Deck");
        assert!(!runtime.request_yield());
        assert!(!runtime.yield_requested(&lease));

        // A withdrawn offer is busy again.
        runtime.offer_yield(&next, true).expect("offered");
        runtime.offer_yield(&next, false).expect("withdrawn");
        assert!(!runtime.request_yield());
        assert_eq!(
            runtime.offer_yield(&lease, true).err(),
            Some(SessionAdmissionError::ForeignLease)
        );
    }

    #[test]
    fn a_slot_held_for_a_refused_lock_is_never_given_up_and_says_why() {
        let runtime = SessionAdmissionRuntime::new();
        let lease = runtime.admit_new().expect("first session");
        assert!(!runtime.holder_lock_refused());
        runtime.set_lock_refused(&lease, true).expect("held");
        assert!(runtime.holder_lock_refused());
        assert!(!runtime.request_yield());
        assert!(!runtime.yield_requested(&lease));
        assert_eq!(
            runtime.admit_new().err(),
            Some(SessionAdmissionError::SessionAlreadyActive)
        );
        // It locked: busy for the ordinary reason until the slot is free.
        runtime.set_lock_refused(&lease, false).expect("cleared");
        assert!(!runtime.holder_lock_refused());
        runtime.set_lock_refused(&lease, true).expect("held again");
        runtime.complete(&lease).expect("completed");
        assert!(!runtime.holder_lock_refused());
        let _next = runtime.admit_new().expect("the next Deck");
        assert!(!runtime.holder_lock_refused());
        assert_eq!(
            runtime.set_lock_refused(&lease, true).err(),
            Some(SessionAdmissionError::ForeignLease)
        );
    }

    #[test]
    fn a_third_connection_from_one_source_is_refused_and_others_still_enter() {
        let admission = Arc::new(PreauthAdmission::default());
        let first = admission.try_enter(address("203.0.113.7")).expect("first");
        let _second = admission.try_enter(address("203.0.113.7")).expect("second");
        assert_eq!(
            admission.try_enter(address("203.0.113.7")).err(),
            Some(PreauthRefusal::SourceBusy)
        );
        let _other = admission
            .try_enter(address("203.0.113.8"))
            .expect("another source");
        drop(first);
        let _again = admission
            .try_enter(address("203.0.113.7"))
            .expect("place given back");
        assert_eq!(admission.held(), 3);
    }

    #[test]
    fn an_ipv6_64_is_one_source_and_a_mapped_ipv4_is_its_ipv4() {
        let admission = Arc::new(PreauthAdmission::default());
        let _a = admission.try_enter(address("2001:db8:1:2::1")).expect("a");
        let _b = admission
            .try_enter(address("2001:db8:1:2:ffff:ffff:ffff:ffff"))
            .expect("b");
        assert_eq!(
            admission.try_enter(address("2001:db8:1:2::99")).err(),
            Some(PreauthRefusal::SourceBusy)
        );
        let _neighbour = admission
            .try_enter(address("2001:db8:1:3::1"))
            .expect("next /64");

        let _v4 = admission.try_enter(address("198.51.100.4")).expect("v4");
        let _mapped = admission
            .try_enter(address("::ffff:198.51.100.4"))
            .expect("mapped");
        assert_eq!(
            admission.try_enter(address("198.51.100.4")).err(),
            Some(PreauthRefusal::SourceBusy)
        );
    }

    #[test]
    fn the_capacity_holds_across_sources() {
        let admission = Arc::new(PreauthAdmission::default());
        let held = (0..PREAUTH_CAPACITY)
            .map(|index| {
                admission
                    .try_enter(IpAddr::from([
                        198,
                        51,
                        100,
                        u8::try_from(index).expect("index"),
                    ]))
                    .expect("place")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            admission.try_enter(address("203.0.113.200")).err(),
            Some(PreauthRefusal::Full)
        );
        drop(held);
        assert_eq!(admission.held(), 0);
    }

    #[test]
    fn unproved_addresses_prove_themselves_once_a_source_or_half_the_pier_is_busy() {
        let admission = Arc::new(PreauthAdmission::default());
        let victim = address("203.0.113.7");
        let forged = admission
            .try_enter_unproved(victim, false)
            .expect("an idle Pier costs no round trip");
        for same_source in [victim, address("::ffff:203.0.113.7")] {
            assert_eq!(
                admission.try_enter_unproved(same_source, false).err(),
                Some(PreauthRefusal::AddressUnproved)
            );
        }
        let proved = admission
            .try_enter_unproved(victim, true)
            .expect("a Deck that answered the Retry takes its second place");
        drop(proved);

        let held = (2..=PREAUTH_CAPACITY / 2)
            .map(|index| {
                admission
                    .try_enter_unproved(
                        IpAddr::from([198, 51, 100, u8::try_from(index).expect("index")]),
                        false,
                    )
                    .expect("place")
            })
            .collect::<Vec<_>>();
        assert_eq!(admission.held(), PREAUTH_CAPACITY / 2);
        assert_eq!(
            admission
                .try_enter_unproved(address("203.0.113.8"), false)
                .err(),
            Some(PreauthRefusal::AddressUnproved),
            "forged sources hold at most half the places"
        );
        assert!(
            admission
                .try_enter_unproved(address("203.0.113.8"), true)
                .is_ok()
        );
        drop((forged, held));
        assert_eq!(admission.held(), 0);
    }

    #[test]
    fn handshake_bounds_are_derived_from_one_another() {
        assert_eq!(APPLICATION_HANDSHAKE_TIMEOUT, Duration::from_secs(90));
        assert!(CREDENTIAL_CHECK_TIMEOUT < AUTH_RESPONSE_TIMEOUT);
    }

    #[test]
    fn enforces_capacity_one_until_completion() {
        let runtime = SessionAdmissionRuntime::new();
        let lease = runtime.admit_new().expect("first lease");

        assert_eq!(
            runtime.admit_new().expect_err("second lease"),
            SessionAdmissionError::SessionAlreadyActive
        );
        runtime.complete(&lease).expect("release first lease");
        assert!(runtime.admit_new().is_ok());
    }

    #[test]
    fn reconnect_hold_is_bounded_and_resumable_at_deadline() {
        let runtime = SessionAdmissionRuntime::new();
        let mut lease = runtime.admit_new().expect("lease");

        runtime
            .hold_for_reconnect(&mut lease, 100, 100 + MAX_RECONNECT_HOLD_SECONDS)
            .expect("maximum hold");
        assert!(lease.is_reconnect_hold());
        runtime
            .resume(&mut lease, 100 + MAX_RECONNECT_HOLD_SECONDS)
            .expect("deadline is inclusive");
        assert!(!lease.is_reconnect_hold());
    }

    #[test]
    fn rejects_expired_and_overlong_holds() {
        let runtime = SessionAdmissionRuntime::new();
        let mut lease = runtime.admit_new().expect("lease");

        assert_eq!(
            runtime.hold_for_reconnect(&mut lease, 10, 9),
            Err(SessionAdmissionError::ReconnectHoldExpired)
        );
        assert_eq!(
            runtime.hold_for_reconnect(&mut lease, 10, 10 + MAX_RECONNECT_HOLD_SECONDS + 1),
            Err(SessionAdmissionError::ReconnectHoldOutOfRange)
        );
    }

    #[test]
    fn foreign_lease_cannot_release_or_resume_another_gate() {
        let first = SessionAdmissionRuntime::new();
        let second = SessionAdmissionRuntime::new();
        let mut lease = first.admit_new().expect("first lease");

        assert_eq!(
            second.resume(&mut lease, 0),
            Err(SessionAdmissionError::ForeignLease)
        );
        assert_eq!(
            second.complete(&lease),
            Err(SessionAdmissionError::ForeignLease)
        );
    }

    #[test]
    fn a_refused_takeover_is_explained_without_blaming_the_password() {
        for refusal in [
            DetachedTakeoverRefusal::AnotherUsersDesktop,
            DetachedTakeoverRefusal::OwnerDidNotAnswer,
        ] {
            let reason = refusal.deck_reason();
            assert!(reason.len() <= 120, "{reason}");
            let lower = reason.to_ascii_lowercase();
            assert!(
                !lower.contains("auth") && !lower.contains("password"),
                "{reason}"
            );
        }
        assert!(
            DetachedTakeoverRefusal::AnotherUsersDesktop
                .deck_reason()
                .contains("Another user's remote desktop is waiting")
        );
    }

    #[test]
    fn a_busy_refusal_is_read_back_from_where_a_deck_finds_it() {
        for refusal in DesktopBusyRefusal::ALL {
            let reason = refusal.deck_reason();
            // A Linux close reason, as a Deck's transport words it.
            let closed = format!("host closed the connection during setup: {reason}");
            assert_eq!(
                DesktopBusyRefusal::from_reason(&closed),
                Some(refusal),
                "{reason}"
            );
            assert!(
                !reason.to_ascii_lowercase().contains("password"),
                "{reason}"
            );
        }
        for refusal in [
            DesktopBusyRefusal::AnotherDeckAttached,
            DesktopBusyRefusal::PersistentSessionOwned,
        ] {
            assert!(refusal.deck_reason().len() <= 120, "{refusal:?}");
        }
        for other in [
            "",
            "Authentication failed",
            DetachedTakeoverRefusal::OwnerDidNotAnswer.deck_reason(),
            "shared display is busy",
        ] {
            assert_eq!(DesktopBusyRefusal::from_reason(other), None, "{other}");
        }
    }
}
