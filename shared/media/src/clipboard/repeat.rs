// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Recognising one local copy announced twice.
//!
//! A host learns about a local copy from a change notification, and some
//! copies announce themselves twice. On Windows, .NET's `Clipboard.SetText`
//! (and every other `OleSetClipboard` + `OleFlushClipboard` writer) places a
//! data object, which raises `WM_CLIPBOARDUPDATE`, then flushes it: the
//! rendered formats are placed again under a new clipboard sequence number and
//! a second notification follows. Both describe the same copy, so the Deck
//! received a 1.3 MB item twice. A sequence-number check cannot see this; only
//! the content can.
//!
//! The filter keeps a keyed fingerprint (never the content) of the item last
//! sent and drops an identical item that follows within
//! [`REPEATED_COPY_WINDOW`]. A deliberate second copy of the same content after
//! that window is sent again, and receiving a remote item forgets the last one,
//! so the host's clipboard changing under it never hides a later local copy.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::{Duration, Instant};

use super::ClipboardKind;

/// How long after a local copy an identical one is taken as the same copy
/// announced again rather than copied again.
pub const REPEATED_COPY_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    kind: ClipboardKind,
    len: usize,
    hash: u64,
}

/// Remembers the last local copy sent, to drop the same copy announced twice.
pub struct RepeatedCopyFilter {
    keys: RandomState,
    last: Option<(Fingerprint, Instant)>,
}

impl RepeatedCopyFilter {
    /// A filter that has seen nothing. Fingerprints are keyed per filter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            keys: RandomState::new(),
            last: None,
        }
    }

    /// Whether the local copy `bytes` of `kind`, observed at `now`, repeats
    /// the copy sent last within [`REPEATED_COPY_WINDOW`]: `true` means drop
    /// it. Otherwise it becomes the copy sent last.
    pub fn is_repeat(&mut self, kind: ClipboardKind, bytes: &[u8], now: Instant) -> bool {
        let fingerprint = Fingerprint {
            kind,
            len: bytes.len(),
            hash: self.keys.hash_one(bytes),
        };
        let repeat = self.last.is_some_and(|(last, at)| {
            last == fingerprint && now.saturating_duration_since(at) < REPEATED_COPY_WINDOW
        });
        if !repeat {
            self.last = Some((fingerprint, now));
        }
        repeat
    }

    /// Forgets the last copy: the clipboard now holds something else (an
    /// item received from the Deck), so the next local copy is new.
    pub fn clear(&mut self) {
        self.last = None;
    }
}

impl Default for RepeatedCopyFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RepeatedCopyFilter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepeatedCopyFilter")
            .field("holds_last", &self.last.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_copy_announced_twice_is_sent_once() {
        let mut filter = RepeatedCopyFilter::new();
        let start = Instant::now();
        let text = vec![b'x'; 1_300_000];
        assert!(!filter.is_repeat(ClipboardKind::TextUtf8, &text, start));
        assert!(filter.is_repeat(
            ClipboardKind::TextUtf8,
            &text,
            start + Duration::from_millis(300)
        ));
    }

    #[test]
    fn different_content_or_kind_is_always_sent() {
        let mut filter = RepeatedCopyFilter::new();
        let now = Instant::now();
        assert!(!filter.is_repeat(ClipboardKind::TextUtf8, b"alpha", now));
        assert!(!filter.is_repeat(ClipboardKind::TextUtf8, b"bravo", now));
        assert!(!filter.is_repeat(ClipboardKind::ImagePng, b"bravo", now));
        assert!(!filter.is_repeat(ClipboardKind::TextUtf8, b"alpha", now));
    }

    #[test]
    fn copying_the_same_content_again_later_or_after_a_remote_item_is_sent() {
        let mut filter = RepeatedCopyFilter::new();
        let start = Instant::now();
        assert!(!filter.is_repeat(ClipboardKind::TextUtf8, b"alpha", start));
        assert!(!filter.is_repeat(
            ClipboardKind::TextUtf8,
            b"alpha",
            start + REPEATED_COPY_WINDOW
        ));
        filter.clear();
        assert!(!filter.is_repeat(
            ClipboardKind::TextUtf8,
            b"alpha",
            start + REPEATED_COPY_WINDOW + Duration::from_millis(10)
        ));
    }
}
