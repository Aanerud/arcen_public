// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The clipboard seam.
//!
//! The shared clipboard controller
//! (`runtime::clipboard_controller`: policy, size limits, PNG
//! validation, echo suppression, sequence numbers) talks to the operating
//! system's clipboard only through [`PlatformClipboard`]. macOS:
//! `NSPasteboard` (change count, TIFF → PNG);
//! Windows: a message-only window with `AddClipboardFormatListener`,
//! `GetClipboardSequenceNumber`, `CF_UNICODETEXT` and a registered `PNG`
//! format (DIB → PNG only when no PNG is offered).

use arcen_protocol::messages::ClipboardContentKind;
use zeroize::Zeroizing;

/// The operating system's clipboard, as the shared controller sees it.
///
/// Not `Send`: macOS requires the main thread, Windows the thread that owns
/// the clipboard window. Every read returns owned bytes; the controller
/// validates them against the session's clipboard policy before anything
/// leaves the Deck.
pub trait PlatformClipboard {
    /// A value that changes whenever any application writes the clipboard
    /// (`changeCount`, `GetClipboardSequenceNumber`). The controller reads
    /// the contents only when it changed.
    fn change_token(&mut self) -> u64;

    /// Arcen's own origin marker, if the current contents carry one (the
    /// controller wrote them; `arcen_media::clipboard::ECHO_MARKER_BYTES`
    /// long). Lets the controller suppress echoes back to the Pier.
    fn read_origin_marker(&mut self) -> Option<Vec<u8>>;

    /// Plain text, wiped on drop (it may be a password).
    fn read_text(&mut self) -> Option<Zeroizing<String>>;

    /// An image as PNG bytes no longer than `max_encoded_bytes`. The
    /// platform converts its native image format (TIFF, DIB) when no PNG is
    /// offered, and refuses rather than truncates anything larger.
    fn read_png(&mut self, max_encoded_bytes: usize) -> Option<Vec<u8>>;

    /// Whether a read since the last call could not look at the clipboard
    /// at all (Windows: another process held it open), as opposed to
    /// finding nothing there. The controller then looks at the same change
    /// again at its next poll instead of treating it as empty. Clears the
    /// flag. `NSPasteboard` reads cannot be blocked.
    fn take_read_blocked(&mut self) -> bool {
        false
    }

    /// Replaces the contents with `bytes` of `kind`, tagged with
    /// `origin_marker` so the next poll recognises them as Arcen's. Returns
    /// whether the write took.
    fn write(&mut self, origin_marker: &[u8], kind: ClipboardContentKind, bytes: &[u8]) -> bool;
}

/// What the in-session notice says when the host blocks the clipboard.
pub const HOST_TURNED_CLIPBOARD_OFF: &str = "Clipboard is turned off by this host.";

/// The host's clipboard policy as its hello advertised it
/// (`ServerHelloMsg::clipboard`). Every Pier advertises its policy, with
/// the direction `disabled` when its operator or its session forbids the
/// clipboard; a Pier that predates the clipboard advertises nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClipboard {
    /// No policy: an older Pier, or no session yet. Nothing is said.
    NotAdvertised,
    /// The Pier turned the clipboard off.
    TurnedOff,
    /// The Pier allows it, within this policy.
    Allowed(arcen_protocol::messages::ClipboardPolicyMsg),
}

impl HostClipboard {
    /// The host's side, from its hello.
    #[must_use]
    pub fn from_hello(policy: Option<arcen_protocol::messages::ClipboardPolicyMsg>) -> Self {
        match policy {
            None => Self::NotAdvertised,
            Some(policy)
                if policy.direction
                    == arcen_protocol::messages::ClipboardDirectionMsg::Disabled =>
            {
                Self::TurnedOff
            }
            Some(policy) => Self::Allowed(policy),
        }
    }

    /// What the session says about the clipboard: only that the host turned
    /// it off, and only when the user shares it (with sharing off in
    /// Settings, the user's own choice already explains it). Never a
    /// setting the user can change: the host has the last word.
    #[must_use]
    pub const fn session_notice(self, user_shares_clipboard: bool) -> Option<&'static str> {
        match self {
            Self::TurnedOff if user_shares_clipboard => Some(HOST_TURNED_CLIPBOARD_OFF),
            Self::TurnedOff | Self::NotAdvertised | Self::Allowed(_) => None,
        }
    }

    /// The notice for a session that connected with `overrides` over the
    /// app-wide consent `app_shares`: the same effective value the session
    /// itself connects with, so a connection that turned the clipboard off
    /// itself is never told the host did.
    #[must_use]
    pub const fn connection_notice(
        self,
        overrides: &crate::overrides::ConnectionOverrides,
        app_shares: bool,
    ) -> Option<&'static str> {
        self.session_notice(overrides.effective_clipboard_enabled(app_shares).value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::{
        ClipboardContentMsg, ClipboardDirectionMsg, ClipboardPolicyMsg,
    };

    fn policy(direction: ClipboardDirectionMsg) -> ClipboardPolicyMsg {
        ClipboardPolicyMsg {
            protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
            direction,
            content: ClipboardContentMsg::All,
            max_bytes: 1 << 20,
        }
    }

    #[test]
    fn the_session_says_the_host_turned_the_clipboard_off_only_when_it_did() {
        let off = HostClipboard::from_hello(Some(policy(ClipboardDirectionMsg::Disabled)));
        assert_eq!(off, HostClipboard::TurnedOff);
        assert_eq!(off.session_notice(true), Some(HOST_TURNED_CLIPBOARD_OFF));
        // The user's own choice explains itself.
        assert_eq!(off.session_notice(false), None);
        // An older Pier says nothing, so neither does the Deck.
        assert_eq!(HostClipboard::from_hello(None).session_notice(true), None);
        for direction in [
            ClipboardDirectionMsg::Both,
            ClipboardDirectionMsg::ClientToHost,
            ClipboardDirectionMsg::HostToClient,
        ] {
            let allowed = HostClipboard::from_hello(Some(policy(direction)));
            assert!(matches!(allowed, HostClipboard::Allowed(_)));
            assert_eq!(allowed.session_notice(true), None);
        }
    }

    #[test]
    fn a_connection_that_turned_the_clipboard_off_is_not_told_the_host_did() {
        use crate::overrides::{ConnectionOverrides, Override};
        let off = HostClipboard::from_hello(Some(policy(ClipboardDirectionMsg::Disabled)));
        let follows = ConnectionOverrides::default();
        let own_off = ConnectionOverrides {
            clipboard_enabled: Override::Own(false),
            ..ConnectionOverrides::default()
        };
        let own_on = ConnectionOverrides {
            clipboard_enabled: Override::Own(true),
            ..ConnectionOverrides::default()
        };
        assert_eq!(
            off.connection_notice(&follows, true),
            Some(HOST_TURNED_CLIPBOARD_OFF)
        );
        assert_eq!(off.connection_notice(&own_off, true), None);
        assert_eq!(
            off.connection_notice(&own_on, true),
            Some(HOST_TURNED_CLIPBOARD_OFF)
        );
        // A connection cannot share what Settings does not.
        assert_eq!(off.connection_notice(&own_on, false), None);
    }
}
