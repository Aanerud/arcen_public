//! Shared capacity-one Pier admission and bounded reconnect hold.

use std::fmt::{Debug, Display, Formatter};
use std::sync::{Arc, Mutex};

/// Maximum reconnect hold retained by one local session admission.
pub const MAX_RECONNECT_HOLD_SECONDS: u64 = 2 * 60 * 60;

/// Validated reconnect-hold policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectHoldPolicy {
    max_hold_seconds: u64,
}

impl ReconnectHoldPolicy {
    /// Creates a policy no longer than the supported two-hour maximum.
    ///
    /// # Errors
    ///
    /// Returns [`SessionAdmissionError::ReconnectHoldOutOfRange`] when the
    /// requested hold exceeds two hours.
    pub const fn new(max_hold_seconds: u64) -> Result<Self, SessionAdmissionError> {
        if max_hold_seconds > MAX_RECONNECT_HOLD_SECONDS {
            return Err(SessionAdmissionError::ReconnectHoldOutOfRange);
        }
        Ok(Self { max_hold_seconds })
    }
}

/// Exclusive capacity-one admission lease.
pub struct SessionAdmissionLease {
    token: u64,
    reconnect_until: Option<u64>,
}

impl Debug for SessionAdmissionLease {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionAdmissionLease")
            .field("token", &"<redacted>")
            .field("reconnect_hold", &self.reconnect_until.is_some())
            .finish()
    }
}

impl SessionAdmissionLease {
    /// Returns whether this lease currently retains a reconnect hold.
    #[must_use]
    pub const fn is_reconnect_hold(&self) -> bool {
        self.reconnect_until.is_some()
    }
}

/// Failure while admitting, resuming, or releasing a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAdmissionError {
    SessionAlreadyActive,
    ForeignLease,
    ReconnectHoldExpired,
    ReconnectHoldOutOfRange,
    LockPoisoned,
}

impl Display for SessionAdmissionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SessionAlreadyActive => "a session is already active",
            Self::ForeignLease => "session admission lease does not belong to this gate",
            Self::ReconnectHoldExpired => "direct reconnect hold expired",
            Self::ReconnectHoldOutOfRange => "direct reconnect hold exceeds 7200 seconds",
            Self::LockPoisoned => "session admission gate lock poisoned",
        })
    }
}

impl std::error::Error for SessionAdmissionError {}

#[derive(Debug)]
struct SessionAdmissionGate {
    active_token: Option<u64>,
    next_token: u64,
}

impl SessionAdmissionGate {
    const fn new() -> Self {
        Self {
            active_token: None,
            next_token: 1,
        }
    }

    fn admit_new(&mut self) -> Result<SessionAdmissionLease, SessionAdmissionError> {
        if self.active_token.is_some() {
            return Err(SessionAdmissionError::SessionAlreadyActive);
        }
        let token = self.next_token;
        self.next_token = self.next_token.saturating_add(1);
        self.active_token = Some(token);
        Ok(SessionAdmissionLease {
            token,
            reconnect_until: None,
        })
    }

    fn validate_lease(&self, lease: &SessionAdmissionLease) -> Result<(), SessionAdmissionError> {
        if self.active_token == Some(lease.token) {
            Ok(())
        } else {
            Err(SessionAdmissionError::ForeignLease)
        }
    }

    fn hold_for_reconnect(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
        reconnect_until: u64,
        policy: ReconnectHoldPolicy,
    ) -> Result<(), SessionAdmissionError> {
        self.validate_lease(lease)?;
        let Some(hold_seconds) = reconnect_until.checked_sub(now_epoch_seconds) else {
            return Err(SessionAdmissionError::ReconnectHoldExpired);
        };
        if hold_seconds > policy.max_hold_seconds {
            return Err(SessionAdmissionError::ReconnectHoldOutOfRange);
        }
        lease.reconnect_until = Some(reconnect_until);
        Ok(())
    }

    fn resume(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
    ) -> Result<(), SessionAdmissionError> {
        self.validate_lease(lease)?;
        match lease.reconnect_until {
            Some(until) if now_epoch_seconds <= until => {
                lease.reconnect_until = None;
                Ok(())
            }
            Some(_) | None => Err(SessionAdmissionError::ReconnectHoldExpired),
        }
    }

    fn complete(&mut self, lease: &SessionAdmissionLease) -> Result<(), SessionAdmissionError> {
        if self.active_token == Some(lease.token) {
            self.active_token = None;
            Ok(())
        } else {
            Err(SessionAdmissionError::ForeignLease)
        }
    }
}

/// Thread-safe shared capacity-one admission gate.
#[derive(Debug)]
pub struct SessionAdmissionRuntime {
    gate: Mutex<SessionAdmissionGate>,
}

impl SessionAdmissionRuntime {
    /// Creates an empty admission gate.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            gate: Mutex::new(SessionAdmissionGate::new()),
        })
    }

    /// Admits a new session when no session or reconnect hold is active.
    ///
    /// # Errors
    ///
    /// Returns [`SessionAdmissionError::SessionAlreadyActive`] when the gate
    /// is occupied, or [`SessionAdmissionError::LockPoisoned`] if its mutex
    /// cannot be acquired.
    pub fn admit_new(&self) -> Result<SessionAdmissionLease, SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .admit_new()
    }

    /// Retains the lease for a bounded direct reconnect window.
    ///
    /// # Errors
    ///
    /// Returns an admission error when the lease is foreign, the deadline is
    /// expired, or the requested hold exceeds the supported maximum.
    pub fn hold_for_reconnect(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
        reconnect_until: u64,
    ) -> Result<(), SessionAdmissionError> {
        let policy = ReconnectHoldPolicy::new(MAX_RECONNECT_HOLD_SECONDS)?;
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .hold_for_reconnect(lease, now_epoch_seconds, reconnect_until, policy)
    }

    /// Resumes a held lease before its deadline.
    ///
    /// # Errors
    ///
    /// Returns [`SessionAdmissionError::ReconnectHoldExpired`] when no valid
    /// hold remains, or [`SessionAdmissionError::ForeignLease`] for another
    /// gate's lease.
    pub fn resume(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
    ) -> Result<(), SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .resume(lease, now_epoch_seconds)
    }

    /// Releases the session slot.
    ///
    /// # Errors
    ///
    /// Returns [`SessionAdmissionError::ForeignLease`] when the lease belongs
    /// to another gate, or [`SessionAdmissionError::LockPoisoned`] if the
    /// gate mutex cannot be acquired.
    pub fn complete(&self, lease: &SessionAdmissionLease) -> Result<(), SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .complete(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_capacity_one_until_completion() {
        let runtime = SessionAdmissionRuntime::new();
        let lease = runtime.admit_new().expect("first lease");

        assert_eq!(
            runtime.admit_new().expect_err("second lease"),
            SessionAdmissionError::SessionAlreadyActive
        );
        runtime.complete(&lease).expect("release first lease");
        assert!(runtime.admit_new().is_ok());
    }

    #[test]
    fn reconnect_hold_is_bounded_and_resumable_at_deadline() {
        let runtime = SessionAdmissionRuntime::new();
        let mut lease = runtime.admit_new().expect("lease");

        runtime
            .hold_for_reconnect(&mut lease, 100, 100 + MAX_RECONNECT_HOLD_SECONDS)
            .expect("maximum hold");
        assert!(lease.is_reconnect_hold());
        runtime
            .resume(&mut lease, 100 + MAX_RECONNECT_HOLD_SECONDS)
            .expect("deadline is inclusive");
        assert!(!lease.is_reconnect_hold());
    }

    #[test]
    fn rejects_expired_and_overlong_holds() {
        let runtime = SessionAdmissionRuntime::new();
        let mut lease = runtime.admit_new().expect("lease");

        assert_eq!(
            runtime.hold_for_reconnect(&mut lease, 10, 9),
            Err(SessionAdmissionError::ReconnectHoldExpired)
        );
        assert_eq!(
            runtime.hold_for_reconnect(&mut lease, 10, 10 + MAX_RECONNECT_HOLD_SECONDS + 1),
            Err(SessionAdmissionError::ReconnectHoldOutOfRange)
        );
    }

    #[test]
    fn foreign_lease_cannot_release_or_resume_another_gate() {
        let first = SessionAdmissionRuntime::new();
        let second = SessionAdmissionRuntime::new();
        let mut lease = first.admit_new().expect("first lease");

        assert_eq!(
            second.resume(&mut lease, 0),
            Err(SessionAdmissionError::ForeignLease)
        );
        assert_eq!(
            second.complete(&lease),
            Err(SessionAdmissionError::ForeignLease)
        );
    }
}
