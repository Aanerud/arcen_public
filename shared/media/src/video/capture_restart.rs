//! When a capture epoch that ended on its own may be restarted inside the
//! same attachment.
//!
//! A capture process can die (a crash, a driver reset) or lose its source
//! (the compositor replaced its stream) while the Deck, audio, microphone and
//! input are fine. The attachment then starts a new capture epoch — new
//! process, keyframe first — instead of ending the session, but only a
//! bounded number of times: a capture that keeps failing ends the attachment
//! with its reason rather than looping.

use std::time::Duration;

/// Restarts allowed within [`CAPTURE_RESTART_WINDOW`].
pub const CAPTURE_RESTARTS_PER_WINDOW: usize = 3;

/// The window restarts are counted in.
pub const CAPTURE_RESTART_WINDOW: Duration = Duration::from_secs(60);

/// What to do about a capture epoch that ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureRestartDecision {
    /// Start a new epoch; this is restart `attempt` within the window.
    Restart {
        /// 1-based restart count within the window.
        attempt: usize,
    },
    /// The budget is spent: end the attachment.
    Exhausted,
}

/// Bounded restart budget over a monotonic clock (any common origin).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureRestartBudget {
    restarts: Vec<Duration>,
}

impl CaptureRestartBudget {
    /// An unused budget.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            restarts: Vec::new(),
        }
    }

    /// Decides about an epoch that ended at `now`, and records the restart
    /// when one is granted.
    pub fn on_epoch_ended(&mut self, now: Duration) -> CaptureRestartDecision {
        self.restarts
            .retain(|at| now.saturating_sub(*at) < CAPTURE_RESTART_WINDOW);
        if self.restarts.len() >= CAPTURE_RESTARTS_PER_WINDOW {
            return CaptureRestartDecision::Exhausted;
        }
        self.restarts.push(now);
        CaptureRestartDecision::Restart {
            attempt: self.restarts.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn three_restarts_per_minute_then_exhausted() {
        let mut budget = CaptureRestartBudget::new();
        assert_eq!(
            budget.on_epoch_ended(S(10)),
            CaptureRestartDecision::Restart { attempt: 1 }
        );
        assert_eq!(
            budget.on_epoch_ended(S(20)),
            CaptureRestartDecision::Restart { attempt: 2 }
        );
        assert_eq!(
            budget.on_epoch_ended(S(30)),
            CaptureRestartDecision::Restart { attempt: 3 }
        );
        assert_eq!(
            budget.on_epoch_ended(S(40)),
            CaptureRestartDecision::Exhausted
        );
    }

    #[test]
    fn restarts_age_out_of_the_window() {
        let mut budget = CaptureRestartBudget::new();
        for at in [0, 1, 2] {
            assert!(matches!(
                budget.on_epoch_ended(S(at)),
                CaptureRestartDecision::Restart { .. }
            ));
        }
        assert_eq!(
            budget.on_epoch_ended(S(59)),
            CaptureRestartDecision::Exhausted
        );
        assert_eq!(
            budget.on_epoch_ended(S(61)),
            CaptureRestartDecision::Restart { attempt: 2 }
        );
    }
}
