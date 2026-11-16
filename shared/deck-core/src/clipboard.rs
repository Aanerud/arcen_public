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
