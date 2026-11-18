// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The shared multi-stream timeline (ADR 0026).
//!
//! A Pier that serves one stream per monitor stamps every access unit of
//! every monitor on one clock: the wire clock (`timestamp_ms`, epoch
//! milliseconds truncated to 32 bits). A Deck that shows those streams in
//! several windows must present what was captured at the same instant at
//! the same instant, although each stream is encoded, carried and decoded
//! on its own and arrives with its own delay.
//!
//! Two pieces, both pure:
//!
//! - [`PierClockSync`] estimates the offset between the Pier's wire clock
//!   and the Deck's, NTP style, from the health ping/pong the session
//!   already exchanges (`t1` = the ping's own timestamp, `t2 = t3` = the
//!   pong's `server_timestamp_ms`, `t4` = `t1` plus the measured round
//!   trip). The sample with the shortest round trip in a short window wins,
//!   as in NTP's clock filter. The offset turns a frame's capture stamp into
//!   Deck time, so the Deck can report true capture-to-present latency.
//! - [`FrameSetTimeline`] decides how long a decoded frame waits before it
//!   is presented. Every frame's transit (Deck time when decoded minus Pier
//!   capture stamp) is measured per stream; the slowest pacing stream's
//!   median transit is the session's playout delay, and a frame of a faster
//!   stream is held for the difference, so frames captured together are
//!   presented together. The wait is bounded ([`TimelineConfig::max_hold`]):
//!   a stream that is later than the bound is shown late rather than
//!   stalling the others, and a lone pacing stream is never held. A stream
//!   paces while it is moving: a frame in the last
//!   [`TimelineConfig::idle_after`] and at least
//!   [`TimelineConfig::min_pacing_frames`] in the transit window.
//!
//! Synchronisation does not depend on the clock offset: both clocks cancel
//! out of the difference between two streams' transits. The offset only
//! makes the reported latency absolute.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

/// Samples [`PierClockSync`] keeps; the shortest round trip among them is
/// the estimate.
pub const CLOCK_SAMPLE_WINDOW: usize = 8;

/// A round trip longer than this says nothing useful about the offset.
pub const MAX_CLOCK_SAMPLE_ROUND_TRIP: Duration = Duration::from_secs(5);

/// Default bound on how long one frame waits for the other streams.
pub const DEFAULT_MAX_HOLD: Duration = Duration::from_millis(50);

/// Default window of transit samples a stream's median is taken over.
pub const DEFAULT_TRANSIT_WINDOW: Duration = Duration::from_secs(1);

/// A stream without a frame for this long no longer sets the playout delay
/// (an idle monitor sends nothing while nothing changes on it).
pub const DEFAULT_IDLE_AFTER: Duration = Duration::from_millis(500);

/// Frames a stream must have decoded within the transit window before it
/// sets the playout delay: a monitor that only sends its keepalive, or a few
/// frames while a window crosses its edge, has a median of a handful of
/// samples, and holding a moving monitor to it put the two further apart.
pub const DEFAULT_MIN_PACING_FRAMES: usize = 8;

/// How often [`FrameSetTimeline::take_report`] summarises.
pub const DEFAULT_REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Transit samples kept per stream at most (bounds memory at any rate).
const MAX_TRANSIT_SAMPLES: usize = 512;

/// Difference `later - earlier` of two wire-clock values, read across the
/// 32-bit wrap. Valid while the true difference is under 24 days.
#[must_use]
#[allow(
    clippy::cast_possible_wrap,
    reason = "the wire clock's difference is read as two's complement on purpose"
)]
pub const fn wire_delta_ms(later: u32, earlier: u32) -> i32 {
    later.wrapping_sub(earlier) as i32
}

/// The low 32 bits of a millisecond clock: the wire clock's format.
#[must_use]
pub const fn wire_ms(milliseconds: u64) -> u32 {
    (milliseconds & 0xffff_ffff) as u32
}

/// The current estimate of the Pier's wire clock against the Deck's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PierClockEstimate {
    /// Pier clock minus Deck clock, in milliseconds.
    pub offset_ms: i32,
    /// Round trip of the sample the estimate came from.
    pub round_trip_ms: u32,
    /// Samples in the window.
    pub samples: usize,
}

impl PierClockEstimate {
    /// The Deck wire time at which the Pier's clock read `pier_ms`.
    #[must_use]
    pub const fn pier_to_deck(self, pier_ms: u32) -> u32 {
        pier_ms.wrapping_add_signed(self.offset_ms.wrapping_neg())
    }
}

#[derive(Debug, Clone, Copy)]
struct ClockSample {
    offset_ms: i32,
    round_trip_ms: u32,
}

/// NTP-style offset estimator between the Pier's and the Deck's wire clocks.
#[derive(Debug, Clone, Default)]
pub struct PierClockSync {
    samples: VecDeque<ClockSample>,
}

impl PierClockSync {
    /// Records one ping/pong exchange: the Deck sent the ping at its wire
    /// time `deck_sent_ms` (`t1`), the Pier stamped the pong at its wire time
    /// `pier_ms` (`t2 = t3`), and the pong arrived `round_trip` later.
    /// Returns the estimate after this sample, or `None` while there is no
    /// usable sample (a round trip of five seconds or more is ignored).
    pub fn observe(
        &mut self,
        deck_sent_ms: u32,
        pier_ms: u32,
        round_trip: Duration,
    ) -> Option<PierClockEstimate> {
        if round_trip < MAX_CLOCK_SAMPLE_ROUND_TRIP {
            let round_trip_ms = u32::try_from(round_trip.as_millis()).unwrap_or(u32::MAX);
            // The pong was stamped, on average, half a round trip after t1.
            let deck_at_stamp = deck_sent_ms.wrapping_add(round_trip_ms / 2);
            self.samples.push_back(ClockSample {
                offset_ms: wire_delta_ms(pier_ms, deck_at_stamp),
                round_trip_ms,
            });
            while self.samples.len() > CLOCK_SAMPLE_WINDOW {
                self.samples.pop_front();
            }
        }
        self.estimate()
    }

    /// The offset from the sample with the shortest round trip in the window
    /// (the one queueing distorted least).
    #[must_use]
    pub fn estimate(&self) -> Option<PierClockEstimate> {
        self.samples
            .iter()
            .min_by_key(|sample| sample.round_trip_ms)
            .map(|best| PierClockEstimate {
                offset_ms: best.offset_ms,
                round_trip_ms: best.round_trip_ms,
                samples: self.samples.len(),
            })
    }
}

/// Policy for [`FrameSetTimeline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineConfig {
    /// Whether frames are held at all. Off still measures, so a session can
    /// report what its skew would have been.
    pub enabled: bool,
    /// Longest any frame waits for the other streams.
    pub max_hold: Duration,
    /// Window a stream's transit median is taken over.
    pub transit_window: Duration,
    /// A stream idle this long stops setting the playout delay.
    pub idle_after: Duration,
    /// Frames a stream needs in the transit window to set the playout
    /// delay (see [`DEFAULT_MIN_PACING_FRAMES`]).
    pub min_pacing_frames: usize,
    /// How often a report is due.
    pub report_interval: Duration,
}

impl Default for TimelineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_hold: DEFAULT_MAX_HOLD,
            transit_window: DEFAULT_TRANSIT_WINDOW,
            idle_after: DEFAULT_IDLE_AFTER,
            min_pacing_frames: DEFAULT_MIN_PACING_FRAMES,
            report_interval: DEFAULT_REPORT_INTERVAL,
        }
    }
}

impl TimelineConfig {
    /// The configuration a Deck runs with: on, unless the diagnostic
    /// environment value `off` (`ARCEN_DECK_TIMELINE`) turns holding off
    /// for an A/B measurement.
    #[must_use]
    pub fn from_diagnostic_value(value: Option<&str>) -> Self {
        Self {
            enabled: !value.is_some_and(|value| value.trim().eq_ignore_ascii_case("off")),
            ..Self::default()
        }
    }
}

/// What to do with one decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldDecision {
    /// How long to wait before presenting it (zero: present now).
    pub hold: Duration,
    /// Its transit, Deck decode time minus Pier capture stamp (includes the
    /// clock offset; only differences between streams are meaningful).
    pub transit_ms: i32,
    /// The playout delay the session aimed for.
    pub target_ms: i32,
    /// The frame could not be brought fully in line: the hold it needed
    /// exceeded [`TimelineConfig::max_hold`].
    pub clamped: bool,
}

#[derive(Debug, Clone, Default)]
struct StreamTrack {
    /// `(deck decode ms, transit ms)`, newest last.
    transits: VecDeque<(u32, i32)>,
    last_decoded_ms: Option<u32>,
    /// Transit as presented (Deck present ms minus capture), this report.
    presented: Vec<i32>,
    /// Transit as decoded, this report (what presenting at once would show).
    decoded: Vec<i32>,
    held: u64,
    clamped: u64,
}

impl StreamTrack {
    /// The median transit since `since_ms`, when at least `min_frames`
    /// frames arrived since then.
    fn median_since(&self, since_ms: u32, min_frames: usize) -> Option<i32> {
        let mut recent: Vec<i32> = self
            .transits
            .iter()
            .filter(|(at, _)| wire_delta_ms(*at, since_ms) >= 0)
            .map(|(_, transit)| *transit)
            .collect();
        if recent.len() < min_frames.max(1) {
            return None;
        }
        median(&mut recent)
    }

    fn active_at(&self, now_ms: u32, idle_after_ms: i32) -> bool {
        self.last_decoded_ms
            .is_some_and(|last| wire_delta_ms(now_ms, last) <= idle_after_ms)
    }
}

/// One stream's line in a [`TimelineReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTimelineReport {
    pub stream: u16,
    pub frames: usize,
    /// Median transit as presented.
    pub presented_p50_ms: i32,
    /// 90th percentile transit as presented.
    pub presented_p90_ms: i32,
    /// Median transit as decoded (no hold).
    pub decoded_p50_ms: i32,
    pub held: u64,
    pub clamped: u64,
}

/// Inter-stream skew over one report interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineReport {
    pub enabled: bool,
    pub streams: Vec<StreamTimelineReport>,
    /// Spread (max - min) of the streams' presented medians: how far apart
    /// frames captured together reached the screen.
    pub skew_p50_ms: i32,
    /// Spread of the streams' presented 90th percentiles.
    pub skew_p90_ms: i32,
    /// Spread of the decoded medians: the skew without the timeline.
    pub unsynced_skew_p50_ms: i32,
    /// Capture-to-present latency of the slowest stream's median, in
    /// absolute terms, once the clock offset is known.
    pub latency_p50_ms: Option<i32>,
    pub clock: Option<PierClockEstimate>,
}

/// Decides when each decoded frame of several streams is presented.
#[derive(Debug, Clone)]
pub struct FrameSetTimeline {
    config: TimelineConfig,
    streams: BTreeMap<u16, StreamTrack>,
    last_report_ms: Option<u32>,
}

impl FrameSetTimeline {
    #[must_use]
    pub fn new(config: TimelineConfig) -> Self {
        Self {
            config,
            streams: BTreeMap::new(),
            last_report_ms: None,
        }
    }

    #[must_use]
    pub const fn config(&self) -> TimelineConfig {
        self.config
    }

    /// Records a frame of `stream` captured at Pier wire time `captured_ms`
    /// and decoded at Deck wire time `decoded_ms`, and says how long to hold
    /// it. Holding needs at least two pacing streams; a frame is never held
    /// longer than [`TimelineConfig::max_hold`].
    pub fn admit(&mut self, stream: u16, captured_ms: u32, decoded_ms: u32) -> HoldDecision {
        let transit_ms = wire_delta_ms(decoded_ms, captured_ms);
        let window_ms = duration_ms_i32(self.config.transit_window);
        let idle_ms = duration_ms_i32(self.config.idle_after);
        let since = decoded_ms.wrapping_sub(window_ms.unsigned_abs());
        let recent_since = decoded_ms.wrapping_sub(window_ms.unsigned_abs() / 2);
        {
            let track = self.streams.entry(stream).or_default();
            track.transits.push_back((decoded_ms, transit_ms));
            while track
                .transits
                .front()
                .is_some_and(|(at, _)| wire_delta_ms(*at, since) < 0)
                || track.transits.len() > MAX_TRANSIT_SAMPLES
            {
                track.transits.pop_front();
            }
            track.last_decoded_ms = Some(decoded_ms);
            track.decoded.push(transit_ms);
        }
        let active: Vec<i32> = self
            .streams
            .values()
            .filter(|track| track.active_at(decoded_ms, idle_ms))
            .filter_map(|track| {
                let window = track.median_since(since, self.config.min_pacing_frames)?;
                let recent = track.median_since(recent_since, self.config.min_pacing_frames / 2);
                Some(recent.map_or(window, |recent| recent.min(window)))
            })
            .collect();
        let target_ms = active.iter().copied().max().unwrap_or(transit_ms);
        let wanted = if active.len() >= 2 {
            target_ms.saturating_sub(transit_ms).max(0)
        } else {
            0
        };
        let max_hold_ms = duration_ms_i32(self.config.max_hold);
        let clamped = wanted > max_hold_ms;
        let hold_ms = if self.config.enabled {
            wanted.min(max_hold_ms)
        } else {
            0
        };
        let track = self.streams.entry(stream).or_default();
        if hold_ms > 0 {
            track.held = track.held.saturating_add(1);
        }
        if clamped {
            track.clamped = track.clamped.saturating_add(1);
        }
        HoldDecision {
            hold: Duration::from_millis(u64::try_from(hold_ms).unwrap_or(0)),
            transit_ms,
            target_ms,
            clamped,
        }
    }

    /// Records that a frame of `stream` captured at `captured_ms` reached
    /// its presenter at Deck wire time `presented_ms`.
    pub fn presented(&mut self, stream: u16, captured_ms: u32, presented_ms: u32) {
        let track = self.streams.entry(stream).or_default();
        if track.presented.len() < MAX_TRANSIT_SAMPLES * 8 {
            track
                .presented
                .push(wire_delta_ms(presented_ms, captured_ms));
        }
    }

    /// Forgets a stream (its monitor left the topology).
    pub fn forget(&mut self, stream: u16) {
        self.streams.remove(&stream);
    }

    /// A summary once per [`TimelineConfig::report_interval`], when at least
    /// one stream presented frames; resets the interval's counters.
    pub fn take_report(
        &mut self,
        now_ms: u32,
        clock: Option<PierClockEstimate>,
    ) -> Option<TimelineReport> {
        let interval = duration_ms_i32(self.config.report_interval);
        match self.last_report_ms {
            None => {
                self.last_report_ms = Some(now_ms);
                return None;
            }
            Some(last) if wire_delta_ms(now_ms, last) < interval => return None,
            Some(_) => {}
        }
        self.last_report_ms = Some(now_ms);
        let mut streams = Vec::new();
        for (stream, track) in &mut self.streams {
            let mut presented = std::mem::take(&mut track.presented);
            let mut decoded = std::mem::take(&mut track.decoded);
            let held = std::mem::take(&mut track.held);
            let clamped = std::mem::take(&mut track.clamped);
            let (Some(p50), Some(p90), Some(d50)) = (
                percentile(&mut presented, 50),
                percentile(&mut presented, 90),
                percentile(&mut decoded, 50),
            ) else {
                continue;
            };
            streams.push(StreamTimelineReport {
                stream: *stream,
                frames: presented.len(),
                presented_p50_ms: p50,
                presented_p90_ms: p90,
                decoded_p50_ms: d50,
                held,
                clamped,
            });
        }
        if streams.is_empty() {
            return None;
        }
        let spread = |value: fn(&StreamTimelineReport) -> i32| {
            let max = streams.iter().map(value).max().unwrap_or(0);
            let min = streams.iter().map(value).min().unwrap_or(0);
            max.saturating_sub(min)
        };
        let skew_p50_ms = spread(|stream| stream.presented_p50_ms);
        let skew_p90_ms = spread(|stream| stream.presented_p90_ms);
        let unsynced_skew_p50_ms = spread(|stream| stream.decoded_p50_ms);
        let latency_p50_ms = clock.map(|clock| {
            streams
                .iter()
                .map(|stream| stream.presented_p50_ms.saturating_add(clock.offset_ms))
                .max()
                .unwrap_or(0)
        });
        Some(TimelineReport {
            enabled: self.config.enabled,
            streams,
            skew_p50_ms,
            skew_p90_ms,
            unsynced_skew_p50_ms,
            latency_p50_ms,
            clock,
        })
    }
}

fn duration_ms_i32(duration: Duration) -> i32 {
    i32::try_from(duration.as_millis()).unwrap_or(i32::MAX)
}

fn median(values: &mut [i32]) -> Option<i32> {
    percentile(values, 50)
}

fn percentile(values: &mut [i32], percent: usize) -> Option<i32> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let index = (values.len() - 1) * percent.min(100) / 100;
    values.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_sync_takes_the_shortest_round_trip_and_reads_across_the_wrap() {
        let mut sync = PierClockSync::default();
        // The Pier runs 1000 ms ahead; the Deck clock is about to wrap.
        let t1 = u32::MAX - 10;
        let slow = sync
            .observe(t1, t1.wrapping_add(1000 + 90), Duration::from_millis(300))
            .expect("estimate");
        assert_eq!(slow.offset_ms, 1000 + 90 - 150);
        let fast = sync
            .observe(
                t1.wrapping_add(5000),
                t1.wrapping_add(6000 + 10),
                Duration::from_millis(20),
            )
            .expect("estimate");
        assert_eq!(fast.offset_ms, 1000);
        assert_eq!(fast.round_trip_ms, 20);
        assert_eq!(fast.pier_to_deck(t1.wrapping_add(1000)), t1);
        // A useless sample changes nothing.
        let same = sync
            .observe(0, 0, Duration::from_secs(9))
            .expect("estimate");
        assert_eq!(same, PierClockEstimate { samples: 2, ..fast });
    }

    #[test]
    fn clock_sync_keeps_a_bounded_window() {
        let mut sync = PierClockSync::default();
        sync.observe(0, 500, Duration::from_millis(1));
        for i in 0..CLOCK_SAMPLE_WINDOW as u32 {
            sync.observe(i * 1000, i * 1000 + 40, Duration::from_millis(30));
        }
        let estimate = sync.estimate().expect("estimate");
        assert_eq!(estimate.samples, CLOCK_SAMPLE_WINDOW);
        assert_eq!(estimate.offset_ms, 25, "the 1 ms sample left the window");
    }

    fn feed(
        timeline: &mut FrameSetTimeline,
        stream: u16,
        captured: u32,
        transit: i32,
    ) -> HoldDecision {
        timeline.admit(stream, captured, captured.wrapping_add(transit as u32))
    }

    #[test]
    fn a_faster_stream_waits_for_the_slower_one() {
        let mut timeline = FrameSetTimeline::new(TimelineConfig::default());
        let mut last = None;
        for frame in 0..30u32 {
            let captured = 1_000 + frame * 16;
            assert_eq!(feed(&mut timeline, 1, captured, 40).hold, Duration::ZERO);
            last = Some(feed(&mut timeline, 2, captured, 28).hold);
        }
        assert_eq!(last, Some(Duration::from_millis(12)));
        let slow = feed(&mut timeline, 1, 2_000, 40);
        assert_eq!(slow.hold, Duration::ZERO);
        assert_eq!(slow.target_ms, 40);
    }

    #[test]
    fn a_lone_active_stream_is_never_held() {
        let mut timeline = FrameSetTimeline::new(TimelineConfig::default());
        for frame in 0..10u32 {
            feed(&mut timeline, 1, frame * 16, 60);
            feed(&mut timeline, 2, frame * 16, 20);
        }
        // Stream 1 goes idle (a monitor where nothing changes): stream 2's
        // frames stop waiting for it.
        let later = 10_000;
        assert_eq!(feed(&mut timeline, 2, later, 20).hold, Duration::ZERO);
        assert_eq!(feed(&mut timeline, 2, later + 16, 10).hold, Duration::ZERO);
    }

    #[test]
    fn a_monitor_sending_a_few_frames_does_not_hold_a_moving_one() {
        let mut timeline = FrameSetTimeline::new(TimelineConfig::default());
        // Stream 1 moves at 60 fps; stream 2 sends a frame every 200 ms (a
        // window's edge crossing it), each slower than stream 1's.
        let mut held = 0;
        for frame in 0..120u32 {
            let captured = 1_000 + frame * 16;
            if frame % 12 == 0 {
                feed(&mut timeline, 2, captured, 35);
            }
            if feed(&mut timeline, 1, captured, 20).hold > Duration::ZERO {
                held += 1;
            }
        }
        assert_eq!(held, 0, "five frames a second never set the playout delay");
        // Once stream 2 moves too, stream 1 waits for it again.
        let mut last = Duration::ZERO;
        for frame in 120..180u32 {
            let captured = 1_000 + frame * 16;
            feed(&mut timeline, 2, captured, 35);
            last = feed(&mut timeline, 1, captured, 20).hold;
        }
        assert_eq!(last, Duration::from_millis(15));
    }

    #[test]
    fn a_stream_that_recovered_from_a_burst_stops_holding_the_others_at_once() {
        let mut timeline = FrameSetTimeline::new(TimelineConfig::default());
        // 600 ms of stream 1 late by 40 ms (a burst queued on its lane),
        // then 300 ms at its usual transit.
        for frame in 0..56u32 {
            let captured = 1_000 + frame * 16;
            let transit = if frame < 38 { 60 } else { 20 };
            feed(&mut timeline, 1, captured, transit);
            feed(&mut timeline, 2, captured, 20);
        }
        // Over the last second stream 1's median is still the burst's; over
        // the last half second it is back to normal.
        let decision = feed(&mut timeline, 2, 1_000 + 56 * 16, 20);
        assert_eq!(decision.target_ms, 20);
        assert_eq!(decision.hold, Duration::ZERO);
    }

    #[test]
    fn the_wait_is_bounded_and_reported() {
        let mut timeline = FrameSetTimeline::new(TimelineConfig::default());
        let mut decision = None;
        for frame in 0..20u32 {
            feed(&mut timeline, 1, frame * 16, 400);
            decision = Some(feed(&mut timeline, 2, frame * 16, 20));
        }
        let decision = decision.expect("decision");
        assert_eq!(decision.hold, DEFAULT_MAX_HOLD);
        assert!(decision.clamped);
    }

    #[test]
    fn off_measures_but_never_holds() {
        let mut timeline =
            FrameSetTimeline::new(TimelineConfig::from_diagnostic_value(Some("OFF")));
        for frame in 0..20u32 {
            feed(&mut timeline, 1, frame * 16, 40);
            assert_eq!(feed(&mut timeline, 2, frame * 16, 10).hold, Duration::ZERO);
        }
        assert!(TimelineConfig::from_diagnostic_value(None).enabled);
        assert!(TimelineConfig::from_diagnostic_value(Some("on")).enabled);
    }

    #[test]
    fn report_measures_presented_and_unsynced_skew() {
        let mut timeline = FrameSetTimeline::new(TimelineConfig::default());
        assert!(
            timeline.take_report(0, None).is_none(),
            "the first call starts the interval"
        );
        for frame in 0..60u32 {
            let captured = 100 + frame * 16;
            for (stream, transit) in [(1u16, 40i32), (2, 25)] {
                let decision = feed(&mut timeline, stream, captured, transit);
                let presented = captured
                    .wrapping_add(transit as u32)
                    .wrapping_add(decision.hold.as_millis() as u32);
                timeline.presented(stream, captured, presented);
            }
        }
        let clock = PierClockEstimate {
            offset_ms: -5,
            round_trip_ms: 30,
            samples: 3,
        };
        assert!(timeline.take_report(1_000, Some(clock)).is_none());
        let report = timeline.take_report(5_000, Some(clock)).expect("report");
        assert_eq!(report.streams.len(), 2);
        assert_eq!(report.unsynced_skew_p50_ms, 15);
        assert_eq!(report.skew_p50_ms, 0);
        assert_eq!(report.latency_p50_ms, Some(35));
        assert!(report.streams.iter().any(|stream| stream.held > 0));
        // Counters reset with the report.
        assert!(timeline.take_report(10_000, None).is_none());
    }
}
