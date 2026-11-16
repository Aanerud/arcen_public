// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The refine lane's side of the refinement budget (ADR 0016 R6): what the
//! refine writer handed to its lane, whether the link budget holds it, and
//! whether video is waiting. One meter per attachment, shared by the refine
//! writer, the video path that feeds it, and the Pier's budget window.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Counters and the hold flag of one attachment's refine lane.
#[derive(Debug, Default)]
pub struct RefineLaneMeter {
    written_bytes: AtomicU64,
    held: AtomicBool,
    video_pending: AtomicU64,
}

impl RefineLaneMeter {
    /// The refine writer handed `bytes` to its lane.
    pub fn note_written(&self, bytes: u64) {
        self.written_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Refinement bytes handed to the lane so far (a running count).
    #[must_use]
    pub fn written_bytes(&self) -> u64 {
        self.written_bytes.load(Ordering::Relaxed)
    }

    /// The link budget holds refinement (`true`) or lets it send.
    pub fn set_held(&self, held: bool) {
        self.held.store(held, Ordering::Relaxed);
    }

    /// Whether the link budget holds refinement.
    #[must_use]
    pub fn held(&self) -> bool {
        self.held.load(Ordering::Relaxed)
    }

    /// A video frame was queued for the video lane's writer.
    pub fn note_video_queued(&self) {
        self.video_pending.fetch_add(1, Ordering::AcqRel);
    }

    /// The video lane's writer took a queued frame.
    pub fn note_video_taken(&self) {
        let _ = self
            .video_pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                Some(pending.saturating_sub(1))
            });
    }

    /// Video is waiting for its writer: motion wins, refinement waits.
    #[must_use]
    pub fn video_backlog(&self) -> bool {
        self.video_pending.load(Ordering::Acquire) > 0
    }

    /// Whether the refine writer may hand its next record to the lane now.
    #[must_use]
    pub fn may_send(&self) -> bool {
        !self.held() && !self.video_backlog()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_waiting_or_a_hold_stops_refinement() {
        let meter = RefineLaneMeter::default();
        assert!(meter.may_send());
        meter.note_video_queued();
        assert!(!meter.may_send(), "video waits");
        meter.note_video_taken();
        meter.note_video_taken();
        assert!(meter.may_send(), "never below zero");
        meter.set_held(true);
        assert!(!meter.may_send(), "held");
        meter.set_held(false);
        meter.note_written(40_000);
        meter.note_written(2_000);
        assert_eq!(meter.written_bytes(), 42_000);
    }
}
