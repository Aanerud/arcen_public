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
}

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
            Self::UsernameMissing => Some(SignInField::Username),
            Self::PasswordMissing
            | Self::PasswordRequired
            | Self::CredentialsRefused
            | Self::Unreachable
            | Self::SessionEnded => Some(SignInField::Password),
            Self::HostClosedConnection
            | Self::HostEndedSession
            | Self::HostRestarted
            | Self::AccountChanged
            | Self::HostIdentityChanged
            | Self::SessionNotStarted
            | Self::ConnectionFailed => None,
        }
    }

    /// What the user reads, naming the host where it helps.
    #[must_use]
    pub fn message(self, host: &str) -> String {
        match self {
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
        self.feedback = Some(SignInFeedback {
            failure,
            field,
            message: failure.message(draft.display_name()),
            detail: detail.filter(|detail| !detail.trim().is_empty()),
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
    }
    .to_owned()
}

#[cfg(feature = "runtime")]
impl SignInFailure {
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
