// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The graceful logout a confirmed change of account asks for, and the one
//! rule that lets the next account in.
//!
//! A Pier asks the operating system for its ordinary logout of exactly the
//! confirmed session (the desktop's session manager on GNOME, a normal
//! logoff in the user's own Windows session, loginwindow's logout event on
//! macOS), and respects every application that inhibits or cancels it. It
//! never forces a logout, kills the user's processes or restarts the
//! machine. The next account signs in only after [`LogoutOutcome::Completed`]
//! with a [`SessionExitProof`] for the exact session; a request, a vetoed or
//! stalled logout, or an end the Pier could not read back lets nobody in.

use std::time::Duration;

use super::consent::{ConnectionId, SwitchNonce};
use super::{AuthenticatedPrincipal, HandoverReason, NativeSessionIdentity};

/// How long a Pier waits for the operating system to finish a graceful
/// logout before it reports [`LogoutOutcome::TimedOut`]: applications may
/// ask their users to save first.
pub const LOGOUT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(120);

/// Permission to ask for the graceful logout of exactly one session, made
/// only by [`super::consent::SwitchLedger::confirm`]. Neither cloned nor
/// built elsewhere.
#[derive(Debug)]
#[must_use = "a confirmed switch must ask for the logout and report its outcome"]
pub struct LogoutAuthority {
    connection: ConnectionId,
    requester: AuthenticatedPrincipal,
    target: NativeSessionIdentity,
    nonce: SwitchNonce,
}

impl LogoutAuthority {
    pub(super) const fn new(
        connection: ConnectionId,
        requester: AuthenticatedPrincipal,
        target: NativeSessionIdentity,
        nonce: SwitchNonce,
    ) -> Self {
        Self {
            connection,
            requester,
            target,
            nonce,
        }
    }

    /// The session to sign out, and no other.
    #[must_use]
    pub const fn target(&self) -> &NativeSessionIdentity {
        &self.target
    }

    /// The account that confirmed.
    #[must_use]
    pub const fn requester(&self) -> &AuthenticatedPrincipal {
        &self.requester
    }

    /// The connection that confirmed.
    #[must_use]
    pub const fn connection(&self) -> ConnectionId {
        self.connection
    }

    pub(super) const fn nonce(&self) -> &SwitchNonce {
        &self.nonce
    }
}

/// Whether one exact session still exists, as the platform reads it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPresence {
    /// The session no longer exists.
    Gone,
    /// The session still exists.
    Present,
    /// The platform could not tell.
    Unknown,
}

/// Proof that a native session ended: the platform read back that the
/// exact identity is gone. A logout request, an empty console or a stream
/// that dropped is not this proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExitProof {
    ended: NativeSessionIdentity,
}

impl SessionExitProof {
    /// Proof that `session` ended, when `presence` read back for exactly it
    /// says it is gone.
    #[must_use]
    pub fn read_back(session: &NativeSessionIdentity, presence: SessionPresence) -> Option<Self> {
        (presence == SessionPresence::Gone).then(|| Self {
            ended: session.clone(),
        })
    }

    /// The session that ended.
    #[must_use]
    pub const fn ended(&self) -> &NativeSessionIdentity {
        &self.ended
    }
}

/// How the graceful logout ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogoutOutcome {
    /// The session ended, proved.
    Completed(SessionExitProof),
    /// An application or the user refused the logout.
    Vetoed,
    /// The logout did not finish within [`LOGOUT_COMPLETION_TIMEOUT`].
    TimedOut,
    /// The operating system refused or failed the request.
    Failed,
}

/// Why the next account may not sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoutBlock {
    /// An application or the user refused the logout.
    Vetoed,
    /// The logout did not finish in time.
    TimedOut,
    /// The logout failed.
    Failed,
    /// The proof named another session than the confirmed one.
    ExitUnproven,
    /// The authority was not this ledger's live transaction.
    NotCommitting,
}

impl LogoutBlock {
    /// The reason logged.
    #[must_use]
    pub const fn reason(self) -> HandoverReason {
        match self {
            Self::Vetoed => HandoverReason::LogoutVetoed,
            Self::TimedOut => HandoverReason::LogoutTimedOut,
            Self::Failed | Self::ExitUnproven | Self::NotCommitting => HandoverReason::LogoutFailed,
        }
    }
}

/// Permission for the requester to sign in through the platform's own
/// login now that the earlier session ended. Neither cloned nor built
/// outside this module.
#[derive(Debug)]
#[must_use = "the requester signs in only with this permit"]
pub struct EntryPermit {
    connection: ConnectionId,
    requester: AuthenticatedPrincipal,
}

impl EntryPermit {
    /// The connection whose account may sign in.
    #[must_use]
    pub const fn connection(&self) -> ConnectionId {
        self.connection
    }

    /// The account that may sign in.
    #[must_use]
    pub const fn requester(&self) -> &AuthenticatedPrincipal {
        &self.requester
    }

    pub(super) fn into_parts(self) -> (ConnectionId, AuthenticatedPrincipal) {
        (self.connection, self.requester)
    }
}

/// What follows a logout.
#[derive(Debug)]
pub enum LogoutVerdict {
    /// The confirmed session ended ([`HandoverReason::LogoutCompleted`]).
    /// The requester signs in afresh while the ledger reserves the console
    /// for it, then reports through
    /// [`super::consent::SwitchLedger::login_finished`]; every resume grant
    /// bound to `ended` is revoked and can never revive it.
    Completed {
        /// Permission for the requester's own sign-in.
        permit: EntryPermit,
        /// The session that ended.
        ended: NativeSessionIdentity,
    },
    /// The confirmed session ended, but its requester's connection had
    /// closed while the logout ran ([`HandoverReason::NewLoginFailed`]).
    /// Nobody holds a permit and the console is not reserved; every resume
    /// grant bound to `ended` is still revoked.
    CompletedRequesterGone {
        /// The session that ended.
        ended: NativeSessionIdentity,
    },
    /// Nobody signs in; the switch stops and the Deck is told why.
    Blocked(LogoutBlock),
}

impl LogoutVerdict {
    /// The reason logged.
    #[must_use]
    pub const fn reason(&self) -> HandoverReason {
        match self {
            Self::Completed { .. } => HandoverReason::LogoutCompleted,
            Self::CompletedRequesterGone { .. } => HandoverReason::NewLoginFailed,
            Self::Blocked(block) => block.reason(),
        }
    }
}

pub(super) fn verdict(
    authority: LogoutAuthority,
    outcome: LogoutOutcome,
    committing: bool,
    requester_present: bool,
) -> LogoutVerdict {
    if !committing {
        return LogoutVerdict::Blocked(LogoutBlock::NotCommitting);
    }
    match outcome {
        LogoutOutcome::Completed(proof)
            if proof.ended == authority.target && !requester_present =>
        {
            LogoutVerdict::CompletedRequesterGone { ended: proof.ended }
        }
        LogoutOutcome::Completed(proof) if proof.ended == authority.target => {
            LogoutVerdict::Completed {
                permit: EntryPermit {
                    connection: authority.connection,
                    requester: authority.requester,
                },
                ended: proof.ended,
            }
        }
        LogoutOutcome::Completed(_) => LogoutVerdict::Blocked(LogoutBlock::ExitUnproven),
        LogoutOutcome::Vetoed => LogoutVerdict::Blocked(LogoutBlock::Vetoed),
        LogoutOutcome::TimedOut => LogoutVerdict::Blocked(LogoutBlock::TimedOut),
        LogoutOutcome::Failed => LogoutVerdict::Blocked(LogoutBlock::Failed),
    }
}
