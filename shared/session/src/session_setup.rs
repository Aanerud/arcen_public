// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The order a Pier answers a sign-in in, and the shape of a refusal that
//! comes after the credentials were accepted.
//!
//! Every Pier answers an `auth_response` the same way:
//!
//! 1. A malformed request (shape, disclaimer, display list) is refused as a
//!    sign-in, before any credential is checked.
//! 2. The credentials are checked. The auth throttle learns this outcome and
//!    nothing else ([`crate::auth_throttle`]).
//! 3. Only then is the session's video request resolved against what this
//!    host can serve.
//! 4. The Pier answers `auth_result` once: success, a sign-in refusal, or —
//!    credentials accepted, session not servable — a session-setup refusal
//!    (`session_setup_failed`) whose message leads with a [`SetupRefusal`]
//!    code.
//!
//! Resolving the plan after the credentials means a peer without them learns
//! only "authentication failed", never what this host can encode, and a plan
//! the host refuses is never counted as a failed sign-in. Resolving it before
//! `auth_result` means a Deck is never told "authenticated" for a session the
//! host is about to drop without a word.

use std::fmt::{Display, Formatter};

/// Code leading a refused video request.
pub const VIDEO_REQUEST_REFUSED: &str = "video_request_refused";

/// Code leading a refused multi-monitor request.
pub const MULTI_MONITOR_REFUSED: &str = "multi_monitor_refused";

/// How Piers before this module worded a refused video request (an
/// unsuccessful `auth_result` without `session_setup_failed`). A Deck still
/// recognises it, so a newer Deck words an older Pier's refusal correctly.
pub const LEGACY_VIDEO_REQUEST_REJECTED: &str = "Video request rejected: ";

/// Why a Pier that accepted the credentials will not start the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupRefusal {
    /// The Deck's video request cannot be served by this host (or names
    /// something the Deck itself said it cannot decode).
    VideoRequest(String),
    /// The Deck's multi-monitor request cannot be admitted.
    MultiMonitor(String),
}

impl SetupRefusal {
    /// A refused video request, from the resolver's own reason.
    #[must_use]
    pub fn video_request(detail: impl Display) -> Self {
        Self::VideoRequest(detail.to_string())
    }

    /// A refused multi-monitor request.
    #[must_use]
    pub fn multi_monitor(detail: impl Display) -> Self {
        Self::MultiMonitor(detail.to_string())
    }

    /// The stable code leading the message.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::VideoRequest(_) => VIDEO_REQUEST_REFUSED,
            Self::MultiMonitor(_) => MULTI_MONITOR_REFUSED,
        }
    }

    /// The reason without the code.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::VideoRequest(detail) | Self::MultiMonitor(detail) => detail,
        }
    }

    /// The `auth_result` message: the code, then the reason.
    #[must_use]
    pub fn message(&self) -> String {
        self.to_string()
    }

    /// The `stage` of the host's `SESSION_AUTH_FAIL` event.
    #[must_use]
    pub const fn log_stage(&self) -> &'static str {
        match self {
            Self::VideoRequest(_) => "validate_video_request",
            Self::MultiMonitor(_) => "validate_multi_monitor_request",
        }
    }

    /// The `reason_class` of the host's `SESSION_AUTH_FAIL` event.
    #[must_use]
    pub const fn log_reason_class(&self) -> &'static str {
        "setup_refused"
    }

    /// Reads a refusal back from an `auth_result` message, on the Deck.
    ///
    /// Recognises the coded form this module writes and, for a video request,
    /// the uncoded wording of older Piers. Anything else is `None`.
    #[must_use]
    pub fn from_message(message: &str) -> Option<Self> {
        let coded = |code: &str| {
            message
                .strip_prefix(code)
                .and_then(|rest| rest.strip_prefix(": "))
        };
        if let Some(detail) = coded(VIDEO_REQUEST_REFUSED)
            .or_else(|| message.strip_prefix(LEGACY_VIDEO_REQUEST_REJECTED))
        {
            return Some(Self::video_request(detail));
        }
        coded(MULTI_MONITOR_REFUSED).map(Self::multi_monitor)
    }
}

impl Display for SetupRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code(), self.detail())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_leads_with_its_code_and_reads_back() {
        let refusal = SetupRefusal::video_request(
            "initial video request selects yuv444 without client decode support",
        );
        assert_eq!(
            refusal.message(),
            "video_request_refused: initial video request selects yuv444 without client decode support"
        );
        assert_eq!(
            SetupRefusal::from_message(&refusal.message()),
            Some(refusal)
        );
        let multi = SetupRefusal::multi_monitor("2 displays need 4:4:4");
        assert_eq!(SetupRefusal::from_message(&multi.message()), Some(multi));
    }

    #[test]
    fn an_older_piers_video_refusal_is_still_recognised() {
        assert_eq!(
            SetupRefusal::from_message(
                "Video request rejected: initial video request: selects yuv444"
            ),
            Some(SetupRefusal::video_request(
                "initial video request: selects yuv444"
            ))
        );
        assert_eq!(SetupRefusal::from_message("Authentication failed"), None);
        assert_eq!(SetupRefusal::from_message("video_request_refused"), None);
        assert_eq!(
            SetupRefusal::from_message("hdr_not_activated: the display stayed SDR"),
            None
        );
    }
}
