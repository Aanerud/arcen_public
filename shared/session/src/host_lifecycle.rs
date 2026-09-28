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

impl PierLifecycleState {
    /// Stable token for logs and telemetry.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Listening => "listening",
            Self::Authenticating => "authenticating",
            Self::PreparingSession => "preparing_session",
            Self::SessionReady => "session_ready",
            Self::Streaming => "streaming",
            Self::Recovering => "recovering",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
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
    /// The session is over: the client left, was refused after
    /// authentication, or its reconnect window closed. The Pier listens for
    /// the next one.
    SessionEnded,
    Shutdown,
    FatalFailure,
}

impl PierLifecycleEvent {
    /// Stable token for logs and telemetry.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NetworkReady => "network_ready",
            Self::AuthenticationAccepted => "authentication_accepted",
            Self::NativeSessionReady => "native_session_ready",
            Self::StreamReady => "stream_ready",
            Self::TransportLost => "transport_lost",
            Self::RecoveryComplete => "recovery_complete",
            Self::SessionEnded => "session_ended",
            Self::Shutdown => "shutdown",
            Self::FatalFailure => "fatal_failure",
        }
    }
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

    /// A lifecycle for one accepted connection on a Pier that is already
    /// listening. Each connection a Pier serves is tracked from here; the
    /// service-level `Starting -> Listening` step happened once, earlier.
    #[must_use]
    pub const fn listening() -> Self {
        Self {
            state: PierLifecycleState::Listening,
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
                PierLifecycleState::Authenticating
                | PierLifecycleState::PreparingSession
                | PierLifecycleState::SessionReady
                | PierLifecycleState::Streaming
                | PierLifecycleState::Recovering,
                PierLifecycleEvent::SessionEnded,
            ) => PierLifecycleState::Listening,
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

/// What one reported step did to a session's lifecycle, for the host to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleReport {
    /// The state before the step; `None` for a session not seen before.
    pub from: Option<PierLifecycleState>,
    /// The state after the step, or why the step was refused. A refused step
    /// leaves the state unchanged.
    pub to: Result<PierLifecycleState, LifecycleApplyError>,
}

/// The most sessions tracked at once. A Pier serves a handful; more than
/// this means ends are not being reported, and a new session is refused
/// tracking rather than growing the table without bound.
pub const MAX_TRACKED_SESSIONS: usize = 64;

/// Every live session's [`PierLifecycle`], keyed by the session's log id.
///
/// Hosts report the steps they already observe (authenticated, stream
/// started, transport lost, ended) at the places they already emit lifecycle
/// telemetry; the ordering and the readiness evidence rule are enforced
/// here, the same for every host.
#[derive(Debug, Default)]
pub struct SessionLifecycles {
    sessions: std::sync::Mutex<Vec<(String, PierLifecycle)>>,
}

impl SessionLifecycles {
    /// An empty table, usable as a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sessions: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn with<T>(&self, operation: impl FnOnce(&mut Vec<(String, PierLifecycle)>) -> T) -> T {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        operation(&mut sessions)
    }

    /// The session passed authentication (or resumed with a valid grant).
    pub fn authenticated(&self, session: &str) -> LifecycleReport {
        self.with(|sessions| {
            if let Some((_, lifecycle)) = sessions.iter_mut().find(|(id, _)| id == session) {
                let from = lifecycle.state();
                // A resume re-authenticates a session that is recovering.
                let to = if from == PierLifecycleState::Recovering {
                    Ok(from)
                } else {
                    lifecycle
                        .apply(PierLifecycleEvent::AuthenticationAccepted)
                        .map_err(LifecycleApplyError::InvalidTransition)
                };
                return LifecycleReport {
                    from: Some(from),
                    to,
                };
            }
            if sessions.len() >= MAX_TRACKED_SESSIONS {
                // A session abandoned before it streamed (a setup failure
                // that returned early) makes room; a live one never does.
                let abandoned = sessions.iter().position(|(_, lifecycle)| {
                    !matches!(
                        lifecycle.state(),
                        PierLifecycleState::Streaming | PierLifecycleState::Recovering
                    )
                });
                match abandoned {
                    Some(index) => {
                        sessions.remove(index);
                    }
                    None => {
                        return LifecycleReport {
                            from: None,
                            to: Err(LifecycleApplyError::InvalidTransition(
                                InvalidLifecycleTransition {
                                    state: PierLifecycleState::Listening,
                                    event: PierLifecycleEvent::AuthenticationAccepted,
                                },
                            )),
                        };
                    }
                }
            }
            let mut lifecycle = PierLifecycle::listening();
            let to = lifecycle
                .apply(PierLifecycleEvent::AuthenticationAccepted)
                .map_err(LifecycleApplyError::InvalidTransition);
            sessions.push((session.to_owned(), lifecycle));
            LifecycleReport { from: None, to }
        })
    }

    /// The session's media is flowing: `server_hello` was delivered and the
    /// native session, outputs, media and input are ready. Moves a fresh
    /// session to `Streaming` through both evidence gates, and a recovering
    /// one back to `Streaming`.
    pub fn stream_started(
        &self,
        session: &str,
        evidence: NativeReadinessEvidence,
    ) -> LifecycleReport {
        if !self.with(|sessions| sessions.iter().any(|(id, _)| id == session)) {
            // A host without authentication still reports its sessions.
            let _ = self.authenticated(session);
        }
        self.with(|sessions| {
            let Some((_, lifecycle)) = sessions.iter_mut().find(|(id, _)| id == session) else {
                return LifecycleReport {
                    from: None,
                    to: Err(LifecycleApplyError::InvalidTransition(
                        InvalidLifecycleTransition {
                            state: PierLifecycleState::Listening,
                            event: PierLifecycleEvent::StreamReady,
                        },
                    )),
                };
            };
            let from = lifecycle.state();
            let mut attempt = *lifecycle;
            let result = (|| {
                match attempt.state() {
                    PierLifecycleState::Authenticating => {
                        attempt.apply_native_session_ready(evidence)?;
                        attempt.apply_stream_ready(evidence)?;
                    }
                    PierLifecycleState::Recovering => {
                        attempt
                            .apply(PierLifecycleEvent::RecoveryComplete)
                            .map_err(LifecycleApplyError::InvalidTransition)?;
                    }
                    _ => {}
                }
                attempt.apply_stream_ready(evidence)
            })();
            if result.is_ok() {
                *lifecycle = attempt;
            }
            LifecycleReport {
                from: Some(from),
                to: result,
            }
        })
    }

    /// The transport failed and the session is held for a resume.
    pub fn transport_lost(&self, session: &str) -> LifecycleReport {
        self.step(session, PierLifecycleEvent::TransportLost, false)
    }

    /// The session is over; it is forgotten.
    pub fn ended(&self, session: &str) -> LifecycleReport {
        self.step(session, PierLifecycleEvent::SessionEnded, true)
    }

    fn step(&self, session: &str, event: PierLifecycleEvent, forget: bool) -> LifecycleReport {
        self.with(|sessions| {
            let Some(index) = sessions.iter().position(|(id, _)| id == session) else {
                return LifecycleReport {
                    from: None,
                    to: Err(LifecycleApplyError::InvalidTransition(
                        InvalidLifecycleTransition {
                            state: PierLifecycleState::Listening,
                            event,
                        },
                    )),
                };
            };
            let lifecycle = &mut sessions[index].1;
            let from = lifecycle.state();
            let to = lifecycle
                .apply(event)
                .map_err(LifecycleApplyError::InvalidTransition);
            if forget && to.is_ok() {
                sessions.swap_remove(index);
            }
            LifecycleReport {
                from: Some(from),
                to,
            }
        })
    }

    /// Sessions currently tracked, for diagnostics.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.with(|sessions| sessions.len())
    }
}

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
    fn a_pier_serves_one_session_after_another() {
        let evidence = NativeReadinessEvidence {
            session_identity: true,
            outputs_verified: true,
            media_verified: true,
            input_verified: true,
        };
        let mut lifecycle = PierLifecycle::listening();
        for _ in 0..2 {
            lifecycle
                .apply(PierLifecycleEvent::AuthenticationAccepted)
                .expect("accepted");
            lifecycle
                .apply_native_session_ready(evidence)
                .expect("native");
            lifecycle.apply_stream_ready(evidence).expect("prepared");
            lifecycle.apply_stream_ready(evidence).expect("streaming");
            lifecycle
                .apply(PierLifecycleEvent::SessionEnded)
                .expect("ended");
            assert_eq!(lifecycle.state(), PierLifecycleState::Listening);
        }
        // A session refused after authentication also ends.
        lifecycle
            .apply(PierLifecycleEvent::AuthenticationAccepted)
            .expect("accepted");
        lifecycle
            .apply(PierLifecycleEvent::SessionEnded)
            .expect("refused");
        assert_eq!(lifecycle.state(), PierLifecycleState::Listening);
        assert!(
            lifecycle.apply(PierLifecycleEvent::SessionEnded).is_err(),
            "nothing to end while listening"
        );
        assert_eq!(
            PierLifecycleState::PreparingSession.token(),
            "preparing_session"
        );
        assert_eq!(PierLifecycleEvent::SessionEnded.token(), "session_ended");
    }

    const READY: NativeReadinessEvidence = NativeReadinessEvidence {
        session_identity: true,
        outputs_verified: true,
        media_verified: true,
        input_verified: true,
    };

    #[test]
    fn the_session_table_follows_a_session_through_a_resume() {
        let table = SessionLifecycles::new();
        assert_eq!(
            table.authenticated("a").to,
            Ok(PierLifecycleState::Authenticating)
        );
        assert_eq!(
            table.stream_started("a", READY).to,
            Ok(PierLifecycleState::Streaming)
        );
        assert_eq!(
            table.transport_lost("a").to,
            Ok(PierLifecycleState::Recovering)
        );
        assert_eq!(
            table.authenticated("a").to,
            Ok(PierLifecycleState::Recovering),
            "a resume re-authenticates without restarting the lifecycle"
        );
        assert_eq!(
            table.stream_started("a", READY).to,
            Ok(PierLifecycleState::Streaming)
        );
        assert_eq!(table.ended("a").to, Ok(PierLifecycleState::Listening));
        assert_eq!(table.tracked(), 0);
    }

    #[test]
    fn the_session_table_enforces_the_evidence_gate() {
        let table = SessionLifecycles::new();
        let _ = table.authenticated("b");
        let incomplete = NativeReadinessEvidence {
            media_verified: false,
            ..READY
        };
        let report = table.stream_started("b", incomplete);
        assert_eq!(report.from, Some(PierLifecycleState::Authenticating));
        assert!(matches!(
            report.to,
            Err(LifecycleApplyError::IncompleteReadiness(_))
        ));
        assert_eq!(
            table.stream_started("b", READY).to,
            Ok(PierLifecycleState::Streaming),
            "a refused step changed nothing"
        );
        assert!(table.ended("unknown").to.is_err());
        assert!(table.transport_lost("unknown").to.is_err());
    }

    #[test]
    fn the_session_table_is_bounded_and_serves_unauthenticated_hosts() {
        let table = SessionLifecycles::new();
        assert_eq!(
            table.stream_started("open", READY).to,
            Ok(PierLifecycleState::Streaming)
        );
        for index in 1..MAX_TRACKED_SESSIONS {
            let _ = table.authenticated(&index.to_string());
        }
        assert_eq!(table.tracked(), MAX_TRACKED_SESSIONS);
        assert_eq!(
            table.authenticated("next").to,
            Ok(PierLifecycleState::Authenticating),
            "a session abandoned before streaming makes room"
        );
        assert_eq!(table.tracked(), MAX_TRACKED_SESSIONS);
        let live = SessionLifecycles::new();
        for index in 0..MAX_TRACKED_SESSIONS {
            let _ = live.stream_started(&index.to_string(), READY);
        }
        assert!(
            live.authenticated("one-too-many").to.is_err(),
            "streaming sessions are never evicted"
        );
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
