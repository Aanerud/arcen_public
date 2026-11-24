// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What every Deck shell does around the [`ReconnectController`]: the
//! connection identity a resume is bound to, recording the host's grant,
//! scheduling after a lost connection, issuing the due resume attempt and
//! the overlay the user sees meanwhile. The shells keep only what is
//! theirs: their own display enumeration, launching the session on their
//! window, and drawing [`crate::screens::reconnect::render_reconnecting`].

use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::connection_draft::ConnectionDraft;
use crate::reconnect::{
    ConnectionIdentity, ControllerError, JitterSource, MonotonicClock, ReconnectController,
    ReconnectPhase, ResumeAttempt, SystemClock,
};
use crate::runtime::session::{
    AuthenticationKind, ConnectOptions, SessionAuthentication, SessionEnd,
};
use crate::settings::SecurityMode;
use crate::sign_in::{SignInFailure, SignInStage};
use arcen_protocol::messages::{ClientMonitor, SafeAreaPolicyMsg};

/// The schedule that follows a host's sign-in screen into its desktop.
pub use arcen_session::login_window_handover::HandoverSchedule;

/// The resume identity of a connection: its endpoint, its security and its
/// display topology, hashed.
///
/// `options.monitors`/`options.multi_monitor_topology` are what was
/// negotiated at connect time and stay frozen in the reconnect snapshot;
/// `live_monitors`/`live_topology` are the Deck's displays now. A display
/// added, removed, moved, rotated or rescaled since the session attached
/// changes the identity, so the controller refuses to resume and the Deck
/// asks for a fresh connection instead.
#[must_use]
pub fn connection_identity_with_live_monitors(
    draft: &ConnectionDraft,
    options: &ConnectOptions,
    security_mode: SecurityMode,
    live_monitors: &[ClientMonitor],
    live_topology: Option<&arcen_media::RequestedMonitorTopology>,
    safe_area_policy: SafeAreaPolicyMsg,
) -> ConnectionIdentity {
    let security_material = format!(
        "{}:{}:{:?}",
        security_mode.key(),
        options.use_tls,
        options.tls
    );
    let topology_material = format!(
        "{:?}:{}:{}:{:?}:{:?}:{}:{:?}:{:?}",
        draft.kind,
        options.displays_mode,
        serde_json::to_string(&options.monitors).unwrap_or_default(),
        options.cursor_preference,
        options.tablet_mode_requested,
        serde_json::to_string(live_monitors).unwrap_or_default(),
        live_topology,
        safe_area_policy,
    );
    ConnectionIdentity {
        endpoint: format!("{}://{}:{}", "quic", options.host.trim(), options.port),
        security: format!("{:x}", Sha256::digest(security_material.as_bytes())),
        topology: format!("{:x}", Sha256::digest(topology_material.as_bytes())),
    }
}

/// The reconnect overlay's state: shown while the controller is
/// reconnecting or resuming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectOverlay {
    pub attempt: u32,
    /// When the next attempt starts (`None` while one is resuming).
    pub next_retry: Option<Duration>,
    pub deadline: Duration,
    pub last_error: String,
}

impl ReconnectOverlay {
    /// The overlay for `phase`, and whether it is resuming; `None` when no
    /// overlay is due (connected, cancelled or terminal).
    #[must_use]
    pub fn from_phase(phase: &ReconnectPhase) -> Option<(Self, bool)> {
        match phase {
            ReconnectPhase::Reconnecting {
                attempt,
                next_retry,
                deadline,
                last_error,
            } => Some((
                Self {
                    attempt: *attempt,
                    next_retry: Some(*next_retry),
                    deadline: *deadline,
                    last_error: last_error.clone(),
                },
                false,
            )),
            ReconnectPhase::Resuming {
                attempt,
                deadline,
                last_error,
            } => Some((
                Self {
                    attempt: *attempt,
                    next_retry: None,
                    deadline: *deadline,
                    last_error: last_error.clone(),
                },
                true,
            )),
            ReconnectPhase::Connected | ReconnectPhase::Cancelled | ReconnectPhase::Terminal => {
                None
            }
        }
    }

    /// Seconds until the next attempt, for "Retry in Ns".
    #[must_use]
    pub fn retry_left_secs(&self, now: Duration) -> Option<f32> {
        self.next_retry
            .map(|retry| retry.saturating_sub(now).as_secs_f32())
    }
}

/// Records the host's authentication outcome (initial grant, resumed
/// attachment or refreshed grant) in `controller`, taking the grant and the
/// session id out of `authentication`.
///
/// # Errors
///
/// The controller's error when the response does not fit its state; the
/// shell then ends the connection and asks for credentials.
pub fn accept_authentication(
    controller: &mut ReconnectController,
    authentication: &mut SessionAuthentication,
    now: Duration,
) -> Result<(), ControllerError> {
    let grant = authentication.resume_grant.take();
    let sid = std::mem::take(&mut authentication.session_log_id);
    match authentication.kind {
        AuthenticationKind::InitialOptIn => controller
            .accept_initial(
                grant,
                authentication.resume_window,
                authentication.resumed,
                sid,
            )
            .map(|_| ()),
        AuthenticationKind::Resume => controller.accept_resume(
            grant,
            authentication.resume_window,
            authentication.resumed,
            sid,
            now,
        ),
        AuthenticationKind::Refresh => controller.accept_refresh(
            grant,
            authentication.resume_window,
            authentication.resumed,
            sid,
        ),
    }
}

/// After a session of this controller's `generation` ended: schedules a
/// resume when the end was a lost transport and the controller holds a
/// grant. Returns whether one was scheduled; otherwise the shell ends the
/// connection as before.
pub fn schedule_after_end<J: JitterSource>(
    controller: Option<&mut ReconnectController>,
    generation: u64,
    end: &SessionEnd,
    clock: &SystemClock,
    jitter: &mut J,
) -> bool {
    if !end.transient() {
        return false;
    }
    controller
        .filter(|controller| controller.generation() == generation)
        .is_some_and(|controller| {
            controller
                .schedule(
                    end.message.clone(),
                    clock.observed_at(end.observed_at),
                    jitter,
                )
                .is_ok()
        })
}

/// Whether a hand-over from a host's sign-in screen follows this end into
/// the desktop that replaces it (the shells' `follow_login_window`, which
/// then asks the shared [`HandoverSchedule`] when).
///
/// The sign-in screen's session closing, a lost transport, a desktop that
/// is not ready yet, or a host still held by the sign-in screen's own
/// session are followed, at most [`HandoverSchedule`]'s attempts. A
/// deliberate disconnect, a host whose identity changed, and a definite
/// refusal ([`SignInFailure::is_definite`]: another account at the console
/// (`console_other_user`), a wrong password…) are not: waiting cannot change
/// them, and ADR 0025 never takes another account's session over. Each
/// hand-over attempt is a fresh sign-in, so the end is read as one.
#[must_use]
pub fn follows_login_window(end: &SessionEnd) -> bool {
    use crate::runtime::session::{DisconnectReason, TerminalDisconnect};
    if matches!(
        end.reason,
        DisconnectReason::Terminal(TerminalDisconnect::Manual | TerminalDisconnect::TlsIdentity)
    ) {
        return false;
    }
    SignInFailure::from_end(end, SignInStage::Attempt).is_none_or(|failure| !failure.is_definite())
}

/// When to reconnect after a followed end (`follows_login_window`) observed
/// at `now`, by the hand-over's `schedule`.
///
/// `None` once every attempt is used: no desktop replaced the sign-in screen,
/// and that is logged as the failure it is. Each end before it is the
/// expected hand-over, which the session engine logs as information.
pub fn login_window_reconnect_due(
    schedule: &mut HandoverSchedule,
    end: &SessionEnd,
    now: Duration,
) -> Option<Duration> {
    let due = schedule.session_ended(now);
    if due.is_none() {
        tracing::error!(
            target: crate::log_target::SESSION,
            attempts = schedule.attempts_made(),
            reason = %end.message,
            "no desktop replaced the host's sign-in screen; the hand-over stopped",
        );
    }
    due
}

/// What [`next_resume_launch`] decided.
#[derive(Debug)]
pub enum ResumeLaunch {
    /// Nothing to start now.
    Idle,
    /// Start this resume attempt with these credential-free options; the
    /// attempt must finish by `deadline`.
    Launch {
        options: Box<ConnectOptions>,
        attempt: ResumeAttempt,
        deadline: Instant,
    },
    /// The resume cannot continue (identity changed, window closed): the
    /// shell returns to its credentials screen.
    Abandon,
}

/// Takes the controller's due resume attempt, if any, against the live
/// `identity`, and prepares its launch from the reconnect snapshot.
pub fn next_resume_launch(
    controller: &mut ReconnectController,
    clock: &SystemClock,
    generation: u64,
    identity: &ConnectionIdentity,
    snapshot: Option<&ConnectOptions>,
) -> ResumeLaunch {
    let Ok(attempt) = controller.take_due(clock.now(), generation, identity) else {
        return ResumeLaunch::Abandon;
    };
    let Some(attempt) = attempt else {
        return ResumeLaunch::Idle;
    };
    let deadline = controller
        .resume_budget(clock.now())
        .ok()
        .and_then(|_| clock.instant_at(controller.resume_deadline().ok()?));
    let Some(deadline) = deadline else {
        return ResumeLaunch::Abandon;
    };
    let Some(options) = snapshot.map(|options| Box::new(options.credential_free_clone())) else {
        return ResumeLaunch::Abandon;
    };
    ResumeLaunch::Launch {
        options,
        attempt,
        deadline,
    }
}

/// Whether the controller is between a lost connection and its resume
/// (only then does it need a live identity).
#[must_use]
pub fn is_reconnecting(controller: Option<&ReconnectController>) -> bool {
    controller
        .is_some_and(|controller| matches!(controller.phase(), ReconnectPhase::Reconnecting { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::session::{DisconnectReason, TerminalDisconnect, TransientTransportError};

    struct Fixed;
    impl JitterSource for Fixed {
        fn inclusive(&mut self, minimum: Duration, _maximum: Duration) -> Option<Duration> {
            Some(minimum)
        }
    }

    fn options() -> ConnectOptions {
        ConnectOptions {
            display_refresh: None,
            services: crate::runtime::test_services(),
            host: "203.0.113.5".to_owned(),
            port: 18_444,
            use_tls: true,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(1),
            tls: crate::runtime::tls::TlsTrustConfig::default(),
            profile: crate::runtime::session::StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: arcen_protocol::messages::CursorMode::Local,
            clipboard_enabled: true,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: arcen_protocol::messages::TabletModeMsg::LocalTermination,
            telemetry: std::sync::Arc::new(crate::client_telemetry::ClientTelemetry::default()),
            exact_still_image: false,
            canvas: crate::canvas::CanvasRequest::Unavailable,
        }
    }

    fn identity() -> ConnectionIdentity {
        ConnectionIdentity {
            endpoint: "quic://203.0.113.5:18444".to_owned(),
            security: "s".to_owned(),
            topology: "t".to_owned(),
        }
    }

    fn granted(clock: &SystemClock) -> ReconnectController {
        let mut controller = ReconnectController::new(7, identity(), "nonce".to_owned());
        let mut authentication = SessionAuthentication {
            kind: AuthenticationKind::InitialOptIn,
            resume_grant: Some("grant".to_owned()),
            resume_window: Some(Duration::from_secs(60)),
            resumed: false,
            session_log_id: "sid".to_owned(),
        };
        accept_authentication(&mut controller, &mut authentication, clock.now()).unwrap();
        assert!(authentication.resume_grant.is_none());
        assert!(controller.has_credential());
        controller
    }

    fn end(reason: DisconnectReason) -> SessionEnd {
        SessionEnd {
            reason,
            message: "connection timed out".to_owned(),
            observed_at: Instant::now(),
        }
    }

    #[test]
    fn a_lost_transport_schedules_the_overlay_and_a_resume() {
        let clock = SystemClock::default();
        let mut controller = granted(&clock);
        let lost = end(DisconnectReason::Transient(
            TransientTransportError::TimedOut,
        ));
        assert!(!schedule_after_end(
            Some(&mut controller),
            8,
            &lost,
            &clock,
            &mut Fixed
        ));
        assert!(schedule_after_end(
            Some(&mut controller),
            7,
            &lost,
            &clock,
            &mut Fixed
        ));
        let (overlay, resuming) = ReconnectOverlay::from_phase(controller.phase()).unwrap();
        assert!(!resuming);
        assert_eq!(overlay.attempt, 1);
        assert_eq!(overlay.last_error, "connection timed out");
        assert!(is_reconnecting(Some(&controller)));

        std::thread::sleep(Duration::from_millis(150));
        let snapshot = options();
        match next_resume_launch(&mut controller, &clock, 7, &identity(), Some(&snapshot)) {
            ResumeLaunch::Launch { attempt, .. } => {
                assert_eq!(attempt.grant, "grant");
                assert_eq!(attempt.attempt, 1);
            }
            other => panic!("expected a launch, got {other:?}"),
        }
        let (_, resuming) = ReconnectOverlay::from_phase(controller.phase()).unwrap();
        assert!(resuming);
    }

    #[test]
    fn a_sign_in_screen_is_followed_but_never_into_another_account_s_console() {
        use crate::runtime::session::{ConnectSmokeError, session_end_for};
        use crate::sign_in::HostRefusal;
        use arcen_session::console_session::ConsoleRefusal;
        use arcen_session::login_window_handover::MAX_ATTEMPTS;

        let host_refused = |refusal, message: &str| {
            session_end_for(&ConnectSmokeError::HostRefused {
                refusal,
                message: message.to_owned(),
            })
        };
        // The sign-in screen's session closing, and a desktop not there yet.
        for end in [
            end(DisconnectReason::Terminal(
                TerminalDisconnect::GracefulHostClose,
            )),
            end(DisconnectReason::Transient(
                TransientTransportError::UnexpectedEof,
            )),
            session_end_for(&ConnectSmokeError::ClosedByHost(
                ": the desktop session at this Mac's console has no Arcen agent running yet"
                    .to_owned(),
            )),
            host_refused(
                HostRefusal::Console(ConsoleRefusal::NoConsoleSession),
                ConsoleRefusal::NoConsoleSession.deck_reason(),
            ),
        ] {
            assert!(follows_login_window(&end), "{}", end.message);
        }
        // Someone else at the console (the 07:07 lab hand-over), as a refused
        // sign-in or as the host closing: definite, never retried.
        let other_user = "console_other_user: flame_db is signed in at this Mac; sign in as \
                          flame_db, or switch to your account at the Mac first";
        let mut host_closed = end(DisconnectReason::Terminal(
            TerminalDisconnect::GracefulHostClose,
        ));
        host_closed.message = format!("Host closed the session: {other_user}");
        for end in [
            host_refused(HostRefusal::Console(ConsoleRefusal::OtherUser), other_user),
            session_end_for(&ConnectSmokeError::ClosedByHost(format!(": {other_user}"))),
            session_end_for(&ConnectSmokeError::AuthFailed(other_user.to_owned())),
            host_closed,
            // A wrong password would count against the host's throttle.
            session_end_for(&ConnectSmokeError::AuthFailed(
                "Authentication failed".to_owned(),
            )),
            end(DisconnectReason::Terminal(TerminalDisconnect::Manual)),
            end(DisconnectReason::Terminal(TerminalDisconnect::TlsIdentity)),
        ] {
            assert!(!follows_login_window(&end), "{}", end.message);
        }

        // A host still busy is followed, but only as far as the schedule goes.
        let busy = host_refused(
            HostRefusal::Busy,
            "another session is already using this Mac",
        );
        assert!(follows_login_window(&busy));
        let mut schedule = HandoverSchedule::new();
        let mut now = Duration::ZERO;
        let mut retries = 0;
        while follows_login_window(&busy)
            && let Some(due) = schedule.session_ended(now)
        {
            assert!(schedule.take_due(due));
            retries += 1;
            now = due;
        }
        assert_eq!(retries, usize::from(MAX_ATTEMPTS));
    }

    #[test]
    fn a_manual_end_or_a_changed_identity_never_resumes() {
        let clock = SystemClock::default();
        let mut controller = granted(&clock);
        let manual = end(DisconnectReason::Terminal(TerminalDisconnect::Manual));
        assert!(!schedule_after_end(
            Some(&mut controller),
            7,
            &manual,
            &clock,
            &mut Fixed
        ));
        let lost = end(DisconnectReason::Transient(
            TransientTransportError::TimedOut,
        ));
        assert!(schedule_after_end(
            Some(&mut controller),
            7,
            &lost,
            &clock,
            &mut Fixed
        ));
        let mut moved = identity();
        moved.topology = "another layout".to_owned();
        assert!(matches!(
            next_resume_launch(&mut controller, &clock, 7, &moved, None),
            ResumeLaunch::Abandon
        ));
        assert_eq!(controller.phase(), &ReconnectPhase::Terminal);
    }
}
