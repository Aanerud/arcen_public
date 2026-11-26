// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The presented frame rate Stream Statistics shows, from frames seen in
//! batches.
//!
//! A Deck learns what its presenters showed when it looks: the macOS Deck
//! frame by frame, the Windows Deck a few times a second, a dozen frames at
//! a time at 60 fps. Summing the batches seen in the last second counted
//! one batch more than the second held (each batch's frames were shown
//! before the look that saw them), and a steady 60 fps read 70. The rate is
//! the frames seen after the first look over the time from that look to
//! the last: real elapsed time, every frame counted once.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How far back the presented rate looks (the macOS Deck's telemetry
/// window).
pub const PRESENTED_RATE_SPAN: Duration = Duration::from_secs(2);

/// Frames per second over `batches` of frames seen at instants: the frames
/// of every batch but the earliest, over the time from the earliest to the
/// latest. Zero without two batches at different instants.
#[must_use]
pub fn presented_rate(batches: impl IntoIterator<Item = (Instant, u64)>) -> f64 {
    let mut first: Option<(Instant, u64)> = None;
    let mut last: Option<Instant> = None;
    let mut frames = 0_u64;
    for (at, count) in batches {
        frames = frames.saturating_add(count);
        if first.is_none_or(|(earliest, _)| at < earliest) {
            first = Some((at, count));
        }
        if last.is_none_or(|latest| at > latest) {
            last = Some(at);
        }
    }
    let (Some((first_at, first_frames)), Some(last_at)) = (first, last) else {
        return 0.0;
    };
    arcen_telemetry::rate_per_second(
        frames.saturating_sub(first_frames),
        last_at.saturating_duration_since(first_at),
    )
}

/// The frames a Deck saw presented over the last [`PRESENTED_RATE_SPAN`],
/// as the batches it saw them in.
#[derive(Debug, Clone, Default)]
pub struct PresentedRate {
    batches: VecDeque<(Instant, u64)>,
}

impl PresentedRate {
    /// `frames` seen presented at `at` (nothing for zero), forgetting
    /// batches older than the span.
    pub fn record(&mut self, at: Instant, frames: u64) {
        if frames != 0 {
            self.batches.push_back((at, frames));
        }
        self.expire(at);
    }

    /// Forgets batches older than the span at `now`.
    pub fn expire(&mut self, now: Instant) {
        while self
            .batches
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > PRESENTED_RATE_SPAN)
        {
            self.batches.pop_front();
        }
    }

    /// Frames per second over the span ending at `now` ([`presented_rate`]).
    #[must_use]
    pub fn rate(&self, now: Instant) -> f64 {
        presented_rate(
            self.batches
                .iter()
                .copied()
                .filter(|(at, _)| now.saturating_duration_since(*at) <= PRESENTED_RATE_SPAN),
        )
    }

    /// Forgets everything (a new connection, a reset history).
    pub fn clear(&mut self) {
        self.batches.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(start: Instant, value: u64) -> Instant {
        start + Duration::from_millis(value)
    }

    /// The live Windows Deck at a steady 60 fps, its presenters' outcomes
    /// taken every 233 ms: the old last-second sum read 70, the rate 60.
    #[test]
    fn sixty_fps_seen_in_batches_reads_sixty() {
        let start = Instant::now();
        let mut rate = PresentedRate::default();
        let (mut frames_shown, mut old_sum_max) = (0_u64, 0_u64);
        let mut old: VecDeque<(Instant, u64)> = VecDeque::new();
        for pass in 1..=40_u64 {
            let at_ms = pass * 233;
            let through = at_ms * 60 / 1_000;
            let batch = through - frames_shown;
            frames_shown = through;
            let at = ms(start, at_ms);
            rate.record(at, batch);
            if pass > 10 {
                let value = rate.rate(at);
                assert!((value - 60.0).abs() < 1.0, "{value}");
            }
            old.push_back((at, batch));
            while old
                .front()
                .is_some_and(|(seen, _)| at.duration_since(*seen) > Duration::from_secs(1))
            {
                old.pop_front();
            }
            old_sum_max = old_sum_max.max(old.iter().map(|(_, count)| count).sum());
        }
        assert!(old_sum_max >= 68, "the old sum over-read: {old_sum_max}");
    }

    /// Bunched presentation: the same frames, unevenly batched, read the
    /// same rate.
    #[test]
    fn bunched_batches_read_the_real_rate() {
        let start = Instant::now();
        let batches = [(0, 3), (100, 4), (150, 2), (500, 26), (1_000, 30)];
        let value = presented_rate(batches.map(|(at, count)| (ms(start, at), count)));
        assert!((value - 62.0).abs() < 0.01, "{value}");
    }

    /// Frame-by-frame samples (the macOS Deck) read as they always did.
    #[test]
    fn single_frames_read_their_rate() {
        let start = Instant::now();
        let value = presented_rate((0..=30).map(|frame| (ms(start, frame * 1_000 / 30), 1)));
        assert!((value - 30.0).abs() < 0.1, "{value}");
        assert!(presented_rate([(start, 12)]).abs() < f64::EPSILON);
        assert!(presented_rate(std::iter::empty()).abs() < f64::EPSILON);
    }

    /// Presentation that stopped reads zero once its batches leave the span.
    #[test]
    fn stopped_presentation_reads_zero_after_the_span() {
        let start = Instant::now();
        let mut rate = PresentedRate::default();
        for pass in 0..=8_u64 {
            rate.record(ms(start, pass * 250), 15);
        }
        assert!(rate.rate(ms(start, 2_000)) > 50.0);
        assert!(rate.rate(ms(start, 4_500)).abs() < f64::EPSILON);
        rate.record(ms(start, 4_500), 0);
        assert!(rate.batches.is_empty());
    }
}
