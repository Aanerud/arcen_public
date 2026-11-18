// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Synchronised presentation of a multi-monitor session's streams
//! (ADR 0026): the media worker's side of the shared
//! [`arcen_media::timeline::FrameSetTimeline`].
//!
//! Every decoded frame of a committed multi-monitor session is admitted
//! here before it is published. The shared timeline says how long it must
//! wait for the slower streams; a frame due now is published at once, a
//! later one is held (in order per monitor, a few at most) and released by
//! the worker loop when it falls due. Nothing waits longer than the
//! timeline's bound, so a slow or idle monitor never stalls the others.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use arcen_media::SessionMonitorId;
use arcen_media::timeline::{
    FrameSetTimeline, PierClockEstimate, TimelineConfig, TimelineReport, wire_ms,
};

use crate::decode::DecodedVideoFrame;

/// Environment value that turns holding off for an A/B skew measurement
/// (`off`); the timeline still measures and reports.
pub const TIMELINE_ENV: &str = "ARCEN_DECK_TIMELINE";

/// Frames held per monitor at most; an older one is released early rather
/// than letting a burst grow the queue. A decoder whose pictures come from
/// a fixed pool (the Windows Deck's D3D11 slots) reserves this many slots
/// beyond what its presenters hold.
pub const MAX_HELD_PER_MONITOR: u8 = 2;

/// One decoded frame waiting for its presentation instant.
pub(crate) struct HeldFrame<F> {
    pub due: Instant,
    pub monitor: SessionMonitorId,
    pub frame: DecodedVideoFrame<F>,
}

/// The worker's timeline state for one connection.
pub struct FrameSync<F> {
    timeline: FrameSetTimeline,
    held: VecDeque<HeldFrame<F>>,
}

impl<F> std::fmt::Debug for FrameSync<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrameSync")
            .field("timeline", &self.timeline)
            .field("held", &self.held.len())
            .finish()
    }
}

impl<F> Default for FrameSync<F> {
    fn default() -> Self {
        Self::new(TimelineConfig::from_diagnostic_value(
            std::env::var(TIMELINE_ENV).ok().as_deref(),
        ))
    }
}

impl<F> FrameSync<F> {
    #[must_use]
    pub fn new(config: TimelineConfig) -> Self {
        Self {
            timeline: FrameSetTimeline::new(config),
            held: VecDeque::new(),
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.timeline.config().enabled
    }

    /// Admits a frame of `monitor` decoded at `now` (`now_wire_ms` on the
    /// wire clock). Returns it when it is due now; otherwise holds it and
    /// returns `None`, plus any frame a full queue released early.
    pub(crate) fn admit(
        &mut self,
        monitor: SessionMonitorId,
        frame: DecodedVideoFrame<F>,
        now: Instant,
        now_wire_ms: u32,
    ) -> (Option<DecodedVideoFrame<F>>, Option<HeldFrame<F>>) {
        let decision = self
            .timeline
            .admit(monitor.get(), frame.timestamp_ms, now_wire_ms);
        let queued = self.held.iter().filter(|held| held.monitor == monitor);
        let last_due = queued.clone().map(|held| held.due).max();
        let queued_count = queued.count();
        let mut due = now + decision.hold;
        if let Some(last) = last_due {
            // Frames of one monitor leave in the order they arrived.
            due = due.max(last);
        }
        if due <= now {
            self.timeline
                .presented(monitor.get(), frame.timestamp_ms, now_wire_ms);
            return (Some(frame), None);
        }
        let overflow = (queued_count >= usize::from(MAX_HELD_PER_MONITOR))
            .then(|| {
                self.held
                    .iter()
                    .position(|held| held.monitor == monitor)
                    .and_then(|index| self.held.remove(index))
            })
            .flatten();
        if let Some(early) = &overflow {
            self.timeline
                .presented(early.monitor.get(), early.frame.timestamp_ms, now_wire_ms);
        }
        self.held.push_back(HeldFrame {
            due,
            monitor,
            frame,
        });
        (None, overflow)
    }

    /// The earliest instant a held frame falls due.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.held.iter().map(|held| held.due).min()
    }

    /// Takes every held frame due at `now`, oldest first.
    pub(crate) fn take_due(&mut self, now: Instant, now_wire_ms: u32) -> Vec<HeldFrame<F>> {
        let mut due = Vec::new();
        let mut index = 0;
        while index < self.held.len() {
            if self.held[index].due <= now {
                if let Some(held) = self.held.remove(index) {
                    self.timeline.presented(
                        held.monitor.get(),
                        held.frame.timestamp_ms,
                        now_wire_ms,
                    );
                    due.push(held);
                }
            } else {
                index += 1;
            }
        }
        due.sort_by_key(|held| held.due);
        due
    }

    /// Drops every held frame (the connection's topology ended).
    pub fn clear(&mut self) {
        self.held.clear();
    }

    /// The periodic skew report, when one is due.
    pub fn take_report(
        &mut self,
        now_wire_ms: u32,
        clock: Option<PierClockEstimate>,
    ) -> Option<TimelineReport> {
        self.timeline.take_report(now_wire_ms, clock)
    }
}

/// The Deck's wire clock now: epoch milliseconds, low 32 bits.
#[must_use]
pub fn now_wire_ms() -> u32 {
    wire_ms(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            }),
    )
}

/// How long the worker may sleep before the next held frame is due, at most
/// `cap` so new packets are still read promptly.
#[must_use]
pub fn wait_until_due(next_due: Option<Instant>, now: Instant, cap: Duration) -> Option<Duration> {
    next_due.map(|due| due.saturating_duration_since(now).min(cap))
}

/// Logs one timeline report at INFO (the multi-monitor skew evidence).
pub fn log_report(report: &TimelineReport) {
    let streams = report
        .streams
        .iter()
        .map(|stream| {
            format!(
                "{}:n={},p50={},p90={},decoded_p50={},held={},clamped={}",
                stream.stream,
                stream.frames,
                stream.presented_p50_ms,
                stream.presented_p90_ms,
                stream.decoded_p50_ms,
                stream.held,
                stream.clamped
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    tracing::info!(
        target: crate::log_target::VIDEO,
        timeline = if report.enabled { "on" } else { "off" },
        skew_p50_ms = report.skew_p50_ms,
        skew_p90_ms = report.skew_p90_ms,
        unsynced_skew_p50_ms = report.unsynced_skew_p50_ms,
        latency_p50_ms = report.latency_p50_ms,
        clock_offset_ms = report.clock.map(|clock| clock.offset_ms),
        clock_round_trip_ms = report.clock.map(|clock| clock.round_trip_ms),
        %streams,
        "multi-monitor timeline"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(timestamp_ms: u32) -> DecodedVideoFrame<()> {
        DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: Vec::new(),
            timestamp_ms,
            pixel_format: String::new(),
            backend: "test",
            native: None,
        }
    }

    fn id(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("monitor id")
    }

    #[test]
    fn a_fast_monitor_is_held_and_released_in_order() {
        let mut sync = FrameSync::<()>::new(TimelineConfig::default());
        let start = Instant::now();
        let mut released = None;
        // Monitor 1 arrives 30 ms after capture, monitor 2 after 10 ms.
        for index in 0..10u32 {
            let captured = 1_000 + index * 16;
            let now = start + Duration::from_millis(u64::from(index) * 16);
            let (slow, _) = sync.admit(id(1), frame(captured), now, captured + 30);
            assert!(slow.is_some(), "the slowest stream is never held");
            let (fast, _) = sync.admit(id(2), frame(captured), now, captured + 10);
            released = Some(fast.is_none());
        }
        assert_eq!(released, Some(true), "monitor 2 waits for monitor 1");
        let due = sync.next_due().expect("held frame");
        let later = due + Duration::from_millis(1);
        let frames = sync.take_due(later, 2_000);
        assert!(!frames.is_empty());
        assert!(frames.windows(2).all(|pair| pair[0].due <= pair[1].due));
        assert!(frames.iter().all(|held| held.monitor == id(2)));
    }

    #[test]
    fn off_publishes_everything_at_once() {
        let mut sync = FrameSync::<()>::new(TimelineConfig::from_diagnostic_value(Some("off")));
        let now = Instant::now();
        for index in 0..10u32 {
            let captured = index * 16;
            assert!(
                sync.admit(id(1), frame(captured), now, captured + 30)
                    .0
                    .is_some()
            );
            assert!(
                sync.admit(id(2), frame(captured), now, captured + 10)
                    .0
                    .is_some()
            );
        }
        assert!(sync.next_due().is_none());
    }

    #[test]
    fn a_full_queue_releases_its_oldest_frame() {
        let config = TimelineConfig {
            max_hold: Duration::from_secs(1),
            ..TimelineConfig::default()
        };
        let mut sync = FrameSync::<()>::new(config);
        let now = Instant::now();
        // Enough frames for stream 1 to pace the playout delay.
        let pacing = u32::try_from(arcen_media::timeline::DEFAULT_MIN_PACING_FRAMES).expect("few");
        for index in 0..pacing {
            sync.admit(id(1), frame(index), now, index + 500);
        }
        let mut early = Vec::new();
        let mut first_held = None;
        for index in 0..pacing + 8 {
            let (published, overflow) = sync.admit(id(2), frame(index), now, index + 10);
            if published.is_none() {
                first_held.get_or_insert(index);
            }
            early.extend(overflow.map(|held| held.frame.timestamp_ms));
        }
        assert!(!early.is_empty());
        assert_eq!(
            Some(early[0]),
            first_held,
            "the oldest held frame goes first"
        );
        assert!(
            sync.held
                .iter()
                .filter(|held| held.monitor == id(2))
                .count()
                <= usize::from(MAX_HELD_PER_MONITOR)
        );
    }

    #[test]
    fn wait_is_capped() {
        let now = Instant::now();
        assert_eq!(wait_until_due(None, now, Duration::from_millis(2)), None);
        assert_eq!(
            wait_until_due(
                Some(now + Duration::from_millis(30)),
                now,
                Duration::from_millis(2)
            ),
            Some(Duration::from_millis(2))
        );
        assert_eq!(
            wait_until_due(Some(now), now, Duration::from_millis(2)),
            Some(Duration::ZERO)
        );
    }
}
