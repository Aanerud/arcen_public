//! Sustained capture-rate measurement, as admission evidence.
//!
//! A copying capture route must be rate-qualified before it may serve a
//! contract above what it can sustain (see `admit_speed`). Producers that
//! deliver only on change (a GNOME `ScreenCast` stream on a still desktop)
//! make an epoch average a measurement of the user, not of the source, so
//! the meter keeps the best rate held over [`SustainedRateMeter::WINDOWS`]
//! consecutive one-second windows in which the source was delivering.

use std::time::{Duration, Instant};

/// How far above the rate it serves a consumer asks a refresh-paced
/// compositor source to deliver, and how far above that rate the output it
/// records must refresh.
///
/// Measured on Mutter 49 (GNOME 49 `ScreenCast` of a virtual monitor, a client
/// drawing 60 fps): asking for exactly the served rate, or recording a 60 Hz
/// output, delivered 36-47 fps; a 120 Hz output with a 120 fps maximum
/// delivered 59.6-60.7 fps at 1080p and 56.3-60.3 fps at 4K. The source's
/// pacing drops a frame whenever its arrival jitters inside one frame
/// interval of the previous one, so it needs an interval of headroom on
/// both. Whether a given stack then sustains the served rate is still a
/// measurement (the host's qualification), never assumed from this factor.
pub const PACED_SOURCE_HEADROOM: u32 = 2;

/// The maximum frame rate to ask a refresh-paced source for when serving
/// `served_fps`.
#[must_use]
pub const fn paced_source_offer_fps(served_fps: u32) -> u32 {
    served_fps.saturating_mul(PACED_SOURCE_HEADROOM)
}

/// The refresh rate a virtual output needs so that a refresh-paced capture
/// of it can serve `served_fps`: the display's own rate when that is already
/// high enough, otherwise [`paced_source_offer_fps`].
#[must_use]
pub const fn paced_output_refresh_hz(display_refresh_hz: u32, served_fps: u32) -> u32 {
    let needed = paced_source_offer_fps(served_fps);
    if display_refresh_hz > needed {
        display_refresh_hz
    } else {
        needed
    }
}

/// What a qualified capture rate (`capture-rate=`) rests on: a
/// qualification measured this exact output, or a larger output of the same
/// refresh, codec and route (`inferred`, reported as such wherever the rate
/// is).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureRateEvidence {
    /// Measured on this output.
    #[default]
    Measured,
    /// Inferred from the measurement of a `width`x`height` output.
    Inferred {
        /// Anchor width.
        width: u32,
        /// Anchor height.
        height: u32,
    },
}

impl CaptureRateEvidence {
    /// `measured`, or `inferred:<W>x<H>`.
    #[must_use]
    pub fn token(self) -> String {
        match self {
            Self::Measured => "measured".to_owned(),
            Self::Inferred { width, height } => format!("inferred:{width}x{height}"),
        }
    }

    /// Parses [`Self::token`].
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        if value == "measured" {
            return Some(Self::Measured);
        }
        let (width, height) = value.strip_prefix("inferred:")?.split_once('x')?;
        let number = |text: &str| {
            text.bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| text.parse::<u32>().ok())
                .flatten()
                .filter(|value| *value > 0)
        };
        Some(Self::Inferred {
            width: number(width)?,
            height: number(height)?,
        })
    }

    /// Whether this is an inference.
    #[must_use]
    pub const fn is_inferred(self) -> bool {
        matches!(self, Self::Inferred { .. })
    }
}

/// Seconds of motion at Speed's rate the runtime guard judges.
pub const SPEED_GUARD_WINDOWS: u32 = 5;

/// Of those, how many below the floor make the guard trip.
pub const SPEED_GUARD_SHORT_WINDOWS: u32 = 3;

/// The runtime guard's verdict on an admitted Speed epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedGuardVerdict {
    /// The epoch encoded what the source delivered at Speed's rate.
    Holding,
    /// The source delivered at Speed's rate but the epoch encoded below the
    /// floor (`speed_runtime_below_floor`).
    BelowFloor {
        /// Lowest encoded rate among the short windows.
        encoded_fps: u32,
        /// Delivered rate in that window.
        source_fps: u32,
    },
}

/// Judges an admitted Speed epoch on the host's own per-second counts
/// (capenc's delivered `source_fps` and encoded `enc_fps`), never on what a
/// Deck received.
///
/// Only seconds in which the source delivered at least the floor count: a
/// still desktop or a 30 or 50 fps video is content, not a slow host, and
/// says nothing. Among the first [`SPEED_GUARD_WINDOWS`] such seconds, if
/// [`SPEED_GUARD_SHORT_WINDOWS`] encoded below the floor, the host did not
/// hold Speed (an encoder that cannot keep up — HEVC at 4K on some GPUs —
/// or an inference that did not hold) and the epoch is served as Auto.
#[derive(Debug, Clone)]
pub struct SpeedRuntimeGuard {
    floor_fps: u32,
    judged: u32,
    short: u32,
    lowest: Option<(u32, u32)>,
    verdict: Option<SpeedGuardVerdict>,
}

impl SpeedRuntimeGuard {
    /// A guard for an epoch whose floor is `floor_fps`.
    #[must_use]
    pub const fn new(floor_fps: u32) -> Self {
        Self {
            floor_fps,
            judged: 0,
            short: 0,
            lowest: None,
            verdict: None,
        }
    }

    /// Counts one second; returns the verdict once there is one (and every
    /// call after).
    pub fn observe(
        &mut self,
        encoded_fps: u32,
        source_fps: Option<u32>,
    ) -> Option<SpeedGuardVerdict> {
        if self.verdict.is_some() {
            return self.verdict;
        }
        let source = source_fps?;
        if source < self.floor_fps {
            return None;
        }
        self.judged += 1;
        if encoded_fps < self.floor_fps {
            self.short += 1;
            if self.lowest.is_none_or(|(lowest, _)| encoded_fps < lowest) {
                self.lowest = Some((encoded_fps, source));
            }
        }
        if self.short >= SPEED_GUARD_SHORT_WINDOWS {
            let (encoded_fps, source_fps) = self.lowest.unwrap_or((encoded_fps, source));
            self.verdict = Some(SpeedGuardVerdict::BelowFloor {
                encoded_fps,
                source_fps,
            });
        } else if self.judged >= SPEED_GUARD_WINDOWS {
            self.verdict = Some(SpeedGuardVerdict::Holding);
        }
        self.verdict
    }

    /// The verdict so far.
    #[must_use]
    pub const fn verdict(&self) -> Option<SpeedGuardVerdict> {
        self.verdict
    }
}

/// Delivered frames per second over consecutive one-second windows.
#[derive(Debug, Clone)]
pub struct SustainedRateMeter {
    window_start: Instant,
    window_frames: u32,
    run: [u32; Self::WINDOWS],
    run_len: usize,
    last_window: Option<u32>,
    best_sustained: Option<u32>,
}

impl SustainedRateMeter {
    /// Consecutive windows a rate must hold for to count as sustained.
    pub const WINDOWS: usize = 3;
    const WINDOW: Duration = Duration::from_secs(1);

    /// A meter whose first window starts at `now`.
    #[must_use]
    pub const fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            window_frames: 0,
            run: [0; Self::WINDOWS],
            run_len: 0,
            last_window: None,
            best_sustained: None,
        }
    }

    /// Counts `frames` delivered at `now`, closing elapsed windows first.
    pub fn record(&mut self, now: Instant, frames: u32) {
        self.tick(now);
        self.window_frames = self.window_frames.saturating_add(frames);
    }

    /// Closes every whole window that ended at or before `now`.
    pub fn tick(&mut self, now: Instant) {
        while now.saturating_duration_since(self.window_start) >= Self::WINDOW {
            self.close_window();
            self.window_start += Self::WINDOW;
        }
    }

    fn close_window(&mut self) {
        let frames = std::mem::take(&mut self.window_frames);
        self.last_window = Some(frames);
        // Fewer than two frames in a second is a still source, not a
        // measurement of its rate.
        if frames < 2 {
            self.run_len = 0;
            return;
        }
        if self.run_len == Self::WINDOWS {
            self.run.rotate_left(1);
            self.run[Self::WINDOWS - 1] = frames;
        } else {
            self.run[self.run_len] = frames;
            self.run_len += 1;
        }
        if self.run_len == Self::WINDOWS {
            let sustained = self.run.iter().copied().min().unwrap_or(0);
            self.best_sustained = Some(
                self.best_sustained
                    .map_or(sustained, |best| best.max(sustained)),
            );
        }
    }

    /// Frames in the last closed window.
    #[must_use]
    pub const fn last_window(&self) -> Option<u32> {
        self.last_window
    }

    /// The best rate held over [`Self::WINDOWS`] consecutive delivering
    /// windows, frames per second.
    #[must_use]
    pub const fn best_sustained(&self) -> Option<u32> {
        self.best_sustained
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_paced_source_is_asked_for_twice_the_served_rate() {
        use super::{paced_output_refresh_hz, paced_source_offer_fps};
        assert_eq!(paced_source_offer_fps(60), 120);
        assert_eq!(paced_source_offer_fps(30), 60);
        assert_eq!(paced_source_offer_fps(u32::MAX), u32::MAX);
        // A 60 Hz display gets a 120 Hz output for 60 fps; a faster display
        // keeps its own rate.
        assert_eq!(paced_output_refresh_hz(60, 60), 120);
        assert_eq!(paced_output_refresh_hz(144, 60), 144);
        assert_eq!(paced_output_refresh_hz(120, 60), 120);
    }

    use super::*;

    #[test]
    fn rate_evidence_tokens_round_trip() {
        for evidence in [
            CaptureRateEvidence::Measured,
            CaptureRateEvidence::Inferred {
                width: 2560,
                height: 1600,
            },
        ] {
            assert_eq!(
                CaptureRateEvidence::from_token(&evidence.token()),
                Some(evidence)
            );
        }
        for bad in [
            "",
            "inferred",
            "inferred:0x10",
            "inferred:x",
            "inferred:+5x5",
            "guessed",
        ] {
            assert_eq!(CaptureRateEvidence::from_token(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_runtime_guard_judges_only_seconds_the_source_ran_at_speed() {
        // Content below the floor (still, typing, a 50 fps video) is never
        // judged, however long it lasts.
        let mut guard = SpeedRuntimeGuard::new(57);
        for (encoded, source) in [(1, Some(1)), (30, Some(30)), (50, Some(50)), (60, None)] {
            for _ in 0..20 {
                assert_eq!(guard.observe(encoded, source), None);
            }
        }
        // An encoder that keeps up holds.
        for _ in 0..4 {
            assert_eq!(guard.observe(60, Some(60)), None);
        }
        assert_eq!(
            guard.observe(58, Some(61)),
            Some(SpeedGuardVerdict::Holding)
        );
        assert_eq!(
            guard.observe(10, Some(60)),
            Some(SpeedGuardVerdict::Holding)
        );
        // One that does not (4K HEVC: 60 delivered, 51 encoded) trips after
        // three short seconds, judged among content-driven seconds only.
        let mut guard = SpeedRuntimeGuard::new(57);
        assert_eq!(guard.observe(51, Some(60)), None);
        assert_eq!(guard.observe(30, Some(30)), None);
        assert_eq!(guard.observe(60, Some(60)), None);
        assert_eq!(guard.observe(52, Some(61)), None);
        assert_eq!(
            guard.observe(50, Some(60)),
            Some(SpeedGuardVerdict::BelowFloor {
                encoded_fps: 50,
                source_fps: 60
            })
        );
        assert!(matches!(
            guard.verdict(),
            Some(SpeedGuardVerdict::BelowFloor { .. })
        ));
        // Two short seconds in five hold.
        let mut guard = SpeedRuntimeGuard::new(57);
        for (encoded, source) in [(50, 60), (60, 60), (55, 60), (60, 60)] {
            assert_eq!(guard.observe(encoded, Some(source)), None);
        }
        assert_eq!(
            guard.observe(59, Some(60)),
            Some(SpeedGuardVerdict::Holding)
        );
    }

    #[test]
    fn a_still_source_is_not_a_rate_and_a_burst_does_not_raise_one() {
        let start = Instant::now();
        let mut meter = SustainedRateMeter::new(start);
        let at = |seconds: f64| start + Duration::from_secs_f64(seconds);
        // Still: one initial frame, then nothing.
        meter.record(at(0.1), 1);
        meter.tick(at(4.0));
        assert_eq!(meter.best_sustained(), None);
        assert_eq!(meter.last_window(), Some(0));
        // Delivering: 35, 36 and 34 frames in three windows.
        for (second, frames) in [(4, 35), (5, 36), (6, 34)] {
            meter.record(at(f64::from(second) + 0.5), frames);
        }
        meter.tick(at(7.0));
        assert_eq!(meter.best_sustained(), Some(34));
        // One fast window does not raise it.
        meter.record(at(7.5), 60);
        meter.tick(at(9.0));
        assert_eq!(meter.best_sustained(), Some(34));
        // Two windows are not three.
        for (second, frames) in [(9, 58), (10, 59)] {
            meter.record(at(f64::from(second) + 0.5), frames);
        }
        meter.tick(at(12.0));
        assert_eq!(meter.best_sustained(), Some(34));
        // Three are.
        for (second, frames) in [(12, 58), (13, 60), (14, 59)] {
            meter.record(at(f64::from(second) + 0.5), frames);
        }
        meter.tick(at(15.0));
        assert_eq!(meter.best_sustained(), Some(58));
        assert_eq!(meter.last_window(), Some(59));
    }
}
