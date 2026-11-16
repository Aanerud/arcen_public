// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
//! [`AuthThrottle`] is pure and clock-free: the caller passes the time, so the
//! policy is tested exactly. [`HostAuthThrottle`] is the one adapter every
//! Pier uses around it — the lock, the monotonic clock, the keys for a
//! sign-in, and the bound on credential checks in flight — so the policy and
//! its wiring cannot drift between hosts. A host only classifies its own
//! verifier's outcome into an [`AttemptOutcome`].
//!
//! Every Pier answers a throttled sign-in with [`THROTTLED_MESSAGE`] and
//! without checking the password.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// What a Deck is told when its sign-in is refused by the throttle. It says
/// nothing about the credential, which was not checked.
pub const THROTTLED_MESSAGE: &str =
    "too many failed sign-ins; wait a few minutes before trying again";

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
    /// table without bound. A key that is backing off is never forgotten to
    /// make room: when the table is full of them, a sign-in that would need
    /// a new key waits instead (fail closed), so a flood of fresh addresses
    /// or names cannot push a throttled account out of the table.
    pub max_keys: usize,
    /// Most credential checks in flight at once for one key. Without it a
    /// client that opens many connections gets every guess checked before
    /// the first failure is recorded, and the back-off never applies to them.
    pub max_in_flight: u32,
}

impl Default for AuthThrottlePolicy {
    fn default() -> Self {
        Self {
            free_failures: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(300),
            forget_after: Duration::from_secs(15 * 60),
            max_keys: 65_536,
            max_in_flight: 2,
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
    ///
    /// A `DOMAIN\` prefix and an `@realm` suffix are dropped too: `bob`,
    /// `CORP\bob`, `.\bob` and `bob@corp.example` may all name one
    /// account, and each spelling must not get a budget of its own. Two
    /// accounts with the same name in different domains then share one
    /// history, which only ever errs towards waiting.
    ///
    /// The name ends at its first NUL, as it does for the C APIs that check
    /// it, so `bob\0anything` is `bob` here too.
    #[must_use]
    pub fn account(name: &str) -> Self {
        let name = name.split('\0').next().unwrap_or_default().trim();
        let name = name.rsplit_once('\\').map_or(name, |(_, user)| user);
        let name = name.split_once('@').map_or(name, |(user, _)| user);
        Self::Account(name.trim().to_lowercase())
    }

    /// A key for a network source.
    ///
    /// An IPv4-mapped IPv6 address is its IPv4 address, and an IPv6 address
    /// counts as its /64: one host is normally given a whole /64, so keying
    /// by the full address would hand every guess a fresh budget.
    #[must_use]
    pub fn source(address: IpAddr) -> Self {
        match crate::session_admission::source_network(address) {
            IpAddr::V6(network) => Self::Source(format!("{network}/64")),
            IpAddr::V4(address) => Self::Source(address.to_string()),
        }
    }
}

/// The address in a peer's text form: a socket address (`198.51.100.7:4433`,
/// `[2001:db8::1]:4433`) or a bare IP address, as host adapters log them.
#[must_use]
pub fn peer_address(peer: &str) -> Option<IpAddr> {
    peer.parse::<std::net::SocketAddr>()
        .map(|socket| socket.ip())
        .or_else(|_| peer.parse::<IpAddr>())
        .ok()
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
    in_flight: HashMap<ThrottleKey, u32>,
}

impl AuthThrottle {
    /// Creates an empty throttle.
    #[must_use]
    pub fn new(policy: AuthThrottlePolicy) -> Self {
        Self {
            policy,
            entries: HashMap::new(),
            in_flight: HashMap::new(),
        }
    }

    /// Admits one credential check for `keys`, or returns how long to wait.
    ///
    /// Refused while any key is backing off ([`Self::retry_after`]) or
    /// already has [`AuthThrottlePolicy::max_in_flight`] checks running; a
    /// check refused for being one too many is told to wait the base delay.
    /// Every admitted check must be ended with [`Self::end_check`].
    ///
    /// # Errors
    ///
    /// The wait, when the check may not run now.
    pub fn begin_check(&mut self, keys: &[ThrottleKey], now: Duration) -> Result<(), Duration> {
        if let Some(wait) = self.retry_after(keys, now) {
            return Err(wait);
        }
        self.forget_stale(now);
        if !self.has_room_for(keys, now) {
            return Err(self.policy.base_delay);
        }
        let busy = keys
            .iter()
            .any(|key| self.in_flight.get(key).copied().unwrap_or(0) >= self.policy.max_in_flight);
        if busy {
            return Err(self.policy.base_delay);
        }
        for key in keys {
            *self.in_flight.entry(key.clone()).or_insert(0) += 1;
        }
        Ok(())
    }

    /// Ends a check admitted by [`Self::begin_check`]. Records nothing: the
    /// outcome is [`Self::record_failure`] or [`Self::record_success`].
    pub fn end_check(&mut self, keys: &[ThrottleKey]) {
        for key in keys {
            if let Some(count) = self.in_flight.get_mut(key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.in_flight.remove(key);
                }
            }
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

    /// Whether a failure against every one of `keys` could be recorded now:
    /// each is already remembered, or there is a free place or one held by
    /// a key that is not backing off.
    fn has_room_for(&self, keys: &[ThrottleKey], now: Duration) -> bool {
        let new_keys = keys
            .iter()
            .filter(|key| !self.entries.contains_key(*key))
            .count();
        if new_keys == 0 {
            return true;
        }
        let free = self.policy.max_keys.saturating_sub(self.entries.len());
        free >= new_keys
            || free
                + self
                    .entries
                    .values()
                    .filter(|entry| entry.blocked_until <= now)
                    .count()
                >= new_keys
    }

    /// Records a failed attempt against every key.
    ///
    /// A key with no place in a table full of keys that are backing off is
    /// not recorded; [`Self::begin_check`] refuses such an attempt up front,
    /// so this only happens to checks admitted before the table filled.
    pub fn record_failure(&mut self, keys: &[ThrottleKey], now: Duration) {
        self.forget_stale(now);
        for key in keys {
            if !self.entries.contains_key(key)
                && self.entries.len() >= self.policy.max_keys
                && !self.evict_oldest_unblocked(now)
            {
                continue;
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

    /// Forgets the key with the oldest failure among those not backing off.
    /// Returns whether one was forgotten.
    fn evict_oldest_unblocked(&mut self, now: Duration) -> bool {
        let oldest = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.blocked_until <= now)
            .min_by_key(|(_, entry)| entry.last_failure)
            .map(|(key, _)| key.clone());
        oldest.is_some_and(|key| self.entries.remove(&key).is_some())
    }
}

/// How one credential check ended, as far as the throttle is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The credential was proved. The source's history is cleared. The
    /// account's is not: its key is folded (case, domain, realm) and may be
    /// shared by another real account, whose sign-in must not wipe the
    /// failures against this one. It expires by itself
    /// ([`AuthThrottlePolicy::forget_after`]).
    Accepted,
    /// The account database refused the credential or the account. Counts
    /// as a failure against every key.
    Refused,
    /// The host could not judge the credential (its verifier was unavailable,
    /// timed out, or failed after the check). Nothing was learned about the
    /// person asking, so nothing is recorded.
    Undecided,
}

/// A sign-in refused by the throttle, without checking the credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleRefusal {
    /// How long until this source and account may try again.
    pub retry_after: Duration,
}

/// The throttle a Pier process shares between all of its connections: the
/// policy, a lock, and a monotonic clock.
///
/// Use one per process, for example in a `static` `LazyLock`, and wrap every
/// credential check in [`Self::begin`] … [`AuthAttempt::finish`].
#[derive(Debug)]
pub struct HostAuthThrottle {
    epoch: Instant,
    state: Mutex<AuthThrottle>,
}

impl Default for HostAuthThrottle {
    fn default() -> Self {
        Self::new(AuthThrottlePolicy::default())
    }
}

impl HostAuthThrottle {
    /// A throttle with `policy`, starting now.
    #[must_use]
    pub fn new(policy: AuthThrottlePolicy) -> Self {
        Self {
            epoch: Instant::now(),
            state: Mutex::new(AuthThrottle::new(policy)),
        }
    }

    /// The keys a sign-in from `source` for `account` counts against.
    #[must_use]
    pub fn keys(source: Option<IpAddr>, account: &str) -> Vec<ThrottleKey> {
        let mut keys = vec![ThrottleKey::account(account)];
        keys.extend(source.map(ThrottleKey::source));
        keys
    }

    /// Asks to check a credential for `account` from `source`.
    ///
    /// Call it before the password is checked; refuse the sign-in with
    /// [`THROTTLED_MESSAGE`] on `Err`.
    ///
    /// # Errors
    ///
    /// [`ThrottleRefusal`] while this source or account is backing off, or
    /// has too many checks in flight.
    pub fn begin(
        &self,
        source: Option<IpAddr>,
        account: &str,
    ) -> Result<AuthAttempt<'_>, ThrottleRefusal> {
        let keys = Self::keys(source, account);
        let now = self.now();
        self.lock()
            .begin_check(&keys, now)
            .map_err(|retry_after| ThrottleRefusal { retry_after })?;
        Ok(AuthAttempt {
            throttle: self,
            keys,
        })
    }

    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    // The state is a pair of maps updated without any call that can panic
    // part-way, so a poisoned lock still holds a consistent table. Recovering
    // keeps the throttle in force; giving up on it would let every attempt
    // through.
    fn lock(&self) -> MutexGuard<'_, AuthThrottle> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One admitted credential check. Dropping it without [`Self::finish`] (a
/// cancelled sign-in) frees its place and records nothing.
#[derive(Debug)]
#[must_use = "finish the attempt with its outcome"]
pub struct AuthAttempt<'a> {
    throttle: &'a HostAuthThrottle,
    keys: Vec<ThrottleKey>,
}

impl AuthAttempt<'_> {
    /// Records how the check ended.
    pub fn finish(self, outcome: AttemptOutcome) {
        let now = self.throttle.now();
        let mut state = self.throttle.lock();
        match outcome {
            AttemptOutcome::Accepted => {
                let sources: Vec<ThrottleKey> = self
                    .keys
                    .iter()
                    .filter(|key| matches!(key, ThrottleKey::Source(_)))
                    .cloned()
                    .collect();
                state.record_success(&sources);
            }
            AttemptOutcome::Refused => state.record_failure(&self.keys, now),
            AttemptOutcome::Undecided => {}
        }
        // `Drop` ends the check, and takes the lock again.
        drop(state);
    }
}

impl Drop for AuthAttempt<'_> {
    fn drop(&mut self) {
        self.throttle.lock().end_check(&self.keys);
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

    #[test]
    fn spellings_of_one_account_share_one_history() {
        let bob = ThrottleKey::account("bob");
        for spelling in [
            " Bob ",
            "CORP\\bob",
            ".\\BOB",
            "bob@corp.example",
            "CORP\\Bob@corp.example",
            "bob\0anything",
        ] {
            assert_eq!(ThrottleKey::account(spelling), bob, "{spelling}");
        }
        assert_ne!(ThrottleKey::account("alice"), bob);
    }

    #[test]
    fn sources_are_keyed_by_address_and_ipv6_by_its_64() {
        let v4: IpAddr = "203.0.113.9".parse().expect("v4");
        let mapped: IpAddr = "::ffff:203.0.113.9".parse().expect("mapped");
        assert_eq!(ThrottleKey::source(mapped), ThrottleKey::source(v4));
        assert_eq!(
            ThrottleKey::source(v4),
            ThrottleKey::Source("203.0.113.9".to_owned())
        );
        let one: IpAddr = "2001:db8:1:2::1".parse().expect("v6");
        let same_64: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:fffe".parse().expect("v6");
        let other_64: IpAddr = "2001:db8:1:3::1".parse().expect("v6");
        assert_eq!(ThrottleKey::source(one), ThrottleKey::source(same_64));
        assert_ne!(ThrottleKey::source(one), ThrottleKey::source(other_64));
        assert_eq!(
            ThrottleKey::source(one),
            ThrottleKey::Source("2001:db8:1:2::/64".to_owned())
        );
    }

    #[test]
    fn peer_text_is_read_with_or_without_a_port() {
        let expected: IpAddr = "198.51.100.7".parse().expect("v4");
        assert_eq!(peer_address("198.51.100.7:4433"), Some(expected));
        assert_eq!(peer_address("198.51.100.7"), Some(expected));
        let v6: IpAddr = "2001:db8::1".parse().expect("v6");
        assert_eq!(peer_address("[2001:db8::1]:4433"), Some(v6));
        assert_eq!(peer_address("2001:db8::1"), Some(v6));
        assert_eq!(peer_address("deck.example.internal"), None);
    }

    #[test]
    fn checks_in_flight_are_bounded_per_key() {
        let mut throttle = AuthThrottle::new(AuthThrottlePolicy::default());
        assert_eq!(throttle.begin_check(&keys(), at(0)), Ok(()));
        assert_eq!(throttle.begin_check(&keys(), at(0)), Ok(()));
        // A third concurrent guess from the same address, even for another
        // account, waits the base delay.
        let other_account = [keys()[0].clone(), ThrottleKey::account("other")];
        assert_eq!(throttle.begin_check(&other_account, at(0)), Err(at(2)));
        throttle.end_check(&keys());
        assert_eq!(throttle.begin_check(&other_account, at(0)), Ok(()));
        throttle.end_check(&keys());
        throttle.end_check(&other_account);
        assert!(throttle.in_flight.is_empty());
    }

    #[test]
    fn a_host_throttle_refuses_after_the_free_failures() {
        let host = HostAuthThrottle::default();
        let source = Some("203.0.113.9".parse().expect("v4"));
        for _ in 0..4 {
            host.begin(source, "someone")
                .expect("admitted")
                .finish(AttemptOutcome::Refused);
        }
        let refusal = host.begin(source, "someone").expect_err("backing off");
        assert!(refusal.retry_after > Duration::ZERO);
        assert!(refusal.retry_after <= at(2));
        // The account is throttled from a fresh address too, and the
        // address for a fresh account.
        assert!(
            host.begin(Some("198.51.100.1".parse().expect("v4")), "SOMEONE")
                .is_err()
        );
        assert!(host.begin(source, "another").is_err());
        assert!(
            host.begin(Some("198.51.100.1".parse().expect("v4")), "another")
                .is_ok()
        );
    }

    #[test]
    fn only_a_refused_credential_counts_and_success_clears_the_source() {
        let host = HostAuthThrottle::default();
        let source = Some("203.0.113.9".parse().expect("v4"));
        let elsewhere = Some("198.51.100.1".parse().expect("v4"));
        for _ in 0..10 {
            host.begin(source, "someone")
                .expect("admitted")
                .finish(AttemptOutcome::Undecided);
            // A cancelled sign-in records nothing and frees its place.
            drop(host.begin(source, "someone").expect("admitted"));
        }
        for _ in 0..3 {
            host.begin(source, "someone")
                .expect("admitted")
                .finish(AttemptOutcome::Refused);
        }
        host.begin(source, "someone")
            .expect("still free")
            .finish(AttemptOutcome::Accepted);
        // The source starts again: three more free failures for another name.
        for _ in 0..3 {
            host.begin(source, "another")
                .expect("source history cleared")
                .finish(AttemptOutcome::Refused);
        }
        // The account keeps its failures: a same-named account's success
        // (`OTHERDOM\someone`) must not wipe them, so one more from
        // anywhere starts the back-off.
        host.begin(elsewhere, "CORP\\someone")
            .expect("fourth failure")
            .finish(AttemptOutcome::Refused);
        assert!(
            host.begin(Some("192.0.2.1".parse().expect("v4")), "someone")
                .is_err()
        );
        assert!(host.lock().in_flight.is_empty(), "every check ended");
    }

    #[test]
    fn a_full_table_never_forgets_a_key_that_is_backing_off() {
        let policy = AuthThrottlePolicy {
            max_keys: 4,
            ..AuthThrottlePolicy::default()
        };
        let mut throttle = AuthThrottle::new(policy);
        let victim = [ThrottleKey::account("victim")];
        for _ in 0..4 {
            throttle.record_failure(&victim, at(0));
        }
        let blocked = throttle.retry_after(&victim, at(0));
        assert!(blocked.is_some());
        // Fresh names fill the rest; each is one failure, so evictable.
        for index in 0..3 {
            throttle.record_failure(&[ThrottleKey::account(&format!("n{index}"))], at(1));
        }
        // A flood of fresh keys cycles through the unblocked places only.
        for index in 0..50 {
            let fresh = [ThrottleKey::account(&format!("flood{index}"))];
            assert_eq!(throttle.begin_check(&fresh, at(1)), Ok(()));
            throttle.end_check(&fresh);
            throttle.record_failure(&fresh, at(1));
        }
        assert_eq!(throttle.retry_after(&victim, at(1)), Some(at(1)));

        // When every place is held by a key that is backing off, a sign-in
        // needing a new key waits rather than going unrecorded.
        let mut full = AuthThrottle::new(policy);
        for index in 0..4 {
            let key = [ThrottleKey::account(&format!("blocked{index}"))];
            for _ in 0..4 {
                full.record_failure(&key, at(0));
            }
        }
        let fresh = [ThrottleKey::account("fresh")];
        assert_eq!(full.begin_check(&fresh, at(0)), Err(at(2)));
        full.record_failure(&fresh, at(0));
        assert_eq!(full.entries.len(), 4);
        assert!(!full.entries.contains_key(&fresh[0]));
        // Once the waits have passed the places are free again.
        assert_eq!(full.begin_check(&fresh, at(3)), Ok(()));
    }

    #[test]
    fn concurrent_attempts_cannot_outrun_the_back_off() {
        let host = HostAuthThrottle::default();
        let source = Some("203.0.113.9".parse().expect("v4"));
        let first = host.begin(source, "someone").expect("admitted");
        let second = host.begin(source, "someone").expect("admitted");
        assert!(host.begin(source, "someone").is_err(), "third in flight");
        first.finish(AttemptOutcome::Refused);
        second.finish(AttemptOutcome::Refused);
    }
}
