//! OS-free Pier lifecycle state machine.

use std::error::Error;
use std::fmt::{Display, Formatter};

/// Shared lifecycle state for every native Pier implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PierLifecycleState {
    Starting,
    Listening,
    Authenticating,
    PreparingSession,
    SessionReady,
    Streaming,
    Recovering,
    Stopped,
    Failed,
}

/// Events produced by native adapters and the shared transport/session layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PierLifecycleEvent {
    NetworkReady,
    AuthenticationAccepted,
    NativeSessionReady,
    StreamReady,
    TransportLost,
    RecoveryComplete,
    Shutdown,
    FatalFailure,
}

/// Native evidence required before a host may advertise a usable session.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NativeReadinessEvidence {
    /// The authenticated native console/session identity is current.
    pub session_identity: bool,
    /// The requested output topology has been bound and verified.
    pub outputs_verified: bool,
    /// Capture and encoder readiness match the resolved media plan.
    pub media_verified: bool,
    /// Native input and cleanup authority are available.
    pub input_verified: bool,
}

impl NativeReadinessEvidence {
    /// Returns whether all evidence required for session readiness is present.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.session_identity && self.outputs_verified && self.media_verified && self.input_verified
    }
}

/// A native readiness report was incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncompleteReadinessEvidence;

impl Display for IncompleteReadinessEvidence {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("native readiness evidence is incomplete")
    }
}

impl Error for IncompleteReadinessEvidence {}

/// A transition that is not valid for the current lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidLifecycleTransition {
    pub state: PierLifecycleState,
    pub event: PierLifecycleEvent,
}

impl Display for InvalidLifecycleTransition {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "lifecycle event {:?} is invalid in state {:?}",
            self.event, self.state
        )
    }
}

impl Error for InvalidLifecycleTransition {}

/// Pure shared lifecycle coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PierLifecycle {
    state: PierLifecycleState,
}

impl Default for PierLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl PierLifecycle {
    /// Creates a lifecycle before the native service is listening.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: PierLifecycleState::Starting,
        }
    }

    /// Returns the current shared state.
    #[must_use]
    pub const fn state(self) -> PierLifecycleState {
        self.state
    }

    /// Applies one non-readiness lifecycle event.
    ///
    /// Native readiness events must use [`Self::apply_native_session_ready`]
    /// and [`Self::apply_stream_ready`] so incomplete evidence cannot advance
    /// the lifecycle.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidLifecycleTransition`] when an adapter reports an
    /// event that cannot occur from the current state.
    pub fn apply(
        &mut self,
        event: PierLifecycleEvent,
    ) -> Result<PierLifecycleState, InvalidLifecycleTransition> {
        if matches!(
            event,
            PierLifecycleEvent::NativeSessionReady | PierLifecycleEvent::StreamReady
        ) {
            return Err(InvalidLifecycleTransition {
                state: self.state,
                event,
            });
        }
        self.apply_transition(event)
    }

    fn apply_transition(
        &mut self,
        event: PierLifecycleEvent,
    ) -> Result<PierLifecycleState, InvalidLifecycleTransition> {
        let next = match (self.state, event) {
            (PierLifecycleState::Starting, PierLifecycleEvent::NetworkReady) => {
                PierLifecycleState::Listening
            }
            (PierLifecycleState::Listening, PierLifecycleEvent::AuthenticationAccepted) => {
                PierLifecycleState::Authenticating
            }
            (PierLifecycleState::Authenticating, PierLifecycleEvent::NativeSessionReady) => {
                PierLifecycleState::PreparingSession
            }
            (PierLifecycleState::PreparingSession, PierLifecycleEvent::StreamReady)
            | (PierLifecycleState::Recovering, PierLifecycleEvent::RecoveryComplete) => {
                PierLifecycleState::SessionReady
            }
            (PierLifecycleState::SessionReady, PierLifecycleEvent::StreamReady) => {
                PierLifecycleState::Streaming
            }
            (PierLifecycleState::Streaming, PierLifecycleEvent::TransportLost) => {
                PierLifecycleState::Recovering
            }
            (
                PierLifecycleState::Listening
                | PierLifecycleState::Authenticating
                | PierLifecycleState::PreparingSession
                | PierLifecycleState::SessionReady
                | PierLifecycleState::Streaming
                | PierLifecycleState::Recovering,
                PierLifecycleEvent::Shutdown,
            ) => PierLifecycleState::Stopped,
            (
                PierLifecycleState::Starting
                | PierLifecycleState::Listening
                | PierLifecycleState::Authenticating
                | PierLifecycleState::PreparingSession
                | PierLifecycleState::SessionReady
                | PierLifecycleState::Streaming
                | PierLifecycleState::Recovering,
                PierLifecycleEvent::FatalFailure,
            ) => PierLifecycleState::Failed,
            _ => {
                return Err(InvalidLifecycleTransition {
                    state: self.state,
                    event,
                });
            }
        };
        self.state = next;
        Ok(next)
    }

    /// Advances native session preparation only with complete native evidence.
    ///
    /// # Errors
    ///
    /// Returns [`IncompleteReadinessEvidence`] when any required native fact is
    /// absent, or [`InvalidLifecycleTransition`] when the state is wrong.
    pub fn apply_native_session_ready(
        &mut self,
        evidence: NativeReadinessEvidence,
    ) -> Result<PierLifecycleState, LifecycleApplyError> {
        if !evidence.is_complete() {
            return Err(LifecycleApplyError::IncompleteReadiness(
                IncompleteReadinessEvidence,
            ));
        }
        self.apply_transition(PierLifecycleEvent::NativeSessionReady)
            .map_err(LifecycleApplyError::InvalidTransition)
    }

    /// Advances stream readiness only with complete native evidence.
    ///
    /// # Errors
    ///
    /// Returns [`IncompleteReadinessEvidence`] when any required native fact is
    /// absent, or [`InvalidLifecycleTransition`] when the state is wrong.
    pub fn apply_stream_ready(
        &mut self,
        evidence: NativeReadinessEvidence,
    ) -> Result<PierLifecycleState, LifecycleApplyError> {
        if !evidence.is_complete() {
            return Err(LifecycleApplyError::IncompleteReadiness(
                IncompleteReadinessEvidence,
            ));
        }
        self.apply_transition(PierLifecycleEvent::StreamReady)
            .map_err(LifecycleApplyError::InvalidTransition)
    }
}

/// Failure applying an evidence-gated lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleApplyError {
    IncompleteReadiness(IncompleteReadinessEvidence),
    InvalidTransition(InvalidLifecycleTransition),
}

impl Display for LifecycleApplyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompleteReadiness(error) => error.fmt(formatter),
            Self::InvalidTransition(error) => error.fmt(formatter),
        }
    }
}

impl Error for LifecycleApplyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_shared_boot_to_streaming_path() {
        let mut lifecycle = PierLifecycle::new();
        lifecycle
            .apply(PierLifecycleEvent::NetworkReady)
            .expect("network");
        lifecycle
            .apply(PierLifecycleEvent::AuthenticationAccepted)
            .expect("authentication");
        let evidence = NativeReadinessEvidence {
            session_identity: true,
            outputs_verified: true,
            media_verified: true,
            input_verified: true,
        };
        lifecycle
            .apply_native_session_ready(evidence)
            .expect("native readiness");
        lifecycle
            .apply_stream_ready(evidence)
            .expect("stream preparation");
        lifecycle
            .apply_stream_ready(evidence)
            .expect("stream readiness");
        assert_eq!(lifecycle.state(), PierLifecycleState::Streaming);
    }

    #[test]
    fn recovery_returns_to_session_ready_before_streaming() {
        let mut lifecycle = PierLifecycle {
            state: PierLifecycleState::Streaming,
        };
        lifecycle
            .apply(PierLifecycleEvent::TransportLost)
            .expect("transport loss");
        lifecycle
            .apply(PierLifecycleEvent::RecoveryComplete)
            .expect("recovery");
        assert_eq!(lifecycle.state(), PierLifecycleState::SessionReady);
    }

    #[test]
    fn invalid_events_do_not_change_state() {
        let mut lifecycle = PierLifecycle::new();
        let error = lifecycle
            .apply(PierLifecycleEvent::StreamReady)
            .expect_err("stream cannot be ready before listening");
        assert_eq!(
            error,
            InvalidLifecycleTransition {
                state: PierLifecycleState::Starting,
                event: PierLifecycleEvent::StreamReady,
            }
        );
        assert_eq!(lifecycle.state(), PierLifecycleState::Starting);
    }

    #[test]
    fn readiness_events_cannot_bypass_evidence_gate() {
        let mut lifecycle = PierLifecycle {
            state: PierLifecycleState::Authenticating,
        };
        assert!(
            lifecycle
                .apply(PierLifecycleEvent::NativeSessionReady)
                .is_err()
        );
        assert_eq!(lifecycle.state(), PierLifecycleState::Authenticating);
    }

    #[test]
    fn shutdown_and_failure_are_terminal() {
        let mut stopped = PierLifecycle {
            state: PierLifecycleState::Listening,
        };
        stopped
            .apply(PierLifecycleEvent::Shutdown)
            .expect("shutdown");
        assert!(stopped.apply(PierLifecycleEvent::NetworkReady).is_err());

        let mut failed = PierLifecycle::new();
        failed
            .apply(PierLifecycleEvent::FatalFailure)
            .expect("failure");
        assert!(failed.apply(PierLifecycleEvent::NetworkReady).is_err());
    }

    #[test]
    fn readiness_requires_all_native_evidence() {
        let mut lifecycle = PierLifecycle {
            state: PierLifecycleState::Authenticating,
        };
        let incomplete = NativeReadinessEvidence {
            session_identity: true,
            outputs_verified: true,
            media_verified: true,
            input_verified: false,
        };
        assert_eq!(
            lifecycle.apply_native_session_ready(incomplete),
            Err(LifecycleApplyError::IncompleteReadiness(
                IncompleteReadinessEvidence
            ))
        );
        assert_eq!(lifecycle.state(), PierLifecycleState::Authenticating);

        let complete = NativeReadinessEvidence {
            input_verified: true,
            ..incomplete
        };
        lifecycle
            .apply_native_session_ready(complete)
            .expect("complete evidence");
        assert_eq!(lifecycle.state(), PierLifecycleState::PreparingSession);
    }

    #[test]
    fn stream_ready_cannot_skip_state_validation() {
        let mut lifecycle = PierLifecycle::new();
        assert!(matches!(
            lifecycle.apply_stream_ready(NativeReadinessEvidence {
                session_identity: true,
                outputs_verified: true,
                media_verified: true,
                input_verified: true,
            }),
            Err(LifecycleApplyError::InvalidTransition(_))
        ));
    }
}
