// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Sign-in outcomes on Home, both Decks alike.
//!
//! There is no separate sign-in page. When Connect cannot go ahead, or an
//! attempt or a session ends without a desktop to show, the Deck stays on
//! (or returns to) Home with the same connection selected, clears its
//! password, focuses the field to fix and says what happened under it in
//! plain words. Only an outcome that is not about a field (the host ended
//! the session, restarted, or its identity changed) opens an acknowledgement
//! box. The technical detail goes to the log and behind "Details".
//!
//! The shells report a typed [`SignInFailure`] and render [`SignInState`];
//! every decision and every word is made here.

use arcen_session::console_session::ConsoleRefusal;

use crate::connection_draft::ConnectionDraft;

/// A field on the selected connection's card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInField {
    Username,
    Password,
}

/// Why sign-in did not lead to a desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInFailure {
    /// Connect was pressed without a username.
    UsernameMissing,
    /// Connect was pressed without a password.
    PasswordMissing,
    /// The host asked for a password this attempt did not carry.
    PasswordRequired,
    /// The host refused the username or password.
    CredentialsRefused,
    /// The host could not be reached (refused, timed out, unreachable).
    Unreachable,
    /// A signed-in session ended and cannot resume: sign in again.
    SessionEnded,
    /// The connection dropped and this Deck's displays changed before it
    /// could resume. The resume was bound to the old displays; signing in
    /// again negotiates the current ones on the same host desktop.
    DisplaysChanged,
    /// The host closed the connection before the session started.
    HostClosedConnection,
    /// The host ended a signed-in session.
    HostEndedSession,
    /// The session the Deck held is gone from the host, or the host changed
    /// since: it restarted, or the session was closed there.
    HostRestarted,
    /// Another account is now signed in on the host.
    AccountChanged,
    /// The host's identity could not be verified or no longer matches.
    HostIdentityChanged,
    /// The host signed the user in (or the Deck asked for something) and
    /// the session could not start.
    SessionNotStarted,
    /// Anything else that ended the connection.
    ConnectionFailed,
    /// The host refused for a reason about its own session, not the
    /// password: someone else is signed in there, its login screen cannot
    /// take a remote sign-in, it could not keep its screen private… A
    /// Pier says which with a stable code
    /// ([`ConsoleRefusal::token`]). `ended`: a running session ended for
    /// it, rather than a sign-in.
    ConsoleRefused {
        refusal: ConsoleRefusal,
        ended: bool,
    },
    /// The host refused without checking the password: too many failed
    /// sign-ins recently.
    TooManyAttempts,
    /// The password was right, but the account may not sign in there.
    AccountNotAllowed,
    /// Another session already holds the host.
    HostBusy,
    /// The host cannot take a sign-in right now (its authentication or
    /// remote sign-in is unavailable).
    SignInUnavailable,
    /// The host's notice before signing in was declined.
    NoticeDeclined,
    /// The host has a notice to accept, and this connection could not show
    /// it (a command-line connection).
    NoticeRequired,
    /// The host refused the sign-in for a reason this Deck does not know;
    /// the reason is under Details.
    HostRefused,
    /// A change of account stopped without a desktop
    /// ([`crate::account_switch`]).
    AccountSwitch(crate::account_switch::AccountSwitchStop),
}

/// A host's refusal about its own state, never about the credentials:
/// another session holds it, another remote desktop holds its seat, or its
/// console refused ([`ConsoleRefusal`]). Telemetry gives it its own reason
/// class ([`Self::reason_class`]), and nothing treats it as a wrong
/// password: no `SESSION_AUTH_FAIL`, no credential outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRefusal {
    /// Another Arcen session already holds the host.
    Busy,
    /// Another remote desktop holds the host's X11 seat (one X11 desktop per
    /// host, `arcen_session::x11_seat`).
    SeatOccupied,
    /// The host's console refused, with its stable code.
    Console(ConsoleRefusal),
}

impl HostRefusal {
    /// The `reason_class` of the Deck's `CLIENT_CONNECT_FAIL` and
    /// `CLIENT_SESSION_END`: `host_busy`, `seat_occupied`, or the console
    /// refusal's own code, as the Pier logs it.
    #[must_use]
    pub const fn reason_class(self) -> &'static str {
        match self {
            Self::Busy => "host_busy",
            Self::SeatOccupied => "seat_occupied",
            Self::Console(refusal) => refusal.token(),
        }
    }

    /// What it means for sign-in. `stage`: a console refusal of a running
    /// session (or its resume, or a hand-over) says the session ended.
    #[must_use]
    pub const fn sign_in_failure(self, stage: SignInStage) -> SignInFailure {
        match self {
            Self::Busy | Self::SeatOccupied => SignInFailure::HostBusy,
            Self::Console(refusal) => SignInFailure::ConsoleRefused {
                refusal,
                ended: matches!(stage, SignInStage::Session),
            },
        }
    }

    /// The host refusal a host's reason states: a console code
    /// ([`console_refusal`]), a busy desktop (the macOS Pier's "another
    /// session is already using this Mac", the Linux and Windows Piers'
    /// shared `DesktopBusyRefusal` words, another user's desktop waiting
    /// for its owner), or the Linux Pier's occupied X11 seat. `None` for
    /// anything else, a wrong password included.
    #[cfg(feature = "runtime")]
    #[must_use]
    pub fn from_reason(reason: &str) -> Option<Self> {
        use arcen_session::session_admission::{DesktopBusyRefusal, DetachedTakeoverRefusal};
        use arcen_session::x11_seat::{DECK_REASON_SEAT_OCCUPIED, DECK_REASON_SEAT_OTHER_PRODUCT};
        if let Some(refusal) = console_refusal(reason) {
            return Some(Self::Console(refusal));
        }
        let plain = reason.trim().to_ascii_lowercase();
        if plain.starts_with(HOST_BUSY_REASON)
            || DesktopBusyRefusal::from_reason(reason).is_some()
            || reason.contains(DetachedTakeoverRefusal::AnotherUsersDesktop.deck_reason())
        {
            return Some(Self::Busy);
        }
        // The named form of `x11_seat::deck_reason_other_product`.
        let named_service = reason.contains("Another remote-desktop service (")
            && reason.contains(") holds this host's display.");
        (reason.contains(DECK_REASON_SEAT_OCCUPIED)
            || reason.contains(DECK_REASON_SEAT_OTHER_PRODUCT)
            || named_service)
            .then_some(Self::SeatOccupied)
    }
}

/// How the macOS Pier words [`HostRefusal::Busy`] (lowercased).
#[cfg(feature = "runtime")]
const HOST_BUSY_REASON: &str = "another session is already using";

/// Whether the user was signing in or had a session when it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInStage {
    /// Connecting and signing in: no desktop was shown yet.
    Attempt,
    /// A signed-in session (or its resume) ended.
    Session,
}

impl SignInFailure {
    /// The field this outcome is about. `None` asks for an acknowledgement
    /// box instead.
    #[must_use]
    pub const fn field(self) -> Option<SignInField> {
        match self {
            Self::UsernameMissing | Self::AccountNotAllowed => Some(SignInField::Username),
            Self::PasswordMissing
            | Self::PasswordRequired
            | Self::CredentialsRefused
            | Self::Unreachable
            | Self::SessionEnded
            | Self::DisplaysChanged
            | Self::TooManyAttempts
            | Self::NoticeDeclined
            | Self::NoticeRequired
            | Self::HostRefused => Some(SignInField::Password),
            Self::ConsoleRefused {
                refusal,
                ended: false,
            } => match refusal {
                ConsoleRefusal::OtherUser
                | ConsoleRefusal::OtherUserOtherProduct
                | ConsoleRefusal::AccountDenied => Some(SignInField::Username),
                ConsoleRefusal::SessionChanged => Some(SignInField::Password),
                ConsoleRefusal::WaylandSession
                | ConsoleRefusal::NoConsoleSession
                | ConsoleRefusal::SessionsUnreadable
                | ConsoleRefusal::SignInUnavailable
                | ConsoleRefusal::SignInFailed
                | ConsoleRefusal::SessionUnreachable
                | ConsoleRefusal::PrivacyUnavailable
                | ConsoleRefusal::InUseOtherProduct
                | ConsoleRefusal::OtherProductStarted
                | ConsoleRefusal::SignInOtherProduct
                | ConsoleRefusal::RemoteStateUnknown
                | ConsoleRefusal::SwitchInProgress
                | ConsoleRefusal::SigningOut => None,
            },
            Self::ConsoleRefused { ended: true, .. }
            | Self::AccountSwitch(_)
            | Self::HostBusy
            | Self::SignInUnavailable
            | Self::HostClosedConnection
            | Self::HostEndedSession
            | Self::HostRestarted
            | Self::AccountChanged
            | Self::HostIdentityChanged
            | Self::SessionNotStarted
            | Self::ConnectionFailed => None,
        }
    }

    /// Whether waiting cannot change this outcome: only the person, or
    /// someone at the host, can (a wrong password, another account at the
    /// console, a declined notice…). A sign-in screen's hand-over
    /// (`reconnect_flow::follows_login_window`) is not followed past one:
    /// retrying would only repeat the refusal, and a refused
    /// password would count against the host's sign-in throttle.
    ///
    /// A host that is busy, has no desktop yet or cannot reach it is not
    /// definite: during a hand-over the sign-in screen's own session may
    /// still hold the host, and the signed-in desktop takes a moment to
    /// appear. The shared schedule bounds those retries.
    #[must_use]
    pub const fn is_definite(self) -> bool {
        match self {
            Self::UsernameMissing
            | Self::PasswordMissing
            | Self::PasswordRequired
            | Self::CredentialsRefused
            | Self::TooManyAttempts
            | Self::AccountNotAllowed
            | Self::AccountChanged
            | Self::HostIdentityChanged
            | Self::NoticeDeclined
            | Self::NoticeRequired
            | Self::HostRefused
            // Only the user can start another change of account; a retry
            // would ask again.
            | Self::AccountSwitch(_) => true,
            Self::ConsoleRefused { refusal, .. } => match refusal {
                ConsoleRefusal::OtherUser
                | ConsoleRefusal::OtherUserOtherProduct
                | ConsoleRefusal::AccountDenied
                | ConsoleRefusal::InUseOtherProduct
                | ConsoleRefusal::OtherProductStarted
                | ConsoleRefusal::SignInOtherProduct
                | ConsoleRefusal::WaylandSession
                | ConsoleRefusal::SignInUnavailable => true,
                ConsoleRefusal::NoConsoleSession
                | ConsoleRefusal::SessionsUnreadable
                | ConsoleRefusal::SignInFailed
                | ConsoleRefusal::SessionUnreachable
                | ConsoleRefusal::PrivacyUnavailable
                | ConsoleRefusal::SessionChanged
                | ConsoleRefusal::RemoteStateUnknown
                | ConsoleRefusal::SwitchInProgress
                | ConsoleRefusal::SigningOut => false,
            },
            Self::Unreachable
            | Self::SessionEnded
            | Self::DisplaysChanged
            | Self::HostClosedConnection
            | Self::HostEndedSession
            | Self::HostRestarted
            | Self::SessionNotStarted
            | Self::ConnectionFailed
            | Self::HostBusy
            | Self::SignInUnavailable => false,
        }
    }

    /// What the user reads, naming the host where it helps.
    #[must_use]
    pub fn message(self, host: &str) -> String {
        self.message_with(host, None)
    }

    /// [`Self::message`], with what the host said (`detail`, shown under
    /// Details) where it names something the headline should: the account
    /// signed in at the host.
    #[must_use]
    pub fn message_with(self, host: &str, detail: Option<&str>) -> String {
        match self {
            Self::ConsoleRefused { refusal, ended } => {
                console_message(refusal, ended, host, detail)
            }
            Self::AccountSwitch(stop) => stop.message(host),
            Self::TooManyAttempts => {
                "Too many failed sign-ins. Wait a few minutes, then try again.".to_owned()
            }
            Self::AccountNotAllowed => format!("This account may not sign in at {host}."),
            Self::HostBusy => format!("Another session is already using {host}."),
            Self::SignInUnavailable => {
                format!("{host} can\u{2019}t take a sign-in right now. Try again later.")
            }
            Self::NoticeDeclined => {
                format!("You declined {host}\u{2019}s notice, so Arcen didn\u{2019}t sign in.")
            }
            Self::NoticeRequired => format!(
                "{host} asks you to accept a notice before signing in. Connect from Home to read it."
            ),
            Self::HostRefused => {
                format!("{host} refused the sign-in. Its reason is under Details.")
            }
            Self::UsernameMissing => "Enter your username to connect.".to_owned(),
            Self::PasswordMissing | Self::PasswordRequired => {
                "Enter your password to connect.".to_owned()
            }
            Self::CredentialsRefused => "Wrong username or password.".to_owned(),
            Self::Unreachable => format!(
                "Couldn\u{2019}t reach {host}. Check that it\u{2019}s on and that Arcen is \
                 running there."
            ),
            Self::SessionEnded => "Your session ended. Sign in again to continue.".to_owned(),
            Self::DisplaysChanged => {
                "Your displays changed. Sign in again to continue on the same desktop with them."
                    .to_owned()
            }
            Self::HostClosedConnection => {
                format!("{host} closed the connection before your session started.")
            }
            Self::HostEndedSession => format!("{host} ended your session."),
            Self::HostRestarted => format!(
                "{host} restarted or closed your session there. Sign in again to start a new one."
            ),
            Self::AccountChanged => {
                format!("Someone else is now signed in on {host}. Sign in again to continue.")
            }
            Self::HostIdentityChanged => format!(
                "Arcen couldn\u{2019}t confirm that {host} is the computer you trusted, so it \
                 didn\u{2019}t connect."
            ),
            Self::SessionNotStarted => format!("Couldn\u{2019}t start a session on {host}."),
            Self::ConnectionFailed => format!("The connection to {host} failed."),
        }
    }
}

/// One sign-in outcome as Home shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInFeedback {
    pub failure: SignInFailure,
    /// The field the message sits under; `None` is an acknowledgement box.
    pub field: Option<SignInField>,
    pub message: String,
    /// The technical reason, behind "Details".
    pub detail: Option<String>,
    /// The saved connection this belongs to; `None` for an unsaved one
    /// (shown above Home).
    pub connection_id: Option<String>,
}

/// Home's sign-in state: the outcome to show and the field to focus. The
/// shell keeps one and hands it to Home every pass.
#[derive(Debug, Default)]
pub struct SignInState {
    feedback: Option<SignInFeedback>,
    focus: Option<SignInField>,
    /// The outcome's connection is not saved (`--connect`): Home keeps a
    /// card for it to sign in again from, also after the box is closed.
    unsaved: bool,
}

impl SignInState {
    /// Home's Connect. `Ok` when the draft has what signing in needs (the
    /// last outcome is cleared); otherwise the field to fill is named and
    /// focused, and nothing is dialled.
    ///
    /// # Errors
    ///
    /// The missing field's [`SignInFailure`].
    pub fn connect(&mut self, draft: &ConnectionDraft) -> Result<(), SignInFailure> {
        let missing = if draft.username.trim().is_empty() {
            Some(SignInFailure::UsernameMissing)
        } else if draft.password.is_empty() {
            Some(SignInFailure::PasswordMissing)
        } else {
            None
        };
        if let Some(failure) = missing {
            self.show(failure, draft, None);
            return Err(failure);
        }
        self.clear();
        Ok(())
    }

    /// An attempt or session ended without a desktop: the draft Home shows
    /// next (the same connection, its password wiped), with the outcome
    /// under its field or in the acknowledgement box. The detail is logged.
    #[must_use = "Home shows the returned draft"]
    pub fn return_home(
        &mut self,
        failure: SignInFailure,
        mut draft: ConnectionDraft,
        detail: Option<String>,
    ) -> ConnectionDraft {
        zeroize::Zeroize::zeroize(&mut draft.password);
        tracing::info!(
            target: crate::log_target::SESSION,
            ?failure,
            host = %draft.endpoint(),
            detail = detail.as_deref().unwrap_or(""),
            "sign-in returned to Home",
        );
        self.show(failure, &draft, detail);
        draft
    }

    fn show(&mut self, failure: SignInFailure, draft: &ConnectionDraft, detail: Option<String>) {
        let field = failure.field();
        let detail = detail.filter(|detail| !detail.trim().is_empty());
        self.feedback = Some(SignInFeedback {
            failure,
            field,
            message: failure.message_with(draft.display_name(), detail.as_deref()),
            detail,
            connection_id: draft.saved_connection_id.clone(),
        });
        self.unsaved = draft.saved_connection_id.is_none();
        // A box is acknowledged first; the password is focused after.
        self.focus = field;
    }

    /// A credential field was edited: an outcome under a field is stale and
    /// goes at once ("Enter your password to connect." while typing it). An
    /// acknowledgement box is not about a field and stays.
    pub fn credentials_edited(&mut self) {
        if self
            .feedback
            .as_ref()
            .is_some_and(|feedback| feedback.field.is_some())
        {
            self.feedback = None;
            self.focus = None;
        }
    }

    /// A connection form opened over the card: the outcome goes, but an
    /// unsaved connection keeps its card for when the form closes (Home
    /// must not show a saved card in its place with this draft).
    pub fn dismiss(&mut self) {
        self.feedback = None;
        self.focus = None;
    }

    /// Drops the outcome (another connection was chosen, or Connect went
    /// ahead).
    pub fn clear(&mut self) {
        self.feedback = None;
        self.focus = None;
        self.unsaved = false;
    }

    /// Whether Home shows the unsaved connection the last outcome was for,
    /// so its password can be typed again there.
    #[must_use]
    pub const fn keeps_unsaved_connection(&self) -> bool {
        self.unsaved
    }

    #[must_use]
    pub const fn feedback(&self) -> Option<&SignInFeedback> {
        self.feedback.as_ref()
    }

    /// The outcome to show under `field` of the connection `connection_id`.
    #[must_use]
    pub fn under_field(
        &self,
        connection_id: Option<&str>,
        field: SignInField,
    ) -> Option<&SignInFeedback> {
        self.feedback.as_ref().filter(|feedback| {
            feedback.field == Some(field) && feedback.connection_id.as_deref() == connection_id
        })
    }

    /// The outcome that asks for an acknowledgement box.
    #[must_use]
    pub fn acknowledgement(&self) -> Option<&SignInFeedback> {
        self.feedback
            .as_ref()
            .filter(|feedback| feedback.field.is_none())
    }

    /// OK on the acknowledgement box: it closes and the password is focused.
    pub fn acknowledge(&mut self) {
        if self.acknowledgement().is_some() {
            self.feedback = None;
            self.focus = Some(SignInField::Password);
        }
    }

    /// The field to focus this pass, once. Nothing while a box is open.
    #[must_use]
    pub fn take_focus(&mut self) -> Option<SignInField> {
        if self.acknowledgement().is_some() {
            return None;
        }
        self.focus.take()
    }
}

/// The headline for a host's console refusal, naming the host. `ended`: it
/// ended a running session rather than a sign-in.
fn console_message(
    refusal: ConsoleRefusal,
    ended: bool,
    host: &str,
    detail: Option<&str>,
) -> String {
    let sentence = match refusal {
        ConsoleRefusal::PrivacyUnavailable => {
            return if ended {
                format!("{host} couldn\u{2019}t keep its screen private, so it ended the session.")
            } else {
                format!(
                    "{host} couldn\u{2019}t keep its screen private, so it didn\u{2019}t connect."
                )
            };
        }
        ConsoleRefusal::OtherUser => match detail.and_then(console_owner) {
            Some(owner) => format!(
                "Someone else is signed in at {host} ({owner}). Sign in as that user, or switch \
                 to your account there first."
            ),
            None => format!(
                "Someone else is signed in at {host}. Sign in as that user, or switch to your \
                 account there first."
            ),
        },
        ConsoleRefusal::OtherUserOtherProduct => {
            format!("Someone else is using {host} through another remote-desktop app.")
        }
        ConsoleRefusal::WaylandSession => {
            format!("{host} runs a desktop session Arcen can\u{2019}t join.")
        }
        ConsoleRefusal::NoConsoleSession => {
            format!("Nobody is signed in at {host}, and its login screen isn\u{2019}t running.")
        }
        ConsoleRefusal::SessionsUnreadable => format!(
            "{host} couldn\u{2019}t tell who is signed in, so Arcen didn\u{2019}t join a session."
        ),
        ConsoleRefusal::SignInUnavailable => {
            format!("{host}\u{2019}s login screen can\u{2019}t take a remote sign-in.")
        }
        ConsoleRefusal::SignInFailed => {
            format!("{host}\u{2019}s login screen didn\u{2019}t start your session.")
        }
        ConsoleRefusal::SessionUnreachable => {
            format!("{host} couldn\u{2019}t open your session\u{2019}s display.")
        }
        ConsoleRefusal::SessionChanged => {
            format!("Your session at {host} changed. Sign in again to join it.")
        }
        ConsoleRefusal::AccountDenied => {
            format!("Your account isn\u{2019}t allowed to sign in at {host}.")
        }
        ConsoleRefusal::InUseOtherProduct => {
            format!("Your desktop on {host} is in use through another remote-desktop app.")
        }
        ConsoleRefusal::OtherProductStarted => {
            format!("Another remote-desktop app took over {host}.")
        }
        ConsoleRefusal::RemoteStateUnknown => format!(
            "Someone else is signed in at {host}, and it couldn\u{2019}t tell whether they are \
             using it remotely, so they weren\u{2019}t signed out."
        ),
        ConsoleRefusal::SwitchInProgress => {
            format!("A change of account is already in progress on {host}.")
        }
        ConsoleRefusal::SigningOut => {
            format!("This account is being signed out on {host}. Try again in a moment.")
        }
        ConsoleRefusal::SignInOtherProduct => match detail.and_then(sign_in_product) {
            Some(product) => format!(
                "{product} runs sign-in on {host}. Sign in with {product} once, disconnect it, \
                 then connect with Arcen."
            ),
            None => format!(
                "Another remote-desktop app runs sign-in on {host}. Sign in with it once, \
                 disconnect it, then connect with Arcen."
            ),
        },
    };
    if ended {
        format!("{host} ended your session. {sentence}")
    } else {
        sentence
    }
}

/// The console refusal a host's reason leads with or carries: its stable
/// code ([`ConsoleRefusal::token`], followed by `:` or the end), anywhere in
/// the text, since a close frame's reason arrives behind the Deck's own
/// words. The macOS Pier's earlier wording of `console_other_user` counts
/// too.
#[cfg(feature = "runtime")]
#[must_use]
fn console_refusal(reason: &str) -> Option<ConsoleRefusal> {
    const MAC_NOT_CONSOLE_OWNER: &str = "this Mac\u{2019}s screen belongs to another account";
    let coded = reason.match_indices("console_").find_map(|(at, _)| {
        let starts_word = reason[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !(before.is_ascii_alphanumeric() || before == '_'));
        let code: &str = reason[at..]
            .split(|c: char| !(c.is_ascii_lowercase() || c == '_'))
            .next()
            .unwrap_or_default();
        starts_word
            .then(|| {
                ConsoleRefusal::ALL
                    .into_iter()
                    .find(|refusal| refusal.token() == code)
            })
            .flatten()
    });
    coded.or_else(|| {
        let reason = reason.replace('\'', "\u{2019}");
        reason
            .contains(MAC_NOT_CONSOLE_OWNER)
            .then_some(ConsoleRefusal::OtherUser)
    })
}

/// The account a `console_other_user` refusal names (`console_other_user:
/// alice is signed in at this Mac; …`), when it is a plain account name.
/// Host text: anything else is left under Details.
fn console_owner(reason: &str) -> Option<&str> {
    const LIMIT: usize = 64;
    let prefix = format!("{}: ", ConsoleRefusal::OtherUser.token());
    let after = &reason[reason.find(&prefix)? + prefix.len()..];
    let owner = &after[..after.find(" is signed in")?];
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '\\');
    (!owner.is_empty() && owner.chars().count() <= LIMIT && owner.chars().all(plain))
        .then_some(owner)
}

/// The product a `console_sign_in_other_product` refusal names
/// (`console_sign_in_other_product: Lab Remote Product runs this workstation's
/// sign-in; …`), when it is a plain name. Host text: anything else is left
/// under Details.
fn sign_in_product(reason: &str) -> Option<&str> {
    const LIMIT: usize = 40;
    let prefix = format!("{}: ", ConsoleRefusal::SignInOtherProduct.token());
    let after = &reason[reason.find(&prefix)? + prefix.len()..];
    let product = &after[..after.find(" runs this workstation")?];
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-' | '+');
    (!product.is_empty()
        && product.chars().count() <= LIMIT
        && product.chars().all(plain)
        && !product.to_ascii_lowercase().starts_with("another "))
    .then_some(product)
}

/// What the words of a refused sign-in mean, for the reasons every Pier
/// gives in plain text (they carry no code). `None` for words this Deck
/// does not know.
#[cfg(feature = "runtime")]
fn sign_in_refusal(reason: &str) -> Option<SignInFailure> {
    let reason = reason.trim().to_ascii_lowercase();
    if reason.contains(arcen_session::auth_throttle::THROTTLED_MESSAGE) {
        return Some(SignInFailure::TooManyAttempts);
    }
    // Linux ("Authentication failed…"), Windows ("Invalid credentials"),
    // macOS ("the username or password is…"), and a host that said
    // nothing.
    let credentials = reason.is_empty()
        || reason.starts_with("authentication failed")
        || reason.starts_with("invalid credentials")
        || reason.starts_with("the username or password is")
        || reason == "the host refused the sign-in";
    if credentials {
        return Some(SignInFailure::CredentialsRefused);
    }
    if reason.starts_with("this account may not sign in") {
        return Some(SignInFailure::AccountNotAllowed);
    }
    if reason.starts_with(HOST_BUSY_REASON) {
        return Some(SignInFailure::HostBusy);
    }
    if reason.starts_with("authentication policy is unavailable")
        || reason.starts_with("remote sign-in is unavailable")
    {
        return Some(SignInFailure::SignInUnavailable);
    }
    None
}

/// The reason a session end gives under "Details", in plain words: a lost
/// or unreachable transport by its typed kind (never the transport's own
/// text), anything else as the engine words it.
#[cfg(feature = "runtime")]
#[must_use]
pub fn end_detail(end: &crate::runtime::session::SessionEnd) -> String {
    use crate::runtime::session::{DisconnectReason, TransientTransportError};
    let DisconnectReason::Transient(kind) = end.reason else {
        return end.message.clone();
    };
    match kind {
        TransientTransportError::TimedOut => "No answer from the host: the connection timed out.",
        TransientTransportError::ConnectionRefused => "The host refused the connection.",
        TransientTransportError::NetworkUnreachable => {
            "This computer has no route to the host's network."
        }
        TransientTransportError::HostUnreachable => "The host could not be reached on the network.",
        TransientTransportError::UnexpectedEof
        | TransientTransportError::ConnectionReset
        | TransientTransportError::ConnectionAborted
        | TransientTransportError::BrokenPipe
        | TransientTransportError::NotConnected => "The connection closed unexpectedly.",
        TransientTransportError::TransientIo => "A network error interrupted the connection.",
        TransientTransportError::SignInScreenClosed => {
            crate::runtime::session::SIGN_IN_SCREEN_CLOSED_TEXT
        }
    }
    .to_owned()
}

#[cfg(feature = "runtime")]
impl SignInFailure {
    /// What a session end means for sign-in: its typed reason
    /// ([`Self::after_end`]), made precise by what the host said where the
    /// reason alone cannot tell (a refused sign-in is not always a wrong
    /// password: [`console_refusal`], and the plain reasons every Pier
    /// gives). `None` for a deliberate disconnect.
    #[must_use]
    pub fn from_end(end: &crate::runtime::session::SessionEnd, stage: SignInStage) -> Option<Self> {
        use crate::runtime::session::{DisconnectReason, TerminalDisconnect};
        let base = Self::after_end(end.reason, stage)?;
        let DisconnectReason::Terminal(
            kind @ (TerminalDisconnect::Authentication
            | TerminalDisconnect::GracefulHostClose
            | TerminalDisconnect::Configuration),
        ) = end.reason
        else {
            return Some(base);
        };
        if let Some(refusal) = console_refusal(&end.message) {
            return Some(Self::ConsoleRefused {
                refusal,
                ended: stage == SignInStage::Session,
            });
        }
        if kind != TerminalDisconnect::Authentication {
            return Some(base);
        }
        let message = end.message.as_str();
        if message == crate::runtime::session::AUTH_DECLINED_TEXT {
            return Some(Self::NoticeDeclined);
        }
        if message == crate::runtime::session::DISCLAIMER_REQUIRED_TEXT {
            return Some(Self::NoticeRequired);
        }
        Some(match (sign_in_refusal(message), stage) {
            (Some(Self::CredentialsRefused), _) | (None, SignInStage::Session) => base,
            (Some(failure), _) => failure,
            (None, SignInStage::Attempt) => Self::HostRefused,
        })
    }

    /// What a session end means for sign-in, by its typed reason (never its
    /// text). `None` for a deliberate disconnect, which needs no word.
    #[must_use]
    pub fn after_end(
        reason: crate::runtime::session::DisconnectReason,
        stage: SignInStage,
    ) -> Option<Self> {
        use crate::runtime::session::{DisconnectReason, TerminalDisconnect};
        use arcen_protocol::messages::ResumeErrorCode;
        let failure = match reason {
            DisconnectReason::Terminal(TerminalDisconnect::Manual) => return None,
            DisconnectReason::Transient(_) => match stage {
                SignInStage::Attempt => Self::Unreachable,
                SignInStage::Session => Self::SessionEnded,
            },
            DisconnectReason::Terminal(TerminalDisconnect::Authentication) => match stage {
                SignInStage::Attempt => Self::CredentialsRefused,
                SignInStage::Session => Self::SessionEnded,
            },
            DisconnectReason::Terminal(TerminalDisconnect::GracefulHostClose) => match stage {
                SignInStage::Attempt => Self::HostClosedConnection,
                SignInStage::Session => Self::HostEndedSession,
            },
            DisconnectReason::Terminal(TerminalDisconnect::TlsIdentity) => {
                Self::HostIdentityChanged
            }
            DisconnectReason::Terminal(TerminalDisconnect::Resume(code)) => match code {
                Some(ResumeErrorCode::SessionGone | ResumeErrorCode::TopologyChanged) => {
                    Self::HostRestarted
                }
                Some(ResumeErrorCode::NativeIdentityChanged) => Self::AccountChanged,
                Some(
                    ResumeErrorCode::Unsupported
                    | ResumeErrorCode::Expired
                    | ResumeErrorCode::Replayed
                    | ResumeErrorCode::InternalFailure,
                )
                | None => Self::SessionEnded,
            },
            DisconnectReason::Terminal(TerminalDisconnect::Configuration) => {
                Self::SessionNotStarted
            }
            DisconnectReason::Terminal(TerminalDisconnect::HostRefused(refusal)) => {
                refusal.sign_in_failure(stage)
            }
            DisconnectReason::Terminal(TerminalDisconnect::AccountSwitch(stop)) => {
                Self::AccountSwitch(stop)
            }
            DisconnectReason::Terminal(TerminalDisconnect::Protocol) => Self::ConnectionFailed,
        };
        Some(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConnectionKind;

    fn selected(password: &str) -> ConnectionDraft {
        let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
        draft.saved_connection_id = Some("studio".to_owned());
        draft.host = "studio.example.internal".to_owned();
        draft.username = "artist".to_owned();
        draft.password = password.to_owned();
        draft
    }

    #[test]
    fn a_refused_sign_in_returns_home_with_the_error_and_the_password_cleared() {
        let mut state = SignInState::default();
        let home = state.return_home(
            SignInFailure::CredentialsRefused,
            selected("wrong"),
            Some("authentication failed: Invalid credentials".to_owned()),
        );
        assert!(home.password.is_empty());
        assert_eq!(home.username, "artist");
        assert_eq!(home.saved_connection_id.as_deref(), Some("studio"));
        let shown = state
            .under_field(Some("studio"), SignInField::Password)
            .expect("under the password");
        assert_eq!(shown.message, "Wrong username or password.");
        assert_eq!(
            shown.detail.as_deref(),
            Some("authentication failed: Invalid credentials")
        );
        assert!(state.acknowledgement().is_none());
        assert_eq!(state.take_focus(), Some(SignInField::Password));
        assert_eq!(state.take_focus(), None, "focused once");
        // Not under another connection's field.
        assert!(
            state
                .under_field(Some("other"), SignInField::Password)
                .is_none()
        );
    }

    #[test]
    fn connect_without_a_password_stays_home_with_the_field_focused() {
        let mut state = SignInState::default();
        assert_eq!(
            state.connect(&selected("")),
            Err(SignInFailure::PasswordMissing)
        );
        let shown = state
            .under_field(Some("studio"), SignInField::Password)
            .unwrap();
        assert_eq!(shown.message, "Enter your password to connect.");
        assert_eq!(state.take_focus(), Some(SignInField::Password));

        let mut nameless = selected("secret");
        nameless.username = "  ".to_owned();
        assert_eq!(
            state.connect(&nameless),
            Err(SignInFailure::UsernameMissing)
        );
        assert_eq!(state.take_focus(), Some(SignInField::Username));

        assert_eq!(state.connect(&selected("secret")), Ok(()));
        assert!(
            state.feedback().is_none(),
            "Connect clears the last outcome"
        );
        assert_eq!(state.take_focus(), None);
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn a_dropped_session_returns_home_asking_to_sign_in_again() {
        let mut state = SignInState::default();
        let failure = SignInFailure::after_end(
            crate::runtime::session::DisconnectReason::Transient(
                crate::runtime::session::TransientTransportError::ConnectionReset,
            ),
            SignInStage::Session,
        )
        .unwrap();
        let home = state.return_home(failure, selected(""), None);
        assert!(home.password.is_empty());
        let shown = state
            .under_field(Some("studio"), SignInField::Password)
            .unwrap();
        assert_eq!(
            shown.message,
            "Your session ended. Sign in again to continue."
        );
        assert_eq!(state.take_focus(), Some(SignInField::Password));
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn a_transport_failure_returns_home_with_a_reach_message() {
        use crate::runtime::session::{DisconnectReason, TransientTransportError};
        let mut state = SignInState::default();
        let failure = SignInFailure::after_end(
            DisconnectReason::Transient(TransientTransportError::TimedOut),
            SignInStage::Attempt,
        )
        .unwrap();
        assert_eq!(failure, SignInFailure::Unreachable);
        let _ = state.return_home(
            failure,
            selected("secret"),
            Some("connection timed out".into()),
        );
        assert_eq!(
            state
                .under_field(Some("studio"), SignInField::Password)
                .unwrap()
                .message,
            "Couldn\u{2019}t reach studio.example.internal. Check that it\u{2019}s on and that Arcen is \
             running there."
        );
    }

    #[test]
    fn changed_displays_ask_for_the_password_under_the_field() {
        let mut state = SignInState::default();
        let _ = state.return_home(SignInFailure::DisplaysChanged, selected("typed"), None);
        assert!(state.acknowledgement().is_none());
        assert_eq!(
            state
                .under_field(Some("studio"), SignInField::Password)
                .unwrap()
                .message,
            "Your displays changed. Sign in again to continue on the same desktop with them."
        );
        assert!(!SignInFailure::DisplaysChanged.is_definite());
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn only_an_outcome_about_no_field_asks_for_an_acknowledgement() {
        use crate::runtime::session::{DisconnectReason, TerminalDisconnect};
        use arcen_protocol::messages::ResumeErrorCode;
        let mut state = SignInState::default();
        let failure = SignInFailure::after_end(
            DisconnectReason::Terminal(TerminalDisconnect::Resume(Some(
                ResumeErrorCode::SessionGone,
            ))),
            SignInStage::Session,
        )
        .unwrap();
        assert_eq!(failure, SignInFailure::HostRestarted);
        let _ = state.return_home(failure, selected(""), None);
        assert!(state.acknowledgement().is_some());
        assert!(
            state
                .under_field(Some("studio"), SignInField::Password)
                .is_none()
        );
        assert_eq!(state.take_focus(), None, "nothing is focused under the box");
        state.acknowledge();
        assert!(state.feedback().is_none());
        assert_eq!(state.take_focus(), Some(SignInField::Password));
    }

    #[test]
    fn editing_a_credential_clears_a_field_outcome_but_not_a_box() {
        let mut state = SignInState::default();
        assert!(state.connect(&selected("")).is_err());
        state.credentials_edited();
        assert!(state.feedback().is_none());
        assert_eq!(state.take_focus(), None);

        let _ = state.return_home(SignInFailure::HostEndedSession, selected(""), None);
        state.credentials_edited();
        assert!(
            state.acknowledgement().is_some(),
            "a box is not about a field"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn transport_details_are_plain_words_without_the_transport_s_jargon() {
        use crate::runtime::session::{
            DisconnectReason, SessionEnd, TerminalDisconnect, TransientTransportError,
        };
        let end = |reason, message: &str| SessionEnd {
            reason,
            message: message.to_owned(),
            observed_at: std::time::Instant::now(),
        };
        let timed_out = end_detail(&end(
            DisconnectReason::Transient(TransientTransportError::TimedOut),
            "websocket error: IO error: timed out",
        ));
        assert_eq!(
            timed_out,
            "No answer from the host: the connection timed out."
        );
        for kind in [
            TransientTransportError::UnexpectedEof,
            TransientTransportError::ConnectionReset,
            TransientTransportError::TimedOut,
            TransientTransportError::ConnectionRefused,
            TransientTransportError::NetworkUnreachable,
            TransientTransportError::HostUnreachable,
            TransientTransportError::ConnectionAborted,
            TransientTransportError::BrokenPipe,
            TransientTransportError::NotConnected,
            TransientTransportError::TransientIo,
        ] {
            let detail = end_detail(&end(DisconnectReason::Transient(kind), "IO error: x"));
            assert!(
                !detail.to_lowercase().contains("websocket") && !detail.contains("IO error"),
                "{kind:?}: {detail}"
            );
        }
        // Anything else keeps the engine's own words.
        assert_eq!(
            end_detail(&end(
                DisconnectReason::Terminal(TerminalDisconnect::Configuration),
                "the host refused the requested video: selects yuv444",
            )),
            "the host refused the requested video: selects yuv444"
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn session_ends_map_by_their_typed_reason() {
        use crate::runtime::session::{DisconnectReason, TerminalDisconnect};
        use arcen_protocol::messages::ResumeErrorCode;
        let terminal = DisconnectReason::Terminal;
        let at = |reason, stage| SignInFailure::after_end(reason, stage);
        assert_eq!(
            at(terminal(TerminalDisconnect::Manual), SignInStage::Session),
            None
        );
        assert_eq!(
            at(
                terminal(TerminalDisconnect::Authentication),
                SignInStage::Attempt
            ),
            Some(SignInFailure::CredentialsRefused)
        );
        assert_eq!(
            at(
                terminal(TerminalDisconnect::GracefulHostClose),
                SignInStage::Session
            ),
            Some(SignInFailure::HostEndedSession)
        );
        assert_eq!(
            at(
                terminal(TerminalDisconnect::TlsIdentity),
                SignInStage::Attempt
            ),
            Some(SignInFailure::HostIdentityChanged)
        );
        assert_eq!(
            at(
                terminal(TerminalDisconnect::Resume(Some(ResumeErrorCode::Expired))),
                SignInStage::Attempt
            ),
            Some(SignInFailure::SessionEnded)
        );
        assert_eq!(
            at(
                terminal(TerminalDisconnect::Resume(Some(
                    ResumeErrorCode::NativeIdentityChanged
                ))),
                SignInStage::Session
            ),
            Some(SignInFailure::AccountChanged)
        );
        assert_eq!(
            at(
                terminal(TerminalDisconnect::Configuration),
                SignInStage::Attempt
            ),
            Some(SignInFailure::SessionNotStarted)
        );
        for failure in [
            SignInFailure::HostEndedSession,
            SignInFailure::HostIdentityChanged,
            SignInFailure::SessionNotStarted,
        ] {
            assert_eq!(failure.field(), None, "{failure:?} is acknowledged");
        }
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn a_stopped_change_of_account_returns_home_in_its_own_words_and_is_not_retried() {
        use crate::account_switch::AccountSwitchStop;
        use crate::runtime::session::{ConnectSmokeError, session_end_for};
        for stop in [
            AccountSwitchStop::Cancelled,
            AccountSwitchStop::Expired,
            AccountSwitchStop::LogoutVetoed,
            AccountSwitchStop::NewLoginFailed,
        ] {
            let end = session_end_for(&ConnectSmokeError::AccountSwitch {
                stop,
                detail: "console_sign_in_failed: the login screen refused".to_owned(),
            });
            let failure = SignInFailure::from_end(&end, SignInStage::Attempt).unwrap();
            assert_eq!(failure, SignInFailure::AccountSwitch(stop));
            assert_eq!(failure.field(), None, "{stop:?} is acknowledged");
            assert!(failure.is_definite(), "{stop:?} never asks again by itself");
            let mut state = SignInState::default();
            let _ = state.return_home(failure, selected("secret"), Some(end_detail(&end)));
            let shown = state.acknowledgement().unwrap();
            assert_eq!(shown.message, stop.message("studio.example.internal"));
        }
    }

    #[cfg(feature = "runtime")]
    fn refused(reason: &str) -> crate::runtime::session::SessionEnd {
        crate::runtime::session::session_end_for(
            &crate::runtime::session::ConnectSmokeError::AuthFailed(reason.to_owned()),
        )
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn a_host_whose_sign_in_another_product_runs_says_which_to_use() {
        use arcen_session::console_session::ConsoleNotice;
        let reason = ConsoleNotice::sign_in_other_product(
            Some("labproduct.service"),
            Some("Lab Remote Product"),
        )
        .deck_reason();
        let end = refused(&reason);
        let failure = SignInFailure::from_end(&end, SignInStage::Attempt).unwrap();
        assert_eq!(
            failure,
            SignInFailure::ConsoleRefused {
                refusal: ConsoleRefusal::SignInOtherProduct,
                ended: false
            }
        );
        assert!(failure.is_definite());
        assert_eq!(failure.field(), None, "an acknowledgement box");
        assert_eq!(
            failure.message_with("studio", Some(&end.message)),
            "Lab Remote Product runs sign-in on studio. Sign in with Lab Remote Product once, \
             disconnect it, then connect with Arcen."
        );
        // Unnamed, or a name that is not plain: the product is not named.
        for reason in [
            ConsoleRefusal::SignInOtherProduct.deck_reason(),
            "console_sign_in_other_product: \u{202e}x runs this workstation's sign-in",
            "console_sign_in_other_product: a (b) runs this workstation's sign-in",
        ] {
            let failure = SignInFailure::from_end(&refused(reason), SignInStage::Attempt).unwrap();
            assert_eq!(
                failure.message_with("studio", Some(reason)),
                "Another remote-desktop app runs sign-in on studio. Sign in with it once, \
                 disconnect it, then connect with Arcen."
            );
        }
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn another_account_at_the_host_is_said_with_its_name_not_as_a_wrong_password() {
        let end = refused(
            "console_other_user: flame_db is signed in at this Mac; sign in as flame_db, or \
             switch to your account at the Mac first",
        );
        let failure = SignInFailure::from_end(&end, SignInStage::Attempt).unwrap();
        assert_eq!(
            failure,
            SignInFailure::ConsoleRefused {
                refusal: ConsoleRefusal::OtherUser,
                ended: false
            }
        );
        let mut state = SignInState::default();
        let _ = state.return_home(failure, selected("secret"), Some(end_detail(&end)));
        let shown = state
            .under_field(Some("studio"), SignInField::Username)
            .expect("under the username");
        assert_eq!(
            shown.message,
            "Someone else is signed in at studio.example.internal (flame_db). Sign in as that \
             user, or switch to your account there first."
        );
        assert!(
            shown
                .detail
                .as_deref()
                .is_some_and(|detail| detail.starts_with("console_other_user: flame_db"))
        );
        assert_eq!(state.take_focus(), Some(SignInField::Username));
        // A Pier that names nobody, or something that is not a plain name.
        for reason in [
            "console_other_user: another account is signed in at this workstation.",
            "console_other_user: al ice is signed in at this Mac",
            "console_other_user: \u{202e}alice is signed in at this Mac",
            "this Mac\u{2019}s screen belongs to another account; sign in as the person at the \
             console, or switch to your account at the machine first",
        ] {
            let failure = SignInFailure::from_end(&refused(reason), SignInStage::Attempt).unwrap();
            assert_eq!(
                failure.message_with("studio", Some(reason)),
                "Someone else is signed in at studio. Sign in as that user, or switch to your \
                 account there first.",
                "{reason}"
            );
        }
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn every_console_refusal_has_its_own_headline_however_it_arrives() {
        use crate::runtime::session::ConnectSmokeError;
        let credentials = SignInFailure::CredentialsRefused.message("studio");
        let mut headlines = std::collections::HashSet::new();
        for refusal in ConsoleRefusal::ALL {
            let reason = refusal.deck_reason();
            let arrivals = [
                (refused(reason), SignInStage::Attempt, false),
                (
                    crate::runtime::session::session_end_for(&ConnectSmokeError::ClosedByHost(
                        format!(": {reason}"),
                    )),
                    SignInStage::Attempt,
                    false,
                ),
                (
                    crate::runtime::session::session_end_for(
                        &ConnectSmokeError::SessionSetupFailed(reason.to_owned()),
                    ),
                    SignInStage::Attempt,
                    false,
                ),
                (
                    crate::runtime::session::SessionEnd {
                        reason: crate::runtime::session::DisconnectReason::Terminal(
                            crate::runtime::session::TerminalDisconnect::GracefulHostClose,
                        ),
                        message: format!("Host closed the session: {reason}"),
                        observed_at: std::time::Instant::now(),
                    },
                    SignInStage::Session,
                    true,
                ),
            ];
            for (end, stage, ended) in arrivals {
                let failure = SignInFailure::from_end(&end, stage).unwrap();
                assert_eq!(
                    failure,
                    SignInFailure::ConsoleRefused { refusal, ended },
                    "{}",
                    end.message
                );
                let message = failure.message_with("studio", Some(&end.message));
                assert_ne!(message, credentials, "{refusal:?}");
                assert!(
                    !message.contains("console_") && message.contains("studio"),
                    "{message}"
                );
                if ended {
                    assert_eq!(failure.field(), None, "a box says a session ended");
                }
                headlines.insert(message);
            }
        }
        // One headline per refusal and per sign-in or session.
        assert_eq!(headlines.len(), 2 * ConsoleRefusal::ALL.len());
        // A longer code is never read as the one it starts with.
        assert_eq!(
            console_refusal("console_other_user_other_product: x"),
            Some(ConsoleRefusal::OtherUserOtherProduct)
        );
        assert_eq!(console_refusal("not_console_other_user: x"), None);
        assert_eq!(console_refusal("console_unknown_future: x"), None);
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn a_host_that_could_not_keep_its_screen_private_says_so_plainly() {
        let reason = "console_privacy_unavailable: the workstation's screens, speakers and input \
                      could not be made private.";
        let end = crate::runtime::session::SessionEnd {
            reason: crate::runtime::session::DisconnectReason::Terminal(
                crate::runtime::session::TerminalDisconnect::GracefulHostClose,
            ),
            message: format!("Host closed the session: {reason}"),
            observed_at: std::time::Instant::now(),
        };
        let failure = SignInFailure::from_end(&end, SignInStage::Session).unwrap();
        let mut state = SignInState::default();
        let _ = state.return_home(failure, selected(""), Some(end_detail(&end)));
        let shown = state.acknowledgement().expect("a box");
        assert_eq!(
            shown.message,
            "studio.example.internal couldn\u{2019}t keep its screen private, so it ended the \
             session."
        );
        assert_eq!(
            shown.detail.as_deref(),
            Some(format!("Host closed the session: {reason}").as_str())
        );
        assert_eq!(
            SignInFailure::from_end(&refused(reason), SignInStage::Attempt)
                .unwrap()
                .message("studio"),
            "studio couldn\u{2019}t keep its screen private, so it didn\u{2019}t connect."
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn only_a_credential_refusal_says_wrong_username_or_password() {
        use crate::runtime::session::ConnectSmokeError;
        // Every Pier's wrong-password words, and a host that said nothing.
        for reason in [
            "Authentication failed",
            "Authentication failed: wrong user name or password, or this account may not sign \
             in at this workstation.",
            "Invalid credentials",
            "the username or password is incorrect",
            "the username or password is not valid text",
            "",
        ] {
            assert_eq!(
                SignInFailure::from_end(&refused(reason), SignInStage::Attempt),
                Some(SignInFailure::CredentialsRefused),
                "{reason:?}"
            );
        }
        let at = |end: &crate::runtime::session::SessionEnd| {
            SignInFailure::from_end(end, SignInStage::Attempt).unwrap()
        };
        let session_end_for = crate::runtime::session::session_end_for;
        for (end, failure, message) in [
            (
                refused(arcen_session::auth_throttle::THROTTLED_MESSAGE),
                SignInFailure::TooManyAttempts,
                "Too many failed sign-ins. Wait a few minutes, then try again.",
            ),
            (
                refused("this account may not sign in here"),
                SignInFailure::AccountNotAllowed,
                "This account may not sign in at studio.",
            ),
            (
                refused("another session is already using this Mac"),
                SignInFailure::HostBusy,
                "Another session is already using studio.",
            ),
            (
                refused("authentication policy is unavailable on this host"),
                SignInFailure::SignInUnavailable,
                "studio can\u{2019}t take a sign-in right now. Try again later.",
            ),
            (
                refused("Remote sign-in is unavailable: console takeover could not be completed."),
                SignInFailure::SignInUnavailable,
                "studio can\u{2019}t take a sign-in right now. Try again later.",
            ),
            (
                session_end_for(&ConnectSmokeError::AuthDeclined),
                SignInFailure::NoticeDeclined,
                "You declined studio\u{2019}s notice, so Arcen didn\u{2019}t sign in.",
            ),
            (
                session_end_for(&ConnectSmokeError::DisclaimerRequired),
                SignInFailure::NoticeRequired,
                "studio asks you to accept a notice before signing in. Connect from Home to read \
                 it.",
            ),
            (
                refused("display request: 9 monitors exceed this host's limit of 4"),
                SignInFailure::HostRefused,
                "studio refused the sign-in. Its reason is under Details.",
            ),
        ] {
            assert_eq!(at(&end), failure, "{}", end.message);
            assert_eq!(failure.message("studio"), message);
        }
        // A resume the host refuses for the password still asks to sign in
        // again, as before.
        assert_eq!(
            SignInFailure::from_end(&refused("Authentication failed"), SignInStage::Session),
            Some(SignInFailure::SessionEnded)
        );
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn each_host_refusal_reason_has_its_own_class_and_none_is_a_password() {
        for refusal in ConsoleRefusal::ALL {
            let read = HostRefusal::from_reason(refusal.deck_reason());
            assert_eq!(read, Some(HostRefusal::Console(refusal)));
            assert_eq!(read.unwrap().reason_class(), refusal.token());
        }
        assert_eq!(
            HostRefusal::from_reason("Another session is already using this Mac"),
            Some(HostRefusal::Busy)
        );
        assert_eq!(HostRefusal::Busy.reason_class(), "host_busy");
        // The Linux and Windows Piers' busy desktops, in the words they send.
        for busy in arcen_session::session_admission::DesktopBusyRefusal::ALL
            .map(arcen_session::session_admission::DesktopBusyRefusal::deck_reason)
            .into_iter()
            .chain([
                arcen_session::session_admission::DetachedTakeoverRefusal::AnotherUsersDesktop
                    .deck_reason(),
            ])
        {
            assert_eq!(
                HostRefusal::from_reason(busy),
                Some(HostRefusal::Busy),
                "{busy}"
            );
        }
        for occupied in [
            arcen_session::x11_seat::DECK_REASON_SEAT_OCCUPIED,
            arcen_session::x11_seat::DECK_REASON_SEAT_OTHER_PRODUCT,
            "Another remote-desktop service (labrdp) holds this host's display. Sign out of it \
             first.",
        ] {
            assert_eq!(
                HostRefusal::from_reason(occupied),
                Some(HostRefusal::SeatOccupied),
                "{occupied}"
            );
        }
        assert_eq!(HostRefusal::SeatOccupied.reason_class(), "seat_occupied");
        for not_the_host in [
            "",
            "Authentication failed",
            "Invalid credentials",
            "the username or password is incorrect",
            "this account may not sign in here",
            arcen_session::auth_throttle::THROTTLED_MESSAGE,
            "not_console_other_user: x",
            // Not another session: the desktop is finishing its last connection.
            arcen_session::session_admission::DetachedTakeoverRefusal::OwnerDidNotAnswer
                .deck_reason(),
        ] {
            assert_eq!(
                HostRefusal::from_reason(not_the_host),
                None,
                "{not_the_host:?}"
            );
        }
        // What each says on Home, by stage.
        assert_eq!(
            HostRefusal::SeatOccupied.sign_in_failure(SignInStage::Attempt),
            SignInFailure::HostBusy
        );
        assert_eq!(
            HostRefusal::Console(ConsoleRefusal::OtherUser).sign_in_failure(SignInStage::Session),
            SignInFailure::ConsoleRefused {
                refusal: ConsoleRefusal::OtherUser,
                ended: true,
            }
        );
    }

    #[test]
    fn only_what_waiting_cannot_change_is_definite() {
        let console = |refusal| SignInFailure::ConsoleRefused {
            refusal,
            ended: false,
        };
        for definite in [
            SignInFailure::CredentialsRefused,
            SignInFailure::TooManyAttempts,
            SignInFailure::AccountNotAllowed,
            SignInFailure::NoticeDeclined,
            SignInFailure::HostRefused,
            SignInFailure::HostIdentityChanged,
            console(ConsoleRefusal::OtherUser),
            console(ConsoleRefusal::OtherUserOtherProduct),
            console(ConsoleRefusal::AccountDenied),
            console(ConsoleRefusal::InUseOtherProduct),
        ] {
            assert!(definite.is_definite(), "{definite:?}");
        }
        for passing in [
            SignInFailure::HostBusy,
            SignInFailure::Unreachable,
            SignInFailure::HostClosedConnection,
            SignInFailure::SessionNotStarted,
            SignInFailure::SignInUnavailable,
            console(ConsoleRefusal::NoConsoleSession),
            console(ConsoleRefusal::SessionUnreachable),
            console(ConsoleRefusal::SessionChanged),
        ] {
            assert!(!passing.is_definite(), "{passing:?}");
        }
    }

    #[test]
    fn an_unsaved_connection_keeps_its_outcome_off_every_saved_card() {
        let mut state = SignInState::default();
        let mut quick = selected("x");
        quick.saved_connection_id = None;
        let _ = state.return_home(SignInFailure::CredentialsRefused, quick, None);
        assert!(
            state
                .under_field(Some("studio"), SignInField::Password)
                .is_none()
        );
        assert!(state.under_field(None, SignInField::Password).is_some());
        assert!(state.keeps_unsaved_connection());
        state.clear();
        assert!(!state.keeps_unsaved_connection());
    }
}
