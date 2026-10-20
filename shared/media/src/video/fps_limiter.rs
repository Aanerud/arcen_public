//! Why a capture loop delivered the frame rate it did, for one window.
//!
//! A host that emits fewer frames than its contract has exactly three
//! reasons, and an operator can act on each of them differently:
//!
//! - nothing changed on screen when a frame was due ([`FrameRateLimiter::NoDamage`]),
//!   which is the application's or the user's rate, never the Pier's;
//! - the rate controller lowered the loop's target below the contract
//!   ([`FrameRateLimiter::RateControl`]);
//! - the capture, conversion and encode work did not fit the frame interval,
//!   so due frames were late or skipped ([`FrameRateLimiter::Pipeline`]).
//!
//! Capture loops count their own ticks and hand one [`FrameRateWindow`] per
//! stats line to [`classify_frame_rate`]; the token rides on that line so a
//! level-2 log answers "why this fps" without a profiler.

use std::time::Duration;

/// The dominant reason a window's frame rate was what it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRateLimiter {
    /// At least 90 % of the contract's frame rate was emitted.
    AtContract,
    /// Due frames found nothing new on screen.
    NoDamage,
    /// The loop's target frame rate was below the contract, and that target
    /// was met.
    RateControl,
    /// Capture, conversion and encode work used the window, or due frames
    /// were skipped because the previous one was late.
    Pipeline,
}

impl FrameRateLimiter {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::AtContract => "at_contract",
            Self::NoDamage => "no_damage",
            Self::RateControl => "rate_control",
            Self::Pipeline => "pipeline",
        }
    }

    /// Parses a [`Self::token`].
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        [
            Self::AtContract,
            Self::NoDamage,
            Self::RateControl,
            Self::Pipeline,
        ]
        .into_iter()
        .find(|limiter| limiter.token() == token)
    }
}

/// One capture loop's counts for a stats window (normally one second).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRateWindow {
    /// The served contract's frame rate.
    pub contract_fps: u32,
    /// The loop's current pacing target; below the contract only when the
    /// rate controller lowered it.
    pub target_fps: u32,
    /// Pacing deadlines that came due in the window.
    pub ticks_due: u32,
    /// Due deadlines that had nothing new to submit.
    pub ticks_without_change: u32,
    /// Frames submitted to the encoder.
    pub frames_submitted: u32,
    /// Wall time spent capturing, converting, uploading and encoding.
    pub busy: Duration,
    /// Length of the window.
    pub elapsed: Duration,
}

/// The verdict for one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRateVerdict {
    /// The dominant limiter.
    pub limiter: FrameRateLimiter,
    /// Whether the pacing target was below the contract in this window,
    /// whatever else limited it.
    pub rate_capped: bool,
}

/// Share of the window, or of a rate, that counts as "all of it".
const NEARLY_ALL_PERCENT: u64 = 90;
/// Share of due ticks without change that makes content the limiter.
const CONTENT_LIMITED_PERCENT: u64 = 10;

/// Classifies one window. Rates are normalised to the window's length, so a
/// stats line written after 1.04 s is judged against 1.04 s of ticks.
#[must_use]
pub fn classify_frame_rate(window: FrameRateWindow) -> FrameRateVerdict {
    let elapsed_us = u64::try_from(window.elapsed.as_micros())
        .unwrap_or(u64::MAX)
        .max(1);
    let per_window = |fps: u32| u64::from(fps).saturating_mul(elapsed_us) / 1_000_000;
    let contract = per_window(window.contract_fps.max(1)).max(1);
    let target = per_window(window.target_fps.max(1)).max(1);
    let rate_capped = window.target_fps < window.contract_fps;
    let submitted = u64::from(window.frames_submitted);
    let due = u64::from(window.ticks_due);
    let unchanged = u64::from(window.ticks_without_change);
    let busy_us = u64::try_from(window.busy.as_micros()).unwrap_or(u64::MAX);

    let limiter = if submitted * 100 >= contract * NEARLY_ALL_PERCENT {
        FrameRateLimiter::AtContract
    } else if busy_us * 100 >= elapsed_us * NEARLY_ALL_PERCENT
        || due * 100 < target * NEARLY_ALL_PERCENT
    {
        FrameRateLimiter::Pipeline
    } else if due > 0 && unchanged * 100 >= due * CONTENT_LIMITED_PERCENT {
        FrameRateLimiter::NoDamage
    } else if rate_capped {
        FrameRateLimiter::RateControl
    } else {
        FrameRateLimiter::Pipeline
    };
    FrameRateVerdict {
        limiter,
        rate_capped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> FrameRateWindow {
        FrameRateWindow {
            contract_fps: 30,
            target_fps: 30,
            ticks_due: 30,
            ticks_without_change: 0,
            frames_submitted: 30,
            busy: Duration::from_millis(510),
            elapsed: Duration::from_secs(1),
        }
    }

    #[test]
    fn a_full_contract_rate_is_at_contract() {
        assert_eq!(
            classify_frame_rate(window()).limiter,
            FrameRateLimiter::AtContract
        );
    }

    /// The Firefox pen test on the Xorg lab host: 30 ticks due, 19-21 found nothing new.
    #[test]
    fn ticks_without_change_make_content_the_limiter() {
        let verdict = classify_frame_rate(FrameRateWindow {
            ticks_without_change: 19,
            frames_submitted: 11,
            busy: Duration::from_millis(190),
            ..window()
        });
        assert_eq!(verdict.limiter, FrameRateLimiter::NoDamage);
        assert!(!verdict.rate_capped);
    }

    /// Run 2 on the Xorg lab host: the controller held the target at 8 and every due tick
    /// had a frame.
    #[test]
    fn a_met_lowered_target_is_rate_control() {
        let verdict = classify_frame_rate(FrameRateWindow {
            target_fps: 8,
            ticks_due: 8,
            frames_submitted: 8,
            busy: Duration::from_millis(140),
            ..window()
        });
        assert_eq!(verdict.limiter, FrameRateLimiter::RateControl);
        assert!(verdict.rate_capped);
    }

    #[test]
    fn content_below_a_lowered_target_is_still_content_but_flagged_capped() {
        let verdict = classify_frame_rate(FrameRateWindow {
            target_fps: 8,
            ticks_due: 8,
            ticks_without_change: 4,
            frames_submitted: 4,
            busy: Duration::from_millis(70),
            ..window()
        });
        assert_eq!(verdict.limiter, FrameRateLimiter::NoDamage);
        assert!(verdict.rate_capped);
    }

    #[test]
    fn a_saturated_window_is_pipeline_bound() {
        let verdict = classify_frame_rate(FrameRateWindow {
            ticks_due: 20,
            frames_submitted: 20,
            busy: Duration::from_millis(980),
            ..window()
        });
        assert_eq!(verdict.limiter, FrameRateLimiter::Pipeline);
    }

    #[test]
    fn skipped_deadlines_are_pipeline_bound_even_with_idle_time() {
        let verdict = classify_frame_rate(FrameRateWindow {
            ticks_due: 15,
            frames_submitted: 15,
            busy: Duration::from_millis(600),
            ..window()
        });
        assert_eq!(verdict.limiter, FrameRateLimiter::Pipeline);
    }

    #[test]
    fn rates_follow_the_window_length() {
        let verdict = classify_frame_rate(FrameRateWindow {
            ticks_due: 31,
            frames_submitted: 28,
            elapsed: Duration::from_millis(1_040),
            ..window()
        });
        assert_eq!(verdict.limiter, FrameRateLimiter::AtContract);
    }

    #[test]
    fn tokens_round_trip() {
        for limiter in [
            FrameRateLimiter::AtContract,
            FrameRateLimiter::NoDamage,
            FrameRateLimiter::RateControl,
            FrameRateLimiter::Pipeline,
        ] {
            assert_eq!(FrameRateLimiter::from_token(limiter.token()), Some(limiter));
        }
        assert_eq!(FrameRateLimiter::from_token("network"), None);
    }
}
