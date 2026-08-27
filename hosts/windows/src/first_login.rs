//! Platform-neutral decision logic for the remote first-login flow.
//!
//! When an authenticated remote account has **no** existing interactive Windows
//! session, the broker hands the credential to the Credential Provider in
//! LogonUI (over the SYSTEM-only control pipe) and then waits for Winlogon to
//! create a session it can bind by SID. The Windows-specific transport and WTS
//! polling live in [`crate::cp_pipe`] and [`crate::windows_session`]; the pieces
//! that can be reasoned about and unit-tested without Windows — the error
//! taxonomy, the bounded-poll loop, correlation ids, and the single-use request
//! id source — live here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arcen_cp_ipc::UsageScenario;

/// How long to wait for a Credential Provider to connect and publish `Ready`
/// before giving up on a first-login attempt.
pub const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait, after pushing the credential, for Winlogon to create a
/// SID-matching unlocked WTS session.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Interval between WTS re-binding probes while waiting for the new session.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The Credential Provider scenarios that may legitimately satisfy a bind
/// classification, most-preferred first.
///
/// The broker picks a scenario from WTS state: a console session that matches
/// the account and is not flagged unlocked is classified `Locked`, so it asks
/// for `UnlockWorkstation`. LogonUI decides independently and does not always
/// agree — an account can be signed in, not unlocked, and still be fronted by
/// the welcome / switch-user screen, which is `CPUS_LOGON`. Measured on
/// pier-windows-software.example.internal, where the broker waited for unlock while the provider logged
/// `SetUsageScenario: logon`, and the handshake timed out with the provider
/// connected and verified the whole time.
///
/// Accepting logon for a locked console is safe, and is not a widening of what
/// the credential can do:
///
/// - the provider builds the LSA buffer from **its own** `SetUsageScenario`
///   state, so `KerbInteractiveLogon` and `KerbWorkstationUnlockLogon` always
///   match what LogonUI is really doing regardless of what the broker expected;
/// - the account's ownership of the console session is proved by token match
///   before dispatch either way; and
/// - an interactive logon for an account that already has a session is how
///   Windows reconnects that session, which is the outcome being asked for.
///
/// The reverse is **not** accepted. A console with no session cannot be
/// unlocked, so `Logon` stays exact.
#[must_use]
pub fn acceptable_scenarios(expected: UsageScenario) -> &'static [UsageScenario] {
    match expected {
        UsageScenario::Logon => &[UsageScenario::Logon],
        UsageScenario::UnlockWorkstation => {
            &[UsageScenario::UnlockWorkstation, UsageScenario::Logon]
        }
    }
}

/// Continuous exact-session stability required after Winlogon first reports the
/// authenticated console as unlocked. WTS can transition before Explorer/DWM
/// finish switching away from LogonUI; starting WGC in that gap can bind a
/// permanently black capture item.
pub const POST_LOGIN_STABILITY: Duration = Duration::from_secs(15);

/// Pure gate used by the Windows WTS poll to require a continuous exact bind
/// before launching the user-session agent.
#[derive(Debug, Default)]
pub struct SessionStability {
    first_exact: Option<Duration>,
}

impl SessionStability {
    pub fn observe(&mut self, elapsed: Duration, exact_bound: bool) -> bool {
        if !exact_bound {
            self.first_exact = None;
            return false;
        }
        let first_exact = self.first_exact.get_or_insert(elapsed);
        elapsed.saturating_sub(*first_exact) >= POST_LOGIN_STABILITY
    }

    pub fn observe_strict<'a>(
        &mut self,
        elapsed: Duration,
        observation: Result<bool, &'a str>,
    ) -> Result<bool, &'a str> {
        match observation {
            Ok(exact_bound) => Ok(self.observe(elapsed, exact_bound)),
            Err(error) => {
                self.first_exact = None;
                Err(error)
            }
        }
    }
}

/// Why a first-login attempt did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirstLoginError {
    /// Another first-login is already in flight; only one is allowed at a time.
    Busy,
    /// No Credential Provider connected and published readiness in time.
    NoCredentialProvider,
    /// The credential could not be built (bounds) before sealing.
    Payload(String),
    /// The sealed push failed, was rejected by the peer check, or the provider
    /// reported it did not arm.
    PushFailed(String),
    /// Winlogon did not produce a SID-matching unlocked session in time.
    SessionTimeout,
    /// A WTS re-binding probe returned a hard error.
    SessionProbe(String),
    /// The platform transport is unavailable (non-Windows build).
    #[allow(dead_code)] // constructed only by the non-Windows `first_login` stub
    Unsupported,
}

impl std::fmt::Display for FirstLoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("another remote first-login is already in progress"),
            Self::NoCredentialProvider => {
                f.write_str("no Arcen Credential Provider became ready at the console")
            }
            Self::Payload(detail) => write!(f, "credential could not be prepared: {detail}"),
            Self::PushFailed(detail) => write!(f, "credential handoff failed: {detail}"),
            Self::SessionTimeout => {
                f.write_str("timed out waiting for the interactive session to be created")
            }
            Self::SessionProbe(detail) => write!(f, "session binding probe failed: {detail}"),
            Self::Unsupported => f.write_str("remote first-login is unavailable on this platform"),
        }
    }
}

impl std::error::Error for FirstLoginError {}

impl FirstLoginError {
    /// A safe, generic message for the remote client. First-login failures never
    /// disclose whether an account or session exists, or why login failed, for
    /// the same reason [`crate::auth`] returns a single "Invalid credentials".
    pub fn client_message(&self) -> &'static str {
        match self {
            Self::Busy => "Another sign-in is completing at the console. Retry in a few seconds.",
            _ => {
                "Remote first sign-in could not be completed. If no one is signed in at the \
                 console, ensure the Arcen Credential Provider is installed and the account \
                 is allowed to sign in, then retry."
            }
        }
    }
}

/// Outcome of one probe in a [`poll_until_deadline`] loop.
#[cfg(test)]
pub enum Probe<T> {
    /// The awaited value is ready.
    Found(T),
    /// Not ready yet; keep waiting.
    Pending,
    /// A hard error; stop immediately.
    Failed(String),
}

/// Drive a bounded polling loop with an injected clock.
///
/// `now` returns a monotonically non-decreasing millisecond timestamp; `probe`
/// is called until it yields `Found`/`Failed` or the deadline passes; `advance`
/// waits between probes (production: sleep; tests: bump the fake clock). This is
/// the tested reference implementation of the wait semantics the async broker
/// mirrors with `tokio::time::sleep` in [`crate::cp_pipe`].
#[cfg(test)]
pub fn poll_until_deadline<T>(
    deadline_ms: u64,
    now: &mut dyn FnMut() -> u64,
    probe: &mut dyn FnMut() -> Probe<T>,
    advance: &mut dyn FnMut(),
) -> Result<T, FirstLoginError> {
    loop {
        match probe() {
            Probe::Found(value) => return Ok(value),
            Probe::Failed(detail) => return Err(FirstLoginError::SessionProbe(detail)),
            Probe::Pending => {}
        }
        if now() >= deadline_ms {
            return Err(FirstLoginError::SessionTimeout);
        }
        advance();
    }
}

/// A monotonic, non-zero source of single-use request ids for sealed pushes.
///
/// Seeded from the wall clock so ids do not restart at a predictable value
/// across broker restarts, then incremented per push. The id is bound into the
/// AEAD associated data; uniqueness per push is all that is required.
pub struct RequestIds {
    next: AtomicU64,
}

impl RequestIds {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            | 1;
        Self {
            next: AtomicU64::new(seed),
        }
    }

    /// Return the next non-zero request id.
    pub fn next_id(&self) -> u64 {
        let mut candidate = self.next.fetch_add(1, Ordering::Relaxed);
        if candidate == 0 {
            candidate = self.next.fetch_add(1, Ordering::Relaxed).max(1);
        }
        candidate
    }
}

impl Default for RequestIds {
    fn default() -> Self {
        Self::new()
    }
}

/// A short, log-safe correlation id for tracing one first-login attempt across
/// the broker, the pipe, and the WTS poll. Derived only from a counter and the
/// clock — never from any secret or account material.
pub fn new_correlation_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!("fl-{:08x}-{:04x}", millis & 0xffff_ffff, seq & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_returns_found_after_a_few_pending_probes() {
        let clock = std::cell::Cell::new(0u64);
        let mut attempts = 0u32;
        let deadline = 10_000;
        let result = poll_until_deadline::<u32>(
            deadline,
            &mut || clock.get(),
            &mut || {
                attempts += 1;
                if attempts < 3 {
                    Probe::Pending
                } else {
                    Probe::Found(attempts)
                }
            },
            &mut || clock.set(clock.get() + 500),
        );
        assert_eq!(result, Ok(3));
    }

    #[test]
    fn poll_times_out_when_deadline_passes() {
        let clock = std::cell::Cell::new(0u64);
        let deadline = 1_000;
        let result = poll_until_deadline::<u32>(
            deadline,
            &mut || clock.get(),
            &mut || Probe::Pending,
            &mut || clock.set(clock.get() + 400),
        );
        assert_eq!(result, Err(FirstLoginError::SessionTimeout));
    }

    #[test]
    fn poll_surfaces_a_hard_probe_error_immediately() {
        let clock = std::cell::Cell::new(0u64);
        let result = poll_until_deadline::<u32>(
            10_000,
            &mut || clock.get(),
            &mut || Probe::Failed("wts blew up".to_string()),
            &mut || clock.set(clock.get() + 1),
        );
        assert_eq!(
            result,
            Err(FirstLoginError::SessionProbe("wts blew up".to_string()))
        );
    }

    #[test]
    fn request_ids_are_non_zero_and_monotonic() {
        let ids = RequestIds::new();
        let a = ids.next_id();
        let b = ids.next_id();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert!(b > a);
    }

    #[test]
    fn correlation_ids_are_distinct_and_prefixed() {
        let a = new_correlation_id();
        let b = new_correlation_id();
        assert!(a.starts_with("fl-"));
        assert_ne!(a, b);
    }

    #[test]
    fn client_message_is_generic_and_leaks_nothing() {
        for error in [
            FirstLoginError::NoCredentialProvider,
            FirstLoginError::PushFailed("secret detail".to_string()),
            FirstLoginError::SessionTimeout,
            FirstLoginError::SessionProbe("S-1-5-21-...".to_string()),
        ] {
            let message = error.client_message();
            assert!(!message.contains("secret"));
            assert!(!message.contains("S-1-5"));
        }
        assert_ne!(
            FirstLoginError::Busy.client_message(),
            FirstLoginError::SessionTimeout.client_message()
        );
    }

    #[test]
    fn exact_session_must_remain_stable_across_desktop_transition() {
        let mut gate = SessionStability::default();
        assert!(!gate.observe(Duration::ZERO, true));
        assert!(!gate.observe(Duration::from_secs(14), true));
        assert!(gate.observe(Duration::from_secs(15), true));
    }

    #[test]
    fn transient_or_wrong_session_resets_desktop_transition_gate() {
        let mut gate = SessionStability::default();
        assert!(!gate.observe(Duration::ZERO, true));
        assert!(!gate.observe(Duration::from_secs(14), false));
        assert!(!gate.observe(Duration::from_secs(15), true));
        assert!(!gate.observe(Duration::from_secs(29), true));
        assert!(gate.observe(Duration::from_secs(30), true));
    }

    #[test]
    fn forbidden_candidate_aborts_and_resets_strict_stability_sequence() {
        let mut gate = SessionStability::default();
        assert_eq!(gate.observe_strict(Duration::ZERO, Ok(true)), Ok(false));
        assert_eq!(
            gate.observe_strict(Duration::from_secs(14), Err("rdp-extra")),
            Err("rdp-extra")
        );
        assert_eq!(
            gate.observe_strict(Duration::from_secs(15), Ok(true)),
            Ok(false)
        );
        assert_eq!(
            gate.observe_strict(Duration::from_secs(29), Ok(true)),
            Ok(false)
        );
        assert_eq!(
            gate.observe_strict(Duration::from_secs(30), Ok(true)),
            Ok(true)
        );
    }
}

#[cfg(test)]
mod scenario_policy_tests {
    use super::{acceptable_scenarios, UsageScenario};

    #[test]
    fn a_locked_console_accepts_the_logon_screen_but_prefers_unlock() {
        // pier-windows-software.example.internal, measured: the broker classified the console as
        // Locked and waited for UnlockWorkstation while the provider logged
        // `SetUsageScenario: logon`, so the handshake timed out with a
        // connected, peer-verified provider sitting there the whole time.
        let accepted = acceptable_scenarios(UsageScenario::UnlockWorkstation);
        assert_eq!(
            accepted,
            [UsageScenario::UnlockWorkstation, UsageScenario::Logon],
            "order matters: the classified scenario must still win when offered"
        );
    }

    #[test]
    fn a_console_with_no_session_never_accepts_unlock() {
        // There is nothing to unlock, so this direction must stay exact. If it
        // were widened, a machine with no interactive session could satisfy an
        // unlock request, which describes an operation that cannot happen.
        assert_eq!(
            acceptable_scenarios(UsageScenario::Logon),
            [UsageScenario::Logon]
        );
    }

    #[test]
    fn every_accepted_scenario_is_one_the_provider_can_serialize() {
        // The provider maps its own SetUsageScenario state onto the LSA message
        // type, so anything accepted here must be a scenario it recognises.
        // Both arms are covered so adding a third scenario fails this test
        // rather than silently becoming acceptable everywhere.
        for expected in [UsageScenario::Logon, UsageScenario::UnlockWorkstation] {
            for accepted in acceptable_scenarios(expected) {
                assert!(matches!(
                    accepted,
                    UsageScenario::Logon | UsageScenario::UnlockWorkstation
                ));
            }
        }
    }
}
