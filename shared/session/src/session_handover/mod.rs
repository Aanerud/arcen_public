// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One desktop per account, entered from any Deck; another account only
//! after a confirmed, graceful sign-out (ADR 0025 §13).
//!
//! An account's own OS session outlives every connection to it. A Deck that
//! signs in as that account joins it again, from home or work, after a
//! transport resume, a fresh password or a Pier restart; nothing here ever
//! logs it out. A different account may take the workstation only when no
//! remote client is attached to the session that holds it, only after it
//! authenticated, and only when its user confirms signing the other account
//! out. The sign-out is the operating system's own graceful logout, which
//! applications may refuse.
//!
//! The model keeps two facts apart that the operating systems blur:
//!
//! - **who holds the desktop**, a [`NativeSessionIdentity`]: the account,
//!   the native session's id, the boot it belongs to and a generation that
//!   changes when the id is reused;
//! - **whether a remote client is attached to it**, a [`RemoteAttachment`]:
//!   attached by Arcen or by another remote-desktop product, proved
//!   detached, or unknown.
//!
//! "Busy" means a remote client is attached, and it matters to a different
//! account. An account that is signed in, or even unlocked, at the desk is
//! not busy ([`DeskState`] is recorded, never decisive). The account's own
//! session is joined, with one exception: a desktop another product started
//! is refused while that product's own client uses it
//! ([`RemoteClient::OwningProduct`], [`HandoverRefusal::InUseOtherProduct`]),
//! one remote product at a time as before 0.21.5. Any other foreign client,
//! or an attachment that cannot be told, only leaves its layout alone
//! ([`GuestLayoutPolicy`]). Unknown is never read as detached: it refuses a
//! change of account.
//!
//! [`decide_handover`] turns the facts into a [`HandoverDecision`]. An offer
//! to sign another account out becomes a transaction in
//! [`consent::SwitchLedger`], bound to the requesting connection, the exact
//! target session, its boot and generation, the ownership epoch and a
//! one-use nonce, and revalidated immediately before the logout is asked
//! for. [`logout`] holds the outcomes of that logout and the one rule that
//! lets the next account in: a completed logout with proof that the exact
//! session ended. [`displays`] holds the rule that every attachment
//! negotiates the Deck's current displays.
//!
//! The platforms supply the facts and act on the answers: Linux (uid,
//! logind session, boot id), Windows (SID, WTS session, logon id) and macOS
//! (uid, audit session, boot session) each read them natively and perform
//! their own graceful logout.

pub mod consent;
pub mod displays;
pub mod logout;
pub mod remote_access;

use std::fmt::{self, Debug, Display, Formatter};

use crate::console_session::guest_layout::{ForeignClient, GuestLayoutPolicy, ShownBecause};
use crate::console_session::{ConsoleOrigin, ConsoleRefusal};
use crate::direct_reconnect::AccountPrincipal;

/// The longest identity part accepted, in bytes.
pub const MAX_IDENTITY_PART_BYTES: usize = 256;

/// Why a native identity part was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    /// The part was empty.
    Empty,
    /// The part was longer than [`MAX_IDENTITY_PART_BYTES`].
    TooLong,
    /// The part held a control character.
    Control,
}

impl Display for IdentityError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "native identity part is empty",
            Self::TooLong => "native identity part is too long",
            Self::Control => "native identity part holds a control character",
        })
    }
}

impl std::error::Error for IdentityError {}

fn identity_part(value: String) -> Result<String, IdentityError> {
    if value.is_empty() {
        Err(IdentityError::Empty)
    } else if value.len() > MAX_IDENTITY_PART_BYTES {
        Err(IdentityError::TooLong)
    } else if value.chars().any(char::is_control) {
        Err(IdentityError::Control)
    } else {
        Ok(value)
    }
}

/// An account as the operating system resolved it after authentication,
/// in a form the platform chooses and keeps stable: a Linux or macOS
/// `uid:name`, a Windows SID. Never a name as the Deck typed it, which may
/// be an alias. Two principals are the same account only when equal.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct NativePrincipal(String);

impl NativePrincipal {
    /// A principal from the platform's own canonical form.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or overlong value or one with a
    /// control character.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        identity_part(value.into()).map(Self)
    }

    /// The principal of a resolved account: `id:name`, so neither an alias
    /// nor a reused name matches another account.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn from_account(account: AccountPrincipal<'_>) -> Result<Self, IdentityError> {
        identity_part(account.name.to_owned())?;
        Self::new(format!("{}:{}", account.id, account.name))
    }

    /// The canonical form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Debug for NativePrincipal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativePrincipal(<redacted>)")
    }
}

/// The operating system's id of one graphical session: a logind session
/// id, a WTS session id, a macOS audit session id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeSessionId(String);

impl NativeSessionId {
    /// A session id as the platform reports it.
    ///
    /// # Errors
    ///
    /// As [`NativePrincipal::new`].
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        identity_part(value.into()).map(Self)
    }

    /// The id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The boot a session belongs to: the Linux kernel's boot id, the macOS
/// boot session UUID, the Windows boot time. A session id read in another
/// boot is another session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BootId(String);

impl BootId {
    /// A boot id as the platform reports it.
    ///
    /// # Errors
    ///
    /// As [`NativePrincipal::new`].
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        identity_part(value.into()).map(Self)
    }

    /// The id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Exactly one native session: its account, id, boot and generation.
///
/// `generation` is a value the platform proves changes when a session id is
/// reused within one boot: the start time of a logind session's leader, a
/// Windows logon id, a macOS session's creation time. Two identities are the
/// same session only when every part is equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NativeSessionIdentity {
    principal: NativePrincipal,
    session: NativeSessionId,
    boot: BootId,
    generation: u64,
}

impl NativeSessionIdentity {
    /// The identity of one native session.
    #[must_use]
    pub const fn new(
        principal: NativePrincipal,
        session: NativeSessionId,
        boot: BootId,
        generation: u64,
    ) -> Self {
        Self {
            principal,
            session,
            boot,
            generation,
        }
    }

    /// The account that holds the session.
    #[must_use]
    pub const fn principal(&self) -> &NativePrincipal {
        &self.principal
    }

    /// The session's native id.
    #[must_use]
    pub const fn session(&self) -> &NativeSessionId {
        &self.session
    }

    /// The boot the session belongs to.
    #[must_use]
    pub const fn boot(&self) -> &BootId {
        &self.boot
    }

    /// The session's generation within its boot.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Who a remote client attached to a session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteClient {
    /// An Arcen Deck.
    Arcen,
    /// Another remote-desktop product's client, as the platform proves it
    /// (a screen-sharing or remote-access server's connection, a
    /// compositor's remote session).
    Foreign,
    /// The client of the remote-desktop product that holds this desktop,
    /// using it now: a desktop that product started (Linux: its own
    /// client's input on a `ConsoleOrigin::OtherProduct` desktop), or a
    /// session Windows shows on that product's station instead of the
    /// console. The desktop is in use through that product, so even its
    /// own account is refused ([`HandoverRefusal::InUseOtherProduct`]).
    OwningProduct,
}

impl RemoteClient {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Arcen => "arcen",
            Self::Foreign => "foreign",
            Self::OwningProduct => "owning_product",
        }
    }
}

/// Whether a remote client is attached to a session.
///
/// A platform reports [`Self::ProvedDetached`] only with positive evidence
/// for every remote route it knows: Arcen's own attachments and every
/// supported foreign product. Anything it cannot read is [`Self::Unknown`].
/// A session that is active, unlocked or has a running service is none of
/// these on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteAttachment {
    /// A remote client is attached.
    Attached(RemoteClient),
    /// No remote client is attached, and the platform proved it.
    ProvedDetached,
    /// The platform could not tell.
    Unknown,
}

impl RemoteAttachment {
    /// Whether another product's client is attached, in the vocabulary of
    /// [`crate::console_session::guest_layout`]. An Arcen Deck is not
    /// another product's client.
    #[must_use]
    pub const fn foreign_client(self) -> ForeignClient {
        match self {
            Self::Attached(RemoteClient::Foreign | RemoteClient::OwningProduct) => {
                ForeignClient::Attached
            }
            Self::Attached(RemoteClient::Arcen) | Self::ProvedDetached => ForeignClient::Absent,
            Self::Unknown => ForeignClient::Unknown,
        }
    }

    /// What an attachment to the account's own session may do to its
    /// layout: lay it out to the Deck ([`GuestLayoutPolicy::ApplyOnAttach`])
    /// only when no other product's client is attached, proved; otherwise
    /// show it as it is and fit each Deck monitor to the output in its
    /// place. The same rule as another product's desktop (ADR 0025 §8); an
    /// adapter still narrows `ApplyOnAttach` by the administrator's
    /// `platform.session.guest_layout` and a desk console's §6 rule.
    #[must_use]
    pub const fn own_session_layout(self) -> GuestLayoutPolicy {
        match self.foreign_client() {
            ForeignClient::Attached => GuestLayoutPolicy::Shown(ShownBecause::ClientAttached),
            ForeignClient::Unknown => GuestLayoutPolicy::Shown(ShownBecause::ClientUnknown),
            ForeignClient::Absent => GuestLayoutPolicy::ApplyOnAttach,
        }
    }
}

/// The local desk's state of a session. Recorded for the log; it never
/// decides whether a session is busy (remote-only occupancy, ADR 0025 §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskState {
    /// The session is locked at the desk.
    Locked,
    /// The session is unlocked at the desk.
    Unlocked,
    /// The platform could not tell.
    Unknown,
}

/// A counter the Pier advances each time any remote client attaches to or
/// detaches from a session. An offer made at one epoch is stale at another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttachmentEpoch(u64);

impl AttachmentEpoch {
    /// Wraps the Pier's counter.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The counter.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// What the platform read about the session that holds the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFacts {
    /// The session.
    pub identity: NativeSessionIdentity,
    /// Whether a remote client is attached to it.
    pub attachment: RemoteAttachment,
    /// The Pier's ownership epoch for it.
    pub epoch: AttachmentEpoch,
    /// The desk's state of it (never decisive).
    pub desk: DeskState,
}

/// Who holds the desktop a Pier serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleOccupant {
    /// Nobody, and the platform proved it: after boot or a logout.
    Nobody,
    /// One account's session.
    Session(SessionFacts),
    /// The platform could not tell who holds it.
    Unknown,
}

/// An account the Pier authenticated natively (password, account policy,
/// the platform's own authority). Built only after that check; nothing in
/// this module runs before it, so failed credentials change nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedPrincipal(NativePrincipal);

impl AuthenticatedPrincipal {
    /// The principal the platform resolved after it authenticated the Deck.
    #[must_use]
    pub const fn after_authentication(principal: NativePrincipal) -> Self {
        Self(principal)
    }

    /// The principal.
    #[must_use]
    pub const fn principal(&self) -> &NativePrincipal {
        &self.0
    }
}

/// Whether this connection can carry a confirmed change of account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchRoute {
    /// The Deck negotiated `account_switch_v1` and the Pier can sign an
    /// account out gracefully.
    Available,
    /// The Deck did not negotiate `account_switch_v1` (an older Deck): it
    /// keeps today's refusal; nothing destructive is inferred.
    DeckCannotConfirm,
    /// This Pier cannot sign an account out gracefully.
    PierCannotSignOut,
}

impl SwitchRoute {
    /// The route for a Deck's capability and the Pier's.
    #[must_use]
    pub const fn from_capabilities(
        deck_account_switch_v1: bool,
        pier_graceful_logout: bool,
    ) -> Self {
        if !pier_graceful_logout {
            Self::PierCannotSignOut
        } else if deck_account_switch_v1 {
            Self::Available
        } else {
            Self::DeckCannotConfirm
        }
    }
}

/// Another account's session that the requester may ask to sign out: only
/// [`decide_handover`] makes one, and only [`consent::SwitchLedger::open`]
/// turns it into an offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchCandidate {
    requester: AuthenticatedPrincipal,
    target: NativeSessionIdentity,
    epoch: AttachmentEpoch,
}

impl SwitchCandidate {
    /// The account asking.
    #[must_use]
    pub const fn requester(&self) -> &AuthenticatedPrincipal {
        &self.requester
    }

    /// The session it would sign out.
    #[must_use]
    pub const fn target(&self) -> &NativeSessionIdentity {
        &self.target
    }

    /// The ownership epoch the facts were read at.
    #[must_use]
    pub const fn epoch(&self) -> AttachmentEpoch {
        self.epoch
    }
}

/// Why a sign-in is refused before anything changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoverRefusal {
    /// Another account's session has a remote client attached.
    Busy(RemoteClient),
    /// Who holds the desktop could not be read.
    OccupantUnknown,
    /// Another account's session, and whether a remote client is attached
    /// to it could not be proved: no offer.
    AttachmentUnknown,
    /// Another account's session, nobody attached, and the Deck cannot
    /// confirm a sign-out.
    DeckCannotConfirm,
    /// Another account's session, nobody attached, and this Pier cannot
    /// sign it out gracefully.
    PierCannotSignOut,
    /// A change of account is waiting for its confirmation, its logout or
    /// its requester's sign-in ([`consent::ConsoleHold`]).
    SwitchInProgress,
    /// The requester's own session is being signed out by a confirmed
    /// change of account; it is not attached to.
    SigningOut,
    /// The requester's own session is a desktop another product holds,
    /// and that product's client is using it
    /// ([`RemoteClient::OwningProduct`]): one remote product at a time.
    InUseOtherProduct,
}

impl HandoverRefusal {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Busy(RemoteClient::Arcen) => "busy_arcen",
            Self::Busy(RemoteClient::Foreign) => "busy_foreign",
            Self::Busy(RemoteClient::OwningProduct) => "busy_owning_product",
            Self::OccupantUnknown => "occupant_unknown",
            Self::AttachmentUnknown => "attachment_unknown",
            Self::DeckCannotConfirm => "deck_cannot_confirm",
            Self::PierCannotSignOut => "pier_cannot_sign_out",
            Self::SwitchInProgress => "switch_in_progress",
            Self::SigningOut => "signing_out",
            Self::InUseOtherProduct => "in_use_other_product",
        }
    }

    /// The code the Deck is shown. `origin`: who started the other
    /// account's session. `deck_account_switch_v1`: the Deck negotiated the
    /// capability, and so knows the codes that came with it
    /// ([`ConsoleRefusal::since_account_switch_v1`]); an older Deck gets the
    /// refusal it has always had instead (`console_other_user*` for another
    /// account's session, `console_session_changed` for its own session
    /// being signed out). `console_in_use_other_product` is older than the
    /// capability, so every Deck gets it.
    #[must_use]
    pub const fn console_refusal(
        self,
        origin: ConsoleOrigin,
        deck_account_switch_v1: bool,
    ) -> ConsoleRefusal {
        match (self, deck_account_switch_v1) {
            (Self::Busy(RemoteClient::Foreign | RemoteClient::OwningProduct), _) => {
                ConsoleRefusal::OtherUserOtherProduct
            }
            (Self::InUseOtherProduct, _) => ConsoleRefusal::InUseOtherProduct,
            (Self::OccupantUnknown, _) => ConsoleRefusal::SessionsUnreadable,
            (Self::AttachmentUnknown, true) => ConsoleRefusal::RemoteStateUnknown,
            (Self::SwitchInProgress, true) => ConsoleRefusal::SwitchInProgress,
            (Self::SigningOut, true) => ConsoleRefusal::SigningOut,
            (Self::SigningOut, false) => ConsoleRefusal::SessionChanged,
            (
                Self::Busy(RemoteClient::Arcen)
                | Self::DeckCannotConfirm
                | Self::PierCannotSignOut
                | Self::AttachmentUnknown
                | Self::SwitchInProgress,
                _,
            ) => ConsoleRefusal::other_user(origin),
        }
    }

    /// The reason logged.
    #[must_use]
    pub const fn reason(self) -> HandoverReason {
        match self {
            Self::Busy(_) | Self::InUseOtherProduct => HandoverReason::RemoteBusy,
            Self::OccupantUnknown | Self::AttachmentUnknown => HandoverReason::StateUnknown,
            Self::DeckCannotConfirm | Self::PierCannotSignOut => HandoverReason::SwitchUnsupported,
            Self::SwitchInProgress | Self::SigningOut => HandoverReason::SwitchInProgress,
        }
    }
}

/// What an authenticated sign-in does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoverDecision {
    /// The account's own session: join it, unless the product that holds
    /// it is using it ([`HandoverRefusal::InUseOtherProduct`]). The same OS
    /// session and its applications; the Deck's
    /// current displays are negotiated afresh ([`displays`]), and `layout`
    /// says whether the desktop may be laid out to them
    /// ([`RemoteAttachment::own_session_layout`]).
    JoinOwnSession {
        /// The session joined.
        identity: NativeSessionIdentity,
        /// Whether its layout may change for the Deck.
        layout: GuestLayoutPolicy,
    },
    /// The account's own session, another Arcen Deck attached: the same
    /// session, after the existing same-account rule for the attached Deck
    /// ([`crate::direct_reconnect::fresh_sign_in`]). The Deck's layout is
    /// applied ([`GuestLayoutPolicy::ApplyOnAttach`]).
    JoinOwnSessionAfterIncumbent(NativeSessionIdentity),
    /// Nobody holds the desktop: sign in through the platform's own login.
    SignInFresh,
    /// Another account's session, no remote client attached: offer to sign
    /// it out ([`consent::SwitchLedger::open`]). Cancel is the default.
    OfferSwitch(SwitchCandidate),
    /// Refused; nothing changes.
    Refuse(HandoverRefusal),
}

impl HandoverDecision {
    /// The reason logged for the decision, if it is one the log names.
    #[must_use]
    pub const fn reason(&self) -> Option<HandoverReason> {
        match self {
            Self::JoinOwnSession { .. } | Self::JoinOwnSessionAfterIncumbent(_) => {
                Some(HandoverReason::ReusedDesktop)
            }
            Self::SignInFresh => None,
            Self::OfferSwitch(_) => Some(HandoverReason::SwitchOffered),
            Self::Refuse(refusal) => Some(refusal.reason()),
        }
    }

    /// What a join may do to the desktop's layout; `None` when nothing is
    /// joined.
    #[must_use]
    pub const fn layout(&self) -> Option<GuestLayoutPolicy> {
        match self {
            Self::JoinOwnSession { layout, .. } => Some(*layout),
            Self::JoinOwnSessionAfterIncumbent(_) => Some(GuestLayoutPolicy::ApplyOnAttach),
            Self::SignInFresh | Self::OfferSwitch(_) | Self::Refuse(_) => None,
        }
    }
}

/// What `requester` gets on a Pier whose desktop `occupant` holds, with
/// `route` saying whether a change of account can be confirmed and `hold`
/// what a change of account reserves now
/// ([`consent::SwitchLedger::hold`]; [`consent::SwitchLedger::decide`]
/// supplies it).
///
/// While a session is being signed out, its own account is refused
/// ([`HandoverRefusal::SigningOut`]) and every other account told a change
/// of account is in progress; while the requester of a completed change
/// signs in, everyone else is told the same. The one exception: an account
/// whose own session holds the desktop, and is not the one being signed
/// out, joins it as ever (the requester's new session seen from another
/// connection, or an account that signed in at the desk meanwhile).
///
/// The account's own session is joined, never replaced. It is refused only
/// while the product that holds it uses it ([`RemoteClient::OwningProduct`]:
/// `console_in_use_other_product`, as before 0.21.5); any other attached or
/// unknown foreign client only keeps its layout as it is. Another account's session is refused while a remote
/// client is attached or that is unknown, and offered for a graceful
/// sign-out only when none is, proved. An unreadable occupant refuses. The
/// desk's lock state takes no part.
#[must_use]
pub fn decide_handover(
    requester: &AuthenticatedPrincipal,
    occupant: &ConsoleOccupant,
    route: SwitchRoute,
    hold: consent::ConsoleHold<'_>,
) -> HandoverDecision {
    let holds_own_session = matches!(
        occupant,
        ConsoleOccupant::Session(facts) if facts.identity.principal() == requester.principal()
    );
    match hold {
        consent::ConsoleHold::SigningOut(target) => {
            let occupant_is_target = matches!(
                occupant,
                ConsoleOccupant::Session(facts) if facts.identity == *target
            );
            if occupant_is_target || !holds_own_session {
                return HandoverDecision::Refuse(if target.principal() == requester.principal() {
                    HandoverRefusal::SigningOut
                } else {
                    HandoverRefusal::SwitchInProgress
                });
            }
        }
        consent::ConsoleHold::Entering(_) if !holds_own_session => {
            return HandoverDecision::Refuse(HandoverRefusal::SwitchInProgress);
        }
        consent::ConsoleHold::Entering(_)
        | consent::ConsoleHold::Free
        | consent::ConsoleHold::Offered(_) => {}
    }
    let facts = match occupant {
        ConsoleOccupant::Unknown => {
            return HandoverDecision::Refuse(HandoverRefusal::OccupantUnknown);
        }
        ConsoleOccupant::Nobody => return HandoverDecision::SignInFresh,
        ConsoleOccupant::Session(facts) => facts,
    };
    let own = facts.identity.principal() == requester.principal();
    match (own, facts.attachment) {
        (true, RemoteAttachment::Attached(RemoteClient::Arcen)) => {
            HandoverDecision::JoinOwnSessionAfterIncumbent(facts.identity.clone())
        }
        (true, RemoteAttachment::Attached(RemoteClient::OwningProduct)) => {
            HandoverDecision::Refuse(HandoverRefusal::InUseOtherProduct)
        }
        (
            true,
            attachment @ (RemoteAttachment::ProvedDetached
            | RemoteAttachment::Attached(RemoteClient::Foreign)
            | RemoteAttachment::Unknown),
        ) => HandoverDecision::JoinOwnSession {
            identity: facts.identity.clone(),
            layout: attachment.own_session_layout(),
        },
        (false, RemoteAttachment::Unknown) => {
            HandoverDecision::Refuse(HandoverRefusal::AttachmentUnknown)
        }
        (false, RemoteAttachment::Attached(client)) => {
            HandoverDecision::Refuse(HandoverRefusal::Busy(client))
        }
        (false, RemoteAttachment::ProvedDetached) => match route {
            SwitchRoute::Available => HandoverDecision::OfferSwitch(SwitchCandidate {
                requester: requester.clone(),
                target: facts.identity.clone(),
                epoch: facts.epoch,
            }),
            SwitchRoute::DeckCannotConfirm => {
                HandoverDecision::Refuse(HandoverRefusal::DeckCannotConfirm)
            }
            SwitchRoute::PierCannotSignOut => {
                HandoverDecision::Refuse(HandoverRefusal::PierCannotSignOut)
            }
        },
    }
}

/// Why a session changed hands, or did not: the `reason_class` of
/// `SESSION_HANDOVER`, the same on every Pier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoverReason {
    /// The account's own session was joined again.
    ReusedDesktop,
    /// Refused: a remote client is attached.
    RemoteBusy,
    /// Refused: who holds the desktop, or whether a client is attached,
    /// could not be read.
    StateUnknown,
    /// Refused: the Deck or the Pier cannot carry a confirmed change of
    /// account.
    SwitchUnsupported,
    /// A sign-out of another account was offered.
    SwitchOffered,
    /// Refused: another change of account is already waiting.
    SwitchInProgress,
    /// The offer was cancelled, or its requester left.
    SwitchCancelled,
    /// The offer was withdrawn: the facts changed before it was confirmed
    /// (a remote client returned, the session changed).
    SwitchStale,
    /// The offer expired.
    SwitchExpired,
    /// A reply named no live offer: replayed, mismatched or from another
    /// connection.
    SwitchRejected,
    /// The confirmed graceful logout was asked for.
    LogoutRequested,
    /// An application or the user refused the logout.
    LogoutVetoed,
    /// The logout did not finish in time.
    LogoutTimedOut,
    /// The logout failed, or the session's end was not proved.
    LogoutFailed,
    /// The exact session ended; the next account may sign in.
    LogoutCompleted,
    /// The next account's sign-in failed after the logout.
    NewLoginFailed,
}

impl HandoverReason {
    /// Every reason.
    pub const ALL: [Self; 16] = [
        Self::ReusedDesktop,
        Self::RemoteBusy,
        Self::StateUnknown,
        Self::SwitchUnsupported,
        Self::SwitchOffered,
        Self::SwitchInProgress,
        Self::SwitchCancelled,
        Self::SwitchStale,
        Self::SwitchExpired,
        Self::SwitchRejected,
        Self::LogoutRequested,
        Self::LogoutVetoed,
        Self::LogoutTimedOut,
        Self::LogoutFailed,
        Self::LogoutCompleted,
        Self::NewLoginFailed,
    ];

    /// The reason class logged.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::ReusedDesktop => "reused_desktop",
            Self::RemoteBusy => "remote_busy",
            Self::StateUnknown => "state_unknown",
            Self::SwitchUnsupported => "switch_unsupported",
            Self::SwitchOffered => "switch_offered",
            Self::SwitchInProgress => "switch_in_progress",
            Self::SwitchCancelled => "switch_cancelled",
            Self::SwitchStale => "switch_stale",
            Self::SwitchExpired => "switch_expired",
            Self::SwitchRejected => "switch_rejected",
            Self::LogoutRequested => "logout_requested",
            Self::LogoutVetoed => "logout_vetoed",
            Self::LogoutTimedOut => "logout_timed_out",
            Self::LogoutFailed => "logout_failed",
            Self::LogoutCompleted => "logout_completed",
            Self::NewLoginFailed => "new_login_failed",
        }
    }

    /// The fields of `SESSION_HANDOVER`. `attachment`: how the Deck came,
    /// when known; `remote_client`: the client a refusal names.
    #[must_use]
    pub fn fields(
        self,
        attachment: Option<displays::AttachmentKind>,
        remote_client: Option<RemoteClient>,
    ) -> Option<arcen_telemetry::StructuredFields> {
        arcen_telemetry::lifecycle_fields::session_handover(
            self.token(),
            attachment.map(displays::AttachmentKind::token),
            remote_client.map(RemoteClient::token),
        )
    }
}

#[cfg(test)]
mod tests;
