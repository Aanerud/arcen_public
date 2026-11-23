// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! How a Pier's stream into the user's session ended, and what that means
//! for the Deck, the log and the hand-back (ADR 0025 §4).
//!
//! The capture a Pier streams from can be stopped from outside Arcen. Since
//! macOS 14 every `ScreenCaptureKit` stream shows a system menu-bar item,
//! "Currently Sharing", whose **Stop Sharing** button anyone at the desk or
//! watching the remote desktop can press. That is not a fault to recover
//! from and not a capture to restart: somebody stopped sharing. The
//! attachment ends through the ordinary leave path, which locks the session
//! and hands the desk back ([`super::DETACH_STEPS`]), and the Deck is told
//! why with a close reason, so it does not reconnect into a desktop its
//! owner just stopped sharing. A stream the system stopped for its own
//! reasons ([`capture_stop_cause`]) is a capture failure, as before: the
//! session is locked, and the Deck, told nothing, may reconnect.

use super::{Departure, DetachStep};
use crate::session_setup::DECK_REASON_CAPTURE_START_FAILED;

/// What the Deck is told, as a close reason, when a person stopped the
/// host's capture (macOS's Stop Sharing). Led by its code, like the other
/// close reasons.
pub const DECK_REASON_SHARING_STOPPED: &str = "capture_stopped: Sharing was stopped on the host.";

/// `ScreenCaptureKit`'s error domain (`SCStreamErrorDomain`).
pub const SCREEN_CAPTURE_KIT_ERROR_DOMAIN: &str = "com.apple.ScreenCaptureKit.SCStreamErrorDomain";

/// `SCStreamErrorUserStopped`: a person pressed the system's Stop Sharing.
pub const SCREEN_CAPTURE_KIT_USER_STOPPED: i64 = -3817;

/// Why a capture the operating system stopped ended, from the error it
/// reported (`domain`, `code`). Only a person's own Stop Sharing
/// (`SCStreamErrorDomain` -3817) is [`StreamEndCause::CaptureStopped`]:
/// the Deck is told and does not reconnect. Every other stop (the system
/// stopping the stream, a display removed or reconfigured, a lost
/// connection to the window server) is [`StreamEndCause::MediaFailed`], as
/// before: no close reason, and the Deck may reconnect. The session is
/// locked either way.
#[must_use]
pub fn capture_stop_cause(domain: &str, code: i64) -> StreamEndCause {
    if domain == SCREEN_CAPTURE_KIT_ERROR_DOMAIN && code == SCREEN_CAPTURE_KIT_USER_STOPPED {
        StreamEndCause::CaptureStopped
    } else {
        StreamEndCause::MediaFailed
    }
}

/// Why the stream stopped, as the Pier's streamer reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEndCause {
    /// It ran to its end (a frame budget, or a Deck that closed cleanly).
    Completed,
    /// A person stopped the host's capture with the system's Stop Sharing
    /// ([`capture_stop_cause`]).
    CaptureStopped,
    /// The Deck's transport went away.
    PeerGone,
    /// Capture or encode failed for another reason.
    MediaFailed,
    /// Anything else (input, clipboard, protocol, a stalled write).
    Other,
}

/// How the stream ended, decided once from its cause and what it sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// It ran to its end.
    Completed,
    /// A person stopped sharing on the host. Never restarted: somebody
    /// stopped it on purpose, and a capture started again would put the same
    /// desktop back on the network against that choice.
    CaptureStopped,
    /// Capture or encode failed before a single frame was sent.
    CaptureStartFailed,
    /// The Deck's transport went away.
    TransportLost,
    /// The host failed while streaming.
    Failed,
}

impl StreamEnd {
    /// Decides how a stream that stopped for `cause` after sending
    /// `frames_sent` frames ended. A stopped capture is named as such
    /// whether or not a frame went out: it was stopped, not broken.
    #[must_use]
    pub const fn classify(cause: StreamEndCause, frames_sent: u64) -> Self {
        match cause {
            StreamEndCause::Completed => Self::Completed,
            StreamEndCause::CaptureStopped => Self::CaptureStopped,
            StreamEndCause::PeerGone => Self::TransportLost,
            StreamEndCause::MediaFailed if frames_sent == 0 => Self::CaptureStartFailed,
            StreamEndCause::MediaFailed | StreamEndCause::Other => Self::Failed,
        }
    }

    /// The `reason_class` of the Pier's `SESSION_END`.
    #[must_use]
    pub const fn reason_class(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::CaptureStopped => "capture_stopped",
            Self::TransportLost => "transport_error",
            Self::CaptureStartFailed | Self::Failed => "failed",
        }
    }

    /// The close reason the Deck is sent before the hand-back, or `None`
    /// when the Deck already knows (it left, or its transport did) or the
    /// stream ended cleanly.
    #[must_use]
    pub const fn deck_reason(self) -> Option<&'static str> {
        match self {
            Self::CaptureStopped => Some(DECK_REASON_SHARING_STOPPED),
            Self::CaptureStartFailed => Some(DECK_REASON_CAPTURE_START_FAILED),
            Self::Completed | Self::TransportLost | Self::Failed => None,
        }
    }

    /// Why the Deck's attachment ended, for the hand-back. Every ending
    /// locks the session (ADR 0025: disconnect locks, never ends).
    #[must_use]
    pub const fn departure(self) -> Departure {
        match self {
            Self::CaptureStopped => Departure::CaptureStopped,
            Self::Completed | Self::CaptureStartFailed | Self::TransportLost | Self::Failed => {
                Departure::DeckLeft
            }
        }
    }

    /// Whether the host failed: the process reports a failed session.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        !matches!(self, Self::Completed)
    }
}

/// One step of ending a Deck's attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveStep {
    /// Send the Deck this close reason.
    TellDeck(&'static str),
    /// One step of the hand-back, in the host's order.
    Detach(DetachStep),
    /// The host handed the session back itself: its lock guard stands down
    /// ([`super::owner_death::GUARD_RELEASE`]).
    ReleaseGuard,
}

/// The steps that end an attachment, in order: the Deck is told why when it
/// does not know ([`StreamEnd::deck_reason`]), then the hand-back in the
/// host's `order` ([`super::DETACH_STEPS`] locks first), and only then does
/// the lock guard stand down, so a host that dies mid-hand-back still has
/// its session locked by the guard. `end` is `None` when something else
/// ended the stream and told the Deck already (the desk watch).
#[must_use]
pub fn leave_steps(end: Option<StreamEnd>, order: &[DetachStep]) -> Vec<LeaveStep> {
    end.and_then(StreamEnd::deck_reason)
        .map(LeaveStep::TellDeck)
        .into_iter()
        .chain(order.iter().copied().map(LeaveStep::Detach))
        .chain(std::iter::once(LeaveStep::ReleaseGuard))
        .collect()
}

/// Why the attachment ended, for the lock: the stream's own departure, or
/// [`Departure::DeckLeft`] when something else ended it.
#[must_use]
pub fn leave_departure(end: Option<StreamEnd>) -> Departure {
    match end {
        Some(end) => end.departure(),
        None => Departure::DeckLeft,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console_session::DETACH_STEPS;

    #[test]
    fn a_stopped_capture_tells_the_deck_before_the_lock_and_releases_the_guard_last() {
        let steps = leave_steps(Some(StreamEnd::CaptureStopped), &DETACH_STEPS);
        assert_eq!(
            steps,
            [
                LeaveStep::TellDeck(DECK_REASON_SHARING_STOPPED),
                LeaveStep::Detach(DetachStep::LockSession),
                LeaveStep::Detach(DetachStep::AwaitLockScreen),
                LeaveStep::Detach(DetachStep::RestoreOutputs),
                LeaveStep::Detach(DetachStep::RestoreAudio),
                LeaveStep::Detach(DetachStep::ReleaseInput),
                LeaveStep::ReleaseGuard,
            ]
        );
        assert_eq!(
            leave_departure(Some(StreamEnd::CaptureStopped)),
            Departure::CaptureStopped
        );
    }

    #[test]
    fn a_deck_that_knows_is_not_told_and_still_gets_the_whole_hand_back() {
        for end in [
            None,
            Some(StreamEnd::TransportLost),
            Some(StreamEnd::Completed),
        ] {
            let steps = leave_steps(end, &DETACH_STEPS);
            assert_eq!(
                steps.first(),
                Some(&LeaveStep::Detach(DetachStep::LockSession))
            );
            assert_eq!(steps.last(), Some(&LeaveStep::ReleaseGuard));
            assert_eq!(steps.len(), DETACH_STEPS.len() + 1);
            assert_eq!(leave_departure(end), Departure::DeckLeft);
        }
    }

    #[test]
    fn only_a_persons_stop_sharing_is_a_stopped_capture() {
        assert_eq!(
            capture_stop_cause(SCREEN_CAPTURE_KIT_ERROR_DOMAIN, -3817),
            StreamEndCause::CaptureStopped
        );
        // The system stopped it (-3821), the connection to the window
        // server was lost (-3805), a display went away (-3815), or another
        // domain reported the same number: the old ending, no close reason.
        for (domain, code) in [
            (SCREEN_CAPTURE_KIT_ERROR_DOMAIN, -3821),
            (SCREEN_CAPTURE_KIT_ERROR_DOMAIN, -3805),
            (SCREEN_CAPTURE_KIT_ERROR_DOMAIN, -3815),
            (SCREEN_CAPTURE_KIT_ERROR_DOMAIN, 0),
            ("NSOSStatusErrorDomain", -3817),
            ("", -3817),
        ] {
            let cause = capture_stop_cause(domain, code);
            assert_eq!(cause, StreamEndCause::MediaFailed, "{domain} {code}");
            let end = StreamEnd::classify(cause, 120);
            assert_eq!(end, StreamEnd::Failed);
            assert_eq!(end.deck_reason(), None);
            assert!(end.departure().locks_session());
        }
    }

    #[test]
    fn stopped_sharing_tells_the_deck_and_locks_the_session() {
        for frames_sent in [0, 1, 9_000] {
            let end = StreamEnd::classify(StreamEndCause::CaptureStopped, frames_sent);
            assert_eq!(end, StreamEnd::CaptureStopped);
            assert_eq!(end.reason_class(), "capture_stopped");
            assert_eq!(end.deck_reason(), Some(DECK_REASON_SHARING_STOPPED));
            assert_eq!(end.departure(), Departure::CaptureStopped);
            assert!(end.departure().locks_session());
        }
    }

    #[test]
    fn the_sharing_stopped_reason_is_coded_and_fits_a_close_frame() {
        // A close frame carries 123 bytes of reason.
        assert!(DECK_REASON_SHARING_STOPPED.len() <= 123);
        assert!(DECK_REASON_SHARING_STOPPED.starts_with("capture_stopped: "));
    }

    #[test]
    fn other_endings_keep_their_classes() {
        let cases = [
            (
                StreamEndCause::Completed,
                5,
                StreamEnd::Completed,
                "completed",
                None,
            ),
            (
                StreamEndCause::MediaFailed,
                0,
                StreamEnd::CaptureStartFailed,
                "failed",
                Some(DECK_REASON_CAPTURE_START_FAILED),
            ),
            (
                StreamEndCause::MediaFailed,
                3,
                StreamEnd::Failed,
                "failed",
                None,
            ),
            (StreamEndCause::Other, 0, StreamEnd::Failed, "failed", None),
            (
                StreamEndCause::PeerGone,
                0,
                StreamEnd::TransportLost,
                "transport_error",
                None,
            ),
        ];
        for (cause, frames, end, class, reason) in cases {
            let decided = StreamEnd::classify(cause, frames);
            assert_eq!(decided, end, "{cause:?}");
            assert_eq!(decided.reason_class(), class);
            assert_eq!(decided.deck_reason(), reason);
            assert_eq!(decided.departure(), Departure::DeckLeft);
            assert!(decided.departure().locks_session());
            assert_eq!(decided.is_failure(), end != StreamEnd::Completed);
        }
    }
}
