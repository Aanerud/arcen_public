#![allow(dead_code)]

use std::fmt::{Debug, Formatter};
use std::sync::{Arc, Mutex};

/// Maximum reconnect hold retained by one local session admission.
///
/// This matches Pier's maximum supported direct reconnect window.
pub const MAX_RECONNECT_HOLD_SECONDS: u64 = 2 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReconnectHoldPolicy {
    max_hold_seconds: u64,
}

impl ReconnectHoldPolicy {
    pub(crate) const fn new(max_hold_seconds: u64) -> Result<Self, SessionAdmissionError> {
        if max_hold_seconds > MAX_RECONNECT_HOLD_SECONDS {
            return Err(SessionAdmissionError::ReconnectHoldOutOfRange);
        }
        Ok(Self { max_hold_seconds })
    }
}

pub(crate) struct SessionAdmissionLease {
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
    pub(crate) const fn is_reconnect_hold(&self) -> bool {
        self.reconnect_until.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionAdmissionError {
    SessionAlreadyActive,
    ForeignLease,
    ReconnectHoldExpired,
    ReconnectHoldOutOfRange,
    LockPoisoned,
}

impl std::fmt::Display for SessionAdmissionError {
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

    fn hold_for_reconnect_with_policy(
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

    fn complete(&mut self, lease: SessionAdmissionLease) -> Result<(), SessionAdmissionError> {
        self.validate_lease(&lease)?;
        self.active_token = None;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SessionAdmissionRuntime {
    gate: Mutex<SessionAdmissionGate>,
}

impl SessionAdmissionRuntime {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            gate: Mutex::new(SessionAdmissionGate::new()),
        })
    }

    pub(crate) fn admit_new(&self) -> Result<SessionAdmissionLease, SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .admit_new()
    }

    pub(crate) fn hold_for_reconnect(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
        reconnect_until: u64,
    ) -> Result<(), SessionAdmissionError> {
        let policy = ReconnectHoldPolicy::new(MAX_RECONNECT_HOLD_SECONDS)?;
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .hold_for_reconnect_with_policy(lease, now_epoch_seconds, reconnect_until, policy)
    }

    pub(crate) fn resume(
        &self,
        lease: &mut SessionAdmissionLease,
        now_epoch_seconds: u64,
    ) -> Result<(), SessionAdmissionError> {
        self.gate
            .lock()
            .map_err(|_| SessionAdmissionError::LockPoisoned)?
            .resume(lease, now_epoch_seconds)
    }

    pub(crate) fn complete(&self, lease: SessionAdmissionLease) {
        match self.gate.lock() {
            Ok(mut gate) => {
                if let Err(error) = gate.complete(lease) {
                    tracing::error!(target: crate::logging::SESSION, %error, "session admission release failed");
                }
            }
            Err(_) => {
                tracing::error!(target: crate::logging::SESSION, "session admission gate lock poisoned during release");
            }
        }
    }
}
