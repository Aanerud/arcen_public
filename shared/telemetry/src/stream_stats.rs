//! Arithmetic every host does to report what its stream did.
//!
//! Frame rate over a window, and per-stage averages over a session. Neither
//! touches an operating system, both are wanted identically by every Pier, and
//! both were written on macOS first — which is the shape the architecture rule
//! exists to catch before the second copy appears.
//!
//! The reason this is worth sharing rather than retyping is that the mistakes
//! are subtle and identical everywhere: measuring a rate over an assumed
//! interval instead of the elapsed one hides a stall, and dividing by a frame
//! count without guarding zero renders `NaN` into a log, where it reads like a
//! measurement rather than the absence of one.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How often a streaming session reports its health by default.
pub const DEFAULT_SNAPSHOT_INTERVAL_SECS: u64 = 5;

/// Tracks when the next health snapshot is due and what the rate was.
///
/// Generic over the caller's clock reading so a test can drive it without
/// sleeping: the type only ever subtracts two readings, and a host that had to
/// wait five real seconds to exercise this would not exercise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCadence {
    elapsed_at_last: Duration,
    frames_at_last: u64,
    interval: Duration,
}

impl SnapshotCadence {
    /// Starts a cadence with the default interval.
    #[must_use]
    pub const fn new() -> Self {
        Self::every_secs(DEFAULT_SNAPSHOT_INTERVAL_SECS)
    }

    /// Starts a cadence with an explicit interval.
    #[must_use]
    pub const fn every_secs(interval_secs: u64) -> Self {
        Self {
            elapsed_at_last: Duration::ZERO,
            frames_at_last: 0,
            interval: Duration::from_secs(interval_secs),
        }
    }

    /// Returns the frames per second observed since the last snapshot, or
    /// `None` when one is not due yet.
    ///
    /// `elapsed` is time since the session started, and must not go backwards.
    ///
    /// The rate is measured over the window that actually elapsed rather than
    /// the interval that was asked for. A loop that stalled for twice the
    /// interval reports half the rate, which is the signal worth having; a
    /// host that assumed the interval would report the rate it would have had
    /// if nothing had gone wrong.
    pub fn due(&mut self, elapsed: Duration, frames_sent: u64) -> Option<u32> {
        let window = elapsed.saturating_sub(self.elapsed_at_last);
        if window < self.interval {
            return None;
        }
        let sent = frames_sent.saturating_sub(self.frames_at_last);
        self.elapsed_at_last = elapsed;
        self.frames_at_last = frames_sent;

        // Integer throughout. A frame count large enough to lose precision as
        // an `f64` is a bug better seen as a wrong number than as one silently
        // rounded into looking plausible.
        let millis = window.as_millis().max(1);
        let rate = u128::from(sent).saturating_mul(1_000) / millis;
        Some(u32::try_from(rate).unwrap_or(u32::MAX))
    }
}

impl Default for SnapshotCadence {
    fn default() -> Self {
        Self::new()
    }
}

/// Durations accumulated while streaming, summed across sent frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageTotals {
    /// Capture-to-socket time, summed.
    pub frame: Duration,
    /// The single worst capture-to-socket time.
    pub worst_frame: Duration,
    /// Time spent waiting for capture, summed.
    pub capture_wait: Duration,
    /// Time spent encoding, summed.
    pub encode: Duration,
    /// Time spent writing to the socket, summed.
    pub send: Duration,
}

/// Per-frame averages, in milliseconds, plus the observed frame rate.
///
/// Reported per stage because the stages fail differently: a slow encoder and
/// a slow network both present as a low frame rate, and only the split says
/// which. Capture-wait separates a host that cannot get frames from one that
/// cannot ship them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StageAverages {
    /// Frames per second over the session.
    pub sent_fps: f64,
    /// Mean capture-to-socket time.
    pub mean_frame_ms: f64,
    /// Worst capture-to-socket time.
    pub max_frame_ms: f64,
    /// Mean time waiting for capture.
    pub mean_capture_wait_ms: f64,
    /// Mean time encoding.
    pub mean_encode_ms: f64,
    /// Mean time writing to the socket.
    pub mean_send_ms: f64,
}

/// Turns accumulated durations into the per-frame averages an operator reads.
///
/// Every mean is guarded on a non-zero frame count. A session that sent
/// nothing reports zero rather than a division by zero rendered as `NaN`,
/// which in a log reads like a measurement rather than the absence of one.
#[must_use]
pub fn stage_averages(elapsed: Duration, frames_sent: u64, totals: StageTotals) -> StageAverages {
    let mut averages = StageAverages {
        max_frame_ms: totals.worst_frame.as_secs_f64() * 1_000.0,
        ..StageAverages::default()
    };
    let seconds = elapsed.as_secs_f64();
    if frames_sent == 0 {
        return averages;
    }
    // `u32::MAX` frames at sixty per second is over two years of streaming, so
    // the cast cannot lose anything a session could actually produce.
    let per = u32::try_from(frames_sent).map_or(f64::from(u32::MAX), f64::from);
    if seconds > 0.0 {
        averages.sent_fps = per / seconds;
    }
    averages.mean_frame_ms = totals.frame.as_secs_f64() * 1_000.0 / per;
    averages.mean_capture_wait_ms = totals.capture_wait.as_secs_f64() * 1_000.0 / per;
    averages.mean_encode_ms = totals.encode.as_secs_f64() * 1_000.0 / per;
    averages.mean_send_ms = totals.send.as_secs_f64() * 1_000.0 / per;
    averages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_is_not_due_before_the_interval() {
        let mut cadence = SnapshotCadence::new();
        assert_eq!(cadence.due(Duration::from_secs(1), 30), None);
    }

    #[test]
    fn the_rate_is_measured_over_the_real_window_not_the_interval() {
        // A loop that stalled for twice the interval must report half the
        // rate, not the rate it would have had if it had kept up.
        let mut cadence = SnapshotCadence::new();
        assert_eq!(cadence.due(Duration::from_secs(10), 300), Some(30));
    }

    #[test]
    fn each_window_counts_only_its_own_frames() {
        let mut cadence = SnapshotCadence::new();
        assert_eq!(cadence.due(Duration::from_secs(5), 300), Some(60));
        // The second window sent 150 more, not 450.
        assert_eq!(cadence.due(Duration::from_secs(10), 450), Some(30));
    }

    #[test]
    fn a_stalled_stream_reports_zero_rather_than_going_quiet() {
        let mut cadence = SnapshotCadence::new();
        assert_eq!(cadence.due(Duration::from_secs(5), 0), Some(0));
    }

    #[test]
    fn a_session_that_sent_nothing_reports_zero_not_nan() {
        // The bug this guards: dividing by a frame count of zero renders NaN
        // into a log, where it reads like a measurement rather than the
        // absence of one.
        let averages = stage_averages(Duration::from_secs(3), 0, StageTotals::default());
        assert!(averages.mean_encode_ms.abs() < f64::EPSILON);
        assert!(averages.sent_fps.abs() < f64::EPSILON);
        assert!(averages.mean_frame_ms.is_finite());
    }

    #[test]
    fn the_worst_frame_is_reported_even_when_nothing_was_sent() {
        // A session can capture, blow its worst-case budget, and then fail to
        // send anything. Losing that number would hide the reason.
        let totals = StageTotals {
            worst_frame: Duration::from_millis(40),
            ..StageTotals::default()
        };
        let averages = stage_averages(Duration::from_secs(1), 0, totals);
        assert!((averages.max_frame_ms - 40.0).abs() < 1e-9);
    }

    #[test]
    fn averages_are_per_frame_not_per_session() {
        let totals = StageTotals {
            frame: Duration::from_millis(900),
            encode: Duration::from_millis(450),
            capture_wait: Duration::from_millis(300),
            send: Duration::from_millis(30),
            worst_frame: Duration::from_millis(25),
        };
        let averages = stage_averages(Duration::from_secs(3), 90, totals);
        assert!((averages.mean_frame_ms - 10.0).abs() < 1e-9);
        assert!((averages.mean_encode_ms - 5.0).abs() < 1e-9);
        assert!((averages.mean_capture_wait_ms - 10.0 / 3.0).abs() < 1e-9);
        assert!((averages.sent_fps - 30.0).abs() < 1e-9);
    }
}
