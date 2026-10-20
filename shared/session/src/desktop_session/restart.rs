//! The auto-restart budget of `X-GNOME-AutoRestart` entries.

use std::collections::VecDeque;
use std::time::Duration;

/// Restarts allowed inside [`RESTART_WINDOW`].
pub const MAX_RESTARTS: usize = 3;
/// The sliding window restarts are counted in.
pub const RESTART_WINDOW: Duration = Duration::from_secs(600);
/// The shortest wait before a restart.
pub const RESTART_BACKOFF: Duration = Duration::from_secs(1);

/// What to do after an auto-restart entry exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Start it again after this delay.
    RestartAfter(Duration),
    /// It failed too often; leave it stopped.
    GiveUp,
}

/// Per-entry restart history on a monotonic clock the caller supplies.
#[derive(Debug, Clone, Default)]
pub struct AutoRestartBudget {
    restarts: VecDeque<Duration>,
}

impl AutoRestartBudget {
    /// Records an exit at monotonic time `now` and decides.
    pub fn on_exit(&mut self, now: Duration) -> RestartDecision {
        while self
            .restarts
            .front()
            .is_some_and(|at| now.saturating_sub(*at) >= RESTART_WINDOW)
        {
            self.restarts.pop_front();
        }
        if self.restarts.len() >= MAX_RESTARTS {
            return RestartDecision::GiveUp;
        }
        self.restarts.push_back(now);
        let backoff = RESTART_BACKOFF * u32::try_from(self.restarts.len()).unwrap_or(u32::MAX);
        RestartDecision::RestartAfter(backoff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restarts_are_bounded_per_window_and_back_off() {
        let mut budget = AutoRestartBudget::default();
        let at = Duration::from_secs;
        assert_eq!(budget.on_exit(at(0)), RestartDecision::RestartAfter(at(1)));
        assert_eq!(budget.on_exit(at(10)), RestartDecision::RestartAfter(at(2)));
        assert_eq!(budget.on_exit(at(20)), RestartDecision::RestartAfter(at(3)));
        assert_eq!(budget.on_exit(at(30)), RestartDecision::GiveUp);
        assert_eq!(budget.on_exit(at(599)), RestartDecision::GiveUp);
        assert_eq!(
            budget.on_exit(at(600)),
            RestartDecision::RestartAfter(at(3))
        );
    }
}
