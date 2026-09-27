//! Backing off repeated failed sign-ins.
//!
//! A Pier authenticates against the machine's own account database, which on
//! a directory-bound machine is the organisation's. Without a limit, anyone
//! who can reach the port can guess passwords as fast as the host answers, and
//! can lock domain accounts out by failing on purpose. The policy here is the
//! same for every host: a few free failures, then an exponentially growing
//! wait, tracked per source address and per account name separately so that
//! neither spreading guesses across accounts nor across addresses escapes it.
//!
//! Pure and clock-free: the caller passes the time, so the policy is tested
//! exactly and a host adapter owns only the lock around it.

use std::collections::HashMap;
use std::time::Duration;

/// How strict the back-off is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthThrottlePolicy {
    /// Failures allowed before any wait is imposed.
    pub free_failures: u32,
    /// The first wait.
    pub base_delay: Duration,
    /// The longest wait.
    pub max_delay: Duration,
    /// How long a key with no new failure is remembered.
    pub forget_after: Duration,
    /// Most keys remembered at once, so a flood of addresses cannot grow the
    /// table without bound.
    pub max_keys: usize,
}

impl Default for AuthThrottlePolicy {
    fn default() -> Self {
        Self {
            free_failures: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(300),
            forget_after: Duration::from_secs(15 * 60),
            max_keys: 4096,
        }
    }
}

/// What is being throttled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ThrottleKey {
    /// A network source.
    Source(String),
    /// An account name as the client typed it, lowercased.
    Account(String),
}

impl ThrottleKey {
    /// A key for an account name. Case-folded, because directory lookups are
    /// case-insensitive and a guesser would otherwise get a fresh budget per
    /// spelling.
    #[must_use]
    pub fn account(name: &str) -> Self {
        Self::Account(name.trim().to_lowercase())
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    last_failure: Duration,
    blocked_until: Duration,
}

/// Failure history, keyed by source and by account.
#[derive(Debug, Clone, Default)]
pub struct AuthThrottle {
    policy: AuthThrottlePolicy,
    entries: HashMap<ThrottleKey, Entry>,
}

impl AuthThrottle {
    /// Creates an empty throttle.
    #[must_use]
    pub fn new(policy: AuthThrottlePolicy) -> Self {
        Self {
            policy,
            entries: HashMap::new(),
        }
    }

    /// Returns how long to wait before any of `keys` may try again, or `None`
    /// when all of them may try now.
    ///
    /// `now` is a monotonic time chosen by the caller, the same clock for
    /// every call.
    #[must_use]
    pub fn retry_after(&self, keys: &[ThrottleKey], now: Duration) -> Option<Duration> {
        keys.iter()
            .filter_map(|key| self.entries.get(key))
            .filter(|entry| entry.blocked_until > now)
            .map(|entry| entry.blocked_until.saturating_sub(now))
            .max()
    }

    /// Records a failed attempt against every key.
    pub fn record_failure(&mut self, keys: &[ThrottleKey], now: Duration) {
        self.forget_stale(now);
        for key in keys {
            if !self.entries.contains_key(key) && self.entries.len() >= self.policy.max_keys {
                self.evict_oldest();
            }
            let entry = self.entries.entry(key.clone()).or_insert(Entry {
                failures: 0,
                last_failure: now,
                blocked_until: Duration::ZERO,
            });
            entry.failures = entry.failures.saturating_add(1);
            entry.last_failure = now;
            let over = entry.failures.saturating_sub(self.policy.free_failures);
            if over > 0 {
                let exponent = (over - 1).min(16);
                let delay = self
                    .policy
                    .base_delay
                    .saturating_mul(1_u32 << exponent)
                    .min(self.policy.max_delay);
                entry.blocked_until = now.saturating_add(delay);
            }
        }
    }

    /// Clears the history of every key after a successful sign-in.
    pub fn record_success(&mut self, keys: &[ThrottleKey]) {
        for key in keys {
            self.entries.remove(key);
        }
    }

    fn forget_stale(&mut self, now: Duration) {
        let forget_after = self.policy.forget_after;
        self.entries.retain(|_, entry| {
            now.saturating_sub(entry.last_failure) < forget_after || entry.blocked_until > now
        });
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_failure)
            .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> [ThrottleKey; 2] {
        [
            ThrottleKey::Source("203.0.113.9".to_owned()),
            ThrottleKey::account("Someone"),
        ]
    }

    const fn at(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn a_few_mistakes_cost_nothing() {
        let mut throttle = AuthThrottle::new(AuthThrottlePolicy::default());
        for second in 0..3 {
            assert_eq!(throttle.retry_after(&keys(), at(second)), None);
            throttle.record_failure(&keys(), at(second));
        }
        assert_eq!(throttle.retry_after(&keys(), at(3)), None);
    }

    #[test]
    fn repeated_failures_wait_longer_each_time() {
        let mut throttle = AuthThrottle::new(AuthThrottlePolicy::default());
        for _ in 0..4 {
            throttle.record_failure(&keys(), at(0));
        }
        assert_eq!(throttle.retry_after(&keys(), at(0)), Some(at(2)));
        throttle.record_failure(&keys(), at(2));
        assert_eq!(throttle.retry_after(&keys(), at(2)), Some(at(4)));
        throttle.record_failure(&keys(), at(6));
        assert_eq!(throttle.retry_after(&keys(), at(6)), Some(at(8)));
        assert_eq!(throttle.retry_after(&keys(), at(14)), None);
    }

    #[test]
    fn the_wait_is_capped() {
        let mut throttle = AuthThrottle::new(AuthThrottlePolicy::default());
        for _ in 0..64 {
            throttle.record_failure(&keys(), at(0));
        }
        assert_eq!(throttle.retry_after(&keys(), at(0)), Some(at(300)));
    }

    #[test]
    fn spreading_guesses_across_addresses_does_not_escape_the_account_limit() {
        let mut throttle = AuthThrottle::new(AuthThrottlePolicy::default());
        for address in 0..4 {
            let keys = [
                ThrottleKey::Source(format!("203.0.113.{address}")),
                ThrottleKey::account("someone"),
            ];
            throttle.record_failure(&keys, at(0));
        }
        let fresh_address = [
            ThrottleKey::Source("203.0.113.200".to_owned()),
            ThrottleKey::account("SOMEONE"),
        ];
        assert!(throttle.retry_after(&fresh_address, at(0)).is_some());
    }

    #[test]
    fn a_success_clears_the_history() {
        let mut throttle = AuthThrottle::new(AuthThrottlePolicy::default());
        for _ in 0..5 {
            throttle.record_failure(&keys(), at(0));
        }
        throttle.record_success(&keys());
        assert_eq!(throttle.retry_after(&keys(), at(0)), None);
    }

    #[test]
    fn old_failures_are_forgotten_and_the_table_is_bounded() {
        let policy = AuthThrottlePolicy {
            max_keys: 8,
            ..AuthThrottlePolicy::default()
        };
        let mut throttle = AuthThrottle::new(policy);
        throttle.record_failure(&keys(), at(0));
        throttle.record_failure(&[ThrottleKey::account("other")], at(16 * 60));
        assert!(!throttle.entries.contains_key(&keys()[0]), "stale key kept");
        for index in 0..100 {
            throttle.record_failure(
                &[ThrottleKey::Source(format!("198.51.100.{index}"))],
                at(16 * 60),
            );
        }
        assert!(throttle.entries.len() <= 8);
    }
}
