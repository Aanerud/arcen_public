// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Every attachment negotiates the Deck's current displays (ADR 0025 §13).
//!
//! Keeping an account's OS session never keeps an earlier connection's
//! displays. A new connection, a transport resume, a confirmed change of
//! account and the hand-over from the login screen to the desktop each
//! send the Deck's displays as they are now: its primary display, or with
//! Match My Layout its active monitors, up to four. The Deck reads them
//! through `DeckServices` and plans them with `arcen-deck-core`'s
//! `connect_plan`; a Pier applies the request to the existing desktop
//! (asking what [`crate::console_session::connect_layout::console_layout_ask`]
//! says), reads the native state back, and binds capture, input and the
//! served geometry to what it measured. A temporary one-display login
//! surface never completes a request of several displays: the desktop
//! behind it is negotiated afresh at the hand-over.
//!
//! A resume grant is bound to the whole request it was issued for (each
//! Pier's resume topology binding). It authorizes that request only; a Deck
//! whose displays changed has its grant refused with the wire's
//! `topology_changed` and authenticates afresh into the **same** OS session.
//! The binding is never loosened to let a changed request through, and the
//! session is never ended to resize it.
//!
//! Negotiating is not always laying out. When another product's client is
//! attached to the desktop, or that cannot be told, the desktop keeps its
//! layout ([`GuestLayoutPolicy::Shown`], ADR 0025 §8): the request is still
//! negotiated and the Deck told truthfully what it is served (each monitor
//! fitted to the output in its place), never a Match My Layout it did not
//! get.

use crate::console_session::guest_layout::GuestLayoutPolicy;

/// How a Deck comes to a desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    /// An ordinary authenticated connection.
    Connect,
    /// A transport resume with a grant (`method = "resume"`).
    TransportResume,
    /// The requester's own sign-in after a confirmed change of account.
    AccountSwitch,
    /// The desktop after a sign-in through the platform's login screen.
    LoginHandover,
}

impl AttachmentKind {
    /// Every kind.
    pub const ALL: [Self; 4] = [
        Self::Connect,
        Self::TransportResume,
        Self::AccountSwitch,
        Self::LoginHandover,
    ];

    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::TransportResume => "transport_resume",
            Self::AccountSwitch => "account_switch",
            Self::LoginHandover => "login_handover",
        }
    }
}

/// What authorizes the Deck's display request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayAuthority {
    /// This attachment's own authentication.
    Authentication,
    /// The resume grant, which was bound to exactly this request.
    ResumeGrant,
}

impl DisplayAuthority {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::ResumeGrant => "resume_grant",
        }
    }
}

/// What an attachment does with the Deck's displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentDisplays {
    /// Negotiate the Deck's current request now and read the native state
    /// back. Nothing an earlier connection left (its monitors, a layout
    /// journal, a satisfied flag) skips this.
    Negotiate {
        /// What authorizes the request.
        authority: DisplayAuthority,
        /// Whether the desktop is laid out to it
        /// ([`GuestLayoutPolicy::ApplyOnAttach`]) or shown as it is.
        layout: GuestLayoutPolicy,
    },
    /// A resume whose grant names another request, or none: the grant is
    /// refused (`topology_changed`) with its binding untouched, and the
    /// Deck authenticates afresh. Its account's session is the same OS
    /// session ([`super::HandoverDecision::JoinOwnSession`]), where the
    /// current request is negotiated.
    AuthenticateAfresh,
}

/// The displays of an attachment of `kind` whose Deck asks for `current`
/// now. `grant_bound`: the request a resume grant was issued for. `layout`:
/// what the desktop's attachment allows
/// ([`super::HandoverDecision::layout`],
/// [`super::RemoteAttachment::own_session_layout`]).
///
/// `R` is whatever a side already holds for a whole request: a Pier's
/// resume topology binding, a Deck's planned topology. A partial match (the
/// primary's size alone) is not equality.
#[must_use]
pub fn attachment_displays<R: PartialEq + ?Sized>(
    kind: AttachmentKind,
    layout: GuestLayoutPolicy,
    current: &R,
    grant_bound: Option<&R>,
) -> AttachmentDisplays {
    let authority = match kind {
        AttachmentKind::TransportResume => match resume_grant_authority(current, grant_bound) {
            Some(authority) => authority,
            None => return AttachmentDisplays::AuthenticateAfresh,
        },
        AttachmentKind::Connect | AttachmentKind::AccountSwitch | AttachmentKind::LoginHandover => {
            DisplayAuthority::Authentication
        }
    };
    AttachmentDisplays::Negotiate { authority, layout }
}

/// The transport-resume half of [`attachment_displays`], for a Pier's
/// resume registry, which decides whether a grant may be used before it
/// knows the desktop's layout policy: the grant authorizes `current` only
/// when it was issued for exactly that request. `None`: the grant is
/// refused (`topology_changed`) and the Deck authenticates afresh into the
/// same OS session.
#[must_use]
pub fn resume_grant_authority<R: PartialEq + ?Sized>(
    current: &R,
    grant_bound: Option<&R>,
) -> Option<DisplayAuthority> {
    grant_bound
        .is_some_and(|bound| bound == current)
        .then_some(DisplayAuthority::ResumeGrant)
}

impl AttachmentDisplays {
    /// Stable token for logs: `negotiate` or `authenticate_afresh`.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Negotiate { .. } => "negotiate",
            Self::AuthenticateAfresh => "authenticate_afresh",
        }
    }

    /// Whether the desktop may be laid out to the Deck's request.
    #[must_use]
    pub const fn may_change_layout(self) -> bool {
        matches!(
            self,
            Self::Negotiate {
                layout: GuestLayoutPolicy::ApplyOnAttach,
                ..
            }
        )
    }
}

/// What holds a desktop a Pier kept for an account between attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldDesktop {
    /// The account's own console session, joined: the Pier holds only an
    /// attachment to it, never the session.
    JoinedConsole,
    /// A desktop of the Pier's own whose displays move to another layout
    /// in place (a reconfigurable headless Wayland desktop).
    OwnReconfigurable,
    /// A desktop of the Pier's own whose displays are fixed for its life.
    OwnFixed,
}

/// How a held desktop takes a returning Deck's request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReattachDisplays {
    /// The held attachment was made for exactly this request: reattach.
    /// The Pier still reads the native layout back before it streams.
    Reattach,
    /// Move the desktop's displays to the request in place.
    ReconfigureInPlace,
    /// End the held attachment, never the session, and attach afresh to
    /// the same OS session with the request laid out.
    FreshAttachment,
    /// Replace the desktop: it cannot take the request in place.
    ReplaceDesktop,
    /// Keep the desktop untouched and refuse: it cannot take the request
    /// in place, and this Pier may not create the one that would replace
    /// it.
    KeepAndRefuse,
}

impl ReattachDisplays {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Reattach => "reattach",
            Self::ReconfigureInPlace => "reconfigure_in_place",
            Self::FreshAttachment => "fresh_attachment",
            Self::ReplaceDesktop => "replace_desktop",
            Self::KeepAndRefuse => "keep_and_refuse",
        }
    }
}

/// How a desktop held as `held`, attached for `held_request`, takes a
/// Deck's `current` request (the whole request: every display's size,
/// place, rotation and scale, the primary, and the Displays mode).
///
/// Only an attachment made for exactly `current` is reattached: an earlier
/// connection's layout never answers a new request. A joined console is
/// attached afresh, in the same OS session, never ended; a desktop of the
/// Pier's own moves in place when it can, and is otherwise replaced unless
/// creating its replacement is refused.
#[must_use]
pub fn reattach_displays<R: PartialEq + ?Sized>(
    held: HeldDesktop,
    held_request: &R,
    current: &R,
    creation_refused: bool,
) -> ReattachDisplays {
    if held_request == current {
        return ReattachDisplays::Reattach;
    }
    match held {
        HeldDesktop::JoinedConsole => ReattachDisplays::FreshAttachment,
        HeldDesktop::OwnReconfigurable => ReattachDisplays::ReconfigureInPlace,
        HeldDesktop::OwnFixed if creation_refused => ReattachDisplays::KeepAndRefuse,
        HeldDesktop::OwnFixed => ReattachDisplays::ReplaceDesktop,
    }
}
