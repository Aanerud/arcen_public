// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! How far the decoder is behind the stream, and when an interactive
//! contract skips ahead to the next keyframe instead of showing old video.
//!
//! The inbox keeps up to half a second of video per monitor so that the
//! burst a network stall releases is decoded rather than thrown away
//! ([`arcen_media::video::pipeline::DECK_BURST_ABSORB`]). A decoder that is slower
//! than the stream fills the same inbox: the picture falls further behind
//! the sound with every batch until the inbox overflows and the video jumps.
//! The two look alike in the inbox's depth, but not in how long its oldest
//! packet waited at the moment the worker takes a batch: after a burst the
//! worker decodes it once and the next batch waited less, while a decoder
//! that cannot keep up waits longer every batch.

use std::time::Duration;

/// How far behind the stream an interactive contract (Auto, Speed) lets
/// its video fall before it skips ahead: six frames at 60 fps, below the
/// 125 ms at which picture lagging sound becomes noticeable
/// (ITU-R BT.1359).
pub const INTERACTIVE_DECODE_LAG_BUDGET: Duration = Duration::from_millis(100);

/// What the guard tells the worker about one batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeLag {
    /// Decode the batch.
    Keep,
    /// The decoder is behind: skip the batch's video to the next keyframe.
    SkipAhead { waited: Duration },
}

/// Watches the wait of each batch's oldest video packet.
#[derive(Debug, Default)]
pub struct DecodeLagGuard {
    /// The previous batch waited longer than the budget.
    over_budget: Option<Duration>,
}

impl DecodeLagGuard {
    /// Judges a batch whose oldest video packet waited `oldest_wait` (`None`
    /// without video). Only an `interactive` contract skips: Grading and HDR
    /// show every frame.
    ///
    /// The decoder is behind when two batches in a row waited longer than
    /// [`INTERACTIVE_DECODE_LAG_BUDGET`] and the second no less than the
    /// first.
    pub fn judge(&mut self, oldest_wait: Option<Duration>, interactive: bool) -> DecodeLag {
        let Some(waited) = oldest_wait else {
            return DecodeLag::Keep;
        };
        if !interactive || waited <= INTERACTIVE_DECODE_LAG_BUDGET {
            self.over_budget = None;
            return DecodeLag::Keep;
        }
        match self.over_budget.take() {
            Some(previous) if waited >= previous => DecodeLag::SkipAhead { waited },
            _ => {
                self.over_budget = Some(waited);
                DecodeLag::Keep
            }
        }
    }
}

/// How often the decoder fell behind in a session, for the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecoderBehind {
    /// Times the video skipped ahead to a keyframe.
    pub skips: u64,
    /// The longest wait that made it skip.
    pub worst_wait: Duration,
}

impl DecoderBehind {
    pub fn note(&mut self, waited: Duration) {
        self.skips = self.skips.saturating_add(1);
        self.worst_wait = self.worst_wait.max(waited);
    }

    /// The Stream Statistics line, the same on every Deck; `None` while the
    /// decoder keeps up.
    #[must_use]
    pub fn line(&self) -> Option<String> {
        (self.skips > 0).then(|| {
            format!(
                "Decoder: fell behind the stream {} {} (up to {} ms) and skipped ahead to the newest picture; this computer cannot decode every frame",
                self.skips,
                if self.skips == 1 { "time" } else { "times" },
                self.worst_wait.as_millis()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::unnecessary_wraps, reason = "a batch's wait is an Option")]
    const fn ms(value: u64) -> Option<Duration> {
        Some(Duration::from_millis(value))
    }

    #[test]
    fn a_decoder_slower_than_the_stream_skips_ahead_on_the_second_late_batch() {
        let mut guard = DecodeLagGuard::default();
        assert_eq!(guard.judge(ms(40), true), DecodeLag::Keep);
        assert_eq!(guard.judge(ms(120), true), DecodeLag::Keep);
        assert_eq!(
            guard.judge(ms(180), true),
            DecodeLag::SkipAhead {
                waited: Duration::from_millis(180)
            }
        );
        // After a skip it starts over.
        assert_eq!(guard.judge(ms(150), true), DecodeLag::Keep);
    }

    #[test]
    fn a_burst_after_a_network_stall_is_decoded_not_skipped() {
        let mut guard = DecodeLagGuard::default();
        // The burst's own batch waited little; decoding it took 150 ms, so
        // the next batch waited that long; the one after caught up.
        assert_eq!(guard.judge(ms(2), true), DecodeLag::Keep);
        assert_eq!(guard.judge(ms(150), true), DecodeLag::Keep);
        assert_eq!(guard.judge(ms(110), true), DecodeLag::Keep);
        assert_eq!(guard.judge(ms(20), true), DecodeLag::Keep);
    }

    #[test]
    fn fidelity_contracts_and_audio_only_batches_never_skip() {
        let mut guard = DecodeLagGuard::default();
        for wait in [200, 300, 400] {
            assert_eq!(guard.judge(ms(wait), false), DecodeLag::Keep);
        }
        assert_eq!(guard.judge(ms(200), true), DecodeLag::Keep);
        assert_eq!(guard.judge(None, true), DecodeLag::Keep);
        assert_eq!(
            guard.judge(ms(250), true),
            DecodeLag::SkipAhead {
                waited: Duration::from_millis(250)
            }
        );
    }

    #[test]
    fn the_statistics_line_names_the_skips_once_there_are_any() {
        let mut behind = DecoderBehind::default();
        assert_eq!(behind.line(), None);
        behind.note(Duration::from_millis(180));
        behind.note(Duration::from_millis(120));
        assert_eq!(
            behind.line().as_deref(),
            Some(
                "Decoder: fell behind the stream 2 times (up to 180 ms) and skipped ahead to the newest picture; this computer cannot decode every frame"
            )
        );
    }
}
