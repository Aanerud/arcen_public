//! Following a sign-in screen into the desktop that replaces it.
//!
//! A host serving its operating system's sign-in screen says so in its hello
//! (`ServerHelloMsg::login_window`). That session ends the moment somebody
//! signs in: the sign-in screen belongs to nobody and goes away, and the
//! signed-in desktop is a different session that takes a moment to appear.
//! macOS and Windows both behave this way.
//!
//! So a client that sees that session close has not lost anything. It holds
//! the last frame and connects again, a little later, to whatever replaced it.
//! This is the schedule for doing that. It is pure: it holds no credentials and
//! reads no clock. The client passes in its own monotonic time, and it keeps
//! whatever it needs to sign in again.

use std::time::Duration;

/// How long after the sign-in screen closes before the first reconnect.
///
/// Measured on a macOS lab host: the signed-in user's desktop agent was ready
/// within two seconds of the sign-in screen closing.
pub const FIRST_RECONNECT_DELAY: Duration = Duration::from_millis(2_500);

/// How long between later attempts, when the desktop was not ready yet.
pub const RETRY_DELAY: Duration = Duration::from_secs(2);

/// How many times to try before giving up and asking the user.
///
/// Five attempts cover about eleven seconds, well beyond a normal sign-in.
/// After that, something other than a sign-in has happened, and the user
/// should see it.
pub const MAX_ATTEMPTS: u8 = 5;

/// When to reconnect after a sign-in screen closes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HandoverSchedule {
    attempts_made: u8,
    due: Option<Duration>,
}

impl HandoverSchedule {
    /// A handover that has not started.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            attempts_made: 0,
            due: None,
        }
    }

    /// The session ended at `now`. Returns when to reconnect, or `None` once
    /// every attempt has been used, which is when the handover is over and
    /// the client reports the disconnection as it would any other.
    pub fn session_ended(&mut self, now: Duration) -> Option<Duration> {
        if self.attempts_made >= MAX_ATTEMPTS {
            self.due = None;
            return None;
        }
        let delay = if self.attempts_made == 0 {
            FIRST_RECONNECT_DELAY
        } else {
            RETRY_DELAY
        };
        let due = now.saturating_add(delay);
        self.due = Some(due);
        Some(due)
    }

    /// Whether an attempt is due at `now`. A due attempt is counted and
    /// consumed, so it is started exactly once.
    pub fn take_due(&mut self, now: Duration) -> bool {
        match self.due {
            Some(due) if now >= due => {
                self.due = None;
                self.attempts_made = self.attempts_made.saturating_add(1);
                true
            }
            _ => false,
        }
    }

    /// When the next attempt will start, if one is waiting.
    #[must_use]
    pub const fn pending(&self) -> Option<Duration> {
        self.due
    }

    /// How many attempts have been started.
    #[must_use]
    pub const fn attempts_made(&self) -> u8 {
        self.attempts_made
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn at(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn the_first_reconnect_waits_for_the_desktop_to_appear() {
        let mut schedule = HandoverSchedule::new();
        assert_eq!(schedule.session_ended(at(1_000)), Some(at(3_500)));
        assert!(!schedule.take_due(at(3_499)), "not before it is due");
        assert!(schedule.take_due(at(3_500)));
        assert!(!schedule.take_due(at(9_000)), "started exactly once");
        assert_eq!(schedule.attempts_made(), 1);
        assert_eq!(schedule.pending(), None);
    }

    #[test]
    fn a_desktop_that_is_not_ready_is_retried_sooner_then_given_up_on() {
        let mut schedule = HandoverSchedule::new();
        let mut now = at(0);
        for attempt in 0..MAX_ATTEMPTS {
            let due = schedule.session_ended(now).expect("attempts remain");
            let expected = if attempt == 0 {
                FIRST_RECONNECT_DELAY
            } else {
                RETRY_DELAY
            };
            assert_eq!(due - now, expected);
            assert!(schedule.take_due(due));
            now = due + at(300);
        }
        assert_eq!(schedule.session_ended(now), None, "the handover is over");
        assert_eq!(schedule.pending(), None);
    }
}
